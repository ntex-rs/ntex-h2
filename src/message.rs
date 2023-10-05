use ntex_bytes::Bytes;
use ntex_http::HeaderMap;

use crate::error::{OperationError, StreamError};
use crate::frame::{PseudoHeaders, StreamId};
use crate::stream::{Capacity, StreamRef};

#[derive(Debug)]
pub struct Message {
    pub stream: StreamRef,
    pub kind: MessageKind,
}

#[derive(Debug)]
pub enum MessageKind {
    Headers {
        pseudo: PseudoHeaders,
        headers: HeaderMap,
        eof: bool,
    },
    Data(Bytes, Capacity),
    Eof(StreamEof),
    Disconnect(OperationError),
}

#[derive(Debug, Clone)]
pub enum StreamEof {
    Data(Bytes),
    Trailers(HeaderMap),
    Error(StreamError),
}

impl Message {
    pub(crate) fn new(
        pseudo: PseudoHeaders,
        headers: HeaderMap,
        eof: bool,
        stream: &StreamRef,
    ) -> Self {
        Message {
            stream: stream.clone(),
            kind: MessageKind::Headers {
                pseudo,
                headers,
                eof,
            },
        }
    }

    pub(crate) fn data(data: Bytes, capacity: Capacity, stream: &StreamRef) -> Self {
        Message {
            stream: stream.clone(),
            kind: MessageKind::Data(data, capacity),
        }
    }

    pub(crate) fn eof_data(data: Bytes, stream: &StreamRef) -> Self {
        Message {
            stream: stream.clone(),
            kind: MessageKind::Eof(StreamEof::Data(data)),
        }
    }

    pub(crate) fn trailers(hdrs: HeaderMap, stream: &StreamRef) -> Self {
        Message {
            stream: stream.clone(),
            kind: MessageKind::Eof(StreamEof::Trailers(hdrs)),
        }
    }

    pub(crate) fn error(err: StreamError, stream: &StreamRef) -> Self {
        Message {
            stream: stream.clone(),
            kind: MessageKind::Eof(StreamEof::Error(err)),
        }
    }

    pub(crate) fn disconnect(err: OperationError, stream: StreamRef) -> Self {
        Message {
            stream,
            kind: MessageKind::Disconnect(err),
        }
    }

    #[inline]
    pub fn id(&self) -> StreamId {
        self.stream.id()
    }

    #[inline]
    pub fn kind(&self) -> &MessageKind {
        &self.kind
    }

    #[inline]
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }
}
