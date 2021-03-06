//! Http2 client

#[allow(clippy::module_inception)]
mod client;
mod connector;

use crate::{error::ConnectionError, frame};

pub use self::client::{Client, ClientConnection};
pub use self::connector::Connector;

/// Errors which can occur when attempting to handle http2 client connection.
#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    /// Protocol error
    #[error("Protocol error: {0}")]
    Protocol(Box<ConnectionError>),
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
    #[error("Peer disconnected err: {0:?}")]
    Disconnected(Option<std::io::Error>),
}

impl From<ConnectionError> for ClientError {
    fn from(err: ConnectionError) -> Self {
        Self::Protocol(Box::new(err))
    }
}

impl From<std::io::Error> for ClientError {
    fn from(err: std::io::Error) -> Self {
        Self::Disconnected(Some(err))
    }
}

impl From<ntex_connect::ConnectError> for ClientError {
    fn from(err: ntex_connect::ConnectError) -> Self {
        Self::Connect(Box::new(err))
    }
}
