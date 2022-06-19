use std::{fmt, io::Cursor};

use ntex_bytes::{ByteString, Bytes, BytesMut};
use ntex_http::{header, uri, HeaderMap, HeaderName, Method, StatusCode, Uri};

use super::{util, Frame, FrameError, Head, Kind, Protocol, StreamId};
use crate::hpack;

/// Header frame
///
/// This could be either a request or a response.
pub struct Headers {
    /// The ID of the stream with which this frame is associated.
    stream_id: StreamId,

    /// The header block fragment
    header_block: HeaderBlock,

    /// The associated flags
    flags: HeadersFlag,
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct HeadersFlag(u8);

#[derive(Debug, Default)]
pub struct PseudoHeaders {
    // Request
    pub method: Option<Method>,
    pub scheme: Option<ByteString>,
    pub authority: Option<ByteString>,
    pub path: Option<ByteString>,
    pub protocol: Option<Protocol>,

    // Response
    pub status: Option<StatusCode>,
}

pub struct Iter<'a> {
    /// Pseudo headers
    pseudo: Option<PseudoHeaders>,

    /// Header fields
    fields: header::Iter<'a>,
}

#[derive(Debug)]
struct HeaderBlock {
    /// The decoded header fields
    fields: HeaderMap,

    /// Set to true if decoding went over the max header list size.
    is_over_size: bool,

    /// Pseudo headers, these are broken out as they must be sent as part of the
    /// headers frame.
    pseudo: PseudoHeaders,
}

#[derive(Debug)]
struct EncodingHeaderBlock {
    hpack: Bytes,
}

const END_STREAM: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PADDED: u8 = 0x8;
const PRIORITY: u8 = 0x20;
const ALL: u8 = END_STREAM | END_HEADERS | PADDED | PRIORITY;

// ===== impl Headers =====

impl Headers {
    /// Create a new HEADERS frame
    pub fn new(stream_id: StreamId, pseudo: PseudoHeaders, fields: HeaderMap) -> Self {
        Headers {
            stream_id,
            header_block: HeaderBlock {
                fields,
                pseudo,
                is_over_size: false,
            },
            flags: HeadersFlag::default(),
        }
    }

    pub fn trailers(stream_id: StreamId, fields: HeaderMap) -> Self {
        let mut flags = HeadersFlag::default();
        flags.set_end_stream();

        Headers {
            stream_id,
            flags,
            header_block: HeaderBlock {
                fields,
                is_over_size: false,
                pseudo: PseudoHeaders::default(),
            },
        }
    }

    /// Loads the header frame but doesn't actually do HPACK decoding.
    ///
    /// HPACK decoding is done in the `load_hpack` step.
    pub fn load(head: Head, src: &mut BytesMut) -> Result<Self, FrameError> {
        let flags = HeadersFlag(head.flag());
        log::trace!("loading headers; flags={:?}", flags);

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
            let _ = src.split_to(1);
            pad
        } else {
            0
        };

        // Read the stream dependency
        if flags.is_priority() {
            if src.len() < 5 {
                return Err(FrameError::MalformedMessage);
            }
            let _ = src.split_to(5);
        }

        if pad > 0 {
            if pad > src.len() {
                return Err(FrameError::TooMuchPadding);
            }
            src.truncate(src.len() - pad);
        }

        Ok(Headers {
            flags,
            stream_id: head.stream_id(),
            header_block: HeaderBlock {
                fields: HeaderMap::new(),
                is_over_size: false,
                pseudo: PseudoHeaders::default(),
            },
        })
    }

    pub fn load_hpack(
        &mut self,
        src: &mut BytesMut,
        max_header_list_size: usize,
        decoder: &mut hpack::Decoder,
    ) -> Result<(), FrameError> {
        self.header_block.load(src, max_header_list_size, decoder)
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    pub fn is_end_headers(&self) -> bool {
        self.flags.is_end_headers()
    }

    pub fn set_end_headers(&mut self) {
        self.flags.set_end_headers();
    }

    pub fn is_end_stream(&self) -> bool {
        self.flags.is_end_stream()
    }

    pub fn set_end_stream(&mut self) {
        self.flags.set_end_stream()
    }

    pub fn is_over_size(&self) -> bool {
        self.header_block.is_over_size
    }

    pub fn into_parts(self) -> (PseudoHeaders, HeaderMap) {
        (self.header_block.pseudo, self.header_block.fields)
    }

    pub fn fields(&self) -> &HeaderMap {
        &self.header_block.fields
    }

    pub fn pseudo(&self) -> &PseudoHeaders {
        &self.header_block.pseudo
    }

    pub fn into_fields(self) -> HeaderMap {
        self.header_block.fields
    }

    pub fn encode(self, encoder: &mut hpack::Encoder, dst: &mut BytesMut) {
        // At this point, the `is_end_headers` flag should always be set
        debug_assert!(self.flags.is_end_headers());

        // Get the HEADERS frame head
        let head = self.head();

        self.header_block
            .into_encoding(encoder)
            .encode(&head, dst, |_| {})
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
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("Headers");
        builder
            .field("stream_id", &self.stream_id)
            .field("flags", &self.flags)
            .field("headers", &self.header_block);

        if let Some(ref protocol) = self.header_block.pseudo.protocol {
            builder.field("protocol", protocol);
        }

        // `fields` and `pseudo` purposefully not included
        builder.finish()
    }
}

// ===== util =====

pub fn parse_u64(src: &[u8]) -> Result<u64, ()> {
    if src.len() > 19 {
        // At danger for overflow...
        return Err(());
    }

    let mut ret = 0;
    for &d in src {
        if !(b'0'..=b'9').contains(&d) {
            return Err(());
        }

        ret *= 10;
        ret += (d - b'0') as u64;
    }

    Ok(ret)
}

// ===== impl Pseudo =====

impl PseudoHeaders {
    pub fn request(method: Method, uri: Uri, protocol: Option<Protocol>) -> Self {
        let parts = uri::Parts::from(uri);

        let mut path = parts
            .path_and_query
            .map(|v| ByteString::from(v.as_str()))
            .unwrap_or(ByteString::from_static(""));

        match method {
            Method::OPTIONS | Method::CONNECT => {}
            _ if path.is_empty() => {
                path = ByteString::from_static("/");
            }
            _ => {}
        }

        let mut pseudo = PseudoHeaders {
            method: Some(method),
            scheme: None,
            authority: None,
            path: Some(path).filter(|p| !p.is_empty()),
            protocol,
            status: None,
        };

        // If the URI includes a scheme component, add it to the pseudo headers
        //
        // TODO: Scheme must be set...
        if let Some(scheme) = parts.scheme {
            pseudo.set_scheme(scheme);
        }

        // If the URI includes an authority component, add it to the pseudo
        // headers
        if let Some(authority) = parts.authority {
            pseudo.set_authority(ByteString::from(authority.as_str()));
        }

        pseudo
    }

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

    pub fn set_status(&mut self, value: StatusCode) {
        self.status = Some(value);
    }

    pub fn set_scheme(&mut self, scheme: uri::Scheme) {
        self.scheme = Some(match scheme.as_str() {
            "http" => ByteString::from_static("http"),
            "https" => ByteString::from_static("https"),
            s => ByteString::from(s),
        });
    }

    pub fn set_protocol(&mut self, protocol: Protocol) {
        self.protocol = Some(protocol);
    }

    pub fn set_authority(&mut self, authority: ByteString) {
        self.authority = Some(authority);
    }
}

// ===== impl EncodingHeaderBlock =====

impl EncodingHeaderBlock {
    fn encode<F>(self, head: &Head, dst: &mut BytesMut, f: F)
    where
        F: FnOnce(&mut BytesMut),
    {
        let head_pos = dst.len();

        // At this point, we don't know how big the h2 frame will be.
        // So, we write the head with length 0, then write the body, and
        // finally write the length once we know the size.
        head.encode(0, dst);

        let payload_pos = dst.len();

        f(dst);

        // Now, encode the header payload
        dst.extend_from_slice(&self.hpack);

        // Compute the header block length
        let payload_len = (dst.len() - payload_pos) as u64;

        // Write the frame length
        let payload_len_be = payload_len.to_be_bytes();
        assert!(payload_len_be[0..5].iter().all(|b| *b == 0));
        (dst[head_pos..head_pos + 3]).copy_from_slice(&payload_len_be[5..]);
    }
}

// ===== impl Iter =====

impl<'a> Iterator for Iter<'a> {
    type Item = hpack::Header<Option<HeaderName>>;

    fn next(&mut self) -> Option<Self::Item> {
        use crate::hpack::Header::*;

        if let Some(ref mut pseudo) = self.pseudo {
            if let Some(method) = pseudo.method.take() {
                return Some(Method(method));
            }

            if let Some(scheme) = pseudo.scheme.take() {
                return Some(Scheme(scheme));
            }

            if let Some(authority) = pseudo.authority.take() {
                return Some(Authority(authority));
            }

            if let Some(path) = pseudo.path.take() {
                return Some(Path(path));
            }

            if let Some(protocol) = pseudo.protocol.take() {
                return Some(Protocol(protocol.into()));
            }

            if let Some(status) = pseudo.status.take() {
                return Some(Status(status));
            }
        }

        self.pseudo = None;

        self.fields.next().map(|(name, value)| Field {
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

    pub fn is_end_stream(&self) -> bool {
        self.0 & END_STREAM == END_STREAM
    }

    pub fn set_end_stream(&mut self) {
        self.0 |= END_STREAM;
    }

    pub fn is_end_headers(&self) -> bool {
        self.0 & END_HEADERS == END_HEADERS
    }

    pub fn set_end_headers(&mut self) {
        self.0 |= END_HEADERS;
    }

    pub fn is_padded(&self) -> bool {
        self.0 & PADDED == PADDED
    }

    pub fn is_priority(&self) -> bool {
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
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        util::debug_flags(fmt, self.0)
            .flag_if(self.is_end_headers(), "END_HEADERS")
            .flag_if(self.is_end_stream(), "END_STREAM")
            .flag_if(self.is_padded(), "PADDED")
            .flag_if(self.is_priority(), "PRIORITY")
            .finish()
    }
}

// ===== HeaderBlock =====

impl HeaderBlock {
    fn load(
        &mut self,
        src: &mut BytesMut,
        max_header_list_size: usize,
        decoder: &mut hpack::Decoder,
    ) -> Result<(), FrameError> {
        let mut reg = !self.fields.is_empty();
        let mut malformed = false;
        let mut headers_size = self.calculate_header_list_size();

        macro_rules! set_pseudo {
            ($field:ident, $len:expr, $val:expr) => {{
                if reg {
                    log::trace!("load_hpack; header malformed -- pseudo not at head of block");
                    malformed = true;
                } else if self.pseudo.$field.is_some() {
                    log::trace!("load_hpack; header malformed -- repeated pseudo");
                    malformed = true;
                } else {
                    let __val = $val;
                    headers_size += decoded_header_size(stringify!($field).len() + 1, $len);
                    if headers_size < max_header_list_size {
                        self.pseudo.$field = Some(__val.into());
                    } else if !self.is_over_size {
                        log::trace!("load_hpack; header list size over max");
                        self.is_over_size = true;
                    }
                }
            }};
        }

        let mut cursor = Cursor::new(src);

        // If the header frame is malformed, we still have to continue decoding
        // the headers. A malformed header frame is a stream level error, but
        // the hpack state is connection level. In order to maintain correct
        // state for other streams, the hpack decoding process must complete.
        let res = decoder.decode(&mut cursor, |header| {
            use crate::hpack::Header::*;

            match header {
                Field { name, value } => {
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
                    } else if name == header::TE && value != "trailers" {
                        log::trace!("load_hpack; TE header not set to trailers; val={:?}", value);
                        malformed = true;
                    } else {
                        reg = true;

                        headers_size += decoded_header_size(name.as_str().len(), value.len());
                        if headers_size < max_header_list_size {
                            self.fields.append(name, value);
                        } else if !self.is_over_size {
                            log::trace!("load_hpack; header list size over max");
                            self.is_over_size = true;
                        }
                    }
                }
                Authority(v) => {
                    let l = v.as_ref().len();
                    set_pseudo!(authority, l, v)
                }
                Method(v) => {
                    let l = v.as_ref().len();
                    set_pseudo!(method, l, v)
                }
                Scheme(v) => {
                    let l = v.as_ref().len();
                    set_pseudo!(scheme, l, v)
                }
                Path(v) => {
                    let l = v.as_ref().len();
                    set_pseudo!(path, l, v)
                }
                Protocol(v) => {
                    let l = v.as_ref().len();
                    set_pseudo!(protocol, l, v)
                }
                Status(v) => {
                    let l = v.as_str().len();
                    set_pseudo!(status, l, v)
                }
            }
        });

        if let Err(e) = res {
            log::trace!("hpack decoding error; err={:?}", e);
            return Err(e.into());
        }

        if malformed {
            log::trace!("malformed message");
            return Err(FrameError::MalformedMessage);
        }

        Ok(())
    }

    fn into_encoding(self, encoder: &mut hpack::Encoder) -> EncodingHeaderBlock {
        let mut hpack = BytesMut::new();
        let headers = Iter {
            pseudo: Some(self.pseudo),
            fields: self.fields.into_iter(),
        };

        encoder.encode(headers, &mut hpack);

        EncodingHeaderBlock {
            hpack: hpack.freeze(),
        }
    }

    /// Calculates the size of the currently decoded header list.
    ///
    /// According to http://httpwg.org/specs/rfc7540.html#SETTINGS_MAX_HEADER_LIST_SIZE
    ///
    /// > The value is based on the uncompressed size of header fields,
    /// > including the length of the name and value in octets plus an
    /// > overhead of 32 octets for each header field.
    fn calculate_header_list_size(&self) -> usize {
        macro_rules! pseudo_size {
            ($name:ident) => {{
                self.pseudo
                    .$name
                    .as_ref()
                    .map(|m| decoded_header_size(stringify!($name).len() + 1, m.as_ref().len()))
                    .unwrap_or(0)
            }};
        }

        pseudo_size!(method)
            + pseudo_size!(scheme)
            + self
                .pseudo
                .status
                .as_ref()
                .map(|m| decoded_header_size("status".len() + 1, m.as_str().len()))
                .unwrap_or(0)
            + pseudo_size!(authority)
            + pseudo_size!(path)
            + self
                .fields
                .iter()
                .map(|(name, value)| decoded_header_size(name.as_str().len(), value.len()))
                .sum::<usize>()
    }
}

fn decoded_header_size(name: usize, value: usize) -> usize {
    name + value + 32
}

#[cfg(test)]
mod test {
    use std::iter::FromIterator;

    use ntex_http::HeaderValue;

    use super::*;
    use crate::frame;
    use crate::hpack::{huffman, Encoder};

    #[test]
    fn test_nameless_header_at_resume() {
        let mut encoder = Encoder::default();
        let mut dst = BytesMut::new();

        let headers = Headers::new(
            StreamId::ZERO,
            Default::default(),
            HeaderMap::from_iter(vec![
                (
                    HeaderName::from_static("hello"),
                    HeaderValue::from_static("world"),
                ),
                (
                    HeaderName::from_static("hello"),
                    HeaderValue::from_static("zomg"),
                ),
                (
                    HeaderName::from_static("hello"),
                    HeaderValue::from_static("sup"),
                ),
            ]),
        );

        let continuation = headers
            .encode(&mut encoder, &mut (&mut dst).limit(frame::HEADER_LEN + 8))
            .unwrap();

        assert_eq!(17, dst.len());
        assert_eq!([0, 0, 8, 1, 0, 0, 0, 0, 0], &dst[0..9]);
        assert_eq!(&[0x40, 0x80 | 4], &dst[9..11]);
        assert_eq!("hello", huff_decode(&dst[11..15]));
        assert_eq!(0x80 | 4, dst[15]);

        let mut world = dst[16..17].to_owned();

        dst.clear();

        assert!(continuation
            .encode(&mut (&mut dst).limit(frame::HEADER_LEN + 16))
            .is_none());

        world.extend_from_slice(&dst[9..12]);
        assert_eq!("world", huff_decode(&world));

        assert_eq!(24, dst.len());
        assert_eq!([0, 0, 15, 9, 4, 0, 0, 0, 0], &dst[0..9]);

        // // Next is not indexed
        assert_eq!(&[15, 47, 0x80 | 3], &dst[12..15]);
        assert_eq!("zomg", huff_decode(&dst[15..18]));
        assert_eq!(&[15, 47, 0x80 | 3], &dst[18..21]);
        assert_eq!("sup", huff_decode(&dst[21..]));
    }

    fn huff_decode(src: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        huffman::decode(src, &mut buf).unwrap()
    }
}
