//! Http2 client
use std::io;

use ntex_error::{ErrorDiagnostic, ResultType};
use ntex_net::connect::ConnectError;
use ntex_util::channel::Canceled;

mod connector;
mod pool;
mod simple;
mod stream;

use crate::{error::ConnectionError, error::OperationError, frame};

pub use self::connector::Connector;
pub use self::pool::{Client, ClientBuilder};
pub use self::simple::SimpleClient;
pub use self::stream::{RecvStream, SendStream};

/// Errors which can occur when attempting to handle http2 client connection.
#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    /// Protocol error
    #[error("Protocol error")]
    Protocol(#[source] ConnectionError),
    /// Operation error
    #[error("Operation error")]
    Operation(
        #[from]
        #[source]
        OperationError,
    ),
    /// Http/2 frame codec error
    #[error("Http/2 codec error: {0}")]
    Frame(#[from] frame::FrameError),
    /// Handshake timeout
    #[error("Handshake timeout")]
    HandshakeTimeout,
    /// Connect error
    #[error("Connect error")]
    Connect(
        #[from]
        #[source]
        ConnectError,
    ),
    /// Peer disconnected
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
            Self::Operation(err) => Self::Operation(err.clone()),
            Self::Frame(err) => Self::Frame(*err),
            Self::HandshakeTimeout => Self::HandshakeTimeout,
            Self::Connect(err) => Self::Connect(err.clone()),
            Self::Disconnected(err) => {
                Self::Disconnected(io::Error::new(err.kind(), format!("{err}")))
            }
        }
    }
}

impl ErrorDiagnostic for ClientError {
    fn typ(&self) -> ResultType {
        ResultType::ServiceError
    }

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
