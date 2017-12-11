use std::rc::Rc;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::HashMap;
use rand::random;
use mio::net::TcpStream;

use sozu_command::messages::{Instance,BackendProtocol};
use network::{AppId,Backend,ConnectionError};
use network::socket::BackendSocket;

pub struct BackendMap {
  pub instances:    HashMap<AppId, BackendList>,
  pub max_failures: usize,
}

impl BackendMap {
  pub fn new() -> BackendMap {
    BackendMap {
      instances:    HashMap::new(),
      max_failures: 3,
    }
  }

  pub fn import_configuration_state(&mut self, instances: &HashMap<AppId, Vec<Instance>>) {
    self.instances.extend(instances.iter().map(|(ref app_id, ref instance_vec)| {
      (app_id.to_string(), BackendList::import_configuration_state(instance_vec))
    }));
  }

  pub fn add_instance(&mut self, app_id: &str, instance_id: &str, instance_address: &SocketAddr) {
    self.instances.entry(app_id.to_string()).or_insert(BackendList::new()).add_instance(instance_id, instance_address);
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr) {
    if let Some(instances) = self.instances.get_mut(app_id) {
      instances.remove_instance(instance_address);
    } else {
      error!("Instance was already removed: app id {}, address {:?}", app_id, instance_address);
    }
  }

  pub fn close_backend_connection(&mut self, app_id: &str, addr: &SocketAddr) {
    if let Some(app_instances) = self.instances.get_mut(app_id) {
      if let Some(ref mut backend) = app_instances.find_instance(addr) {
        (*backend.borrow_mut()).dec_connections();
      }
    }
  }

  pub fn has_backend(&self, app_id: &str, backend: &Backend) -> bool {
    self.instances.get(app_id).map(|backends| {
      backends.has_instance(&backend.address)
    }).unwrap_or(false)
  }

  pub fn backend_from_app_id(&mut self, app_id: &str, protocol: BackendProtocol, server_name: Option<&str>)
    -> Result<(Rc<RefCell<Backend>>,BackendSocket),ConnectionError> {

    if let Some(ref mut app_instances) = self.instances.get_mut(app_id) {
      if app_instances.instances.len() == 0 {
        return Err(ConnectionError::NoBackendAvailable);
      }

      for _ in 0..self.max_failures {
        if let Some(ref mut b) = app_instances.next_available_instance() {
          let ref mut backend = *b.borrow_mut();
          debug!("Connecting {} -> {:?}", app_id, (backend.address, backend.active_connections, backend.failures));
          let conn = backend.try_connect(protocol, server_name);
          if backend.failures >= MAX_FAILURES_PER_BACKEND {
            error!("backend {:?} connections failed {} times, disabling it", (backend.address, backend.active_connections), backend.failures);
          }

          return conn.map(|c| (b.clone(), c)).map_err(|e| {
            error!("could not connect {} to {:?} ({} failures)", app_id, backend.address, backend.failures);
            e
          });
        } else {
          error!("no more available backends for app {}", app_id);
          return Err(ConnectionError::NoBackendAvailable);
        }
      }
      Err(ConnectionError::NoBackendAvailable)
    } else {
      Err(ConnectionError::NoBackendAvailable)
    }
  }

  pub fn backend_from_sticky_session(&mut self, app_id: &str, sticky_session: u32, protocol: BackendProtocol,
    server_name: Option<&str>) -> Result<(Rc<RefCell<Backend>>,BackendSocket),ConnectionError> {

    let sticky_conn: Option<Result<(Rc<RefCell<Backend>>,BackendSocket),ConnectionError>> = self.instances
      .get_mut(app_id)
      .and_then(|app_instances| app_instances.find_sticky(sticky_session))
      .map(|b| {
        let ref mut backend = *b.borrow_mut();
        let conn = backend.try_connect(protocol, server_name);
        info!("Connecting {} -> {:?} using session {}", app_id, (backend.address, backend.active_connections, backend.failures), sticky_session);
        if backend.failures >= MAX_FAILURES_PER_BACKEND {
          error!("backend {:?} connections failed {} times, disabling it", (backend.address, backend.active_connections), backend.failures);
        }

        conn.map(|c| (b.clone(), c)).map_err(|e| {
          error!("could not connect {} to {:?} using session {} ({} failures)",
            app_id, backend.address, sticky_session, backend.failures);
          e
        })
      });

    if let Some(res) = sticky_conn {
      return res;
    } else {
      debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
      return self.backend_from_app_id(app_id, protocol, server_name);
    }
  }
}

const MAX_FAILURES_PER_BACKEND: usize = 10;

pub struct BackendList {
  pub instances: Vec<Rc<RefCell<Backend>>>,
  pub next_id:   u32,
}

impl BackendList {
  pub fn new() -> BackendList {
    BackendList {
      instances: Vec::new(),
      next_id:   0,
    }
  }

  pub fn import_configuration_state(instance_vec: &Vec<Instance>) -> BackendList {
    let mut list = BackendList::new();
    for ref instance in instance_vec {
      let addr_string = instance.ip_address.to_string() + ":" + &instance.port.to_string();
      let parsed:Option<SocketAddr> = addr_string.parse().ok();
      if let Some(addr) = parsed {
        list.add_instance(&instance.instance_id, &addr);
      }
    }

    list
  }

  pub fn add_instance(&mut self, instance_id: &str, instance_address: &SocketAddr) {
    if self.instances.iter().find(|b| &(*b.borrow()).address == instance_address).is_none() {
      let backend = Rc::new(RefCell::new(Backend::new(instance_id, *instance_address, self.next_id)));
      self.instances.push(backend);
      self.next_id += 1;
    }
  }

  pub fn remove_instance(&mut self, instance_address: &SocketAddr) {
    self.instances.retain(|backend| &(*backend.borrow()).address != instance_address);
  }

  pub fn has_instance(&self, instance_address: &SocketAddr) -> bool {
    self.instances.iter().any(|backend| &(*backend.borrow()).address == instance_address)
  }

  pub fn find_instance(&mut self, instance_address: &SocketAddr) -> Option<&mut Rc<RefCell<Backend>>> {
    self.instances.iter_mut().find(|backend| &(*backend.borrow()).address == instance_address)
  }

  pub fn find_sticky(&mut self, sticky_session: u32) -> Option<&mut Rc<RefCell<Backend>>> {
    self.instances.iter_mut()
      .find(|b| b.borrow().id == sticky_session )
      .and_then(|b| {
        if b.borrow().can_open() {
          Some(b)
        } else {
          None
        }
      })
  }

  pub fn available_instances(&mut self) -> Vec<&mut Rc<RefCell<Backend>>> {
    self.instances.iter_mut()
      .filter(|backend| (*backend.borrow()).can_open())
      .collect()
  }

  pub fn next_available_instance(&mut self) -> Option<&mut Rc<RefCell<Backend>>> {
    let mut instances:Vec<&mut Rc<RefCell<Backend>>> = self.available_instances();
    if instances.is_empty() {
      return None;
    }

    let rnd = random::<usize>();
    let idx = rnd % instances.len();

    Some(instances.remove(idx))
  }
}
