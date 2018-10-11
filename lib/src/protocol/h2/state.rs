use super::{parser, serializer};
use nom::Offset;
use std::collections::VecDeque;
use mio::Ready;
use mio::unix::UnixReady;
use hpack::Decoder;
use std::str::from_utf8;

#[derive(Clone,Debug,PartialEq)]
pub struct OutputFrame {
  header: parser::FrameHeader,
  payload: Option<Vec<u8>>,
}

#[derive(Clone,Debug,PartialEq)]
pub enum St {
  Init,
  ClientPrefaceReceived,
  ServerPrefaceSent,
}

#[derive(Clone,Debug,PartialEq)]
pub struct State {
  pub output: VecDeque<OutputFrame>,
  pub state: St,
  pub interest: UnixReady,
  //FIXME: make it configurable,
  pub max_frame_size: u32,
}

impl State {
  pub fn new() -> State {
    State {
      output: VecDeque::new(),
      state: St::Init,
      interest: UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error(),
      max_frame_size: 16384,
    }
  }

  pub fn parse<'a>(&mut self, mut input: &'a [u8]) -> (usize, Result<parser::Frame<'a>, ()>) {
    let mut consumed = 0usize;

    if self.state == St::Init {
      match parser::preface(input) {
        Err(e) => {
          error!("parser::preface error: {:?}", e);
          return (0, Err(()));
        },
        Ok((i, _)) => {
          consumed += input.offset(i);
          self.state = St::ClientPrefaceReceived;
          input = i;
        }
      }
    }


    match parser::frame(input, self.max_frame_size) {
      Err(e) => {
        error!("parser::frame error: {:?}", e);
        return (consumed, Err(()));
      },
      Ok((i, frame)) => {
        consumed += input.offset(i);
        (consumed, Ok(frame))
      }
    }
  }

  pub fn handle(&mut self, frame: &parser::Frame) -> bool {
    match self.state {
      St::Init => true,
      St::ClientPrefaceReceived => {
        match frame {
          parser::Frame::Settings(s) => {
            let server_settings = OutputFrame {
              header: parser::FrameHeader {
                payload_len: 0,
                frame_type: parser::FrameType::Settings,
                //FIXME: setting 1 for ACK?
                flags: 1,
                stream_id: 0,
              },
              payload: None,
            };

            self.output.push_back(server_settings);
            self.state = St::ServerPrefaceSent;
            self.interest.insert(UnixReady::from(Ready::writable()));
            true
          },
          f => {
            unimplemented!("invalid frame: {:?}, should send back an error", f);
          }
        }
      },
      St::ServerPrefaceSent => {
        match frame {
          parser::Frame::Headers(h) => {
            let mut decoder = Decoder::new();
            match decoder.decode(h.header_block_fragment) {
              Err(e) => {
                error!("error decoding headers: {:?}", e);
              },
              Ok(h) => {
                info!("got header list: {:?}", h);
                for header in &h {
                  info!("{}: {}",
                    from_utf8(&header.0).unwrap(), from_utf8(&header.1).unwrap());
                }
              }
            };

            false
          },
          frame => {
            panic!("unknown frame for now: {:?}", frame);
          }
        }

      }
    }
  }

  pub fn parse_and_handle<'a>(&mut self, mut input: &'a [u8]) -> (usize, bool) {
    let (sz, res) = self.parse(input);
    match res {
      Err(e) => {
        error!("error parsing frame: {:?}", e);
        (sz, false)
      },
      Ok(frame) => {
        info!("parsed frame: {:?}", frame);
        (sz, self.handle(&frame))
      }
    }
  }

  pub fn gen(&mut self, mut output: &mut [u8]) -> Result<usize, ()> {
    if let Some(frame) = self.output.pop_front() {
      match serializer::gen_frame_header((output, 0), &frame.header) {
        Err(e) => {
          panic!("error serializing: {:?}", e);
        },
        Ok((sl, index)) => {
          Ok(index)
        }
      }
    } else {
      self.interest.remove(Ready::writable());
      Ok(0)
    }
  }
}