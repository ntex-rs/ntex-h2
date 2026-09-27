//! Split a byte stream into HTTP/2 frames
use std::cell::Cell;

use ntex_bytes::{Bytes, BytesMut};

use crate::frame::{self, FrameError};

/// Size of the frame length field
const LENGTH_FIELD_LEN: usize = 3;

/// Splits HTTP/2 frames, including the 9 byte frame header.
#[derive(Debug)]
pub(super) struct FrameDecoder {
    max_frame_len: Cell<usize>,
    // Total length of the frame currently being decoded
    pending: Cell<Option<usize>>,
}

impl FrameDecoder {
    pub(super) fn new(max_frame_len: usize) -> Self {
        Self {
            max_frame_len: Cell::new(max_frame_len),
            pending: Cell::new(None),
        }
    }

    pub(super) fn max_frame_length(&self) -> usize {
        self.max_frame_len.get()
    }

    /// Updates the max frame size, a frame that is already in progress is not affected.
    pub(super) fn set_max_frame_length(&self, val: usize) {
        self.max_frame_len.set(val);
    }

    pub(super) fn decode(&self, src: &mut BytesMut) -> Result<Option<Bytes>, FrameError> {
        let len = if let Some(len) = self.pending.get() {
            len
        } else {
            if src.len() < LENGTH_FIELD_LEN {
                return Ok(None);
            }
            let payload_len =
                (usize::from(src[0]) << 16) | (usize::from(src[1]) << 8) | usize::from(src[2]);
            if payload_len > self.max_frame_len.get() {
                return Err(FrameError::MaxFrameSize);
            }
            let len = payload_len + frame::HEADER_LEN;
            self.pending.set(Some(len));
            len
        };

        if src.len() < len {
            Ok(None)
        } else {
            self.pending.set(None);
            Ok(Some(src.split_to(len)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_frames() {
        let dec = FrameDecoder::new(16_384);
        let mut buf = BytesMut::new();
        assert_eq!(dec.decode(&mut buf).unwrap(), None);

        buf.extend_from_slice(&[0, 0]);
        assert_eq!(dec.decode(&mut buf).unwrap(), None);

        // length 2, 9 byte head, then 2 byte payload and the next frame
        buf.extend_from_slice(&[2, 0, 0, 0, 0, 0, 1, b'a']);
        assert_eq!(dec.decode(&mut buf).unwrap(), None);
        buf.extend_from_slice(&[b'b', 0, 0, 0, 4, 0, 0, 0, 0, 0]);
        assert_eq!(
            dec.decode(&mut buf).unwrap().unwrap(),
            &[0, 0, 2, 0, 0, 0, 0, 0, 1, b'a', b'b'][..]
        );
        assert_eq!(
            dec.decode(&mut buf).unwrap().unwrap(),
            &[0, 0, 0, 4, 0, 0, 0, 0, 0][..]
        );
        assert!(buf.is_empty());
        assert_eq!(dec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn max_frame_length() {
        let dec = FrameDecoder::new(16_384);
        let mut buf = BytesMut::new();

        // 16_384 is allowed, a frame in progress is not affected by a lower limit
        buf.extend_from_slice(&[0, 0x40, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(dec.decode(&mut buf).unwrap(), None);
        dec.set_max_frame_length(100);
        buf.extend_from_slice(&[0; 16_384]);
        assert_eq!(dec.decode(&mut buf).unwrap().unwrap().len(), 16_384 + 9);

        buf.extend_from_slice(&[0, 0, 101]);
        assert_eq!(dec.decode(&mut buf), Err(FrameError::MaxFrameSize));

        let dec = FrameDecoder::new(16_384);
        let mut buf = BytesMut::from(&[0, 0x40, 1][..]);
        assert_eq!(dec.decode(&mut buf), Err(FrameError::MaxFrameSize));
        assert_eq!(dec.max_frame_length(), 16_384);
    }
}
