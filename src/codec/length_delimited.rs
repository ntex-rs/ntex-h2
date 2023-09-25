//! Frame a stream of bytes based on a length prefix
use std::{cell::Cell, cmp, error::Error as StdError, fmt, io::Cursor};

use ntex_bytes::{Buf, BufMut, Bytes, BytesMut};
use ntex_codec::{Decoder, Encoder};

/// Configure length delimited `LengthDelimitedCodec`s.
#[derive(Debug, Clone, Copy)]
pub struct Builder {
    // Maximum frame length
    max_frame_len: usize,

    // Number of bytes representing the field length
    length_field_len: usize,

    // Adjust the length specified in the header field by this amount
    length_adjustment: isize,

    // Total number of bytes to skip before reading the payload, if not set,
    // `length_field_len
    num_skip: Option<usize>,
}

/// An error when the number of bytes read is more than max frame length.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LengthDelimitedCodecError {
    MaxSize,
    Adjusted,
}

/// A codec for frames delimited by a frame head specifying their lengths.
#[derive(Debug, Clone)]
pub struct LengthDelimitedCodec {
    // Configuration values
    builder: Builder,

    // Read state
    state: Cell<DecodeState>,
}

#[derive(Debug, Clone, Copy, Default)]
enum DecodeState {
    #[default]
    Head,
    Data(usize),
}

// ===== impl LengthDelimitedCodec ======

impl LengthDelimitedCodec {
    /// Creates a new `LengthDelimitedCodec` with the default configuration values.
    pub fn new() -> Self {
        Self {
            builder: Builder::new(),
            state: Cell::new(DecodeState::Head),
        }
    }

    /// max frame size setting.
    pub fn max_frame_length(&self) -> usize {
        self.builder.max_frame_len
    }

    /// Updates the max frame setting.
    pub fn set_max_frame_length(&mut self, val: usize) {
        self.builder.max_frame_length(val);
    }

    fn decode_head(&self, src: &mut BytesMut) -> Result<Option<usize>, LengthDelimitedCodecError> {
        let head_len = self.builder.num_head_bytes();
        let field_len = self.builder.length_field_len;

        if src.len() < head_len {
            // Not enough data
            return Ok(None);
        }

        let n = {
            let mut src = Cursor::new(&mut *src);

            let n = src.get_uint(field_len);
            if n > self.builder.max_frame_len as u64 {
                return Err(LengthDelimitedCodecError::MaxSize);
            }

            // The check above ensures there is no overflow
            let n = n as usize;

            // Adjust `n` with bounds checking
            let n = if self.builder.length_adjustment < 0 {
                n.checked_sub(-self.builder.length_adjustment as usize)
            } else {
                n.checked_add(self.builder.length_adjustment as usize)
            };

            // Error handling
            n.ok_or(LengthDelimitedCodecError::Adjusted)?
        };

        let num_skip = self.builder.get_num_skip();
        if num_skip > 0 {
            src.advance(num_skip);
        }

        // Ensure that the buffer has enough space to read the incoming
        // payload
        src.reserve(n);

        Ok(Some(n))
    }

    fn decode_data(&self, n: usize, src: &mut BytesMut) -> Option<BytesMut> {
        // At this point, the buffer has already had the required capacity
        // reserved. All there is to do is read.
        if src.len() < n {
            return None;
        }

        Some(src.split_to(n))
    }
}

impl Decoder for LengthDelimitedCodec {
    type Item = BytesMut;
    type Error = LengthDelimitedCodecError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<BytesMut>, LengthDelimitedCodecError> {
        let n = match self.state.get() {
            DecodeState::Head => match self.decode_head(src)? {
                Some(n) => {
                    self.state.set(DecodeState::Data(n));
                    n
                }
                None => return Ok(None),
            },
            DecodeState::Data(n) => n,
        };

        match self.decode_data(n, src) {
            Some(data) => {
                // Update the decode state
                self.state.set(DecodeState::Head);

                // Make sure the buffer has enough space to read the next head
                src.reserve(self.builder.num_head_bytes());

                Ok(Some(data))
            }
            None => Ok(None),
        }
    }
}

impl Encoder for LengthDelimitedCodec {
    type Item = Bytes;
    type Error = LengthDelimitedCodecError;

    fn encode(&self, data: Bytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let n = data.len();

        if n > self.builder.max_frame_len {
            return Err(LengthDelimitedCodecError::MaxSize);
        }

        // Adjust `n` with bounds checking
        let n = if self.builder.length_adjustment < 0 {
            n.checked_add(-self.builder.length_adjustment as usize)
        } else {
            n.checked_sub(self.builder.length_adjustment as usize)
        };

        let n = n.ok_or(LengthDelimitedCodecError::Adjusted)?;

        // Reserve capacity in the destination buffer to fit the frame and
        // length field (plus adjustment).
        dst.reserve(self.builder.length_field_len + n);

        dst.put_uint(n as u64, self.builder.length_field_len);

        // Write the frame to the buffer
        dst.extend_from_slice(&data[..]);

        Ok(())
    }
}

impl Default for LengthDelimitedCodec {
    fn default() -> Self {
        Self::new()
    }
}

// ===== impl Builder =====

impl Builder {
    /// Creates a new length delimited codec builder with default configuration
    /// values.
    pub fn new() -> Builder {
        Builder {
            // Default max frame length of 8MB
            max_frame_len: 8 * 1_024 * 1_024,

            // Default byte length of 4
            length_field_len: 4,

            length_adjustment: 0,

            // Total number of bytes to skip before reading the payload, if not set,
            // `length_field_len
            num_skip: None,
        }
    }

    /// Sets the max frame length in bytes
    pub fn max_frame_length(&mut self, val: usize) -> &mut Self {
        self.max_frame_len = val;
        self
    }

    /// Sets the number of bytes used to represent the length field
    pub fn length_field_length(&mut self, val: usize) -> &mut Self {
        assert!(val > 0 && val <= 8, "invalid length field length");
        self.length_field_len = val;
        self
    }

    /// Delta between the payload length specified in the header and the real
    /// payload length
    pub fn length_adjustment(&mut self, val: isize) -> &mut Self {
        self.length_adjustment = val;
        self
    }

    /// Sets the number of bytes to skip before reading the payload
    pub fn num_skip(&mut self, val: usize) -> &mut Self {
        self.num_skip = Some(val);
        self
    }

    /// Create a configured length delimited `LengthDelimitedCodec`
    pub fn new_codec(&self) -> LengthDelimitedCodec {
        LengthDelimitedCodec {
            builder: *self,
            state: Cell::new(DecodeState::Head),
        }
    }

    fn num_head_bytes(&self) -> usize {
        cmp::max(self.length_field_len, self.num_skip.unwrap_or(0))
    }

    fn get_num_skip(&self) -> usize {
        self.num_skip.unwrap_or(self.length_field_len)
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

// ===== impl LengthDelimitedCodecError =====

impl fmt::Display for LengthDelimitedCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("frame size too big")
    }
}

impl StdError for LengthDelimitedCodecError {}
