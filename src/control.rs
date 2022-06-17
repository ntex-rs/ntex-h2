use std::{fmt, io};

use crate::frame::{self, Frame, Reason, Reset, StreamId};
use crate::{error, stream::Stream};

#[derive(Debug)]
pub enum ControlMessage<E> {
    // /// Ping frame is received
    // Ping(frame::Ping),
    /// Application level error from publish service
    AppError(AppError<E>),
    /// Stream level error
    StreamError(StreamError),
    /// Protocol level error
    ProtocolError(ProtocolError),
    /// Remote GoAway is received
    GoAway(GoAway),
    /// Peer is gone
    PeerGone(PeerGone),
    /// Protocol dispatcher is terminated
    Terminated(Terminated),
}

#[derive(Debug)]
pub struct ControlResult {
    pub(crate) frame: Option<Frame>,
    pub(crate) disconnect: bool,
}

impl<E> ControlMessage<E> {
    // pub(crate) fn new(session: Cell<SessionInner>, kind: ControlFrameKind) -> Self {
    //     ControlFrame(Cell::new(FrameInner {
    //         session: Some(session),
    //         kind,
    //     }))
    // }

    /// Create a new `ControlMessage` for app level errors
    pub(super) fn app_error(err: E, stream: Stream) -> Self {
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

    pub(super) fn terminated(is_error: bool) -> Self {
        ControlMessage::Terminated(Terminated::new(is_error))
    }

    /// Create a new `ControlMessage` for stream level errors
    pub(super) fn stream_error(err: error::StreamError, stream: Stream) -> Self {
        ControlMessage::StreamError(StreamError::new(err, stream))
    }

    /// Create a new `ControlMessage` for protocol level errors
    pub(super) fn proto_error(err: error::ProtocolError) -> Self {
        ControlMessage::ProtocolError(ProtocolError::new(err))
    }

    /// Default ack impl
    pub fn ack(self) -> ControlResult {
        match self {
            ControlMessage::AppError(item) => item.ack(),
            ControlMessage::StreamError(item) => item.ack(),
            ControlMessage::ProtocolError(item) => item.ack(),
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
    stream: Stream,
    reason: Reason,
}

impl<E> AppError<E> {
    pub fn new(err: E, stream: Stream) -> Self {
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
    pub fn ack(mut self) -> ControlResult {
        ControlResult {
            frame: Some(Reset::new(self.stream.id(), self.reason).into()),
            disconnect: false,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Disconnect;

impl Disconnect {
    pub fn ack(self) -> ControlResult {
        ControlResult {
            frame: None,
            disconnect: true,
        }
    }
}

/// Dispatcher has been terminated
#[derive(Debug)]
pub struct Terminated {
    is_error: bool,
}

impl Terminated {
    pub(crate) fn new(is_error: bool) -> Self {
        Self { is_error }
    }

    /// Returns error state on connection close
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    #[inline]
    /// convert packet to a result
    pub fn ack(self) -> ControlResult {
        ControlResult {
            frame: None,
            disconnect: true,
        }
    }
}

/// Stream level error
#[derive(Debug)]
pub struct StreamError {
    err: error::StreamError,
    frm: frame::Reset,
    stream: Stream,
}

impl StreamError {
    pub fn new(err: error::StreamError, stream: Stream) -> Self {
        Self {
            frm: frame::Reset::new(stream.id(), err.into()),
            err,
            stream,
        }
    }

    #[inline]
    /// Returns reference to a protocol error
    pub fn get_ref(&self) -> &error::StreamError {
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
            disconnect: false,
        }
    }
}

/// Protocol level error
#[derive(Debug)]
pub struct ProtocolError {
    err: error::ProtocolError,
    frm: frame::GoAway,
}

impl ProtocolError {
    pub fn new(err: error::ProtocolError) -> Self {
        Self {
            frm: err.into(),
            err,
        }
    }

    #[inline]
    /// Returns reference to a protocol error
    pub fn get_ref(&self) -> &error::ProtocolError {
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
