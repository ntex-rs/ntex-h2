use std::{error, fmt, io};

use ntex_bytes::Bytes;

use crate::frame::{self, GoAway, Reason, StreamId};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    UnexpectedSettingsAck,
    Frame(frame::Error),
}

impl From<frame::Error> for ProtocolError {
    fn from(err: frame::Error) -> Self {
        ProtocolError::Frame(err)
    }
}

impl From<ProtocolError> for GoAway {
    fn from(err: ProtocolError) -> GoAway {
        match err {
            ProtocolError::UnexpectedSettingsAck => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("received unexpected settings ack")
            }
            ProtocolError::Frame(err) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data(format!("protocol error: {:?}", err))
            }
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum StreamError {
    UnexpectedHeadersFrame,
    UnexpectedDataFrame,
    InternalError(&'static str),
}

impl From<StreamError> for Reason {
    fn from(err: StreamError) -> Reason {
        match err {
            StreamError::UnexpectedHeadersFrame => Reason::PROTOCOL_ERROR,
            StreamError::UnexpectedDataFrame => Reason::PROTOCOL_ERROR,
            StreamError::InternalError(_) => Reason::INTERNAL_ERROR,
        }
    }
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
