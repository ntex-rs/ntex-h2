use ntex_bytes::Bytes;
use ntex_error::Error;
use ntex_http::HeaderMap;

use crate::error::{OperationError, StreamError};
use crate::frame::{PseudoHeaders, StreamId};
use crate::stream::{Capacity, StreamRef};

/// Message delivered by an HTTP/2 connection dispatcher.
#[derive(Debug)]
pub struct Message {
    /// Stream associated with this message.
    pub stream: StreamRef,
    /// Message payload or lifecycle event.
    pub kind: MessageKind,
}

/// HTTP/2 stream message.
#[derive(Debug)]
pub enum MessageKind {
    /// Initial request or response headers.
    Headers {
        /// HTTP/2 pseudo-headers.
        pseudo: PseudoHeaders,
        /// Regular HTTP headers.
        headers: HeaderMap,
        /// Whether the peer closed its send side with these headers.
        eof: bool,
    },
    /// Payload bytes and their receive-window capacity.
    Data(Bytes, Capacity),
    /// End-of-stream data, trailers, or error.
    Eof(StreamEof),
    /// Connection-level failure affecting the stream.
    Disconnect(Error<OperationError>),
}

/// Final event for an HTTP/2 stream.
#[derive(Debug, Clone)]
pub enum StreamEof {
    /// Final payload bytes.
    Data(Bytes),
    /// Trailing headers.
    Trailers(HeaderMap),
    /// Stream-level error.
    Error(Error<StreamError>),
}

impl Message {
    pub(crate) fn new(pseudo: PseudoHeaders, headers: HeaderMap, eof: bool, stream: &StreamRef) -> Self {
        Message {
            stream: stream.clone(),
            kind: MessageKind::Headers { pseudo, headers, eof },
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

    pub(crate) fn error(err: Error<StreamError>, stream: &StreamRef) -> Self {
        Message {
            stream: stream.clone(),
            kind: MessageKind::Eof(StreamEof::Error(err)),
        }
    }

    pub(crate) fn disconnect(err: Error<OperationError>, stream: StreamRef) -> Self {
        Message {
            stream,
            kind: MessageKind::Disconnect(err),
        }
    }

    /// Returns the associated stream identifier.
    #[inline]
    pub fn id(&self) -> StreamId {
        self.stream.id()
    }

    /// Returns the message kind.
    #[inline]
    pub fn kind(&self) -> &MessageKind {
        &self.kind
    }

    /// Returns the associated stream.
    #[inline]
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }
}
