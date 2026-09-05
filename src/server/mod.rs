use std::{error::Error, io};

mod service;
pub use self::service::{Server, handle_one};

use crate::frame;

/// Errors which can occur when attempting to handle amqp connection.
#[derive(thiserror::Error, Debug)]
pub enum ServerError<Err> {
    /// Request handler error
    #[error("Message handler service error")]
    Service(Err),
    /// Http/2 frame codec error
    #[error("Http/2 codec error: {0}")]
    Frame(#[from] frame::FrameError),
    /// Publish service init error
    #[error("Publish service init error")]
    PublishService(Box<dyn Error>),
    /// Handshake timeout
    #[error("Handshake timeout")]
    HandshakeTimeout,
    /// Peer disconnect
    #[error("Peer is disconnected, error: {0:?}")]
    Disconnected(Option<io::Error>),
}

impl<Err> From<io::Error> for ServerError<Err> {
    fn from(e: io::Error) -> Self {
        ServerError::Disconnected(Some(e))
    }
}
