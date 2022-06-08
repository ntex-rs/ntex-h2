//! Frame a stream of bytes based on a length prefix
//!
//! Many protocols delimit their frames by prefacing frame data with a
//! frame head that specifies the length of the frame. The
//! `length_delimited` module provides utilities for handling the length
//! based framing. This allows the consumer to work with entire frames
//! without having to worry about buffering or other framing logic.
//!
//! # Getting started
//!
//! If implementing a protocol from scratch, using length delimited framing
//! is an easy way to get started. [`LengthDelimitedCodec::new()`] will
//! return a length delimited codec using default configuration values.
//! This can then be used to construct a framer to adapt a full-duplex
//! byte stream into a stream of frames.
//!
//! ```
//! use tokio::io::{AsyncRead, AsyncWrite};
//! use tokio_util::codec::{Framed, LengthDelimitedCodec};
//!
//! fn bind_transport<T: AsyncRead + AsyncWrite>(io: T)
//!     -> Framed<T, LengthDelimitedCodec>
//! {
//!     Framed::new(io, LengthDelimitedCodec::new())
//! }
//! # pub fn main() {}
//! ```
//!
//! The returned transport implements `Sink + Stream` for `BytesMut`. It
//! encodes the frame with a big-endian `u32` header denoting the frame
//! payload length:
//!
//! ```text
//! +----------+--------------------------------+
//! | len: u32 |          frame payload         |
//! +----------+--------------------------------+
//! ```
//!
//! Specifically, given the following:
//!
//! ```
//! use tokio::io::{AsyncRead, AsyncWrite};
//! use tokio_util::codec::{Framed, LengthDelimitedCodec};
//!
//! use futures::SinkExt;
//! use bytes::Bytes;
//!
//! async fn write_frame<T>(io: T) -> Result<(), Box<dyn std::error::Error>>
//! where
//!     T: AsyncRead + AsyncWrite + Unpin,
//! {
//!     let mut transport = Framed::new(io, LengthDelimitedCodec::new());
//!     let frame = Bytes::from("hello world");
//!
//!     transport.send(frame).await?;
//!     Ok(())
//! }
//! ```
//!
//! The encoded frame will look like this:
//!
//! ```text
//! +---- len: u32 ----+---- data ----+
//! | \x00\x00\x00\x0b |  hello world |
//! +------------------+--------------+
//! ```
//!
//! # Decoding
//!
//! [`FramedRead`] adapts an [`AsyncRead`] into a `Stream` of [`BytesMut`],
//! such that each yielded [`BytesMut`] value contains the contents of an
//! entire frame. There are many configuration parameters enabling
//! [`FramedRead`] to handle a wide range of protocols. Here are some
//! examples that will cover the various options at a high level.
//!
//! ## Example 1
//!
//! The following will parse a `u16` length field at offset 0, including the
//! frame head in the yielded `BytesMut`.
//!
//! ```
//! # use tokio::io::AsyncRead;
//! # use tokio_util::codec::LengthDelimitedCodec;
//! # fn bind_read<T: AsyncRead>(io: T) {
//! LengthDelimitedCodec::builder()
//!     .length_field_type::<u16>()
//!     .length_adjustment(0)   // default value
//!     .num_skip(0) // Do not strip frame header
//!     .new_read(io);
//! # }
//! # pub fn main() {}
//! ```
//!
//! The following frame will be decoded as such:
//!
//! ```text
//!          INPUT                           DECODED
//! +-- len ---+--- Payload ---+     +-- len ---+--- Payload ---+
//! | \x00\x0B |  Hello world  | --> | \x00\x0B |  Hello world  |
//! +----------+---------------+     +----------+---------------+
//! ```
//!
//! The value of the length field is 11 (`\x0B`) which represents the length
//! of the payload, `hello world`. By default, [`FramedRead`] assumes that
//! the length field represents the number of bytes that **follows** the
//! length field. Thus, the entire frame has a length of 13: 2 bytes for the
//! frame head + 11 bytes for the payload.
//!
//! ## Example 2
//!
//! The following will parse a `u16` length field at offset 0, omitting the
//! frame head in the yielded `BytesMut`.
//!
//! ```
//! # use tokio::io::AsyncRead;
//! # use tokio_util::codec::LengthDelimitedCodec;
//! # fn bind_read<T: AsyncRead>(io: T) {
//! LengthDelimitedCodec::builder()
//!     .length_field_type::<u16>()
//!     .length_adjustment(0)   // default value
//!     // `num_skip` is not needed, the default is to skip
//!     .new_read(io);
//! # }
//! # pub fn main() {}
//! ```
//!
//! The following frame will be decoded as such:
//!
//! ```text
//!          INPUT                        DECODED
//! +-- len ---+--- Payload ---+     +--- Payload ---+
//! | \x00\x0B |  Hello world  | --> |  Hello world  |
//! +----------+---------------+     +---------------+
//! ```
//!
//! This is similar to the first example, the only difference is that the
//! frame head is **not** included in the yielded `BytesMut` value.
//!
//! ## Example 3
//!
//! The following will parse a `u16` length field at offset 0, including the
//! frame head in the yielded `BytesMut`. In this case, the length field
//! **includes** the frame head length.
//!
//! ```
//! # use tokio::io::AsyncRead;
//! # use tokio_util::codec::LengthDelimitedCodec;
//! # fn bind_read<T: AsyncRead>(io: T) {
//! LengthDelimitedCodec::builder()
//!     .length_field_type::<u16>()
//!     .length_adjustment(-2)  // size of head
//!     .num_skip(0)
//!     .new_read(io);
//! # }
//! # pub fn main() {}
//! ```
//!
//! The following frame will be decoded as such:
//!
//! ```text
//!          INPUT                           DECODED
//! +-- len ---+--- Payload ---+     +-- len ---+--- Payload ---+
//! | \x00\x0D |  Hello world  | --> | \x00\x0D |  Hello world  |
//! +----------+---------------+     +----------+---------------+
//! ```
//!
//! In most cases, the length field represents the length of the payload
//! only, as shown in the previous examples. However, in some protocols the
//! length field represents the length of the whole frame, including the
//! head. In such cases, we specify a negative `length_adjustment` to adjust
//! the value provided in the frame head to represent the payload length.
//!
//! ## Example 4
//!
//! The following will parse a 3 byte length field at offset 0 in a 5 byte
//! frame head, including the frame head in the yielded `BytesMut`.
//!
//! ```
//! # use tokio::io::AsyncRead;
//! # use tokio_util::codec::LengthDelimitedCodec;
//! # fn bind_read<T: AsyncRead>(io: T) {
//! LengthDelimitedCodec::builder()
//!     .length_field_length(3)
//!     .length_adjustment(2)  // remaining head
//!     .num_skip(0)
//!     .new_read(io);
//! # }
//! # pub fn main() {}
//! ```
//!
//! The following frame will be decoded as such:
//!
//! ```text
//!                  INPUT
//! +---- len -----+- head -+--- Payload ---+
//! | \x00\x00\x0B | \xCAFE |  Hello world  |
//! +--------------+--------+---------------+
//!
//!                  DECODED
//! +---- len -----+- head -+--- Payload ---+
//! | \x00\x00\x0B | \xCAFE |  Hello world  |
//! +--------------+--------+---------------+
//! ```
//!
//! A more advanced example that shows a case where there is extra frame
//! head data between the length field and the payload. In such cases, it is
//! usually desirable to include the frame head as part of the yielded
//! `BytesMut`. This lets consumers of the length delimited framer to
//! process the frame head as needed.
//!
//! The positive `length_adjustment` value lets `FramedRead` factor in the
//! additional head into the frame length calculation.
//!
//! ## Example 5
//!
//! The following will parse a `u16` length field at offset 1 of a 4 byte
//! frame head. The first byte and the length field will be omitted from the
//! yielded `BytesMut`, but the trailing 2 bytes of the frame head will be
//! included.
//!
//! ```
//! # use tokio::io::AsyncRead;
//! # use tokio_util::codec::LengthDelimitedCodec;
//! # fn bind_read<T: AsyncRead>(io: T) {
//! LengthDelimitedCodec::builder()
//!     .length_field_type::<u16>()
//!     .length_adjustment(1)  // length of hdr2
//!     .num_skip(3) // length of hdr1 + LEN
//!     .new_read(io);
//! # }
//! # pub fn main() {}
//! ```
//!
//! The following frame will be decoded as such:
//!
//! ```text
//!                  INPUT
//! +- hdr1 -+-- len ---+- hdr2 -+--- Payload ---+
//! |  \xCA  | \x00\x0B |  \xFE  |  Hello world  |
//! +--------+----------+--------+---------------+
//!
//!          DECODED
//! +- hdr2 -+--- Payload ---+
//! |  \xFE  |  Hello world  |
//! +--------+---------------+
//! ```
//!
//! The length field is situated in the middle of the frame head. In this
//! case, the first byte in the frame head could be a version or some other
//! identifier that is not needed for processing. On the other hand, the
//! second half of the head is needed.
//!
//! ## Example 6
//!
//! The following will parse a `u16` length field at offset 1 of a 4 byte
//! frame head. The first byte and the length field will be omitted from the
//! yielded `BytesMut`, but the trailing 2 bytes of the frame head will be
//! included. In this case, the length field **includes** the frame head
//! length.
//!
//! ```
//! # use tokio::io::AsyncRead;
//! # use tokio_util::codec::LengthDelimitedCodec;
//! # fn bind_read<T: AsyncRead>(io: T) {
//! LengthDelimitedCodec::builder()
//!     .length_field_type::<u16>()
//!     .length_adjustment(-3)  // length of hdr1 + LEN, negative
//!     .num_skip(3)
//!     .new_read(io);
//! # }
//! ```
//!
//! The following frame will be decoded as such:
//!
//! ```text
//!                  INPUT
//! +- hdr1 -+-- len ---+- hdr2 -+--- Payload ---+
//! |  \xCA  | \x00\x0F |  \xFE  |  Hello world  |
//! +--------+----------+--------+---------------+
//!
//!          DECODED
//! +- hdr2 -+--- Payload ---+
//! |  \xFE  |  Hello world  |
//! +--------+---------------+
//! ```
//!
//! Similar to the example above, the difference is that the length field
//! represents the length of the entire frame instead of just the payload.
//! The length of `hdr1` and `len` must be counted in `length_adjustment`.
//! Note that the length of `hdr2` does **not** need to be explicitly set
//! anywhere because it already is factored into the total frame length that
//! is read from the byte stream.
//!
//! ## Example 7
//!
//! The following will parse a 3 byte length field at offset 0 in a 4 byte
//! frame head, excluding the 4th byte from the yielded `BytesMut`.
//!
//! ```
//! # use tokio::io::AsyncRead;
//! # use tokio_util::codec::LengthDelimitedCodec;
//! # fn bind_read<T: AsyncRead>(io: T) {
//! LengthDelimitedCodec::builder()
//!     .length_field_length(3)
//!     .length_adjustment(0)  // default value
//!     .num_skip(4) // skip the first 4 bytes
//!     .new_read(io);
//! # }
//! # pub fn main() {}
//! ```
//!
//! The following frame will be decoded as such:
//!
//! ```text
//!                  INPUT                       DECODED
//! +------- len ------+--- Payload ---+    +--- Payload ---+
//! | \x00\x00\x0B\xFF |  Hello world  | => |  Hello world  |
//! +------------------+---------------+    +---------------+
//! ```
//!
//! A simple example where there are unused bytes between the length field
//! and the payload.
//!
//! # Encoding
//!
//! [`FramedWrite`] adapts an [`AsyncWrite`] into a `Sink` of [`BytesMut`],
//! such that each submitted [`BytesMut`] is prefaced by a length field.
//! There are fewer configuration options than [`FramedRead`]. Given
//! protocols that have more complex frame heads, an encoder should probably
//! be written by hand using [`Encoder`].
//!
//! Here is a simple example, given a `FramedWrite` with the following
//! configuration:
//!
//! ```
//! # use tokio::io::AsyncWrite;
//! # use tokio_util::codec::LengthDelimitedCodec;
//! # fn write_frame<T: AsyncWrite>(io: T) {
//! # let _ =
//! LengthDelimitedCodec::builder()
//!     .length_field_type::<u16>()
//!     .new_write(io);
//! # }
//! # pub fn main() {}
//! ```
//!
//! A payload of `hello world` will be encoded as:
//!
//! ```text
//! +- len: u16 -+---- data ----+
//! |  \x00\x0b  |  hello world |
//! +------------+--------------+
//! ```
//!
//! [`LengthDelimitedCodec::new()`]: method@LengthDelimitedCodec::new
//! [`FramedRead`]: struct@FramedRead
//! [`FramedWrite`]: struct@FramedWrite
//! [`AsyncRead`]: trait@tokio::io::AsyncRead
//! [`AsyncWrite`]: trait@tokio::io::AsyncWrite
//! [`Encoder`]: trait@Encoder
//! [`BytesMut`]: bytes::BytesMut

use std::{cell::Cell, cmp, error::Error as StdError, fmt, io::Cursor};

use ntex_bytes::{Buf, BufMut, Bytes, BytesMut};
use ntex_codec::{Decoder, Encoder};

/// Configure length delimited `LengthDelimitedCodec`s.
///
/// `Builder` enables constructing configured length delimited codecs. Note
/// that not all configuration settings apply to both encoding and decoding. See
/// the documentation for specific methods for more detail.
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
pub struct LengthDelimitedCodecError {
    _priv: (),
}

/// A codec for frames delimited by a frame head specifying their lengths.
///
/// This allows the consumer to work with entire frames without having to worry
/// about buffering or other framing logic.
///
/// See [module level] documentation for more detail.
///
/// [module level]: index.html
#[derive(Debug, Clone)]
pub struct LengthDelimitedCodec {
    // Configuration values
    builder: Builder,

    // Read state
    state: Cell<DecodeState>,
}

#[derive(Debug, Clone, Copy)]
enum DecodeState {
    Head,
    Data(usize),
}

impl Default for DecodeState {
    fn default() -> Self {
        DecodeState::Head
    }
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

    /// Updates the max frame setting.
    ///
    /// The change takes effect the next time a frame is decoded. In other
    /// words, if a frame is currently in process of being decoded with a frame
    /// size greater than `val` but less than the max frame length in effect
    /// before calling this function, then the frame will be allowed.
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
                return Err(LengthDelimitedCodecError { _priv: () });
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
            n.ok_or(LengthDelimitedCodecError { _priv: () })?
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
            return Err(LengthDelimitedCodecError { _priv: () });
        }

        // Adjust `n` with bounds checking
        let n = if self.builder.length_adjustment < 0 {
            n.checked_add(-self.builder.length_adjustment as usize)
        } else {
            n.checked_sub(self.builder.length_adjustment as usize)
        };

        let n = n.ok_or_else(|| LengthDelimitedCodecError { _priv: () })?;

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
    ///
    /// # Examples
    ///
    /// ```
    /// # use tokio::io::AsyncRead;
    /// use tokio_util::codec::LengthDelimitedCodec;
    ///
    /// # fn bind_read<T: AsyncRead>(io: T) {
    /// LengthDelimitedCodec::builder()
    ///     .length_field_type::<u16>()
    ///     .length_adjustment(0)
    ///     .num_skip(0)
    ///     .new_read(io);
    /// # }
    /// # pub fn main() {}
    /// ```
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
    ///
    /// This configuration option applies to both encoding and decoding. The
    /// default value is 8MB.
    ///
    /// When decoding, the length field read from the byte stream is checked
    /// against this setting **before** any adjustments are applied. When
    /// encoding, the length of the submitted payload is checked against this
    /// setting.
    ///
    /// When frames exceed the max length, an `io::Error` with the custom value
    /// of the `LengthDelimitedCodecError` type will be returned.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tokio::io::AsyncRead;
    /// use tokio_util::codec::LengthDelimitedCodec;
    ///
    /// # fn bind_read<T: AsyncRead>(io: T) {
    /// LengthDelimitedCodec::builder()
    ///     .max_frame_length(8 * 1024 * 1024)
    ///     .new_read(io);
    /// # }
    /// # pub fn main() {}
    /// ```
    pub fn max_frame_length(&mut self, val: usize) -> &mut Self {
        self.max_frame_len = val;
        self
    }

    /// Sets the number of bytes used to represent the length field
    ///
    /// The default value is `4`. The max value is `8`.
    ///
    /// This configuration option applies to both encoding and decoding.
    ///
    /// # Examples
    ///
    /// ```
    /// # use tokio::io::AsyncRead;
    /// use tokio_util::codec::LengthDelimitedCodec;
    ///
    /// # fn bind_read<T: AsyncRead>(io: T) {
    /// LengthDelimitedCodec::builder()
    ///     .length_field_length(4)
    ///     .new_read(io);
    /// # }
    /// # pub fn main() {}
    /// ```
    pub fn length_field_length(&mut self, val: usize) -> &mut Self {
        assert!(val > 0 && val <= 8, "invalid length field length");
        self.length_field_len = val;
        self
    }

    /// Delta between the payload length specified in the header and the real
    /// payload length
    ///
    /// # Examples
    ///
    /// ```
    /// # use tokio::io::AsyncRead;
    /// use tokio_util::codec::LengthDelimitedCodec;
    ///
    /// # fn bind_read<T: AsyncRead>(io: T) {
    /// LengthDelimitedCodec::builder()
    ///     .length_adjustment(-2)
    ///     .new_read(io);
    /// # }
    /// # pub fn main() {}
    /// ```
    pub fn length_adjustment(&mut self, val: isize) -> &mut Self {
        self.length_adjustment = val;
        self
    }

    /// Sets the number of bytes to skip before reading the payload
    ///
    /// Default value is `length_field_len
    ///
    /// This configuration option only applies to decoding
    ///
    /// # Examples
    ///
    /// ```
    /// # use tokio::io::AsyncRead;
    /// use tokio_util::codec::LengthDelimitedCodec;
    ///
    /// # fn bind_read<T: AsyncRead>(io: T) {
    /// LengthDelimitedCodec::builder()
    ///     .num_skip(4)
    ///     .new_read(io);
    /// # }
    /// # pub fn main() {}
    /// ```
    pub fn num_skip(&mut self, val: usize) -> &mut Self {
        self.num_skip = Some(val);
        self
    }

    /// Create a configured length delimited `LengthDelimitedCodec`
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_util::codec::LengthDelimitedCodec;
    /// # pub fn main() {
    /// LengthDelimitedCodec::builder()
    ///     .length_field_type::<u16>()
    ///     .length_adjustment(0)
    ///     .num_skip(0)
    ///     .new_codec();
    /// # }
    /// ```
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

impl fmt::Debug for LengthDelimitedCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LengthDelimitedCodecError").finish()
    }
}

impl fmt::Display for LengthDelimitedCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("frame size too big")
    }
}

impl StdError for LengthDelimitedCodecError {}
