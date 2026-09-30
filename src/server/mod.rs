//! HTTP/2 server services.

use std::{error::Error, io};

mod service;
pub use self::service::{Server, handle_one};

use crate::frame;

/// Errors that can occur while establishing or serving an HTTP/2 connection.
#[derive(thiserror::Error, Debug)]
pub enum ServerError<Err> {
    /// Control service error.
    ///
    /// Request handler errors are passed to the control service as
    /// [`Control::Disconnect`](crate::Control::Disconnect) and close the
    /// connection. This error is returned only if the control service
    /// itself fails.
    #[error("Control service error")]
    Service(Err),
    /// HTTP/2 frame codec error.
    #[error("Http/2 codec error: {0}")]
    Frame(#[from] frame::FrameError),
    /// Request service initialization error.
    #[error("Publish service init error")]
    PublishService(Box<dyn Error>),
    /// Handshake timeout
    #[error("Handshake timeout")]
    HandshakeTimeout,
    /// Peer disconnection.
    #[error("Peer is disconnected, error: {0:?}")]
    Disconnected(Option<io::Error>),
}

impl<Err> From<io::Error> for ServerError<Err> {
    fn from(e: io::Error) -> Self {
        ServerError::Disconnected(Some(e))
    }
}
