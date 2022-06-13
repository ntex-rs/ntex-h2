use std::mem;

use ntex_bytes::Bytes;

use crate::connection::Stream;
use crate::frame::Reason;
use crate::request::Request;

#[derive(Debug)]
pub struct Message {
    stream: Stream,
    kind: MessageKind,
}

#[derive(Debug)]
pub enum MessageKind {
    Request { req: Request, eof: bool },
    Data(Bytes),
    DataEof(Bytes),
    Reset(Reason),
    Empty,
}

impl Message {
    pub(crate) fn new(request: Request, eof: bool, stream: &Stream) -> Self {
        Message {
            stream: stream.clone(),
            kind: MessageKind::Request { req: request, eof },
        }
    }

    pub(crate) fn data(data: Bytes, eof: bool, stream: &Stream) -> Self {
        if eof {
            Message {
                stream: stream.clone(),
                kind: MessageKind::DataEof(data),
            }
        } else {
            Message {
                stream: stream.clone(),
                kind: MessageKind::Data(data),
            }
        }
    }

    pub fn kind(&mut self) -> &mut MessageKind {
        &mut self.kind
    }

    pub fn stream(&self) -> &Stream {
        &self.stream
    }
}

impl MessageKind {
    pub fn take(&mut self) -> MessageKind {
        mem::replace(self, MessageKind::Empty)
    }
}
