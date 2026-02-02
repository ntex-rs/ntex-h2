use std::io;

use crate::frame::Frame;
use crate::{error, frame, stream::StreamRef};

#[derive(Debug)]
pub enum Control<E> {
    /// Connection is prepared to disconnect
    Disconnect(Reason<E>),
    /// Protocol dispatcher is terminated
    Terminated(Terminated),
}

#[derive(Debug)]
/// Disconnect reason
pub enum Reason<E> {
    /// Application level error from publish service
    Error(Error<E>),
    /// Protocol level error
    ProtocolError(ConnectionError),
    /// Remote GoAway is received
    GoAway(GoAway),
    /// Peer is gone
    PeerGone(PeerGone),
}

#[derive(Clone, Debug)]
pub struct ControlAck {
    pub(crate) frame: Option<Frame>,
}

impl<E> Control<E> {
    /// Create a new `Control` message for app level errors
    pub(super) fn error(err: E, stream: Option<StreamRef>) -> Self {
        Control::Disconnect(Reason::Error(Error::new(err, stream)))
    }

    /// Create a new `Control` message from GOAWAY packet.
    pub(super) fn go_away(frm: frame::GoAway) -> Self {
        Control::Disconnect(Reason::GoAway(GoAway(frm)))
    }

    /// Create a new `Control` message from DISCONNECT packet.
    pub(super) fn peer_gone(err: Option<io::Error>) -> Self {
        Control::Disconnect(Reason::PeerGone(PeerGone(err)))
    }

    /// Create a new `Control` message for protocol level errors
    pub(super) fn proto_error(err: error::ConnectionError) -> Self {
        Control::Disconnect(Reason::ProtocolError(ConnectionError::new(err)))
    }

    pub(super) fn terminated() -> Self {
        Control::Terminated(Terminated)
    }

    /// Default ack impl
    pub fn ack(self) -> ControlAck {
        match self {
            Control::Disconnect(item) => item.ack(),
            Control::Terminated(item) => item.ack(),
        }
    }
}

impl<E> Reason<E> {
    /// Default ack impl
    pub fn ack(self) -> ControlAck {
        match self {
            Reason::Error(item) => item.ack(),
            Reason::ProtocolError(item) => item.ack(),
            Reason::GoAway(item) => item.ack(),
            Reason::PeerGone(item) => item.ack(),
        }
    }
}

/// Application level error
#[derive(Debug)]
pub struct Error<E> {
    err: E,
    goaway: frame::GoAway,
}

impl<E> Error<E> {
    fn new(err: E, stream: Option<StreamRef>) -> Self {
        let goaway = if let Some(ref stream) = stream {
            frame::GoAway::new(frame::Reason::INTERNAL_ERROR).set_last_stream_id(stream.id())
        } else {
            frame::GoAway::new(frame::Reason::INTERNAL_ERROR)
        };

        Self { err, goaway }
    }

    #[inline]
    /// Returns reference to mqtt error
    pub fn get_ref(&self) -> &E {
        &self.err
    }

    #[inline]
    /// Set reason code for go away packet
    pub fn reason(mut self, reason: frame::Reason) -> Self {
        self.goaway = self.goaway.set_reason(reason);
        self
    }

    #[inline]
    /// Ack service error, return disconnect packet and close connection.
    pub fn ack(self) -> ControlAck {
        ControlAck {
            frame: Some(self.goaway.into()),
        }
    }
}

/// Dispatcher has been terminated
#[derive(Debug)]
pub struct Terminated;

impl Terminated {
    #[inline]
    /// convert packet to a result
    pub fn ack(self) -> ControlAck {
        ControlAck { frame: None }
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
    pub fn reason(mut self, reason: frame::Reason) -> Self {
        self.frm = self.frm.set_reason(reason);
        self
    }

    #[inline]
    /// Ack protocol error, return disconnect packet and close connection.
    pub fn ack(self) -> ControlAck {
        ControlAck {
            frame: Some(self.frm.into()),
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

    pub fn ack(self) -> ControlAck {
        ControlAck { frame: None }
    }
}

#[derive(Debug)]
pub struct GoAway(frame::GoAway);

impl GoAway {
    /// Returns error reference
    pub fn frame(&self) -> &frame::GoAway {
        &self.0
    }

    pub fn ack(self) -> ControlAck {
        ControlAck { frame: None }
    }
}
