//! Http2 client

#[allow(clippy::module_inception)]
mod client;
mod connector;

use crate::error::ProtocolError;

pub use self::client::{Client, ClientConnection};
pub use self::connector::Connector;

/// Errors which can occur when attempting to handle http2 client connection.
#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    /// Protocol error
    #[error("Protocol error: {0}")]
    Protocol(Box<ProtocolError>),
    /// Handshake timeout
    #[error("Handshake timeout")]
    HandshakeTimeout,
    /// Peer disconnected
    #[error("Peer disconnected err: {0:?}")]
    Disconnected(Option<std::io::Error>),
    /// Connect error
    #[error("Connect error: {0}")]
    Connect(Box<ntex_connect::ConnectError>),
}

impl From<ProtocolError> for ClientError {
    fn from(err: ProtocolError) -> Self {
        Self::Protocol(Box::new(err))
    }
}

impl From<ntex_connect::ConnectError> for ClientError {
    fn from(err: ntex_connect::ConnectError) -> Self {
        Self::Connect(Box::new(err))
    }
}
