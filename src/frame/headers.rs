use std::{cell::RefCell, cmp, fmt, io::Cursor};

use ntex_bytes::{BytePages, ByteString, Bytes, BytesMut};
use ntex_http::{HeaderMap, HeaderName, Method, StatusCode, header};

use crate::hpack;

use super::priority::StreamDependency;
use super::{Frame, FrameError, Head, Kind, Protocol, StreamId, util};

/// HTTP/2 HEADERS frame.
///
/// This could be either a request or a response.
#[derive(Clone, PartialEq, Eq)]
pub struct Headers {
    /// The ID of the stream with which this frame is associated.
    stream_id: StreamId,

    /// The header block fragment
    header_block: HeaderBlock,

    /// The associated flags
    flags: HeadersFlag,
}

/// Flags carried by a HEADERS frame.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct HeadersFlag(u8);

/// Decoded HTTP/2 pseudo-header fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PseudoHeaders {
    /// Request `:method`.
    pub method: Option<Method>,
    /// Request `:scheme`.
    pub scheme: Option<ByteString>,
    /// Request `:authority`.
    pub authority: Option<ByteString>,
    /// Request `:path`.
    pub path: Option<ByteString>,
    /// Extended CONNECT `:protocol` (RFC 8441).
    pub protocol: Option<Protocol>,
    /// Response `:status`.
    pub status: Option<StatusCode>,
}

pub(super) struct Iter<'a> {
    /// Pseudo headers
    pseudo: Option<PseudoHeaders>,

    /// Header fields
    fields: header::Iter<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderBlock {
    /// The decoded header fields
    fields: HeaderMap,

    /// Pseudo headers, these are broken out as they must be sent as part of the
    /// headers frame.
    pseudo: PseudoHeaders,
}

const END_STREAM: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PADDED: u8 = 0x8;
const PRIORITY: u8 = 0x20;
const ALL: u8 = END_STREAM | END_HEADERS | PADDED | PRIORITY;

/// Max number of spare header maps kept per thread.
const HDRS_MAP_POOL_SIZE: usize = 8;

/// A header map with a larger capacity is not reused.
const HDRS_MAP_MAX_CAPACITY: usize = 64;

thread_local! {
    static HDRS_MAP_POOL: RefCell<Vec<HeaderMap>> = const { RefCell::new(Vec::new()) };
}

fn take_header_map() -> HeaderMap {
    HDRS_MAP_POOL
        .try_with(|pool| pool.borrow_mut().pop())
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Returns an unused header map, decoding of the next header block reuses its allocation.
///
/// The map is cleared. Maps without allocation or with a large capacity are dropped.
pub fn recycle_header_map(mut map: HeaderMap) {
    let cap = map.capacity();
    if cap == 0 || cap > HDRS_MAP_MAX_CAPACITY {
        return;
    }
    map.clear();
    let _ = HDRS_MAP_POOL.try_with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() < HDRS_MAP_POOL_SIZE {
            pool.push(map);
        }
    });
}

// ===== impl Headers =====

impl Headers {
    /// Creates a HEADERS frame.
    pub fn new(stream_id: StreamId, pseudo: PseudoHeaders, fields: HeaderMap, eof: bool) -> Self {
        let mut flags = HeadersFlag::default();
        if eof {
            flags.set_end_stream();
        }
        Headers {
            flags,
            stream_id,
            header_block: HeaderBlock { fields, pseudo },
        }
    }

    /// Creates an end-of-stream trailers frame.
    pub fn trailers(stream_id: StreamId, fields: HeaderMap) -> Self {
        let mut flags = HeadersFlag::default();
        flags.set_end_stream();

        Headers {
            stream_id,
            flags,
            header_block: HeaderBlock {
                fields,
                pseudo: PseudoHeaders::default(),
            },
        }
    }

    /// Loads the header frame but doesn't actually do HPACK decoding.
    ///
    /// HPACK decoding is done in the `load_hpack` step.
    pub fn load(head: Head, src: &mut Bytes) -> Result<Self, FrameError> {
        match Self::load_head(head, src)? {
            (_, true) => Err(FrameError::InvalidDependencyId),
            (frame, false) => Ok(frame),
        }
    }

    /// Loads the header frame, also returns whether the stream depends on itself.
    ///
    /// A self dependency is a stream error, the header block still must be
    /// decoded to keep the HPACK state in sync.
    pub(crate) fn load_head(head: Head, src: &mut Bytes) -> Result<(Self, bool), FrameError> {
        let flags = HeadersFlag::load(head.flag());
        let mut self_dependency = false;

        if head.stream_id().is_zero() {
            return Err(FrameError::InvalidStreamId);
        }

        // Read the padding length
        let pad = if flags.is_padded() {
            if src.is_empty() {
                return Err(FrameError::MalformedMessage);
            }
            let pad = src[0] as usize;

            // Drop the padding
            src.advance_to(1);
            pad
        } else {
            0
        };

        // Read the stream dependency
        if flags.is_priority() {
            if src.len() < 5 {
                return Err(FrameError::MalformedMessage);
            }
            let stream_dep = StreamDependency::load(&src[..5])?;

            self_dependency = stream_dep.dependency_id() == head.stream_id();

            // Drop the next 5 bytes
            src.advance_to(5);
        }

        if pad > 0 {
            if pad > src.len() {
                return Err(FrameError::TooMuchPadding);
            }
            src.truncate(src.len() - pad);
        }

        let frame = Headers {
            flags,
            stream_id: head.stream_id(),
            header_block: HeaderBlock {
                fields: take_header_map(),
                pseudo: PseudoHeaders::default(),
            },
        };
        Ok((frame, self_dependency))
    }

    /// Returns the stream id and flags of a frame whose header block continues
    /// in CONTINUATION frames.
    pub(crate) fn into_head(self) -> (StreamId, HeadersFlag) {
        recycle_header_map(self.header_block.fields);
        (self.stream_id, self.flags)
    }

    /// Creates a frame for a header block received in CONTINUATION frames.
    pub(crate) fn from_head(stream_id: StreamId, mut flags: HeadersFlag) -> Self {
        flags.set_end_headers();
        Headers {
            flags,
            stream_id,
            header_block: HeaderBlock {
                fields: take_header_map(),
                pseudo: PseudoHeaders::default(),
            },
        }
    }

    /// Decodes the HPACK header block.
    ///
    /// Fails with `FrameError::TooManyHeaders` if the block contains more than
    /// `max_headers` regular fields, or if the decoded header list size
    /// (name + value + 32 per field, RFC 9113 §6.5.2) exceeds `max_list_size`.
    pub fn load_hpack(
        &mut self,
        src: &mut Bytes,
        decoder: &mut hpack::Decoder,
        max_headers: usize,
        max_list_size: usize,
    ) -> Result<(), FrameError> {
        self.header_block
            .load(self.stream_id, src, decoder, max_headers, max_list_size)
    }

    /// Returns the associated stream identifier.
    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    /// Returns whether the complete header block ends in this frame sequence.
    pub fn is_end_headers(&self) -> bool {
        self.flags.is_end_headers()
    }

    /// Marks the header block as complete.
    pub fn set_end_headers(&mut self) {
        self.flags.set_end_headers();
    }

    /// Returns whether these headers close the sending side of the stream.
    pub fn is_end_stream(&self) -> bool {
        self.flags.is_end_stream()
    }

    /// Marks these headers as closing the sending side of the stream.
    pub fn set_end_stream(&mut self) {
        self.flags.set_end_stream();
    }

    /// Splits pseudo-headers and regular header fields.
    pub fn into_parts(self) -> (PseudoHeaders, HeaderMap) {
        (self.header_block.pseudo, self.header_block.fields)
    }

    /// Returns the regular header fields.
    pub fn fields(&self) -> &HeaderMap {
        &self.header_block.fields
    }

    /// Returns the pseudo-header fields.
    pub fn pseudo(&self) -> &PseudoHeaders {
        &self.header_block.pseudo
    }

    /// Returns the regular header fields.
    pub fn into_fields(self) -> HeaderMap {
        self.header_block.fields
    }

    /// Encodes the header block, split into `CONTINUATION` frames if it
    /// exceeds `max_size`.
    pub fn encode(self, encoder: &mut hpack::Encoder, dst: &mut BytePages, max_size: usize) {
        // At this point, the `is_end_headers` flag should always be set
        debug_assert!(self.flags.is_end_headers());

        // Get the HEADERS frame head
        let head = self.head();

        self.header_block.encode(encoder, head, dst, max_size);
    }

    fn head(&self) -> Head {
        Head::new(Kind::Headers, self.flags.into(), self.stream_id)
    }
}

impl From<Headers> for Frame {
    fn from(src: Headers) -> Self {
        Frame::Headers(src)
    }
}

impl fmt::Debug for Headers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Headers")
            .field("stream_id", &self.stream_id)
            .field("flags", &self.flags)
            .field("pseudo", &self.header_block.pseudo)
            // header `fields` are purposefully not included
            .finish()
    }
}

// ===== impl Pseudo =====

impl PseudoHeaders {
    /// Returns the header list size of the pseudo-headers and `fields`
    /// (name + value + 32 per field, RFC 9113 §6.5.2).
    pub(crate) fn header_list_size(&self, fields: &HeaderMap) -> usize {
        let pseudo = self.method.as_ref().map_or(0, |v| 32 + 7 + v.as_str().len())
            + self.scheme.as_ref().map_or(0, |v| 32 + 7 + v.len())
            + self.authority.as_ref().map_or(0, |v| 32 + 10 + v.len())
            + self.path.as_ref().map_or(0, |v| 32 + 5 + v.len())
            + self.protocol.as_ref().map_or(0, |v| 32 + 9 + v.as_str().len())
            + self.status.map_or(0, |_| 32 + 7 + 3);
        fields.iter().fold(pseudo, |acc, (name, value)| {
            acc + 32 + name.as_str().len() + value.len()
        })
    }

    /// Creates response pseudo headers.
    pub fn response(status: StatusCode) -> Self {
        PseudoHeaders {
            method: None,
            scheme: None,
            authority: None,
            path: None,
            protocol: None,
            status: Some(status),
        }
    }
}

// ===== impl Iter =====

impl Iterator for Iter<'_> {
    type Item = hpack::Header<Option<HeaderName>>;

    fn next(&mut self) -> Option<Self::Item> {
        use crate::hpack::Header;

        if let Some(ref mut pseudo) = self.pseudo {
            if let Some(method) = pseudo.method.take() {
                return Some(Header::Method(method));
            }

            if let Some(scheme) = pseudo.scheme.take() {
                return Some(Header::Scheme(scheme));
            }

            if let Some(authority) = pseudo.authority.take() {
                return Some(Header::Authority(authority));
            }

            if let Some(path) = pseudo.path.take() {
                return Some(Header::Path(path));
            }

            if let Some(protocol) = pseudo.protocol.take() {
                return Some(Header::Protocol(protocol.into()));
            }

            if let Some(status) = pseudo.status.take() {
                return Some(Header::Status(status));
            }
        }

        self.pseudo = None;

        self.fields.next().map(|(name, value)| Header::Field {
            name: Some(name.clone()),
            value: value.clone(),
        })
    }
}

// ===== impl HeadersFlag =====

impl HeadersFlag {
    pub fn empty() -> HeadersFlag {
        HeadersFlag(0)
    }

    pub fn load(bits: u8) -> HeadersFlag {
        HeadersFlag(bits & ALL)
    }

    pub fn is_end_stream(self) -> bool {
        self.0 & END_STREAM == END_STREAM
    }

    pub fn set_end_stream(&mut self) {
        self.0 |= END_STREAM;
    }

    pub fn is_end_headers(self) -> bool {
        self.0 & END_HEADERS == END_HEADERS
    }

    pub fn set_end_headers(&mut self) {
        self.0 |= END_HEADERS;
    }

    pub fn is_padded(self) -> bool {
        self.0 & PADDED == PADDED
    }

    pub fn is_priority(self) -> bool {
        self.0 & PRIORITY == PRIORITY
    }
}

impl Default for HeadersFlag {
    /// Returns a `HeadersFlag` value with `END_HEADERS` set.
    fn default() -> Self {
        HeadersFlag(END_HEADERS)
    }
}

impl From<HeadersFlag> for u8 {
    fn from(src: HeadersFlag) -> u8 {
        src.0
    }
}

impl fmt::Debug for HeadersFlag {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        util::debug_flags(fmt, self.0)
            .flag_if(self.is_end_headers(), "END_HEADERS")
            .flag_if(self.is_end_stream(), "END_STREAM")
            .flag_if(self.is_padded(), "PADDED")
            .flag_if(self.is_priority(), "PRIORITY")
            .finish()
    }
}

// ===== HeaderBlock =====

/// Initial capacity of the header encoding buffer.
const HDRS_BUF_SIZE: usize = 1024;

/// A larger header encoding buffer is released after use.
const HDRS_BUF_MAX_RETAINED: usize = 64 * 1024;

thread_local! {
    static HDRS_BUF: RefCell<BytesMut> = RefCell::new(BytesMut::with_capacity(HDRS_BUF_SIZE));
}

impl HeaderBlock {
    fn load(
        &mut self,
        id: StreamId,
        src: &mut Bytes,
        decoder: &mut hpack::Decoder,
        max_headers: usize,
        max_list_size: usize,
    ) -> Result<(), FrameError> {
        let mut reg = !self.fields.is_empty();
        let mut malformed = false;
        let mut too_many_headers = false;
        let mut num_fields = 0;
        let mut list_size = 0usize;

        macro_rules! set_pseudo {
            ($field:ident, $val:expr) => {{
                if reg {
                    log::trace!("load_hpack; header malformed -- pseudo not at head of block");
                    malformed = true;
                } else if self.pseudo.$field.is_some() {
                    log::trace!("load_hpack; header malformed -- repeated pseudo");
                    malformed = true;
                } else {
                    self.pseudo.$field = Some($val.into());
                }
            }};
        }

        let mut cursor = Cursor::new(src);

        // If the header frame is malformed, we still have to continue decoding
        // the headers. A malformed header frame is a stream level error, but
        // the hpack state is connection level. In order to maintain correct
        // state for other streams, the hpack decoding process must complete.
        let res = decoder.decode(&mut cursor, |header| {
            use crate::hpack::Header;

            // Once the block is rejected, remaining fields are decoded only
            // to keep the hpack state in sync, they are not stored
            list_size = list_size.saturating_add(header.len());
            if list_size > max_list_size {
                too_many_headers = true;
            }
            if too_many_headers {
                return;
            }

            match header {
                Header::Field { name, value } => {
                    // Connection level header fields are not supported and must
                    // result in a protocol error.

                    if name == header::CONNECTION
                        || name == header::TRANSFER_ENCODING
                        || name == header::UPGRADE
                        || name == "keep-alive"
                        || name == "proxy-connection"
                    {
                        log::trace!("load_hpack; connection level header");
                        malformed = true;
                    } else if name == header::TE && !value.as_bytes().eq_ignore_ascii_case(b"trailers") {
                        log::trace!("load_hpack; TE header not set to trailers; val={value:?}");
                        malformed = true;
                    } else {
                        reg = true;
                        num_fields += 1;
                        if num_fields > max_headers {
                            too_many_headers = true;
                        } else {
                            self.fields.append(name, value);
                        }
                    }
                }
                Header::Authority(v) => {
                    set_pseudo!(authority, v);
                }
                Header::Method(v) => {
                    set_pseudo!(method, v);
                }
                Header::Scheme(v) => {
                    set_pseudo!(scheme, v);
                }
                Header::Path(v) => {
                    set_pseudo!(path, v);
                }
                Header::Protocol(v) => {
                    set_pseudo!(protocol, v);
                }
                Header::Status(v) => {
                    set_pseudo!(status, v);
                }
            }
        });

        if let Err(e) = res {
            log::trace!("hpack decoding error; err={e:?}");
            Err(e.into())
        } else if malformed {
            log::trace!("malformed message");
            Err(FrameError::MalformedMessage)
        } else if too_many_headers {
            log::trace!("too many headers");
            Err(FrameError::TooManyHeaders(id))
        } else {
            Ok(())
        }
    }

    fn encode(self, encoder: &mut hpack::Encoder, head: Head, dst: &mut BytePages, max_size: usize) {
        encode_block(self.pseudo, &self.fields, encoder, head, dst, max_size);
    }
}

/// Encodes a HEADERS frame with `END_HEADERS` set from borrowed header fields.
pub(crate) fn encode_headers_ref(
    stream_id: StreamId,
    pseudo: PseudoHeaders,
    fields: &HeaderMap,
    eof: bool,
    encoder: &mut hpack::Encoder,
    dst: &mut BytePages,
    max_size: usize,
) {
    let mut flags = HeadersFlag::default();
    if eof {
        flags.set_end_stream();
    }
    let head = Head::new(Kind::Headers, flags.into(), stream_id);
    encode_block(pseudo, fields, encoder, head, dst, max_size);
}

/// Encodes the header block, split into `CONTINUATION` frames if it
/// exceeds `max_size`.
fn encode_block(
    pseudo: PseudoHeaders,
    fields: &HeaderMap,
    encoder: &mut hpack::Encoder,
    head: Head,
    dst: &mut BytePages,
    max_size: usize,
) {
    HDRS_BUF.with(|buf| {
        let mut b = buf.borrow_mut();
        let hpack = &mut b;
        hpack.clear();

        // encode hpack
        let headers = Iter {
            pseudo: Some(pseudo),
            fields: fields.into_iter(),
        };
        encoder.encode(headers, hpack);

        let mut head = head;
        let mut start = 0;
        loop {
            let end = cmp::min(start + max_size, hpack.len());

            // encode the header payload
            if hpack.len() > end {
                Head::new(head.kind(), head.flag() ^ END_HEADERS, head.stream_id()).encode(max_size, dst);
                dst.extend_from_slice(&hpack[start..end]);
                head = Head::new(Kind::Continuation, END_HEADERS, head.stream_id());
                start = end;
            } else {
                head.encode(end - start, dst);
                dst.extend_from_slice(&hpack[start..end]);
                break;
            }
        }

        // do not keep the buffer of a rare large header block
        if hpack.capacity() > HDRS_BUF_MAX_RETAINED {
            **hpack = BytesMut::with_capacity(HDRS_BUF_SIZE);
        }
    });
}

#[cfg(test)]
mod test {
    use ntex_http::HeaderValue;

    use super::*;
    use crate::hpack::{Encoder, huffman};

    #[test]
    fn test_nameless_header_at_resume() {
        let mut encoder = Encoder::default();
        let mut dst = BytePages::default();

        let mut hdrs = HeaderMap::default();
        hdrs.append(
            HeaderName::from_static("hello"),
            HeaderValue::from_static("world"),
        );
        hdrs.append(HeaderName::from_static("hello"), HeaderValue::from_static("zomg"));
        hdrs.append(HeaderName::from_static("hello"), HeaderValue::from_static("sup"));

        let mut headers = Headers::new(StreamId::CON, PseudoHeaders::default(), hdrs, false);
        headers.set_end_headers();
        headers.encode(&mut encoder, &mut dst, 8);

        let dst = dst.take().unwrap().freeze();
        assert_eq!(48, dst.len());
        assert_eq!([0, 0, 8, 1, 0, 0, 0, 0, 0], &dst[0..9]);
        assert_eq!(&[0x40, 0x80 | 4], &dst[9..11]);
        assert_eq!("hello", huff_decode(&dst[11..15]));
        assert_eq!(0x80 | 4, dst[15]);

        let mut world = BytesMut::from(&dst[16..17]);
        world.extend_from_slice(&dst[26..29]);
        // assert_eq!("world", huff_decode(&world));

        assert_eq!([0, 0, 8, 9, 0, 0, 0, 0, 0], &dst[17..26]);

        // // Next is not indexed
        //assert_eq!(&[15, 47, 0x80 | 3], &dst[12..15]);
        //assert_eq!("zomg", huff_decode(&dst[15..18]));
        //assert_eq!(&[15, 47, 0x80 | 3], &dst[18..21]);
        //assert_eq!("sup", huff_decode(&dst[21..]));
    }

    fn huff_decode(src: &[u8]) -> Bytes {
        let mut buf = BytesMut::new();
        huffman::decode(src, &mut buf).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use ntex_http::HeaderValue;

    use super::*;

    fn encode(value_len: usize) -> BytePages {
        let mut fields = HeaderMap::new();
        fields.insert(
            HeaderName::from_static("x-large"),
            HeaderValue::from_str(&"a".repeat(value_len)).unwrap(),
        );
        let hdrs = Headers::new(StreamId::from(1), PseudoHeaders::default(), fields, false);
        let mut dst = BytePages::default();
        hdrs.encode(&mut hpack::Encoder::default(), &mut dst, 16_384);
        dst
    }

    #[test]
    fn large_header_buffer_is_released() {
        encode(100);
        let cap = HDRS_BUF.with(|b| b.borrow().capacity());
        assert!(cap <= HDRS_BUF_MAX_RETAINED);

        let dst = encode(256 * 1024);
        assert!(dst.len() > HDRS_BUF_MAX_RETAINED);
        let cap = HDRS_BUF.with(|b| b.borrow().capacity());
        assert!(cap <= HDRS_BUF_MAX_RETAINED, "retained {cap} bytes");
    }

    #[test]
    fn encode_headers_ref_matches_owned() {
        for (value_len, eof) in [(5, false), (5, true), (40_000, false), (40_000, true)] {
            let mut fields = HeaderMap::new();
            fields.insert(
                HeaderName::from_static("x-large"),
                HeaderValue::from_str(&"a".repeat(value_len)).unwrap(),
            );
            fields.append(header::DATE, HeaderValue::from_static("now"));
            let pseudo = PseudoHeaders::response(StatusCode::OK);

            // hpack encoders keep state, encode twice to check dynamic table use
            let mut enc1 = hpack::Encoder::default();
            let mut owned = BytePages::default();
            for _ in 0..2 {
                Headers::new(StreamId::from(3), pseudo.clone(), fields.clone(), eof)
                    .encode(&mut enc1, &mut owned, 16_384);
            }

            let mut enc2 = hpack::Encoder::default();
            let mut borrowed = BytePages::default();
            for _ in 0..2 {
                encode_headers_ref(
                    StreamId::from(3),
                    pseudo.clone(),
                    &fields,
                    eof,
                    &mut enc2,
                    &mut borrowed,
                    16_384,
                );
            }

            let collect = |mut pages: BytePages| {
                let mut v = Vec::new();
                while let Some(page) = pages.take() {
                    v.extend_from_slice(&page.freeze());
                }
                v
            };
            let owned = collect(owned);
            let borrowed = collect(borrowed);
            assert_eq!(owned, borrowed, "value_len={value_len} eof={eof}");
            assert!(
                value_len < 16_384
                    || borrowed[3] == Kind::Headers as u8
                        && borrowed[16_384 + 9 + 3] == Kind::Continuation as u8
            );
            // HEADERS frame flags
            assert_eq!(borrowed[4] & END_STREAM == END_STREAM, eof);
            assert_eq!(fields.len(), 2);
        }
    }

    fn pool_len() -> usize {
        HDRS_MAP_POOL.with(|pool| pool.borrow().len())
    }

    fn decode(fields: HeaderMap) -> Headers {
        let pseudo = PseudoHeaders {
            method: Some(Method::GET),
            scheme: Some("https".into()),
            path: Some("/".into()),
            ..Default::default()
        };
        let mut dst = BytePages::default();
        Headers::new(StreamId::from(1), pseudo, fields, true).encode(
            &mut hpack::Encoder::default(),
            &mut dst,
            16_384,
        );
        let mut buf = BytesMut::new();
        while let Some(page) = dst.take() {
            buf.extend_from_slice(&page.freeze());
        }
        let mut src = buf.freeze();
        let head = Head::parse(&src[..9]);
        src.advance_to(9);
        let mut hdrs = Headers::load(head, &mut src).unwrap();
        hdrs.load_hpack(&mut src, &mut hpack::Decoder::new(4096), 100, 16_384)
            .unwrap();
        hdrs
    }

    #[test]
    fn recycled_header_map_is_reused() {
        let mut stale = HeaderMap::with_capacity(32);
        stale.insert(HeaderName::from_static("x-stale"), HeaderValue::from_static("1"));
        let cap = stale.capacity();
        recycle_header_map(stale);
        assert_eq!(pool_len(), 1);

        let mut fields = HeaderMap::new();
        fields.insert(HeaderName::from_static("x-new"), HeaderValue::from_static("2"));
        let hdrs = decode(fields);
        assert_eq!(pool_len(), 0);
        let fields = hdrs.into_fields();
        assert_eq!(fields.capacity(), cap);
        // the recycled map is cleared
        assert_eq!(fields.len(), 1);
        assert!(fields.get("x-stale").is_none());
        assert_eq!(fields.get("x-new").unwrap(), "2");
    }

    #[test]
    fn recycle_header_map_limits() {
        // maps without allocation or with a large capacity are dropped
        recycle_header_map(HeaderMap::new());
        recycle_header_map(HeaderMap::with_capacity(HDRS_MAP_MAX_CAPACITY * 2));
        assert_eq!(pool_len(), 0);

        for _ in 0..HDRS_MAP_POOL_SIZE * 2 {
            recycle_header_map(HeaderMap::with_capacity(4));
        }
        assert_eq!(pool_len(), HDRS_MAP_POOL_SIZE);
    }

    #[test]
    fn headers_flag_debug() {
        assert_eq!(format!("{:?}", HeadersFlag::empty()), "(0x0)");
        assert_eq!(format!("{:?}", HeadersFlag::default()), "(0x4: END_HEADERS)");
        assert_eq!(
            format!("{:?}", HeadersFlag::load(ALL)),
            "(0x2d: END_HEADERS | END_STREAM | PADDED | PRIORITY)"
        );

        let mut flags = HeadersFlag::empty();
        flags.set_end_stream();
        assert_eq!(format!("{flags:?}"), "(0x1: END_STREAM)");
        assert_eq!(
            format!("{:?}", HeadersFlag::load(0xff)),
            format!("{:?}", HeadersFlag::load(ALL))
        );
    }

    #[test]
    fn headers_debug() {
        let mut fields = HeaderMap::new();
        fields.insert(
            HeaderName::from_static("x-secret"),
            HeaderValue::from_static("value"),
        );
        let hdrs = Headers::new(
            StreamId::from(1),
            PseudoHeaders::response(StatusCode::OK),
            fields,
            true,
        );
        assert_eq!(
            format!("{hdrs:?}"),
            "Headers { stream_id: StreamId(1), flags: (0x5: END_HEADERS | END_STREAM), \
             pseudo: PseudoHeaders { method: None, scheme: None, authority: None, \
             path: None, protocol: None, status: Some(200) } }"
        );

        let pseudo = PseudoHeaders {
            method: Some(Method::CONNECT),
            protocol: Some(Protocol::from("websocket")),
            ..Default::default()
        };
        let hdrs = Headers::new(StreamId::from(3), pseudo, HeaderMap::new(), false);
        assert_eq!(
            format!("{hdrs:?}"),
            "Headers { stream_id: StreamId(3), flags: (0x4: END_HEADERS), \
             pseudo: PseudoHeaders { method: Some(CONNECT), scheme: None, authority: None, \
             path: None, protocol: Some(\"websocket\"), status: None } }"
        );
    }
}
