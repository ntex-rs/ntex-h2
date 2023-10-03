//! Http2 client

#[allow(clippy::module_inception)]
mod client;
mod connector;
mod pool;
mod stream;

use crate::{error::ConnectionError, error::OperationError, frame};

pub use self::client::Client;
pub use self::connector::Connector;
pub use self::pool::{Pool, PoolBuilder};
pub use self::stream::{RecvStream, SendStream};

/// Errors which can occur when attempting to handle http2 client connection.
#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    /// Protocol error
    #[error("Protocol error: {0}")]
    Protocol(Box<ConnectionError>),
    /// Operation error
    #[error("Operation error: {0}")]
    Operation(#[from] OperationError),
    /// Http/2 frame codec error
    #[error("Http/2 codec error: {0}")]
    Frame(#[from] frame::FrameError),
    /// Handshake timeout
    #[error("Handshake timeout")]
    HandshakeTimeout,
    /// Connect error
    #[error("Connect error: {0}")]
    Connect(Box<ntex_connect::ConnectError>),
    /// Peer disconnected
    #[error("Peer disconnected err: {0}")]
    Disconnected(#[from] std::io::Error),
}

impl From<ConnectionError> for ClientError {
    fn from(err: ConnectionError) -> Self {
        Self::Protocol(Box::new(err))
    }
}

impl From<ntex_util::channel::Canceled> for ClientError {
    fn from(err: ntex_util::channel::Canceled) -> Self {
        Self::Disconnected(std::io::Error::new(std::io::ErrorKind::Other, err))
    }
}

impl From<ntex_connect::ConnectError> for ClientError {
    fn from(err: ntex_connect::ConnectError) -> Self {
        Self::Connect(Box::new(err))
    }
}

impl Clone for ClientError {
    fn clone(&self) -> Self {
        match self {
            Self::Protocol(err) => Self::Protocol(err.clone()),
            Self::Operation(err) => Self::Operation(err.clone()),
            Self::Frame(err) => Self::Frame(*err),
            Self::HandshakeTimeout => Self::HandshakeTimeout,
            Self::Connect(err) => Self::Connect(err.clone()),
            Self::Disconnected(err) => {
                Self::Disconnected(std::io::Error::new(err.kind(), format!("{}", err)))
            }
        }
    }
}

#[cfg(feature = "unstable")]
pub trait Observer {
    /// New request is prepared
    fn on_request(&mut self, id: frame::StreamId, headers: &mut frame::Headers);

    /// Bytes has been written to memory
    fn on_request_sent(&mut self, id: frame::StreamId);

    /// Payload data has been written to memory
    fn on_request_payload(&mut self, id: frame::StreamId, data: &frame::Data);

    /// Response is received
    fn on_response(&mut self, id: frame::StreamId, headers: &mut frame::Headers);

    /// Payload data has been received
    fn on_response_payload(&mut self, id: frame::StreamId, data: &frame::Data);
}
