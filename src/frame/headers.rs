use std::{cell::RefCell, cmp, fmt, io::Cursor};

use ntex_bytes::{ByteString, BytesMut};
use ntex_http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header, uri};

use crate::hpack;

use super::priority::StreamDependency;
use super::{Frame, FrameError, Head, Kind, Protocol, StreamId, util};

/// Header frame
///
/// This could be either a request or a response.
#[derive(Clone)]
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

#[derive(Clone, Debug, Default)]
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

#[derive(Debug, Clone)]
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

// ===== impl Headers =====

impl Headers {
    /// Create a new HEADERS frame
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
    pub fn load(head: Head, src: &mut BytesMut) -> Result<Self, FrameError> {
        let flags = HeadersFlag(head.flag());

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
            let stream_dep = StreamDependency::load(&src[..5])?;

            if stream_dep.dependency_id() == head.stream_id() {
                return Err(FrameError::InvalidDependencyId);
            }

            // Drop the next 5 bytes
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
                pseudo: PseudoHeaders::default(),
            },
        })
    }

    pub fn load_hpack(
        &mut self,
        src: &mut BytesMut,
        decoder: &mut hpack::Decoder,
    ) -> Result<(), FrameError> {
        self.header_block.load(src, decoder)
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

    pub fn encode(self, encoder: &mut hpack::Encoder, dst: &mut BytesMut, max_size: usize) {
        // At this point, the `is_end_headers` flag should always be set
        debug_assert!(self.flags.is_end_headers());

        // Get the HEADERS frame head
        let head = self.head();

        self.header_block.encode(encoder, &head, dst, max_size);
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
        let mut builder = f.debug_struct("Headers");
        builder
            .field("stream_id", &self.stream_id)
            .field("flags", &self.flags)
            .field("pseudo", &self.header_block.pseudo);

        if let Some(ref protocol) = self.header_block.pseudo.protocol {
            builder.field("protocol", protocol);
        }

        // `fields` and `pseudo` purposefully not included
        builder.finish()
    }
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

// ===== impl Iter =====

impl Iterator for Iter<'_> {
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

thread_local! {
    static HDRS_BUF: RefCell<BytesMut> = RefCell::new(BytesMut::with_capacity(1024));
}

impl HeaderBlock {
    fn load(&mut self, src: &mut BytesMut, decoder: &mut hpack::Decoder) -> Result<(), FrameError> {
        let mut reg = !self.fields.is_empty();
        let mut malformed = false;

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
                        log::trace!("load_hpack; TE header not set to trailers; val={value:?}");
                        malformed = true;
                    } else {
                        reg = true;
                        self.fields.append(name, value);
                    }
                }
                Authority(v) => {
                    set_pseudo!(authority, v)
                }
                Method(v) => {
                    set_pseudo!(method, v)
                }
                Scheme(v) => {
                    set_pseudo!(scheme, v)
                }
                Path(v) => {
                    set_pseudo!(path, v)
                }
                Protocol(v) => {
                    set_pseudo!(protocol, v)
                }
                Status(v) => {
                    set_pseudo!(status, v)
                }
            }
        });

        if let Err(e) = res {
            log::trace!("hpack decoding error; err={e:?}");
            return Err(e.into());
        }

        if malformed {
            log::trace!("malformed message");
            return Err(FrameError::MalformedMessage);
        }

        Ok(())
    }

    fn encode(
        self,
        encoder: &mut hpack::Encoder,
        head: &Head,
        dst: &mut BytesMut,
        max_size: usize,
    ) {
        HDRS_BUF.with(|buf| {
            let mut b = buf.borrow_mut();
            let hpack = &mut b;
            hpack.clear();

            // encode hpack
            let headers = Iter {
                pseudo: Some(self.pseudo),
                fields: self.fields.into_iter(),
            };
            encoder.encode(headers, hpack);

            let mut head = *head;
            let mut start = 0;
            loop {
                let end = cmp::min(start + max_size, hpack.len());

                // encode the header payload
                if hpack.len() > end {
                    Head::new(head.kind(), head.flag() ^ END_HEADERS, head.stream_id())
                        .encode(max_size, dst);
                    dst.extend_from_slice(&hpack[start..end]);
                    head = Head::new(Kind::Continuation, END_HEADERS, head.stream_id());
                    start = end;
                } else {
                    head.encode(end - start, dst);
                    dst.extend_from_slice(&hpack[start..end]);
                    break;
                }
            }
        });
    }
}

#[cfg(test)]
mod test {
    use ntex_http::HeaderValue;

    use super::*;
    use crate::hpack::{Encoder, huffman};

    #[test]
    fn test_nameless_header_at_resume() {
        let mut encoder = Encoder::default();
        let mut dst = BytesMut::new();

        let mut hdrs = HeaderMap::default();
        hdrs.append(
            HeaderName::from_static("hello"),
            HeaderValue::from_static("world"),
        );
        hdrs.append(
            HeaderName::from_static("hello"),
            HeaderValue::from_static("zomg"),
        );
        hdrs.append(
            HeaderName::from_static("hello"),
            HeaderValue::from_static("sup"),
        );

        let mut headers = Headers::new(StreamId::CON, Default::default(), hdrs, false);
        headers.set_end_headers();
        headers.encode(&mut encoder, &mut dst, 8);
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

    fn huff_decode(src: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        huffman::decode(src, &mut buf).unwrap()
    }
}
