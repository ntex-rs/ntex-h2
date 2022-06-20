use std::fmt;

pub use crate::codec::EncoderError;

use crate::frame::{self, GoAway, Reason, StreamId};
use crate::stream::StreamRef;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("Unknown stream {0:?}")]
    UnknownStream(frame::Frame),
    #[error("Reason: {0}")]
    Reason(Reason),
    #[error("{0}")]
    Encoder(#[from] EncoderError),
    #[error("Stream idle: {0}")]
    StreamIdle(&'static str),
    #[error("Unexpected setting ack received")]
    UnexpectedSettingsAck,
    /// Window update value is zero
    #[error("Window update value is zero")]
    ZeroWindowUpdateValue,
    #[error("{0}")]
    Frame(#[from] frame::FrameError),
}

impl From<Reason> for ProtocolError {
    fn from(r: Reason) -> Self {
        ProtocolError::Reason(r)
    }
}

impl ProtocolError {
    pub fn to_goaway(&self) -> GoAway {
        match self {
            ProtocolError::Reason(reason) => GoAway::new(*reason),
            ProtocolError::Encoder(_) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("error during frame encoding")
            }
            ProtocolError::UnknownStream(_) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("unknown stream")
            }
            ProtocolError::StreamIdle(s) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data(format!("Stream idle: {}", s))
            }
            ProtocolError::UnexpectedSettingsAck => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("received unexpected settings ack")
            }
            ProtocolError::ZeroWindowUpdateValue => GoAway::new(Reason::PROTOCOL_ERROR)
                .set_data("zero value for window update frame is not allowed"),
            ProtocolError::Frame(err) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data(format!("protocol error: {:?}", err))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamError {
    kind: StreamErrorKind,
    stream: StreamRef,
}

impl StreamError {
    pub(crate) fn new(stream: StreamRef, kind: StreamErrorKind) -> Self {
        Self { kind, stream }
    }

    #[inline]
    pub fn id(&self) -> StreamId {
        self.stream.id()
    }

    #[inline]
    pub fn kind(&self) -> &StreamErrorKind {
        &self.kind
    }

    #[inline]
    pub fn reason(&self) -> Reason {
        match self.kind {
            StreamErrorKind::Reset(r) => r,
            StreamErrorKind::LocalReason(r) => r,
            StreamErrorKind::ZeroWindowUpdateValue => Reason::PROTOCOL_ERROR,
            StreamErrorKind::UnexpectedHeadersFrame => Reason::PROTOCOL_ERROR,
            StreamErrorKind::UnexpectedDataFrame => Reason::PROTOCOL_ERROR,
            StreamErrorKind::InternalError(_) => Reason::INTERNAL_ERROR,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum StreamErrorKind {
    Reset(Reason),
    LocalReason(Reason),
    ZeroWindowUpdateValue,
    UnexpectedHeadersFrame,
    UnexpectedDataFrame,
    InternalError(&'static str),
}

/// Errors caused by users of the library
#[derive(Debug)]
pub enum UserError {
    /// The stream ID is no longer accepting frames.
    InactiveStreamId,

    /// The stream is not currently expecting a frame of this type.
    UnexpectedFrameType,

    /// The payload size is too big
    PayloadTooBig,

    /// The application attempted to initiate too many streams to remote.
    Rejected,

    /// The released capacity is larger than claimed capacity.
    ReleaseCapacityTooBig,

    /// The stream ID space is overflowed.
    ///
    /// A new connection is needed.
    OverflowedStreamId,

    /// Illegal headers, such as connection-specific headers.
    MalformedHeaders,

    /// Request submitted with relative URI.
    MissingUriSchemeAndAuthority,

    /// Calls `SendResponse::poll_reset` after having called `send_response`.
    PollResetAfterSendResponse,

    /// Calls `PingPong::send_ping` before receiving a pong.
    SendPingWhilePending,

    /// Tries to update local SETTINGS while ACK has not been received.
    SendSettingsWhilePending,

    /// Tries to send push promise to peer who has disabled server push
    PeerDisabledServerPush,
}

impl std::error::Error for UserError {}

impl fmt::Display for UserError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        use self::UserError::*;

        fmt.write_str(match *self {
            InactiveStreamId => "inactive stream",
            UnexpectedFrameType => "unexpected frame type",
            PayloadTooBig => "payload too big",
            Rejected => "rejected",
            ReleaseCapacityTooBig => "release capacity too big",
            OverflowedStreamId => "stream ID overflowed",
            MalformedHeaders => "malformed headers",
            MissingUriSchemeAndAuthority => "request URI missing scheme and authority",
            PollResetAfterSendResponse => "poll_reset after send_response is illegal",
            SendPingWhilePending => "send_ping before received previous pong",
            SendSettingsWhilePending => "sending SETTINGS before received previous ACK",
            PeerDisabledServerPush => "sending PUSH_PROMISE to peer who disabled server push",
        })
    }
}
