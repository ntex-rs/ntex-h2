use crate::frame;

/// Errors which can occur when attempting to handle amqp connection.
#[derive(Debug)]
pub enum ServerError<E> {
    // #[display(fmt = "Message handler service error")]
    /// Request handler error
    Service(E),
    /// Http/2 frame codec error
    // #[display(fmt = "Http/2 codec error: {:?}", _0)]
    Frame(frame::Error),
    // /// Amqp protocol error
    // #[display(fmt = "Amqp protocol error: {:?}", _0)]
    // Protocol(AmqpProtocolError),
    // /// Dispatcher error
    // Dispatcher(AmqpDispatcherError),
    Dispatcher,
    /// Control service init error
    // #[display(fmt = "Control service init error")]
    ControlServiceError,
    /// Publish service init error
    // #[display(fmt = "Publish service init error")]
    PublishServiceError,
    /// Hadnshake timeout
    HandshakeTimeout,
    /// Peer disconnect
    // #[display(fmt = "Peer is disconnected, error: {:?}", _0)]
    Disconnected(Option<std::io::Error>),
}

impl<E> From<std::io::Error> for ServerError<E> {
    fn from(err: std::io::Error) -> Self {
        ServerError::Disconnected(Some(err))
    }
}
