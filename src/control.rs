//! Connection control events, delivered to the control service.

use std::io;

use ntex_http::{HeaderMap, StatusCode};

use crate::frame::{Frame, PseudoHeaders};
use crate::{error, frame, stream::StreamRef};

/// Connection control event.
#[derive(Debug)]
pub enum Control<E> {
    /// The connection is preparing to disconnect.
    Disconnect(Reason<E>),
    /// A request contains `Expect: 100-continue`.
    ///
    /// The server does not emit this event, it is created by the application
    /// layer with [`Control::expect`] before the request is processed.
    Expect(Expect),
}

#[derive(Debug)]
/// Reason a connection is disconnecting.
pub enum Reason<E> {
    /// Application-level request service error.
    Error(Error<E>),
    /// HTTP/2 connection protocol error.
    ProtocolError(ConnectionError),
    /// A remote `GOAWAY` frame was received.
    GoAway(GoAway),
    /// The peer disconnected.
    PeerGone(PeerGone),
}

/// Response from a connection control service.
#[derive(Clone, Debug)]
pub struct ControlAck {
    pub(crate) frame: Option<Frame>,
    pub(crate) expect: Option<ExpectResult>,
}

impl ControlAck {
    fn frame(frame: Option<Frame>) -> Self {
        ControlAck { frame, expect: None }
    }

    #[inline]
    /// Returns the result of the [`Control::Expect`] event.
    pub fn into_expect(self) -> Option<ExpectResult> {
        self.expect
    }
}

impl<E> Control<E> {
    /// Creates a new `Control` message for a request with `Expect: 100-continue`.
    pub fn expect(stream: StreamRef, pseudo: PseudoHeaders, headers: HeaderMap) -> Self {
        Control::Expect(Expect {
            stream,
            pseudo,
            headers,
        })
    }

    /// Create a new `Control` message for app level errors
    ///
    /// `last_id` is the last stream initiated by the peer.
    pub(super) fn error(err: E, last_id: frame::StreamId) -> Self {
        Control::Disconnect(Reason::Error(Error::new(err, last_id)))
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
    ///
    /// `last_id` is the last stream initiated by the peer.
    pub(super) fn proto_error(
        err: ntex_error::Error<error::ConnectionError>,
        last_id: frame::StreamId,
    ) -> Self {
        let mut err = ConnectionError::new(err);
        err.frm = err.frm.set_last_stream_id(last_id);
        Control::Disconnect(Reason::ProtocolError(err))
    }

    /// Returns the default acknowledgment for this event.
    pub fn ack(self) -> ControlAck {
        match self {
            Control::Disconnect(item) => item.ack(),
            Control::Expect(item) => item.ack(),
        }
    }
}

impl<E> Reason<E> {
    /// Returns the default acknowledgment for this reason.
    pub fn ack(self) -> ControlAck {
        match self {
            Reason::Error(item) => item.ack(),
            Reason::ProtocolError(item) => item.ack(),
            Reason::GoAway(item) => item.ack(),
            Reason::PeerGone(item) => item.ack(),
        }
    }
}

/// Application-level service error control event.
#[derive(Debug)]
pub struct Error<E> {
    err: E,
    goaway: frame::GoAway,
}

impl<E> Error<E> {
    fn new(err: E, last_id: frame::StreamId) -> Self {
        let goaway = frame::GoAway::new(frame::Reason::INTERNAL_ERROR).set_last_stream_id(last_id);
        Self { err, goaway }
    }

    #[inline]
    /// Returns the application error.
    pub fn get_ref(&self) -> &E {
        &self.err
    }

    #[inline]
    #[must_use]
    /// Sets the reason code for the generated `GOAWAY` frame.
    pub fn reason(mut self, reason: frame::Reason) -> Self {
        self.goaway = self.goaway.set_reason(reason);
        self
    }

    #[inline]
    /// Acknowledges the error and returns a `GOAWAY` response.
    pub fn ack(self) -> ControlAck {
        ControlAck::frame(Some(self.goaway.into()))
    }
}

/// HTTP/2 connection protocol error control event.
#[derive(Debug)]
pub struct ConnectionError {
    err: ntex_error::Error<error::ConnectionError>,
    frm: frame::GoAway,
}

impl ConnectionError {
    /// Creates a protocol error event.
    pub fn new(err: ntex_error::Error<error::ConnectionError>) -> Self {
        Self {
            frm: err.to_goaway(),
            err,
        }
    }

    #[inline]
    /// Returns the protocol error.
    pub fn get_ref(&self) -> &ntex_error::Error<error::ConnectionError> {
        &self.err
    }

    #[inline]
    #[must_use]
    /// Overrides the reason code for the generated `GOAWAY` frame.
    pub fn reason(mut self, reason: frame::Reason) -> Self {
        self.frm = self.frm.set_reason(reason);
        self
    }

    #[inline]
    /// Acknowledges the error and returns a `GOAWAY` response.
    pub fn ack(self) -> ControlAck {
        ControlAck::frame(Some(self.frm.into()))
    }
}

/// Notification that the peer disconnected.
#[derive(Debug)]
pub struct PeerGone(pub(super) Option<io::Error>);

impl PeerGone {
    /// Returns the underlying I/O error, if available.
    pub fn err(&self) -> Option<&io::Error> {
        self.0.as_ref()
    }

    /// Removes and returns the underlying I/O error.
    pub fn take(&mut self) -> Option<io::Error> {
        self.0.take()
    }

    /// Acknowledges the event without sending a frame.
    pub fn ack(self) -> ControlAck {
        ControlAck::frame(None)
    }
}

/// A `GOAWAY` frame received from the peer.
#[derive(Debug)]
pub struct GoAway(frame::GoAway);

impl GoAway {
    /// Returns the received `GOAWAY` frame.
    pub fn frame(&self) -> &frame::GoAway {
        &self.0
    }

    /// Acknowledges the event without sending a frame.
    pub fn ack(self) -> ControlAck {
        ControlAck::frame(None)
    }
}

/// A request containing an `Expect: 100-continue` header.
#[derive(Clone, Debug)]
pub struct Expect {
    stream: StreamRef,
    pseudo: PseudoHeaders,
    headers: HeaderMap,
}

impl Expect {
    #[inline]
    /// Returns the request stream.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    #[inline]
    /// Returns the request pseudo headers.
    pub fn pseudo(&self) -> &PseudoHeaders {
        &self.pseudo
    }

    #[inline]
    /// Returns the request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[inline]
    /// Returns mutable access to the request headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    #[inline]
    /// Returns the request stream, pseudo headers and headers.
    pub fn into_parts(self) -> (StreamRef, PseudoHeaders, HeaderMap) {
        (self.stream, self.pseudo, self.headers)
    }

    #[inline]
    /// Accepts the expectation, `100 Continue` is sent and the request is processed.
    pub fn ack(self) -> ControlAck {
        ControlAck {
            frame: None,
            expect: Some(ExpectResult::Continue(self)),
        }
    }

    #[inline]
    /// Rejects the expectation, the response with `status` and `headers`
    /// completes the request.
    ///
    /// # Panics
    ///
    /// Panics if `status` is informational.
    pub fn fail(self, status: StatusCode, headers: HeaderMap) -> ControlAck {
        assert!(
            !status.is_informational(),
            "Status {status} is not a final status"
        );
        ControlAck {
            frame: None,
            expect: Some(ExpectResult::Failed(self, status, headers)),
        }
    }
}

/// Result of the [`Control::Expect`] event.
#[derive(Clone, Debug)]
pub enum ExpectResult {
    /// Send `100 Continue` and process the request.
    Continue(Expect),
    /// Complete the request with the response status and headers.
    Failed(Expect, StatusCode, HeaderMap),
}
