use std::{cmp, collections::VecDeque, str::Utf8Error};

use ntex_bytes::{ByteString, Bytes, BytesMut};
use ntex_http::{Method, StatusCode, compat, header};

use super::{Header, huffman};

/// Stateful HPACK header decoder.
#[derive(Debug)]
pub struct Decoder {
    // Protocol indicated that the max table size will update
    max_size_update: Option<usize>,
    last_max_update: usize,
    table: Table,
    buffer: BytesMut,
}

/// Represents all errors that can be encountered while performing the decoding
/// of an HPACK header set.
#[derive(thiserror::Error, Debug, Copy, Clone, PartialEq, Eq)]
pub enum DecoderError {
    /// The encoded header representation is invalid.
    #[error("InvalidRepresentation")]
    InvalidRepresentation,
    /// An integer uses an invalid prefix.
    #[error("InvalidIntegerPrefix")]
    InvalidIntegerPrefix,
    /// A table index is invalid.
    #[error("InvalidTableIndex")]
    InvalidTableIndex,
    /// A Huffman-encoded string is invalid.
    #[error("InvalidHuffmanCode")]
    InvalidHuffmanCode,
    /// A pseudo-header string is not valid UTF-8.
    #[error("InvalidUtf8")]
    InvalidUtf8,
    /// A status pseudo-header is invalid.
    #[error("InvalidStatusCode")]
    InvalidStatusCode,
    /// A pseudo-header name is invalid.
    #[error("InvalidPseudoheader")]
    InvalidPseudoheader,
    /// A dynamic table size update exceeds the configured maximum.
    #[error("InvalidMaxDynamicSize")]
    InvalidMaxDynamicSize,
    /// An encoded integer overflowed.
    #[error("IntegerOverflow")]
    IntegerOverflow,
    /// More bytes are required to finish decoding.
    #[error("{0}")]
    NeedMore(NeedMore),
}

/// Reason an HPACK representation needs more input.
#[derive(thiserror::Error, Debug, Copy, Clone, PartialEq, Eq)]
pub enum NeedMore {
    /// The input ended before a representation was complete.
    #[error("Unexpected end of stream")]
    UnexpectedEndOfStream,
    /// The input ended while decoding an integer.
    #[error("Integer underflow")]
    IntegerUnderflow,
    /// The input ended while decoding a string.
    #[error("String underflow")]
    StringUnderflow,
}

enum Representation {
    /// Indexed header field representation
    ///
    /// An indexed header field representation identifies an entry in either the
    /// static table or the dynamic table (see Section 2.3).
    ///
    /// # Header encoding
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 1 |        Index (7+)         |
    /// +---+---------------------------+
    /// ```
    Indexed,

    /// Literal Header Field with Incremental Indexing
    ///
    /// A literal header field with incremental indexing representation results
    /// in appending a header field to the decoded header list and inserting it
    /// as a new entry into the dynamic table.
    ///
    /// # Header encoding
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 1 |      Index (6+)       |
    /// +---+---+-----------------------+
    /// | H |     Value Length (7+)     |
    /// +---+---------------------------+
    /// | Value String (Length octets)  |
    /// +-------------------------------+
    /// ```
    LiteralWithIndexing,

    /// Literal Header Field without Indexing
    ///
    /// A literal header field without indexing representation results in
    /// appending a header field to the decoded header list without altering the
    /// dynamic table.
    ///
    /// # Header encoding
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 0 | 0 | 0 |  Index (4+)   |
    /// +---+---+-----------------------+
    /// | H |     Value Length (7+)     |
    /// +---+---------------------------+
    /// | Value String (Length octets)  |
    /// +-------------------------------+
    /// ```
    LiteralWithoutIndexing,

    /// Literal Header Field Never Indexed
    ///
    /// A literal header field never-indexed representation results in appending
    /// a header field to the decoded header list without altering the dynamic
    /// table. Intermediaries MUST use the same representation for encoding this
    /// header field.
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 0 | 0 | 1 |  Index (4+)   |
    /// +---+---+-----------------------+
    /// | H |     Value Length (7+)     |
    /// +---+---------------------------+
    /// | Value String (Length octets)  |
    /// +-------------------------------+
    /// ```
    LiteralNeverIndexed,

    /// Dynamic Table Size Update
    ///
    /// A dynamic table size update signals a change to the size of the dynamic
    /// table.
    ///
    /// # Header encoding
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 0 | 1 |   Max size (5+)   |
    /// +---+---------------------------+
    /// ```
    SizeUpdate,
}

#[derive(Debug)]
struct Table {
    entries: VecDeque<Header>,
    size: usize,
    max_size: usize,
}

/// Read position within the header block being decoded.
struct Input<'a> {
    src: &'a Bytes,
    data: &'a [u8],
    pos: usize,
    /// End of the last fully decoded representation
    committed: usize,
}

// ===== impl Decoder =====

impl Decoder {
    /// Creates a decoder with the specified maximum dynamic table size.
    pub fn new(size: usize) -> Decoder {
        Decoder {
            max_size_update: None,
            last_max_update: size,
            table: Table::new(size),
            buffer: BytesMut::with_capacity(4096),
        }
    }

    /// Releases the dynamic table and the decoding buffer.
    ///
    /// The peer's table state is lost, the decoder must not be used for the
    /// same connection afterwards.
    pub(crate) fn release(&mut self) {
        self.max_size_update = None;
        self.table = Table::new(self.table.max_size);
        self.buffer = BytesMut::new();
    }

    #[cfg(test)]
    /// Returns the dynamic table size and the decoding buffer capacity.
    pub(crate) fn retained(&self) -> (usize, usize) {
        (self.table.size(), self.buffer.capacity())
    }

    /// Queues a maximum dynamic table size update.
    #[allow(dead_code)]
    pub fn queue_size_update(&mut self, size: usize) {
        let size = match self.max_size_update {
            Some(v) => cmp::max(v, size),
            None => size,
        };

        self.max_size_update = Some(size);
    }

    /// Decodes one HPACK header block and invokes `f` for each header.
    ///
    /// Decoded representations are removed from `src`. On error `src` starts
    /// with the representation that failed, so decoding can be resumed once
    /// more data is available.
    pub fn decode<F>(&mut self, src: &mut Bytes, f: F) -> Result<(), DecoderError>
    where
        F: FnMut(Header),
    {
        if let Some(size) = self.max_size_update.take() {
            self.last_max_update = size;
        }

        let mut input = Input::new(src);
        let res = self.decode_block(&mut input, f);
        let (committed, len) = (input.committed, input.data.len());

        if committed == len {
            // Advancing alone would keep the allocation alive.
            src.clear();
        } else if committed != 0 {
            src.advance_to(committed);
        }
        res
    }

    fn decode_block<F>(&mut self, src: &mut Input<'_>, mut f: F) -> Result<(), DecoderError>
    where
        F: FnMut(Header),
    {
        let mut can_resize = true;

        while let Some(ty) = src.peek() {
            // At this point we are always at the beginning of the next block
            // within the HPACK data. The type of the block can always be
            // determined from the first byte.
            match Representation::load(ty)? {
                Representation::Indexed => {
                    // tracing::trace!(rem = src.remaining(), kind = %"Indexed");
                    can_resize = false;
                    let entry = self.decode_indexed(src)?;
                    src.commit();
                    f(entry);
                }
                Representation::LiteralWithIndexing => {
                    // tracing::trace!(rem = src.remaining(), kind = %"LiteralWithIndexing");
                    can_resize = false;
                    let entry = self.decode_literal(src, true)?;

                    // Insert the header into the table
                    self.table.insert(entry.clone());
                    src.commit();

                    f(entry);
                }
                Representation::LiteralWithoutIndexing => {
                    // tracing::trace!(rem = src.remaining(), kind = %"LiteralWithoutIndexing");
                    can_resize = false;
                    let entry = self.decode_literal(src, false)?;
                    src.commit();
                    f(entry);
                }
                Representation::LiteralNeverIndexed => {
                    // tracing::trace!(rem = src.remaining(), kind = %"LiteralNeverIndexed");
                    can_resize = false;
                    let entry = self.decode_literal(src, false)?;
                    src.commit();

                    // TODO: Track that this should never be indexed

                    f(entry);
                }
                Representation::SizeUpdate => {
                    // tracing::trace!(rem = src.remaining(), kind = %"SizeUpdate");
                    if !can_resize {
                        return Err(DecoderError::InvalidMaxDynamicSize);
                    }

                    // Handle the dynamic table size update
                    self.process_size_update(src)?;
                    src.commit();
                }
            }
        }

        Ok(())
    }

    fn process_size_update(&mut self, buf: &mut Input<'_>) -> Result<(), DecoderError> {
        let new_size = decode_int(buf, 5)?;

        if new_size > self.last_max_update {
            return Err(DecoderError::InvalidMaxDynamicSize);
        }

        log::debug!(
            "Decoder changed max table size, from {} to {}",
            self.table.size(),
            new_size,
        );

        self.table.set_max_size(new_size);

        Ok(())
    }

    fn decode_indexed(&self, buf: &mut Input<'_>) -> Result<Header, DecoderError> {
        let index = decode_int(buf, 7)?;
        self.table.get(index)
    }

    fn decode_literal(&mut self, buf: &mut Input<'_>, index: bool) -> Result<Header, DecoderError> {
        let prefix = if index { 6 } else { 4 };

        // Extract the table index for the name, or 0 if not indexed
        let table_idx = decode_int(buf, prefix)?;

        // First, read the header name
        if table_idx == 0 {
            // Read the name as a literal
            let name = self.decode_string(buf)?;
            let value = self.decode_string(buf)?;
            Header::new(&name, value)
        } else {
            // validate the index before reading the value
            self.table.check_index(table_idx)?;
            let value = self.decode_string(buf)?;

            self.table.get_with_value(table_idx, value)
        }
    }

    fn decode_string(&mut self, buf: &mut Input<'_>) -> Result<Bytes, DecoderError> {
        const HUFF_FLAG: u8 = 0b1000_0000;

        // The first bit in the first byte contains the huffman encoded flag.
        let huff = match buf.peek() {
            Some(hdr) => (hdr & HUFF_FLAG) == HUFF_FLAG,
            None => return Err(DecoderError::NeedMore(NeedMore::UnexpectedEndOfStream)),
        };

        // Decode the string length using 7 bit prefix
        let len = decode_int(buf, 7)?;

        if len > buf.remaining() {
            log::trace!("decode_string underflow {:?} {:?}", len, buf.remaining());
            return Err(DecoderError::NeedMore(NeedMore::StringUnderflow));
        }

        let start = buf.pos;
        buf.pos += len;
        if huff {
            huffman::decode(&buf.data[start..buf.pos], &mut self.buffer)
        } else {
            // Short values are copied inline and do not retain the source
            Ok(buf.src.slice(start..buf.pos))
        }
    }
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder::new(4096)
    }
}

// ===== impl Representation =====

impl Representation {
    pub(super) fn load(byte: u8) -> Result<Representation, DecoderError> {
        const INDEXED: u8 = 0b1000_0000;
        const LITERAL_WITH_INDEXING: u8 = 0b0100_0000;
        const LITERAL_WITHOUT_INDEXING: u8 = 0b1111_0000;
        const LITERAL_NEVER_INDEXED: u8 = 0b0001_0000;
        const SIZE_UPDATE_MASK: u8 = 0b1110_0000;
        const SIZE_UPDATE: u8 = 0b0010_0000;

        // TODO: What did I even write here?

        if byte & INDEXED == INDEXED {
            Ok(Representation::Indexed)
        } else if byte & LITERAL_WITH_INDEXING == LITERAL_WITH_INDEXING {
            Ok(Representation::LiteralWithIndexing)
        } else if byte & LITERAL_WITHOUT_INDEXING == 0 {
            Ok(Representation::LiteralWithoutIndexing)
        } else if byte & LITERAL_WITHOUT_INDEXING == LITERAL_NEVER_INDEXED {
            Ok(Representation::LiteralNeverIndexed)
        } else if byte & SIZE_UPDATE_MASK == SIZE_UPDATE {
            Ok(Representation::SizeUpdate)
        } else {
            Err(DecoderError::InvalidRepresentation)
        }
    }
}

#[inline]
fn decode_int(buf: &mut Input<'_>, prefix_size: u8) -> Result<usize, DecoderError> {
    if !(1..=8).contains(&prefix_size) {
        return Err(DecoderError::InvalidIntegerPrefix);
    }

    let Some(b) = buf.read_u8() else {
        return Err(DecoderError::NeedMore(NeedMore::IntegerUnderflow));
    };

    let mask = if prefix_size == 8 {
        0xFF
    } else {
        (1u8 << prefix_size).wrapping_sub(1)
    };

    let ret = (b & mask) as usize;

    if ret < mask as usize {
        // Value fits in the prefix bits
        Ok(ret)
    } else {
        decode_int_multi(buf, ret)
    }
}

/// Decodes the varint continuation of an integer that did not fit in the
/// prefix bits.
#[cold]
#[inline(never)]
fn decode_int_multi(buf: &mut Input<'_>, mut ret: usize) -> Result<usize, DecoderError> {
    // The octet limit is chosen such that the maximum allowed *value* can
    // never overflow an unsigned 32-bit integer. The maximum value of any
    // integer that can be encoded with 5 octets is ~2^28
    const MAX_BYTES: usize = 5;
    const VARINT_MASK: u8 = 0b0111_1111;
    const VARINT_FLAG: u8 = 0b1000_0000;

    // The total number of bytes used to represent the int. The first byte was
    // the prefix, so start at 1.
    let mut bytes = 1;

    // The rest of the int is stored as a varint -- 7 bits for the value and 1
    // bit to indicate if it is the last byte.
    let mut shift = 0;

    while let Some(b) = buf.read_u8() {
        bytes += 1;
        ret += ((b & VARINT_MASK) as usize) << shift;
        shift += 7;

        if b & VARINT_FLAG == 0 {
            return Ok(ret);
        }

        if bytes == MAX_BYTES {
            // The spec requires that this situation is an error
            return Err(DecoderError::IntegerOverflow);
        }
    }

    Err(DecoderError::NeedMore(NeedMore::IntegerUnderflow))
}

// ===== impl Input =====

impl<'a> Input<'a> {
    fn new(src: &'a Bytes) -> Self {
        Input {
            src,
            data: src.as_ref(),
            pos: 0,
            committed: 0,
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    #[inline]
    fn read_u8(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    #[inline]
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Marks the input read so far as decoded.
    #[inline]
    fn commit(&mut self) {
        self.committed = self.pos;
    }
}

// ===== impl Table =====

impl Table {
    fn new(max_size: usize) -> Table {
        Table {
            entries: VecDeque::new(),
            size: 0,
            max_size,
        }
    }

    fn size(&self) -> usize {
        self.size
    }

    /// Returns the entry located at the given index.
    ///
    /// The table is 1-indexed and constructed in such a way that the first
    /// entries belong to the static table, followed by entries in the dynamic
    /// table. They are merged into a single index address space, though.
    ///
    /// This follows [HPACK section 2.3.3].
    ///
    /// [HPACK section 2.3.3]: https://www.rfc-editor.org/rfc/rfc7541#section-2.3.3
    #[inline]
    pub(super) fn get(&self, index: usize) -> Result<Header, DecoderError> {
        if index == 0 {
            return Err(DecoderError::InvalidTableIndex);
        }

        if index <= 61 {
            return Ok(get_static(index));
        }

        // Convert the index for lookup in the entries structure.
        match self.entries.get(index - 62) {
            Some(e) => Ok(e.clone()),
            None => Err(DecoderError::InvalidTableIndex),
        }
    }

    /// Checks that the index refers to an existing entry.
    fn check_index(&self, index: usize) -> Result<(), DecoderError> {
        if index == 0 || index > 61 + self.entries.len() {
            Err(DecoderError::InvalidTableIndex)
        } else {
            Ok(())
        }
    }

    /// Combines the name of the entry at `index` with `value`, without
    /// cloning the stored value.
    fn get_with_value(&self, index: usize, value: Bytes) -> Result<Header, DecoderError> {
        if index == 0 {
            return Err(DecoderError::InvalidTableIndex);
        }

        if index <= 61 {
            return get_static(index).name().into_entry(value);
        }

        match self.entries.get(index - 62) {
            Some(e) => e.name().into_entry(value),
            None => Err(DecoderError::InvalidTableIndex),
        }
    }

    fn insert(&mut self, entry: Header) {
        let len = entry.len();

        self.reserve(len);

        if self.size + len <= self.max_size {
            self.size += len;

            // Track the entry, the copy does not pin the read buffer
            self.entries.push_front(entry.detach());
        }
    }

    fn set_max_size(&mut self, size: usize) {
        self.max_size = size;
        // Make the table size fit within the new constraints.
        self.consolidate();
    }

    fn reserve(&mut self, size: usize) {
        while self.size + size > self.max_size {
            match self.entries.pop_back() {
                Some(last) => {
                    self.size -= last.len();
                }
                None => return,
            }
        }
    }

    fn consolidate(&mut self) {
        while self.size > self.max_size {
            {
                let Some(last) = self.entries.back() else {
                    // Can never happen as the size of the table must reach
                    // 0 by the time we've exhausted all elements.
                    panic!("Size of table != 0, but no headers left!");
                };

                self.size -= last.len();
            }

            self.entries.pop_back();
        }
    }
}

// ===== impl DecoderError =====

impl From<Utf8Error> for DecoderError {
    fn from(_: Utf8Error) -> DecoderError {
        DecoderError::InvalidUtf8
    }
}

impl From<()> for DecoderError {
    fn from(_e: ()) -> DecoderError {
        DecoderError::InvalidUtf8
    }
}

impl From<header::InvalidHeaderValue> for DecoderError {
    fn from(_: header::InvalidHeaderValue) -> DecoderError {
        DecoderError::InvalidUtf8
    }
}

impl From<header::InvalidHeaderName> for DecoderError {
    fn from(_: header::InvalidHeaderName) -> DecoderError {
        DecoderError::InvalidUtf8
    }
}

impl From<compat::InvalidMethod> for DecoderError {
    fn from(_: compat::InvalidMethod) -> DecoderError {
        DecoderError::InvalidUtf8
    }
}

impl From<compat::InvalidStatusCode> for DecoderError {
    fn from(_: compat::InvalidStatusCode) -> DecoderError {
        // TODO: Better error
        DecoderError::InvalidUtf8
    }
}

/// Get an entry from the static table
pub(super) fn get_static(idx: usize) -> Header {
    use ntex_http::header::HeaderValue;

    match idx {
        1 => Header::Authority(ByteString::from_static("")),
        2 => Header::Method(Method::GET),
        3 => Header::Method(Method::POST),
        4 => Header::Path(ByteString::from_static("/")),
        5 => Header::Path(ByteString::from_static("/index.html")),
        6 => Header::Scheme(ByteString::from_static("http")),
        7 => Header::Scheme(ByteString::from_static("https")),
        8 => Header::Status(StatusCode::OK),
        9 => Header::Status(StatusCode::NO_CONTENT),
        10 => Header::Status(StatusCode::PARTIAL_CONTENT),
        11 => Header::Status(StatusCode::NOT_MODIFIED),
        12 => Header::Status(StatusCode::BAD_REQUEST),
        13 => Header::Status(StatusCode::NOT_FOUND),
        14 => Header::Status(StatusCode::INTERNAL_SERVER_ERROR),
        15 => Header::Field {
            name: header::ACCEPT_CHARSET,
            value: HeaderValue::from_static(""),
        },
        16 => Header::Field {
            name: header::ACCEPT_ENCODING,
            value: HeaderValue::from_static("gzip, deflate"),
        },
        17 => Header::Field {
            name: header::ACCEPT_LANGUAGE,
            value: HeaderValue::from_static(""),
        },
        18 => Header::Field {
            name: header::ACCEPT_RANGES,
            value: HeaderValue::from_static(""),
        },
        19 => Header::Field {
            name: header::ACCEPT,
            value: HeaderValue::from_static(""),
        },
        20 => Header::Field {
            name: header::ACCESS_CONTROL_ALLOW_ORIGIN,
            value: HeaderValue::from_static(""),
        },
        21 => Header::Field {
            name: header::AGE,
            value: HeaderValue::from_static(""),
        },
        22 => Header::Field {
            name: header::ALLOW,
            value: HeaderValue::from_static(""),
        },
        23 => Header::Field {
            name: header::AUTHORIZATION,
            value: HeaderValue::from_static(""),
        },
        24 => Header::Field {
            name: header::CACHE_CONTROL,
            value: HeaderValue::from_static(""),
        },
        25 => Header::Field {
            name: header::CONTENT_DISPOSITION,
            value: HeaderValue::from_static(""),
        },
        26 => Header::Field {
            name: header::CONTENT_ENCODING,
            value: HeaderValue::from_static(""),
        },
        27 => Header::Field {
            name: header::CONTENT_LANGUAGE,
            value: HeaderValue::from_static(""),
        },
        28 => Header::Field {
            name: header::CONTENT_LENGTH,
            value: HeaderValue::from_static(""),
        },
        29 => Header::Field {
            name: header::CONTENT_LOCATION,
            value: HeaderValue::from_static(""),
        },
        30 => Header::Field {
            name: header::CONTENT_RANGE,
            value: HeaderValue::from_static(""),
        },
        31 => Header::Field {
            name: header::CONTENT_TYPE,
            value: HeaderValue::from_static(""),
        },
        32 => Header::Field {
            name: header::COOKIE,
            value: HeaderValue::from_static(""),
        },
        33 => Header::Field {
            name: header::DATE,
            value: HeaderValue::from_static(""),
        },
        34 => Header::Field {
            name: header::ETAG,
            value: HeaderValue::from_static(""),
        },
        35 => Header::Field {
            name: header::EXPECT,
            value: HeaderValue::from_static(""),
        },
        36 => Header::Field {
            name: header::EXPIRES,
            value: HeaderValue::from_static(""),
        },
        37 => Header::Field {
            name: header::FROM,
            value: HeaderValue::from_static(""),
        },
        38 => Header::Field {
            name: header::HOST,
            value: HeaderValue::from_static(""),
        },
        39 => Header::Field {
            name: header::IF_MATCH,
            value: HeaderValue::from_static(""),
        },
        40 => Header::Field {
            name: header::IF_MODIFIED_SINCE,
            value: HeaderValue::from_static(""),
        },
        41 => Header::Field {
            name: header::IF_NONE_MATCH,
            value: HeaderValue::from_static(""),
        },
        42 => Header::Field {
            name: header::IF_RANGE,
            value: HeaderValue::from_static(""),
        },
        43 => Header::Field {
            name: header::IF_UNMODIFIED_SINCE,
            value: HeaderValue::from_static(""),
        },
        44 => Header::Field {
            name: header::LAST_MODIFIED,
            value: HeaderValue::from_static(""),
        },
        45 => Header::Field {
            name: header::LINK,
            value: HeaderValue::from_static(""),
        },
        46 => Header::Field {
            name: header::LOCATION,
            value: HeaderValue::from_static(""),
        },
        47 => Header::Field {
            name: header::MAX_FORWARDS,
            value: HeaderValue::from_static(""),
        },
        48 => Header::Field {
            name: header::PROXY_AUTHENTICATE,
            value: HeaderValue::from_static(""),
        },
        49 => Header::Field {
            name: header::PROXY_AUTHORIZATION,
            value: HeaderValue::from_static(""),
        },
        50 => Header::Field {
            name: header::RANGE,
            value: HeaderValue::from_static(""),
        },
        51 => Header::Field {
            name: header::REFERER,
            value: HeaderValue::from_static(""),
        },
        52 => Header::Field {
            name: header::REFRESH,
            value: HeaderValue::from_static(""),
        },
        53 => Header::Field {
            name: header::RETRY_AFTER,
            value: HeaderValue::from_static(""),
        },
        54 => Header::Field {
            name: header::SERVER,
            value: HeaderValue::from_static(""),
        },
        55 => Header::Field {
            name: header::SET_COOKIE,
            value: HeaderValue::from_static(""),
        },
        56 => Header::Field {
            name: header::STRICT_TRANSPORT_SECURITY,
            value: HeaderValue::from_static(""),
        },
        57 => Header::Field {
            name: header::TRANSFER_ENCODING,
            value: HeaderValue::from_static(""),
        },
        58 => Header::Field {
            name: header::USER_AGENT,
            value: HeaderValue::from_static(""),
        },
        59 => Header::Field {
            name: header::VARY,
            value: HeaderValue::from_static(""),
        },
        60 => Header::Field {
            name: header::VIA,
            value: HeaderValue::from_static(""),
        },
        61 => Header::Field {
            name: header::WWW_AUTHENTICATE,
            value: HeaderValue::from_static(""),
        },
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// STATEMENT: `decode` removes decoded representations from `src`, keeps an incomplete one,
    /// and releases a fully decoded buffer.
    ///
    /// HOW:
    /// - Decode an indexed `:method` followed by a truncated literal and check `NeedMore`, the
    ///   single emitted header, and that `src` holds exactly the truncated literal.
    /// - Decode the complete literal from a heap-allocated buffer and check that `src` ends
    ///   empty and inline.
    #[test]
    fn test_decode_commits_representations() {
        let mut data = vec![0x82, 0x00, 3];
        data.extend(b"foo");
        data.push(100);
        data.extend([b'v'; 100]);

        let mut de = Decoder::default();
        let mut res = vec![];
        let mut src = Bytes::copy_from_slice(&data[..50]);
        let err = de.decode(&mut src, |h| res.push(h)).unwrap_err();
        assert_eq!(err, DecoderError::NeedMore(NeedMore::StringUnderflow));
        assert_eq!(res, vec![Header::Method(Method::GET)]);
        assert_eq!(src, &data[1..50]);

        let mut src = Bytes::copy_from_slice(&data[1..]);
        assert!(!src.is_inline());
        de.decode(&mut src, |h| res.push(h)).unwrap();
        assert_eq!(res.len(), 2);
        assert!(src.is_empty());
        assert!(src.is_inline(), "decoded buffer must not retain the allocation");
    }

    /// STATEMENT: Raw literal values share the source buffer, values that fit inline are copied
    /// and do not retain it.
    ///
    /// HOW:
    /// - Decode two non-indexed literals with raw 4-byte and 64-byte values.
    /// - Check that only the 64-byte value points into the source allocation.
    #[test]
    fn test_decode_raw_values() {
        let mut data = vec![];
        for len in [4, 64] {
            data.extend([0x00, 3]);
            data.extend(b"foo");
            data.push(len as u8);
            data.extend(vec![b'v'; len]);
        }
        let src = Bytes::copy_from_slice(&data);
        let range = src.as_ref().as_ptr_range();

        let mut res = vec![];
        Decoder::default()
            .decode(&mut src.clone(), |h| res.push(h))
            .unwrap();

        let values: Vec<_> = res
            .iter()
            .map(|h| match h {
                Header::Field { value, .. } => (value.len(), range.contains(&value.as_bytes().as_ptr())),
                h => panic!("unexpected header {h:?}"),
            })
            .collect();
        assert_eq!(values, vec![(4, false), (64, true)]);
    }

    /// STATEMENT: Decoding a string from empty input requests more bytes.
    ///
    /// HOW:
    /// - Create a `Decoder` with zero table capacity and an empty input buffer.
    /// - Call `decode_string` and assert `NeedMore::UnexpectedEndOfStream`.
    #[test]
    fn test_decode_string_empty() {
        let mut de = Decoder::new(0);
        let buf = Bytes::new();
        let err = de.decode_string(&mut Input::new(&buf)).unwrap_err();
        assert_eq!(err, DecoderError::NeedMore(NeedMore::UnexpectedEndOfStream));
    }

    /// STATEMENT: Decoding an empty HPACK header block succeeds.
    ///
    /// HOW:
    /// - Create a `Decoder` with zero table capacity and an empty input buffer.
    /// - Call `decode` and assert that it returns `Ok(())`.
    #[test]
    #[allow(clippy::unit_cmp, clippy::let_unit_value)]
    fn test_decode_empty() {
        let mut de = Decoder::new(0);
        let mut buf = Bytes::new();
        let empty = de.decode(&mut buf, |_| {}).unwrap();
        assert_eq!(empty, ());
    }

    /// STATEMENT: Decoded dynamic-table values do not retain the source allocation.
    ///
    /// HOW:
    /// - Place indexed literals for `foo` and `:path` after a 16 KiB allocation prefix.
    /// - Decode the sliced header block and check that two headers and table entries result.
    /// - Assert each stored value pointer lies outside the original allocation range.
    #[test]
    fn test_table_entries_do_not_reference_source() {
        let mut de = Decoder::default();

        // the header block is at the end of a larger allocation
        let mut data = vec![0; 16 * 1024];
        // literal with indexing, new name, not huffman encoded
        data.extend(&[0b0100_0000, 3]);
        data.extend(b"foo");
        data.extend(&[40]);
        data.extend(&[b'b'; 40]);
        // `:path` literal with indexing, indexed name
        data.extend(&[0b0100_0000 | 4, 40]);
        data.extend(&[b'/'; 40]);
        let src = Bytes::from(data);
        let range = src.as_ref().as_ptr_range();
        let mut buf = src.slice(16 * 1024..);
        drop(src);

        let mut res = vec![];
        de.decode(&mut buf, |h| res.push(h)).unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(de.table.entries.len(), 2);

        for entry in &de.table.entries {
            let ptr = match entry {
                Header::Field { value, .. } => value.as_bytes().as_ptr(),
                Header::Path(v) => v.as_bytes().as_ptr(),
                h => panic!("unexpected entry {h:?}"),
            };
            assert!(!range.contains(&ptr), "{entry:?}");
        }
    }

    /// STATEMENT: An incrementally indexed literal decodes even when the dynamic table has no
    /// capacity.
    ///
    /// HOW:
    /// - Decode a literal with Huffman-encoded name `foo` and value `bar` using
    ///   `Decoder::new(0)`.
    /// - Assert one matching `Header::Field` is emitted and the dynamic table size remains
    ///   zero.
    #[test]
    fn test_decode_indexed_larger_than_table() {
        let mut de = Decoder::new(0);

        let mut buf = BytesMut::new();
        buf.extend(&[0b0100_0000, 0x80 | 2]);
        buf.extend(huff_encode(b"foo"));
        buf.extend(&[0x80 | 3]);
        buf.extend(huff_encode(b"bar"));
        let mut buf = buf.freeze();

        let mut res = vec![];
        de.decode(&mut buf, |h| {
            res.push(h);
        })
        .unwrap();

        assert_eq!(res.len(), 1);
        assert_eq!(de.table.size(), 0);

        match res[0] {
            Header::Field { ref name, ref value } => {
                assert_eq!(name, "foo");
                assert_eq!(value, "bar");
            }
            _ => panic!(),
        }
    }

    /// STATEMENT: A literal with an indexed dynamic-table name takes the name from the table
    /// and the value from the block.
    ///
    /// HOW:
    /// - Insert `foo: bar` with an incremental-indexing literal.
    /// - Decode a non-indexed literal that names dynamic index 62 with value `baz`.
    /// - Check both decoded fields and that the table entry still holds `bar`.
    #[test]
    fn test_decode_literal_with_dynamic_name() {
        let mut de = Decoder::default();
        let mut buf = Bytes::from_static(b"\x40\x03foo\x03bar\x0f\x2f\x03baz");

        let mut res = vec![];
        de.decode(&mut buf, |h| res.push(h)).unwrap();

        let foo = Bytes::from_static(b"foo");
        assert_eq!(
            res,
            vec![
                Header::new(&foo, Bytes::from_static(b"bar")).unwrap(),
                Header::new(&foo, Bytes::from_static(b"baz")).unwrap(),
            ]
        );
        assert_eq!(
            de.table.get(62).unwrap(),
            Header::new(&foo, Bytes::from_static(b"bar")).unwrap()
        );
    }

    /// STATEMENT: A literal naming a missing table entry fails before its value is read.
    ///
    /// HOW:
    /// - Decode a non-indexed literal naming index 62 against an empty dynamic table, with no
    ///   value bytes following.
    /// - Check the result is `InvalidTableIndex`, not `NeedMore`.
    /// - Check that `Table::get_with_value` also rejects index 0.
    #[test]
    fn test_decode_literal_with_invalid_name_index() {
        let mut de = Decoder::default();
        let mut buf = Bytes::from_static(b"\x0f\x2f");

        let res = de.decode(&mut buf, |_| {});
        assert!(matches!(res, Err(DecoderError::InvalidTableIndex)), "{res:?}");
        assert!(matches!(
            de.table.get_with_value(0, Bytes::new()),
            Err(DecoderError::InvalidTableIndex)
        ));
    }

    fn huff_encode(src: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        huffman::encode(src, &mut buf);
        buf
    }

    /// STATEMENT: HPACK decoding resumes correctly at every byte boundary of a mixed header
    /// block.
    ///
    /// HOW:
    /// - Combine static indexes, a new literal, and a dynamic index with every name/value
    ///   Huffman choice.
    /// - Decode each possible prefix, accepting success or `DecoderError::NeedMore`, then
    ///   append the suffix.
    /// - Check the four expected headers in order and an empty, inline source buffer.
    #[test]
    fn test_decode_at_every_split() {
        for huff_name in [false, true] {
            for huff_value in [false, true] {
                let mut data = BytesMut::new();
                data.extend_from_slice(&[0x82, 0x40]);
                for (text, huff) in [(b"foo".as_slice(), huff_name), (b"bar", huff_value)] {
                    if huff {
                        let encoded = huff_encode(text);
                        data.extend_from_slice(&[0x80 | encoded.len() as u8]);
                        data.extend_from_slice(&encoded);
                    } else {
                        data.extend_from_slice(&[text.len() as u8]);
                        data.extend_from_slice(text);
                    }
                }
                data.extend_from_slice(&[0xbe, 0x84]);

                let field = Header::new(&Bytes::from_static(b"foo"), Bytes::from_static(b"bar")).unwrap();
                let expected = vec![
                    Header::Method(Method::GET),
                    field.clone(),
                    field,
                    Header::Path(ByteString::from_static("/")),
                ];

                for split in 0..=data.len() {
                    let mut de = Decoder::default();
                    let mut src = Bytes::copy_from_slice(&data[..split]);
                    let mut headers = Vec::new();
                    let result = de.decode(&mut src, |h| headers.push(h));
                    assert!(
                        matches!(result, Ok(()) | Err(DecoderError::NeedMore(_))),
                        "split={split}, huff_name={huff_name}, huff_value={huff_value}: {result:?}"
                    );

                    let mut src = BytesMut::from(src);
                    src.extend_from_slice(&data[split..]);
                    let mut src = src.freeze();
                    de.decode(&mut src, |h| headers.push(h)).unwrap();
                    assert_eq!(headers, expected, "split={split}");
                    assert!(src.is_empty());
                    assert!(src.is_inline());
                }
            }
        }
    }

    /// STATEMENT: A literal with a plain name resumes decoding after an incomplete Huffman
    /// value.
    ///
    /// HOW:
    /// - Decode `foo` with only the first byte of the Huffman-encoded `bar` value.
    /// - Assert `NeedMore::StringUnderflow`, append the remaining value bytes, and decode
    ///   again.
    /// - Check one `foo: bar` field is emitted and the zero-capacity table stays empty.
    #[test]
    fn test_decode_continuation_header_with_non_huff_encoded_name() {
        let mut de = Decoder::new(0);
        let value = huff_encode(b"bar");
        let mut buf = BytesMut::new();
        // header name is non_huff encoded
        buf.extend(&[0b0100_0000, 3]);
        buf.extend(b"foo");
        // header value is partial
        buf.extend(&[0x80 | 3]);
        buf.extend(&value[0..1]);
        let mut buf = buf.freeze();

        let mut res = vec![];
        let e = de
            .decode(&mut buf, |h| {
                res.push(h);
            })
            .unwrap_err();
        // decode error because the header value is partial
        assert_eq!(e, DecoderError::NeedMore(NeedMore::StringUnderflow));

        // extend buf with the remaining header value
        let mut buf = BytesMut::from(buf);
        buf.extend(&value[1..]);
        let mut buf = buf.freeze();
        de.decode(&mut buf, |h| {
            res.push(h);
        })
        .unwrap();

        assert_eq!(res.len(), 1);
        assert_eq!(de.table.size(), 0);

        match res[0] {
            Header::Field { ref name, ref value } => {
                assert_eq!(name, "foo");
                assert_eq!(value, "bar");
            }
            _ => panic!(),
        }
    }
}
