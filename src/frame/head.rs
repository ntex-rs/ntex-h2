use super::{HEADER_LEN, StreamId};

use ntex_bytes::BufMut;

/// The 9-byte header that starts every frame (RFC 9113 §4.1).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Head {
    kind: Kind,
    flag: u8,
    stream_id: StreamId,
}

/// Frame type.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `DATA` frame.
    Data = 0,
    /// `HEADERS` frame.
    Headers = 1,
    /// `PRIORITY` frame.
    Priority = 2,
    /// `RST_STREAM` frame.
    Reset = 3,
    /// `SETTINGS` frame.
    Settings = 4,
    /// `PING` frame.
    Ping = 6,
    /// `GOAWAY` frame.
    GoAway = 7,
    /// `WINDOW_UPDATE` frame.
    WindowUpdate = 8,
    /// `CONTINUATION` frame.
    Continuation = 9,
    /// Unsupported frame type, `PUSH_PROMISE` or an extension frame.
    Unknown,
}

// ===== impl Head =====

impl Head {
    /// Creates a frame header.
    pub fn new(kind: Kind, flag: u8, stream_id: StreamId) -> Head {
        Head {
            kind,
            flag,
            stream_id,
        }
    }

    /// Parse an HTTP/2 frame header
    pub fn parse(header: &[u8]) -> Head {
        let (stream_id, _) = StreamId::parse(&header[5..]);

        Head {
            stream_id,
            kind: Kind::new(header[3]),
            flag: header[4],
        }
    }

    /// Returns the stream id, 0 for connection-level frames.
    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    /// Returns the frame type.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Returns the frame flags.
    pub fn flag(&self) -> u8 {
        self.flag
    }

    /// Encodes the frame header for a payload of `payload_len` bytes.
    pub fn encode<T: BufMut>(&self, payload_len: usize, dst: &mut T) {
        dst.put_slice(&self.to_bytes(payload_len));
    }

    /// Returns the encoded frame header for a payload of `payload_len` bytes.
    #[inline]
    pub(crate) fn to_bytes(self, payload_len: usize) -> [u8; HEADER_LEN] {
        let len = (payload_len as u32).to_be_bytes();
        let id = u32::from(self.stream_id).to_be_bytes();
        [
            len[1],
            len[2],
            len[3],
            self.kind as u8,
            self.flag,
            id[0],
            id[1],
            id[2],
            id[3],
        ]
    }
}

// ===== impl Kind =====

impl Kind {
    /// Converts a frame type byte, unsupported types map to [`Kind::Unknown`].
    pub fn new(byte: u8) -> Kind {
        match byte {
            0 => Kind::Data,
            1 => Kind::Headers,
            2 => Kind::Priority,
            3 => Kind::Reset,
            4 => Kind::Settings,
            6 => Kind::Ping,
            7 => Kind::GoAway,
            8 => Kind::WindowUpdate,
            9 => Kind::Continuation,
            _ => Kind::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use ntex_bytes::BytesMut;

    use super::*;

    /// STATEMENT: A frame header encodes as a 24-bit length, type, flags and 31-bit stream id.
    ///
    /// HOW:
    /// - Encode a HEADERS head with flag `0x5` on stream `0x0102_0304` for a `0x0A_0B0C` payload.
    /// - Compare the nine bytes written by `Head::encode` and returned by `Head::to_bytes`.
    #[test]
    fn test_head_encode() {
        let head = Head::new(Kind::Headers, 0x5, StreamId::from(0x0102_0304));
        let expected = [0x0a, 0x0b, 0x0c, 0x01, 0x05, 0x01, 0x02, 0x03, 0x04];

        let mut dst = BytesMut::new();
        head.encode(0x0a_0b0c, &mut dst);
        assert_eq!(&dst[..], &expected);
        assert_eq!(head.to_bytes(0x0a_0b0c), expected);
    }
}
