mod service;
pub use self::service::{handle_one, Server, ServerHandler};

use crate::frame;

/// Errors which can occur when attempting to handle amqp connection.
#[derive(thiserror::Error, Debug)]
pub enum ServerError<E> {
    /// Request handler error
    #[error("Message handler service error")]
    Service(E),
    /// Http/2 frame codec error
    #[error("Http/2 codec error: {0}")]
    Frame(#[from] frame::FrameError),
    /// Dispatcher error
    #[error("Dispatcher error")]
    Dispatcher,
    /// Control service init error
    #[error("Control service init error")]
    ControlServiceError,
    /// Publish service init error
    #[error("Publish service init error")]
    PublishServiceError,
    /// Handshake timeout
    #[error("Handshake timeout")]
    HandshakeTimeout,
    /// Peer disconnect
    #[error("Peer is disconnected, error: {0:?}")]
    Disconnected(Option<std::io::Error>),
}

impl<E> From<std::io::Error> for ServerError<E> {
    fn from(err: std::io::Error) -> Self {
        ServerError::Disconnected(Some(err))
    }
}
