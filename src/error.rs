use std::{error, fmt, io};

use ntex_bytes::Bytes;

pub use crate::frame::Reason;
use crate::frame::StreamId;

/// Represents HTTP/2 operation errors.
///
/// `Error` covers error cases raised by protocol errors caused by the
/// peer, I/O (transport) errors, and errors caused by the user of the library.
///
/// If the error was caused by the remote peer, then it will contain a
/// [`Reason`] which can be obtained with the [`reason`] function.
///
/// [`Reason`]: struct.Reason.html
/// [`reason`]: #method.reason
#[derive(Debug)]
pub struct Error {
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    /// A RST_STREAM frame was received or sent.
    Reset(StreamId, Reason, Initiator),

    /// A GO_AWAY frame was received or sent.
    GoAway(Bytes, Reason, Initiator),

    /// The user created an error from a bare Reason.
    Reason(Reason),

    /// An error resulting from an invalid action taken by the user of this
    /// library.
    User(UserError),

    /// An `io::Error` occurred while trying to read or write.
    Io(io::Error),
}

// ===== impl Error =====

impl Error {
    /// If the error was caused by the remote peer, the error reason.
    ///
    /// This is either an error received by the peer or caused by an invalid
    /// action taken by the peer (i.e. a protocol error).
    pub fn reason(&self) -> Option<Reason> {
        match self.kind {
            Kind::Reset(_, reason, _) | Kind::GoAway(_, reason, _) | Kind::Reason(reason) => {
                Some(reason)
            }
            _ => None,
        }
    }

    /// Returns true if the error is an io::Error
    pub fn is_io(&self) -> bool {
        matches!(self.kind, Kind::Io(..))
    }

    /// Returns the error if the error is an io::Error
    pub fn get_io(&self) -> Option<&io::Error> {
        match self.kind {
            Kind::Io(ref e) => Some(e),
            _ => None,
        }
    }

    /// Returns the error if the error is an io::Error
    pub fn into_io(self) -> Option<io::Error> {
        match self.kind {
            Kind::Io(e) => Some(e),
            _ => None,
        }
    }

    /// Returns true if the error is from a `GOAWAY`.
    pub fn is_go_away(&self) -> bool {
        matches!(self.kind, Kind::GoAway(..))
    }

    /// Returns true if the error is from a `RST_STREAM`.
    pub fn is_reset(&self) -> bool {
        matches!(self.kind, Kind::Reset(..))
    }

    /// Returns true if the error was received in a frame from the remote.
    ///
    /// Such as from a received `RST_STREAM` or `GOAWAY` frame.
    pub fn is_remote(&self) -> bool {
        matches!(
            self.kind,
            Kind::GoAway(_, _, Initiator::Remote) | Kind::Reset(_, _, Initiator::Remote)
        )
    }
}

impl From<ProtocolError> for Error {
    fn from(src: ProtocolError) -> Error {
        use ProtocolError::*;

        Error {
            kind: match src {
                Reset(stream_id, reason, initiator) => Kind::Reset(stream_id, reason, initiator),
                GoAway(debug_data, reason, initiator) => {
                    Kind::GoAway(debug_data, reason, initiator)
                }
                Io(kind, inner) => {
                    Kind::Io(inner.map_or_else(|| kind.into(), |inner| io::Error::new(kind, inner)))
                }
            },
        }
    }
}

impl From<io::Error> for Error {
    fn from(src: io::Error) -> Error {
        Error {
            kind: Kind::Io(src),
        }
    }
}

impl From<Reason> for Error {
    fn from(src: Reason) -> Error {
        Error {
            kind: Kind::Reason(src),
        }
    }
}

impl From<UserError> for Error {
    fn from(src: UserError) -> Error {
        Error {
            kind: Kind::User(src),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let debug_data = match self.kind {
            Kind::Reset(_, reason, Initiator::User) => {
                return write!(fmt, "stream error sent by user: {}", reason)
            }
            Kind::Reset(_, reason, Initiator::Library) => {
                return write!(fmt, "stream error detected: {}", reason)
            }
            Kind::Reset(_, reason, Initiator::Remote) => {
                return write!(fmt, "stream error received: {}", reason)
            }
            Kind::GoAway(ref debug_data, reason, Initiator::User) => {
                write!(fmt, "connection error sent by user: {}", reason)?;
                debug_data
            }
            Kind::GoAway(ref debug_data, reason, Initiator::Library) => {
                write!(fmt, "connection error detected: {}", reason)?;
                debug_data
            }
            Kind::GoAway(ref debug_data, reason, Initiator::Remote) => {
                write!(fmt, "connection error received: {}", reason)?;
                debug_data
            }
            Kind::Reason(reason) => return write!(fmt, "protocol error: {}", reason),
            Kind::User(ref e) => return write!(fmt, "user error: {}", e),
            Kind::Io(ref e) => return e.fmt(fmt),
        };

        if !debug_data.is_empty() {
            write!(fmt, " ({:?})", debug_data)?;
        }

        Ok(())
    }
}

impl error::Error for Error {}

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

impl error::Error for UserError {}

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

/// Either an H2 reason or an I/O error
#[derive(Clone, Debug)]
pub enum ProtocolError {
    Reset(StreamId, Reason, Initiator),
    GoAway(Bytes, Reason, Initiator),
    Io(io::ErrorKind, Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Initiator {
    User,
    Library,
    Remote,
}

impl ProtocolError {
    pub(crate) fn is_local(&self) -> bool {
        match *self {
            Self::Reset(_, _, initiator) | Self::GoAway(_, _, initiator) => initiator.is_local(),
            Self::Io(..) => true,
        }
    }

    pub(crate) fn user_go_away(reason: Reason) -> Self {
        Self::GoAway(Bytes::new(), reason, Initiator::User)
    }

    pub(crate) fn library_reset(stream_id: StreamId, reason: Reason) -> Self {
        Self::Reset(stream_id, reason, Initiator::Library)
    }

    pub(crate) fn library_go_away(reason: Reason) -> Self {
        Self::GoAway(Bytes::new(), reason, Initiator::Library)
    }

    pub(crate) fn remote_reset(stream_id: StreamId, reason: Reason) -> Self {
        Self::Reset(stream_id, reason, Initiator::Remote)
    }

    pub(crate) fn remote_go_away(debug_data: Bytes, reason: Reason) -> Self {
        Self::GoAway(debug_data, reason, Initiator::Remote)
    }
}

impl Initiator {
    fn is_local(&self) -> bool {
        match *self {
            Self::User | Self::Library => true,
            Self::Remote => false,
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Self::Reset(_, reason, _) | Self::GoAway(_, reason, _) => reason.fmt(fmt),
            Self::Io(_, Some(ref inner)) => inner.fmt(fmt),
            Self::Io(kind, None) => io::Error::from(kind).fmt(fmt),
        }
    }
}

impl From<io::ErrorKind> for ProtocolError {
    fn from(src: io::ErrorKind) -> Self {
        Self::Io(src.into(), None)
    }
}

impl From<io::Error> for ProtocolError {
    fn from(src: io::Error) -> Self {
        Self::Io(src.kind(), src.get_ref().map(|inner| inner.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::Reason;

    #[test]
    fn error_from_reason() {
        let err = Error::from(Reason::HTTP_1_1_REQUIRED);
        assert_eq!(err.reason(), Some(Reason::HTTP_1_1_REQUIRED));
    }
}
