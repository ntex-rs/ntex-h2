//! HPACK header compression primitives.
//!
//! [`Encoder`] and [`Decoder`] maintain the dynamic table state for one
//! HTTP/2 connection.

mod decoder;
mod encoder;
pub(crate) mod header;
pub(crate) mod huffman;
mod table;

#[cfg(test)]
mod test;

pub use self::decoder::{Decoder, DecoderError, NeedMore};
pub use self::encoder::Encoder;
pub use self::header::Header;
