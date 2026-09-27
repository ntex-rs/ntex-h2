use ntex_bytes::BufMut;

use crate::frame::{Frame, FrameError, Head, Kind, Reason, StreamId};

/// A `RST_STREAM` frame, terminates a stream (RFC 9113 §6.4).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Reset {
    stream_id: StreamId,
    error_code: Reason,
}

impl Reset {
    /// Creates a `RST_STREAM` frame.
    pub fn new(stream_id: StreamId, error: Reason) -> Reset {
        Reset {
            stream_id,
            error_code: error,
        }
    }

    #[must_use]
    /// Sets the error code.
    pub fn set_reason(mut self, error_code: Reason) -> Self {
        self.error_code = error_code;
        self
    }

    #[must_use]
    /// Returns the reset stream id.
    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    #[must_use]
    /// Returns the error code.
    pub fn reason(&self) -> Reason {
        self.error_code
    }

    /// Parses a `RST_STREAM` frame payload.
    pub fn load(head: Head, payload: &[u8]) -> Result<Reset, FrameError> {
        if payload.len() != 4 {
            return Err(FrameError::InvalidPayloadLength);
        }

        let error_code = unpack_octets_4!(payload, 0, u32);

        Ok(Reset {
            stream_id: head.stream_id(),
            error_code: error_code.into(),
        })
    }

    /// Encodes the frame, including the frame header.
    pub fn encode<B: BufMut>(&self, dst: &mut B) {
        log::trace!(
            "encoding RESET; id={:?} code={:?}",
            self.stream_id,
            self.error_code
        );
        let head = Head::new(Kind::Reset, 0, self.stream_id);
        head.encode(4, dst);
        dst.put_u32(self.error_code.into());
    }
}

impl From<Reset> for Frame {
    fn from(src: Reset) -> Self {
        Frame::Reset(src)
    }
}
