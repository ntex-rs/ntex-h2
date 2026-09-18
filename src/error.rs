use ntex_error::{Error, ErrorDiagnostic};

pub use crate::codec::EncoderError;

use crate::frame::{self, GoAway, Reason, StreamId};
use crate::stream::StreamRef;

/// HTTP/2 connection-level protocol error.
#[derive(Debug, Copy, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionError {
    /// The connection is closing with the specified reason.
    #[error("Go away: {0}")]
    GoAway(Reason),
    /// A frame referenced an unknown stream.
    #[error("Unknown stream id in {0} frame")]
    UnknownStream(&'static str),
    /// Frame encoding failed.
    #[error("Encoder error: {0}")]
    Encoder(#[from] EncoderError),
    /// Frame decoding failed.
    #[error("Decoder error: {0}")]
    Decoder(#[from] frame::FrameError),
    /// A frame was received for a closed stream.
    #[error("{0:?} is closed, {1}")]
    StreamClosed(StreamId, &'static str),
    /// An invalid stream identifier was provided
    #[error("An invalid stream identifier was provided: {0}")]
    InvalidStreamId(&'static str),
    /// A SETTINGS acknowledgment was not expected.
    #[error("Unexpected setting ack received")]
    UnexpectedSettingsAck,
    /// A required pseudo-header is missing.
    #[error("Missing pseudo header {0:?}")]
    MissingPseudo(&'static str),
    /// A pseudo-header is not valid in this message.
    #[error("Unexpected pseudo header {0:?}")]
    UnexpectedPseudo(&'static str),
    /// A WINDOW_UPDATE increment was zero.
    #[error("Window update value is zero")]
    ZeroWindowUpdateValue,
    /// A flow-control window overflowed.
    #[error("Window value is overflowed")]
    WindowValueOverflow,
    /// The peer exceeded the concurrent stream limit.
    #[error("Max concurrent streams count achieved")]
    ConcurrencyOverflow,
    /// The peer exceeded the rapid-reset limit.
    #[error("Stream rapid reset count achieved")]
    StreamResetsLimit,
    /// Keep-alive ping timed out.
    #[error("Keep-alive timeout")]
    KeepaliveTimeout,
    /// Frame reading timed out.
    #[error("Read timeout")]
    ReadTimeout,
}

impl ConnectionError {
    /// Converts this error into a `GOAWAY` frame for connection shutdown.
    pub fn to_goaway(&self) -> GoAway {
        match self {
            ConnectionError::GoAway(reason) => GoAway::new(*reason),
            ConnectionError::Encoder(_) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("Error during frame encoding")
            }
            ConnectionError::Decoder(_) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("Error during frame decoding")
            }
            ConnectionError::MissingPseudo(s) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data(format!("Missing pseudo header {s:?}"))
            }
            ConnectionError::UnexpectedPseudo(s) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data(format!("Unexpected pseudo header {s:?}"))
            }
            ConnectionError::UnknownStream(_) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("Unknown stream")
            }
            ConnectionError::InvalidStreamId(_) => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("An invalid stream identifier was provided")
            }
            ConnectionError::StreamClosed(s, _) => {
                GoAway::new(Reason::STREAM_CLOSED).set_data(format!("{s:?} is closed"))
            }
            ConnectionError::UnexpectedSettingsAck => {
                GoAway::new(Reason::PROTOCOL_ERROR).set_data("Received unexpected settings ack")
            }
            ConnectionError::ZeroWindowUpdateValue => GoAway::new(Reason::PROTOCOL_ERROR)
                .set_data("Zero value for window update frame is not allowed"),
            ConnectionError::WindowValueOverflow => {
                GoAway::new(Reason::FLOW_CONTROL_ERROR).set_data("Updated value for window is overflowed")
            }
            ConnectionError::ConcurrencyOverflow => {
                GoAway::new(Reason::FLOW_CONTROL_ERROR).set_data("Max concurrent streams count achieved")
            }
            ConnectionError::StreamResetsLimit => {
                GoAway::new(Reason::FLOW_CONTROL_ERROR).set_data("Stream rapid reset count achieved")
            }
            ConnectionError::KeepaliveTimeout => {
                GoAway::new(Reason::NO_ERROR).set_data("Keep-alive timeout")
            }
            ConnectionError::ReadTimeout => GoAway::new(Reason::NO_ERROR).set_data("Frame read timeout"),
        }
    }
}

impl ErrorDiagnostic for ConnectionError {
    fn signature(&self) -> &'static str {
        match self {
            ConnectionError::GoAway(_) => "h2-conn-GoAway",
            ConnectionError::UnknownStream(_) => "h2-conn-UnknownStream",
            ConnectionError::Encoder(_) => "h2-conn-Encoder",
            ConnectionError::Decoder(_) => "h2-conn-Decoder",
            ConnectionError::StreamClosed(..) => "h2-conn-StreamClosed",
            ConnectionError::InvalidStreamId(_) => "h2-conn-InvalidStreamId",
            ConnectionError::UnexpectedSettingsAck => "h2-conn-UnexpectedSettingsAck",
            ConnectionError::MissingPseudo(_) => "h2-conn-MissingPseudo",
            ConnectionError::UnexpectedPseudo(_) => "h2-conn-UnexpectedPseudo",
            ConnectionError::ZeroWindowUpdateValue => "h2-conn-ZeroWindowUpdateValue",
            ConnectionError::WindowValueOverflow => "h2-conn-WindowValueOverflow",
            ConnectionError::ConcurrencyOverflow => "h2-conn-ConcurrencyOverflow",
            ConnectionError::StreamResetsLimit => "h2-conn-StreamResetsLimit",
            ConnectionError::KeepaliveTimeout => "h2-conn-KeepaliveTimeout",
            ConnectionError::ReadTimeout => "h2-conn-ReadTimeout",
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("Stream error: {kind:?}")]
pub(crate) struct StreamErrorInner {
    kind: Error<StreamError>,
    stream: StreamRef,
}

impl StreamErrorInner {
    pub(crate) fn new(stream: StreamRef, kind: Error<StreamError>) -> Self {
        Self { kind, stream }
    }

    pub(crate) fn into_inner(self) -> (StreamRef, Error<StreamError>) {
        (self.stream, self.kind)
    }
}

/// HTTP/2 stream-level protocol error.
#[derive(Debug, Copy, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// A frame or operation is invalid for an idle stream.
    #[error("Stream in idle state: {0}")]
    Idle(&'static str),
    /// The stream is closed.
    #[error("Stream is closed")]
    Closed,
    /// The stream flow-control window overflowed.
    #[error("Window value is overflowed")]
    WindowOverflowed,
    /// A stream WINDOW_UPDATE increment was zero.
    #[error("Zero value for window")]
    WindowZeroUpdateValue,
    /// Trailers were received without END_STREAM.
    #[error("Trailers headers without end of stream flags")]
    TrailersWithoutEos,
    /// The content-length header is invalid.
    #[error("Invalid content length")]
    InvalidContentLength,
    /// Payload size does not match the content-length header.
    #[error("Payload length does not match content-length header")]
    WrongPayloadLength,
    /// A HEAD response contained payload bytes.
    #[error("Non-empty payload for HEAD response")]
    NonEmptyPayload,
    /// Waiting for send capacity timed out.
    #[error("Capacity availability timeout")]
    CapacityTimeout,
    /// The stream was reset with the specified reason.
    #[error("Stream has been reset with {0}")]
    Reset(Reason),
}

impl StreamError {
    #[inline]
    pub(crate) fn reason(&self) -> Reason {
        match self {
            StreamError::Closed => Reason::STREAM_CLOSED,
            StreamError::WindowOverflowed | StreamError::CapacityTimeout => Reason::FLOW_CONTROL_ERROR,
            StreamError::Idle(_)
            | StreamError::WindowZeroUpdateValue
            | StreamError::TrailersWithoutEos
            | StreamError::InvalidContentLength
            | StreamError::WrongPayloadLength
            | StreamError::NonEmptyPayload => Reason::PROTOCOL_ERROR,
            StreamError::Reset(r) => *r,
        }
    }
}

impl ErrorDiagnostic for StreamError {
    fn signature(&self) -> &'static str {
        match self {
            StreamError::Idle(_) => "h2-stream-Idle",
            StreamError::Closed => "h2-stream-Closed",
            StreamError::WindowOverflowed => "h2-stream-WindowOverflowed",
            StreamError::WindowZeroUpdateValue => "h2-stream-WindowZeroUpdateValue",
            StreamError::TrailersWithoutEos => "h2-stream-TrailersWithoutEos",
            StreamError::InvalidContentLength => "h2-stream-InvalidContentLength",
            StreamError::WrongPayloadLength => "h2-stream-WrongPayloadLength",
            StreamError::NonEmptyPayload => "h2-stream-NonEmptyPayload",
            StreamError::CapacityTimeout => "h2-stream-CapacityTimeout",
            StreamError::Reset(_) => "h2-stream-Reset",
        }
    }
}

/// Errors returned by stream and connection operations.
#[derive(Debug, Copy, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OperationError {
    /// Stream-level protocol error.
    #[error("{0:?}")]
    Stream(#[from] StreamError),

    /// Connection-level protocol error.
    #[error("{0}")]
    Connection(#[from] ConnectionError),

    /// Cannot process operation for idle stream
    #[error("Cannot process operation for idle stream")]
    Idle,

    /// Cannot process operation for stream in payload state
    #[error("Cannot process operation for stream in payload state")]
    Payload,

    /// Stream is closed
    #[error("Stream is closed {0:?}")]
    Closed(Option<Reason>),

    /// Stream has been reset from the peer
    #[error("Stream has been reset from the peer with {0}")]
    RemoteReset(Reason),

    /// Stream has been reset from local side
    #[error("Stream has been reset from local side with {0}")]
    LocalReset(Reason),

    /// The stream ID space is overflowed
    ///
    /// A new connection is needed.
    #[error("The stream ID space is overflowed")]
    OverflowedStreamId,

    /// Disconnecting
    #[error("Connection is disconnecting")]
    Disconnecting,

    /// Disconnected
    #[error("Connection is closed")]
    Disconnected,
}

impl ErrorDiagnostic for OperationError {
    fn signature(&self) -> &'static str {
        match self {
            OperationError::Stream(err) => err.signature(),
            OperationError::Connection(err) => err.signature(),
            OperationError::Idle => "h2-oper-Idle",
            OperationError::Payload => "h2-oper-Payload",
            OperationError::Closed(_) => "h2-oper-Closed",
            OperationError::RemoteReset(_) => "h2-oper-RemoteReset",
            OperationError::LocalReset(_) => "h2-oper-LocalReset",
            OperationError::OverflowedStreamId => "h2-oper-OverflowedStreamId",
            OperationError::Disconnecting => "h2-oper-Disconnecting",
            OperationError::Disconnected => "h2-oper-Disconnected",
        }
    }
}
