//! HTTP/2 clients and connections pool.
//!
//! [`SimpleClient`] manages one established HTTP/2 connection. [`Client`]
//! maintains a pool of connections, while [`Connector`] adapts an ntex
//! connector service into a `SimpleClient`.
use std::io;

use ntex_error::ErrorDiagnostic;
use ntex_net::connect::ConnectError;
use ntex_util::channel::Canceled;

mod connector;
mod pool;
mod simple;
mod stream;

use crate::{error::ConnectionError, error::OperationError, frame};

pub use self::connector::Connector;
pub use self::pool::{Client, ClientBuilder};
pub use self::simple::{ClientDisconnect, SimpleClient, StreamReservation};
pub use self::stream::{RecvStream, SendStream};

/// Errors that can occur while establishing or operating an HTTP/2 client.
#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    /// Connection-level protocol error.
    #[error("Protocol error")]
    Protocol(#[source] ConnectionError),
    /// Stream or connection operation failed, see [`OperationError`].
    #[error("Operation error")]
    Operation(
        #[from]
        #[source]
        OperationError,
    ),
    /// HTTP/2 frame codec error.
    #[error("Http/2 codec error: {0}")]
    Frame(#[from] frame::FrameError),
    /// The connection was not established within the handshake timeout.
    #[error("Handshake timeout")]
    HandshakeTimeout,
    /// The transport connection failed.
    #[error("Connect error")]
    Connect(
        #[from]
        #[source]
        ConnectError,
    ),
    /// The peer disconnected, or the connection was dropped.
    #[error("Peer disconnected")]
    Disconnected(
        #[from]
        #[source]
        io::Error,
    ),
}

impl From<ConnectionError> for ClientError {
    fn from(err: ConnectionError) -> Self {
        Self::Protocol(err)
    }
}

impl From<Canceled> for ClientError {
    fn from(err: Canceled) -> Self {
        Self::Disconnected(io::Error::other(err))
    }
}

impl Clone for ClientError {
    fn clone(&self) -> Self {
        match self {
            Self::Protocol(err) => Self::Protocol(*err),
            Self::Operation(err) => Self::Operation(*err),
            Self::Frame(err) => Self::Frame(*err),
            Self::HandshakeTimeout => Self::HandshakeTimeout,
            Self::Connect(err) => Self::Connect(err.clone()),
            Self::Disconnected(err) => Self::Disconnected(io::Error::new(err.kind(), format!("{err}"))),
        }
    }
}

impl ErrorDiagnostic for ClientError {
    fn signature(&self) -> &'static str {
        match self {
            ClientError::Protocol(err) => err.signature(),
            ClientError::Operation(err) => err.signature(),
            ClientError::Connect(err) => err.signature(),
            ClientError::Disconnected(err) => err.signature(),
            ClientError::Frame(_) => "h2-client-Frame",
            ClientError::HandshakeTimeout => "h2-client-HandshakeTimeout",
        }
    }
}
