//! Http2 client

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
    Protocol(#[from] ProtocolError),
    /// Handshake timeout
    #[error("Handshake timeout")]
    HandshakeTimeout,
    /// Peer disconnected
    #[error("Peer disconnected err: {0:?}")]
    Disconnected(Option<std::io::Error>),
    /// Connect error
    #[error("Connect error: {0}")]
    Connect(#[from] ntex::connect::ConnectError),
}
