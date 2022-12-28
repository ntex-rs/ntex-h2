use std::io;

use crate::frame::{Frame, Reason, Reset};
use crate::{error, frame, stream::StreamRef};

#[derive(Debug)]
pub enum ControlMessage<E> {
    /// Application level error from publish service
    AppError(AppError<E>),
    /// Protocol level error
    ConnectionError(ConnectionError),
    /// Remote GoAway is received
    GoAway(GoAway),
    /// Peer is gone
    PeerGone(PeerGone),
    /// Protocol dispatcher is terminated
    Terminated(Terminated),
}

pub struct ControlResult {
    pub(crate) frame: Option<Frame>,
    pub(crate) disconnect: bool,
}

impl<E> ControlMessage<E> {
    /// Create a new `ControlMessage` for app level errors
    pub(super) fn app_error(err: E, stream: StreamRef) -> Self {
        ControlMessage::AppError(AppError::new(err, stream))
    }

    /// Create a new `ControlMessage` from GOAWAY packet.
    pub(super) fn go_away(frm: frame::GoAway) -> Self {
        ControlMessage::GoAway(GoAway(frm))
    }

    /// Create a new `ControlMessage` from DISCONNECT packet.
    pub(super) fn peer_gone(err: Option<io::Error>) -> Self {
        ControlMessage::PeerGone(PeerGone(err))
    }

    pub(super) fn terminated() -> Self {
        ControlMessage::Terminated(Terminated)
    }

    /// Create a new `ControlMessage` for protocol level errors
    pub(super) fn proto_error(err: error::ConnectionError) -> Self {
        ControlMessage::ConnectionError(ConnectionError::new(err))
    }

    /// Default ack impl
    pub fn ack(self) -> ControlResult {
        match self {
            ControlMessage::AppError(item) => item.ack(),
            ControlMessage::ConnectionError(item) => item.ack(),
            ControlMessage::GoAway(item) => item.ack(),
            ControlMessage::PeerGone(item) => item.ack(),
            ControlMessage::Terminated(item) => item.ack(),
        }
    }
}

/// Service level error
#[derive(Debug)]
pub struct AppError<E> {
    err: E,
    reason: Reason,
    stream: StreamRef,
}

impl<E> AppError<E> {
    fn new(err: E, stream: StreamRef) -> Self {
        Self {
            err,
            stream,
            reason: Reason::CANCEL,
        }
    }

    #[inline]
    /// Returns reference to mqtt error
    pub fn get_ref(&self) -> &E {
        &self.err
    }

    #[inline]
    /// Set reason code for go away packet
    pub fn reason(mut self, reason: Reason) -> Self {
        self.reason = reason;
        self
    }

    #[inline]
    /// Ack service error, return disconnect packet and close connection.
    pub fn ack(self) -> ControlResult {
        ControlResult {
            frame: Some(Reset::new(self.stream.id(), self.reason).into()),
            disconnect: false,
        }
    }
}

/// Dispatcher has been terminated
#[derive(Debug)]
pub struct Terminated;

impl Terminated {
    #[inline]
    /// convert packet to a result
    pub fn ack(self) -> ControlResult {
        ControlResult {
            frame: None,
            disconnect: true,
        }
    }
}

/// Protocol level error
#[derive(Debug)]
pub struct ConnectionError {
    err: error::ConnectionError,
    frm: frame::GoAway,
}

impl ConnectionError {
    pub fn new(err: error::ConnectionError) -> Self {
        Self {
            frm: err.to_goaway(),
            err,
        }
    }

    #[inline]
    /// Returns reference to a protocol error
    pub fn get_ref(&self) -> &error::ConnectionError {
        &self.err
    }

    #[inline]
    /// Set reason code for go away packet
    pub fn reason(mut self, reason: Reason) -> Self {
        self.frm = self.frm.set_reason(reason);
        self
    }

    #[inline]
    /// Ack protocol error, return disconnect packet and close connection.
    pub fn ack(self) -> ControlResult {
        ControlResult {
            frame: Some(self.frm.into()),
            disconnect: true,
        }
    }
}

#[derive(Debug)]
pub struct PeerGone(pub(super) Option<io::Error>);

impl PeerGone {
    /// Returns error reference
    pub fn err(&self) -> Option<&io::Error> {
        self.0.as_ref()
    }

    /// Take error
    pub fn take(&mut self) -> Option<io::Error> {
        self.0.take()
    }

    pub fn ack(self) -> ControlResult {
        ControlResult {
            frame: None,
            disconnect: true,
        }
    }
}

#[derive(Debug)]
pub struct GoAway(frame::GoAway);

impl GoAway {
    /// Returns error reference
    pub fn frame(&self) -> &frame::GoAway {
        &self.0
    }

    pub fn ack(self) -> ControlResult {
        ControlResult {
            frame: None,
            disconnect: true,
        }
    }
}
