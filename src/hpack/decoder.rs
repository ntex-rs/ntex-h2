use std::{cmp, collections::VecDeque, io::Cursor, str::Utf8Error};

use ntex_bytes::{Buf, ByteString, Bytes, BytesMut};
use ntex_http::{Method, StatusCode, compat, header};

use super::{Header, huffman};

/// Stateful HPACK header decoder.
#[derive(Debug)]
pub struct Decoder {
    // Protocol indicated that the max table size will update
    max_size_update: Option<usize>,
    last_max_update: usize,
    // For testing
    #[allow(private_interfaces)]
    pub(super) table: Table,
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
pub(super) struct Table {
    entries: VecDeque<Header>,
    size: usize,
    max_size: usize,
}

struct StringMarker {
    offset: usize,
    len: usize,
    string: Option<Bytes>,
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
    pub fn decode<F>(&mut self, src: &mut Cursor<&mut Bytes>, mut f: F) -> Result<(), DecoderError>
    where
        F: FnMut(Header),
    {
        let mut can_resize = true;

        if let Some(size) = self.max_size_update.take() {
            self.last_max_update = size;
        }

        while let Some(ty) = peek_u8(src) {
            // At this point we are always at the beginning of the next block
            // within the HPACK data. The type of the block can always be
            // determined from the first byte.
            match Representation::load(ty)? {
                Representation::Indexed => {
                    // tracing::trace!(rem = src.remaining(), kind = %"Indexed");
                    can_resize = false;
                    let entry = self.decode_indexed(src)?;
                    consume(src);
                    f(entry);
                }
                Representation::LiteralWithIndexing => {
                    // tracing::trace!(rem = src.remaining(), kind = %"LiteralWithIndexing");
                    can_resize = false;
                    let entry = self.decode_literal(src, true, false)?;

                    // Insert the header into the table
                    self.table.insert(entry.clone());
                    consume(src);

                    f(entry);
                }
                Representation::LiteralWithoutIndexing => {
                    // tracing::trace!(rem = src.remaining(), kind = %"LiteralWithoutIndexing");
                    can_resize = false;
                    let entry = self.decode_literal(src, false, false)?;
                    consume(src);
                    f(entry);
                }
                Representation::LiteralNeverIndexed => {
                    // tracing::trace!(rem = src.remaining(), kind = %"LiteralNeverIndexed");
                    can_resize = false;
                    let entry = self.decode_literal(src, false, true)?;
                    consume(src);

                    f(entry);
                }
                Representation::SizeUpdate => {
                    // tracing::trace!(rem = src.remaining(), kind = %"SizeUpdate");
                    if !can_resize {
                        return Err(DecoderError::InvalidMaxDynamicSize);
                    }

                    // Handle the dynamic table size update
                    self.process_size_update(src)?;
                    consume(src);
                }
            }
        }

        Ok(())
    }

    fn process_size_update(&mut self, buf: &mut Cursor<&mut Bytes>) -> Result<(), DecoderError> {
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

    fn decode_indexed(&self, buf: &mut Cursor<&mut Bytes>) -> Result<Header, DecoderError> {
        let index = decode_int(buf, 7)?;
        self.table.get(index)
    }

    fn decode_literal(
        &mut self,
        buf: &mut Cursor<&mut Bytes>,
        index: bool,
        sensitive: bool,
    ) -> Result<Header, DecoderError> {
        let prefix = if index { 6 } else { 4 };

        // Extract the table index for the name, or 0 if not indexed
        let table_idx = decode_int(buf, prefix)?;

        // First, read the header name
        if table_idx == 0 {
            let old_pos = buf.position();
            let name_marker = self.try_decode_string(buf)?;
            let value_marker = self.try_decode_string(buf)?;
            buf.set_position(old_pos);
            // Read the name as a literal
            let name = name_marker.consume(buf);
            let value = value_marker.consume(buf);
            Header::new(&name, value, sensitive)
        } else {
            // validate the index before reading the value
            self.table.check_index(table_idx)?;
            let value = self.decode_string(buf)?;

            self.table.get_with_value(table_idx, value)
        }
    }

    fn try_decode_string(&mut self, buf: &mut Cursor<&mut Bytes>) -> Result<StringMarker, DecoderError> {
        const HUFF_FLAG: u8 = 0b1000_0000;

        let old_pos = buf.position();

        // The first bit in the first byte contains the huffman encoded flag.
        let huff = match peek_u8(buf) {
            Some(hdr) => (hdr & HUFF_FLAG) == HUFF_FLAG,
            None => return Err(DecoderError::NeedMore(NeedMore::UnexpectedEndOfStream)),
        };

        // Decode the string length using 7 bit prefix
        let len = decode_int(buf, 7)?;

        if len > buf.remaining() {
            log::trace!("decode_string underflow {:?} {:?}", len, buf.remaining());
            return Err(DecoderError::NeedMore(NeedMore::StringUnderflow));
        }

        let offset = (buf.position() - old_pos) as usize;
        if huff {
            let ret = {
                let raw = &buf.chunk()[..len];
                huffman::decode(raw, &mut self.buffer).map(|buf| StringMarker {
                    offset,
                    len,
                    string: Some(buf),
                })
            };

            buf.advance(len);
            ret
        } else {
            buf.advance(len);
            Ok(StringMarker {
                offset,
                len,
                string: None,
            })
        }
    }

    fn decode_string(&mut self, buf: &mut Cursor<&mut Bytes>) -> Result<Bytes, DecoderError> {
        let old_pos = buf.position();
        let marker = self.try_decode_string(buf)?;
        buf.set_position(old_pos);
        Ok(marker.consume(buf))
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
fn decode_int<B: Buf>(buf: &mut B, prefix_size: u8) -> Result<usize, DecoderError> {
    if !(1..=8).contains(&prefix_size) {
        return Err(DecoderError::InvalidIntegerPrefix);
    }

    if !buf.has_remaining() {
        return Err(DecoderError::NeedMore(NeedMore::IntegerUnderflow));
    }

    let mask = if prefix_size == 8 {
        0xFF
    } else {
        (1u8 << prefix_size).wrapping_sub(1)
    };

    let ret = (buf.get_u8() & mask) as usize;

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
fn decode_int_multi<B: Buf>(buf: &mut B, mut ret: usize) -> Result<usize, DecoderError> {
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

    while buf.has_remaining() {
        let b = buf.get_u8();

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

fn peek_u8<B: Buf>(buf: &B) -> Option<u8> {
    if buf.has_remaining() { Some(buf.chunk()[0]) } else { None }
}

fn take(buf: &mut Cursor<&mut Bytes>, n: usize) -> Bytes {
    let pos = buf.position() as usize;
    let bytes = buf.get_mut();
    bytes.advance(pos);
    let head = bytes.split_to(n);
    buf.set_position(0);
    head
}

impl StringMarker {
    fn consume(self, buf: &mut Cursor<&mut Bytes>) -> Bytes {
        buf.advance(self.offset);
        match self.string {
            Some(string) => {
                buf.advance(self.len);
                string
            }
            None => take(buf, self.len),
        }
    }
}

fn consume(buf: &mut Cursor<&mut Bytes>) {
    // Commit decoded bytes so an incomplete representation can be retried.
    let pos = buf.position() as usize;
    let bytes = buf.get_mut();
    if pos == bytes.len() {
        // Advancing alone would keep the allocation alive.
        bytes.clear();
    } else {
        bytes.advance(pos);
    }
    buf.set_position(0);
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

    pub(super) fn size(&self) -> usize {
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
    use crate::hpack::header::HeaderRef;

    use super::*;

    /// STATEMENT: `peek_u8` observes the next byte without consuming it and returns `None` at
    /// EOF.
    ///
    /// HOW:
    /// - Peek at a one-byte `Cursor`, then read the same byte with `get_u8`.
    /// - Confirm that peeking after the read returns `None`.
    #[test]
    fn test_peek_u8() {
        let b = 0xff;
        let mut buf = Cursor::new(vec![b]);
        assert_eq!(peek_u8(&buf), Some(b));
        assert_eq!(buf.get_u8(), b);
        assert_eq!(peek_u8(&buf), None);
    }

    /// STATEMENT: `consume` discards the cursor prefix, resets its position, and releases empty
    /// storage.
    ///
    /// HOW:
    /// - Consume empty, inline, and larger buffers at the start, midpoint, and end.
    /// - Check the remaining bytes, zero cursor position, and unchanged original clone.
    /// - Confirm fully consumed buffers use inline storage.
    #[test]
    fn test_consume() {
        for data in [b"".as_slice(), b"inline", &[b'x'; 128]] {
            for pos in [0, data.len() / 2, data.len()] {
                let mut src = Bytes::copy_from_slice(data);
                let original = src.clone();
                let mut cursor = Cursor::new(&mut src);
                cursor.set_position(pos as u64);
                consume(&mut cursor);

                assert_eq!(cursor.position(), 0);
                assert_eq!(cursor.chunk(), &data[pos..]);
                assert_eq!(original, data);
                if pos == data.len() {
                    assert!(src.is_inline(), "empty buffer must not retain the allocation");
                }
            }
        }
    }

    /// STATEMENT: `take` extracts a value after the cursor prefix without retaining unnecessary
    /// storage.
    ///
    /// HOW:
    /// - Extract values of several lengths after a 32-byte prefix, with and without a tail.
    /// - Check extracted bytes, remaining tail, zero cursor position, and unchanged original
    ///   clone.
    /// - Confirm short values with a tail and fully consumed source buffers use inline storage.
    #[test]
    fn test_take() {
        for len in [0, 4, 24, 64] {
            for tail in [0, 8] {
                let mut data = vec![b'p'; 32];
                data.extend(vec![b'v'; len]);
                data.extend(vec![b't'; tail]);
                let mut src = Bytes::copy_from_slice(&data);
                let original = src.clone();
                let mut cursor = Cursor::new(&mut src);
                cursor.set_position(32);
                let value = take(&mut cursor, len);

                assert_eq!(value, &data[32..32 + len]);
                assert_eq!(cursor.position(), 0);
                assert_eq!(cursor.chunk(), &data[32 + len..]);
                assert_eq!(original, data);
                if len <= 4 && tail > 0 {
                    assert!(value.is_inline(), "short value must not retain the prefix");
                }
                if tail == 0 {
                    assert!(src.is_inline(), "empty buffer must not retain the allocation");
                }
            }
        }
    }

    /// STATEMENT: Decoding a string from empty input requests more bytes.
    ///
    /// HOW:
    /// - Create a `Decoder` with zero table capacity and an empty input buffer.
    /// - Call `decode_string` and assert `NeedMore::UnexpectedEndOfStream`.
    #[test]
    fn test_decode_string_empty() {
        let mut de = Decoder::new(0);
        let mut buf = Bytes::new();
        let err = de.decode_string(&mut Cursor::new(&mut buf)).unwrap_err();
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
        let empty = de.decode(&mut Cursor::new(&mut buf), |_| {}).unwrap();
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
        de.decode(&mut Cursor::new(&mut buf), |h| res.push(h)).unwrap();
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
        de.decode(&mut Cursor::new(&mut buf), |h| {
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
        de.decode(&mut Cursor::new(&mut buf), |h| res.push(h)).unwrap();

        let foo = Bytes::from_static(b"foo");
        assert_eq!(
            res,
            vec![
                Header::new(&foo, Bytes::from_static(b"bar"), false).unwrap(),
                Header::new(&foo, Bytes::from_static(b"baz"), false).unwrap(),
            ]
        );
        assert_eq!(
            de.table.get(62).unwrap(),
            Header::new(&foo, Bytes::from_static(b"bar"), false).unwrap()
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

        let res = de.decode(&mut Cursor::new(&mut buf), |_| {});
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

                let field =
                    Header::new(&Bytes::from_static(b"foo"), Bytes::from_static(b"bar"), false).unwrap();
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
                    let result = de.decode(&mut Cursor::new(&mut src), |h| headers.push(h));
                    assert!(
                        matches!(result, Ok(()) | Err(DecoderError::NeedMore(_))),
                        "split={split}, huff_name={huff_name}, huff_value={huff_value}: {result:?}"
                    );

                    let mut src = BytesMut::from(src);
                    src.extend_from_slice(&data[split..]);
                    let mut src = src.freeze();
                    de.decode(&mut Cursor::new(&mut src), |h| headers.push(h))
                        .unwrap();
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
            .decode(&mut Cursor::new(&mut buf), |h| {
                res.push(h);
            })
            .unwrap_err();
        // decode error because the header value is partial
        assert_eq!(e, DecoderError::NeedMore(NeedMore::StringUnderflow));

        // extend buf with the remaining header value
        let mut buf = BytesMut::from(buf);
        buf.extend(&value[1..]);
        let mut buf = buf.freeze();
        de.decode(&mut Cursor::new(&mut buf), |h| {
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

    /// STATEMENT: A pseudo-header `:authority` or `:path` created with sensitive=true
    /// properly decodes into its corresponding sensible variant.
    ///
    /// HOW:
    /// - Create an `:authority` header with sensitive flag set to true using `Header::new`.
    /// - Assert it resolves to `Header::AuthoritySensible` containing the correct value.
    /// - Repeat the check for `:path` with sensitive=true to ensure it resolves to `Header::PathSensible`.
    #[test]
    fn test_sensitive_pseudo_headers_creation() {
        let name_auth = Bytes::from_static(b":authority");
        let val_auth = Bytes::from("example.com");

        let header_auth = Header::new(&name_auth, val_auth.clone(), true).unwrap();
        match header_auth {
            Header::AuthoritySensitive(v) => assert_eq!(v.as_str(), "example.com"),
            _ => panic!("Expected Header::AuthoritySensible"),
        }

        let name_path = Bytes::from_static(b":path");
        let val_path = Bytes::from("/secret-path");

        let header_path = Header::new(&name_path, val_path.clone(), true).unwrap();
        match header_path {
            Header::PathSensitive(v) => assert_eq!(v.as_str(), "/secret-path"),
            _ => panic!("Expected Header::PathSensible"),
        }
    }

    /// STATEMENT: Header reference correctly identifies sensitive pseudo-headers
    /// and standard fields.
    ///
    /// HOW:
    /// - Construct both sensitive and non-sensitive variants of pseudo-headers.
    /// - Call `is_sensitive()` via `HeaderRef`.
    /// - Assert that sensitive pseudo-headers return true while normal ones return false.
    #[test]
    fn test_header_ref_sensitivity_check() {
        let auth_sensible = Header::new(
            &Bytes::from_static(b":authority"),
            Bytes::from("secure.local"),
            true,
        )
        .unwrap();

        assert!(auth_sensible.as_ref().is_sensitive());

        let auth_normal = Header::new(
            &Bytes::from_static(b":authority"),
            Bytes::from("secure.local"),
            false,
        )
        .unwrap();

        assert!(!auth_normal.as_ref().is_sensitive());
    }
    /// STATEMENT: A standard pseudo-header `:authority` or `:path` created with
    /// sensitive=false defaults to its non-sensitive variant.
    ///
    /// HOW:
    /// - Create an `:authority` header with `sensitive: false` using `Header::new`.
    /// - Assert it resolves to the standard `Header::Authority` variant.
    /// - Perform the same verification for `:path` with `sensitive: false`.
    #[test]
    fn test_non_sensitive_pseudo_headers_default() {
        let name_auth = Bytes::from_static(b":authority");
        let val_auth = Bytes::from("api.example.com");

        let header_auth = Header::new(&name_auth, val_auth, false).unwrap();
        match header_auth {
            Header::Authority(v) => assert_eq!(v.as_str(), "api.example.com"),
            _ => panic!("Expected Header::Authority"),
        }

        let name_path = Bytes::from_static(b":path");
        let val_path = Bytes::from("/healthz");

        let header_path = Header::new(&name_path, val_path, false).unwrap();
        match header_path {
            Header::Path(v) => assert_eq!(v.as_str(), "/healthz"),
            _ => panic!("Expected Header::Path"),
        }
    }

    /// STATEMENT: Regular field headers correctly propagate the sensitive flag
    /// when created via `Header::new`.
    ///
    /// HOW:
    /// - Create a custom field header (e.g., `authorization`) with `sensitive: true`.
    /// - Assert that the underlying `HeaderValue` has its sensitivity flag set to true.
    /// - Verify that with `sensitive: false`, the sensitivity flag remains false.
    #[test]
    fn test_field_header_sensitivity_flag() {
        let name_field = Bytes::from_static(b"authorization");
        let val_field = Bytes::from("Bearer token123");

        let header_sensitive = Header::new(&name_field, val_field.clone(), true).unwrap();
        if let Header::Field { value, .. } = header_sensitive {
            assert!(value.is_sensitive());
        } else {
            panic!("Expected Header::Field");
        }

        let header_normal = Header::new(&name_field, val_field, false).unwrap();
        if let Header::Field { value, .. } = header_normal {
            assert!(!value.is_sensitive());
        } else {
            panic!("Expected Header::Field");
        }
    }

    /// STATEMENT: Malformed or unexpected pseudo-headers return an `InvalidPseudoheader` error.
    ///
    /// HOW:
    /// - Pass an unknown pseudo-header name starting with `:` (e.g., `:unknown`) to `Header::new`.
    /// - Assert that the result evaluates to `Err(DecoderError::InvalidPseudoheader)`.
    #[test]
    fn test_invalid_pseudo_header_error() {
        let invalid_name = Bytes::from_static(b":unknown");
        let value = Bytes::from("value");

        let result = Header::new(&invalid_name, value, false);
        assert!(matches!(result, Err(DecoderError::InvalidPseudoheader)));
    }

    /// STATEMENT: Header reference correctly calculates the HPACK table entry size
    /// for both standard and sensitive pseudo-headers and fields.
    ///
    /// HOW:
    /// - Create an `AuthoritySensible` header reference and check its calculated `len()`.
    /// - Create a standard field header reference and verify its size calculation.
    /// - Assert that sizes account correctly for the 32-byte overhead plus name and value lengths.
    #[test]
    fn test_header_ref_length_calculation() {
        let auth_val = ByteString::from("example.com");
        let ref_sensible = HeaderRef::AuthoritySensitive(&auth_val);

        // 32 bytes overhead + 10 bytes name length (":authority") + value length (11)
        assert_eq!(ref_sensible.len(), 32 + 10 + 11);

        let path_val = ByteString::from("/");
        let ref_path_sensible = HeaderRef::PathSensitive(&path_val);

        // 32 bytes overhead + 5 bytes name length (":path") + value length (1)
        assert_eq!(ref_path_sensible.len(), 32 + 5 + 1);
    }

    /// STATEMENT: Header reference properly identifies the correct pseudo-header or field name type.
    ///
    /// HOW:
    /// - Construct references for `AuthoritySensible` and `PathSensible`.
    /// - Call `HeaderRef::name()` on them.
    /// - Assert they map to `Name::Authority` and `Name::Path` respectively.
    #[test]
    fn test_header_ref_name_mapping() {
        use crate::hpack::header::Name;

        let auth_val = ByteString::from("test.com");
        let ref_auth = HeaderRef::AuthoritySensitive(&auth_val);
        assert_eq!(ref_auth.name(), Name::Authority);

        let path_val = ByteString::from("/home");
        let ref_path = HeaderRef::PathSensitive(&path_val);
        assert_eq!(ref_path.name(), Name::Path);
    }

    /// STATEMENT: Cloning a header reference successfully produces an owned `Header` instance
    /// matching the original sensitive pseudo-header variant.
    ///
    /// HOW:
    /// - Create a `Header::AuthoritySensible` and get its `HeaderRef`.
    /// - Call `.to_owned()` (or the conversion implementation) on the reference.
    /// - Assert the resulting owned header is correctly preserved as `Header::AuthoritySensible`.
    #[test]
    fn test_header_ref_cloning_to_owned() {
        let original = Header::new(
            &Bytes::from_static(b":authority"),
            Bytes::from("auth.local"),
            true,
        )
        .unwrap();

        let reference = original.as_ref();
        let cloned_header = reference.to_owned();

        match cloned_header {
            Header::AuthoritySensitive(v) => assert_eq!(v.as_str(), "auth.local"),
            _ => panic!("Expected cloned header to remain AuthoritySensible"),
        }
    }

    /// STATEMENT: `Decoder::decode` properly handles literal representation flags
    /// (without indexing vs never-indexed) to correctly emit sensitive versus
    /// non-sensitive headers across authority, path, and regular fields.
    ///
    /// HOW:
    /// - Test literal header fields starting with `0x00` (Without Indexing / non-sensitive)
    ///   and `0x10` (Never Indexed / sensitive).
    /// - Pass byte sequences for `:authority`, `:path`, and standard custom fields
    ///   into `decoder.decode` using a `Cursor<&mut Bytes>`.
    /// - Verify that `0x00` produces standard variants (`Header::Authority`, `Header::Path`,
    ///   `Header::Field` with `is_sensitive() == false`) and `0x10` produces sensitive
    ///   variants (`Header::AuthoritySensible`, `Header::PathSensible`, `Header::Field` with `is_sensitive() == true`).
    #[test]
    fn test_decoder_decode_literal_with_sensitivity_all_cases() {
        use crate::hpack::{Decoder, header::Header};
        use ntex::util::Bytes;
        use std::io::Cursor;

        // Test cases: (byte prefix, header name, value, variant validator)
        let test_cases = [
            // 1. :authority without indexing (non-sensitive)
            (
                0x40u8,
                &b":authority"[..],
                &b"localhost"[..],
                Box::new(|h: Header| match h {
                    Header::Authority(v) => assert_eq!(v.as_str(), "localhost"),
                    other => panic!("Expected Header::Authority, got {:?}", other),
                }) as Box<dyn Fn(Header)>,
            ),
            // 2. :authority without indexing (non-sensitive)
            (
                0u8,
                &b":authority"[..],
                &b"localhost"[..],
                Box::new(|h: Header| match h {
                    Header::Authority(v) => assert_eq!(v.as_str(), "localhost"),
                    other => panic!("Expected Header::Authority, got {:?}", other),
                }) as Box<dyn Fn(Header)>,
            ),
            // 3. :authority never indexed (sensitive)
            (
                0x10u8,
                &b":authority"[..],
                &b"secure.local"[..],
                Box::new(|h: Header| match h {
                    Header::AuthoritySensitive(v) => assert_eq!(v.as_str(), "secure.local"),
                    other => panic!("Expected Header::AuthoritySensible, got {:?}", other),
                }),
            ),
            // 4. :path without indexing (non-sensitive)
            (
                0x40u8,
                &b":path"[..],
                &b"/index.html"[..],
                Box::new(|h: Header| match h {
                    Header::Path(v) => assert_eq!(v.as_str(), "/index.html"),
                    other => panic!("Expected Header::Path, got {:?}", other),
                }),
            ),
            // 5. :path without indexing (non-sensitive)
            (
                0u8,
                &b":path"[..],
                &b"/index.html"[..],
                Box::new(|h: Header| match h {
                    Header::Path(v) => assert_eq!(v.as_str(), "/index.html"),
                    other => panic!("Expected Header::Path, got {:?}", other),
                }),
            ),
            // 4. :path never indexed (sensitive)
            (
                0x10u8,
                &b":path"[..],
                &b"/secret-doc"[..],
                Box::new(|h: Header| match h {
                    Header::PathSensitive(v) => assert_eq!(v.as_str(), "/secret-doc"),
                    other => panic!("Expected Header::PathSensible, got {:?}", other),
                }),
            ),
            // 5. Regular field without indexing (non-sensitive)
            (
                0x40u8,
                &b"x-custom-header"[..],
                &b"standard-value"[..],
                Box::new(|h: Header| match h {
                    Header::Field { name, value } => {
                        assert_eq!(name.as_str(), "x-custom-header");
                        assert_eq!(value.to_str().unwrap(), "standard-value");
                        assert!(!value.is_sensitive());
                    }
                    other => panic!("Expected Header::Field, got {:?}", other),
                }),
            ),
            // 5. Regular field without indexing (non-sensitive)
            (
                0u8,
                &b"x-custom-header"[..],
                &b"standard-value"[..],
                Box::new(|h: Header| match h {
                    Header::Field { name, value } => {
                        assert_eq!(name.as_str(), "x-custom-header");
                        assert_eq!(value.to_str().unwrap(), "standard-value");
                        assert!(!value.is_sensitive());
                    }
                    other => panic!("Expected Header::Field, got {:?}", other),
                }),
            ),
            // 6. Regular field never indexed (sensitive)
            (
                0x10u8,
                &b"authorization"[..],
                &b"Bearer secret-token"[..],
                Box::new(|h: Header| match h {
                    Header::Field { name, value } => {
                        assert_eq!(name.as_str(), "authorization");
                        assert_eq!(value.to_str().unwrap(), "Bearer secret-token");
                        assert!(value.is_sensitive());
                    }
                    other => panic!("Expected sensitive Header::Field, got {:?}", other),
                }),
            ),
        ];

        for (byte, name_bytes, val_bytes, validator) in test_cases {
            let mut decoder = Decoder::new(4096);

            // Dynamically build the HPACK byte buffer for the literal field
            let mut data_vec = Vec::new();
            data_vec.push(byte);

            // Length and name
            data_vec.push(name_bytes.len() as u8);
            data_vec.extend_from_slice(name_bytes);

            // Length and value
            data_vec.push(val_bytes.len() as u8);
            data_vec.extend_from_slice(val_bytes);

            let mut data = Bytes::from(data_vec);
            let mut cursor = Cursor::new(&mut data);

            let mut decoded_header = None;
            let result = decoder.decode(&mut cursor, |h| {
                decoded_header = Some(h);
            });

            assert!(result.is_ok(), "Failed to decode stream for prefix {:#x}", byte);
            let header = decoded_header.expect("Expected header to be emitted by decoder");
            validator(header);
        }
    }

    /// STATEMENT: `Decoder::decode` correctly handles incomplete streams (underflow)
    /// returning a `NeedMore` decoder error when buffer data runs out.
    ///
    /// HOW:
    /// - Pass a truncated byte sequence to `decoder.decode` via a `Cursor`.
    /// - Assert that the result evaluates to an `Err` containing `DecoderError::NeedMore`.
    #[test]
    fn test_decoder_decode_underflow_error() {
        use crate::hpack::Decoder;
        use ntex::util::Bytes;
        use std::io::Cursor;

        let mut decoder = Decoder::new(4096);

        // Incomplete data (declared length greater than available bytes)
        let mut data = Bytes::from_static(&[
            0x40, // Literal header field with incremental indexing
            0xFF, // Exaggerated length without sufficient name/value bytes
        ]);
        let mut cursor = Cursor::new(&mut data);

        let result = decoder.decode(&mut cursor, |_| {});
        match result {
            Err(DecoderError::NeedMore(_)) => (),
            other => panic!("Expected NeedMore error, but got {:?}", other),
        }
    }
}
