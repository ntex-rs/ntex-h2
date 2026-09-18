use ntex_bytes::{ByteString, Bytes};
use ntex_http::{HeaderName, HeaderValue, Method, StatusCode};

use super::{DecoderError, NeedMore};

/// Header field represented for HPACK encoding or decoding.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Header<T = HeaderName> {
    /// Regular header field.
    Field { name: T, value: HeaderValue },
    // TODO: Change these types to `http::uri` types.
    /// `:authority` pseudo-header.
    Authority(ByteString),
    /// `:method` pseudo-header.
    Method(Method),
    /// `:scheme` pseudo-header.
    Scheme(ByteString),
    /// `:path` pseudo-header.
    Path(ByteString),
    /// `:protocol` pseudo-header.
    Protocol(ByteString),
    /// `:status` pseudo-header.
    Status(StatusCode),
}

/// Header or pseudo-header name.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum Name<'a> {
    /// Regular header name.
    Field(&'a HeaderName),
    /// `:authority`.
    Authority,
    /// `:method`.
    Method,
    /// `:scheme`.
    Scheme,
    /// `:path`.
    Path,
    /// `:protocol`.
    Protocol,
    /// `:status`.
    Status,
}

pub(super) fn len(name: &HeaderName, value: &HeaderValue) -> usize {
    let n: &str = name.as_ref();
    32 + n.len() + value.len()
}

impl Header<Option<HeaderName>> {
    /// Resolves an optional repeated name into a complete header.
    pub fn reify(self) -> Result<Header, HeaderValue> {
        Ok(match self {
            Header::Field { name: Some(n), value } => Header::Field { name: n, value },
            Header::Field { name: None, value } => return Err(value),
            Header::Authority(v) => Header::Authority(v),
            Header::Method(v) => Header::Method(v),
            Header::Scheme(v) => Header::Scheme(v),
            Header::Path(v) => Header::Path(v),
            Header::Protocol(v) => Header::Protocol(v),
            Header::Status(v) => Header::Status(v),
        })
    }
}

impl Header {
    /// Parses a header name and value.
    pub fn new(name: &Bytes, value: Bytes) -> Result<Header, DecoderError> {
        if name.is_empty() {
            return Err(DecoderError::NeedMore(NeedMore::UnexpectedEndOfStream));
        }
        if name[0] == b':' {
            match &name[1..] {
                b"authority" => Ok(Header::Authority(ByteString::try_from(value)?)),
                b"method" => Ok(Header::Method(Method::from_bytes(&value)?)),
                b"scheme" => Ok(Header::Scheme(ByteString::try_from(value)?)),
                b"path" => Ok(Header::Path(ByteString::try_from(value)?)),
                b"protocol" => Ok(Header::Protocol(ByteString::try_from(value)?)),
                b"status" => Ok(Header::Status(StatusCode::from_bytes(&value)?)),
                _ => Err(DecoderError::InvalidPseudoheader),
            }
        } else {
            // HTTP/2 requires lower case header names
            let name = HeaderName::from_lowercase(name)?;
            let value = HeaderValue::from_shared(value)?;

            Ok(Header::Field { name, value })
        }
    }

    /// Returns the HPACK table size of this header.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        match self {
            Header::Field { name, value } => len(name, value),
            Header::Authority(v) => 32 + 10 + v.len(),
            Header::Method(v) => 32 + 7 + v.as_ref().len(),
            Header::Scheme(v) => 32 + 7 + v.len(),
            Header::Path(v) => 32 + 5 + v.len(),
            Header::Protocol(v) => 32 + 9 + v.len(),
            Header::Status(_) => 32 + 7 + 3,
        }
    }

    /// Returns the header name.
    pub fn name(&self) -> Name<'_> {
        match self {
            Header::Field { name, .. } => Name::Field(name),
            Header::Authority(..) => Name::Authority,
            Header::Method(..) => Name::Method,
            Header::Scheme(..) => Name::Scheme,
            Header::Path(..) => Name::Path,
            Header::Protocol(..) => Name::Protocol,
            Header::Status(..) => Name::Status,
        }
    }

    /// Returns the encoded header value.
    pub fn value_slice(&self) -> &[u8] {
        match self {
            Header::Field { value, .. } => value.as_ref(),
            Header::Authority(v) | Header::Scheme(v) | Header::Path(v) | Header::Protocol(v) => {
                v.as_bytes()
            }
            Header::Method(v) => v.as_ref().as_ref(),
            Header::Status(v) => v.as_str().as_ref(),
        }
    }

    /// Returns whether both headers have the same kind and value.
    pub fn value_eq(&self, other: &Header) -> bool {
        match (self, other) {
            (Header::Field { value: a, .. }, Header::Field { value: b, .. }) => a == b,
            (Header::Authority(a), Header::Authority(b))
            | (Header::Scheme(a), Header::Scheme(b))
            | (Header::Path(a), Header::Path(b))
            | (Header::Protocol(a), Header::Protocol(b)) => a == b,
            (Header::Method(a), Header::Method(b)) => a == b,
            _ => false,
        }
    }

    /// Returns whether the header value is marked sensitive.
    pub fn is_sensitive(&self) -> bool {
        if let Header::Field { value, .. } = self {
            value.is_sensitive()
        } else {
            // TODO: Technically these other header values can be sensitive too.
            false
        }
    }

    /// Returns whether the value should be omitted from the dynamic table.
    pub fn skip_value_index(&self) -> bool {
        use ntex_http::header;

        match self {
            Header::Field { name, .. } => matches!(
                *name,
                header::AGE
                    | header::AUTHORIZATION
                    | header::CONTENT_LENGTH
                    | header::ETAG
                    | header::IF_MODIFIED_SINCE
                    | header::IF_NONE_MATCH
                    | header::LOCATION
                    | header::COOKIE
                    | header::SET_COOKIE
            ),
            Header::Path(..) => true,
            _ => false,
        }
    }
}

// Mostly for tests
impl From<Header> for Header<Option<HeaderName>> {
    fn from(src: Header) -> Self {
        match src {
            Header::Field { name, value } => Header::Field {
                value,
                name: Some(name),
            },
            Header::Authority(v) => Header::Authority(v),
            Header::Method(v) => Header::Method(v),
            Header::Scheme(v) => Header::Scheme(v),
            Header::Path(v) => Header::Path(v),
            Header::Protocol(v) => Header::Protocol(v),
            Header::Status(v) => Header::Status(v),
        }
    }
}

impl Name<'_> {
    /// Combines this name with a decoded value.
    pub fn into_entry(self, value: Bytes) -> Result<Header, DecoderError> {
        match self {
            Name::Field(name) => Ok(Header::Field {
                name: name.clone(),
                value: HeaderValue::from_shared(value)?,
            }),
            Name::Authority => Ok(Header::Authority(ByteString::try_from(value)?)),
            Name::Method => Ok(Header::Method(Method::from_bytes(&value)?)),
            Name::Scheme => Ok(Header::Scheme(ByteString::try_from(value)?)),
            Name::Path => Ok(Header::Path(ByteString::try_from(value)?)),
            Name::Protocol => Ok(Header::Protocol(ByteString::try_from(value)?)),
            Name::Status => {
                match StatusCode::from_bytes(&value) {
                    Ok(status) => Ok(Header::Status(status)),
                    // TODO: better error handling
                    Err(_) => Err(DecoderError::InvalidStatusCode),
                }
            }
        }
    }

    /// Returns the encoded header name.
    pub fn as_slice(&self) -> &[u8] {
        match *self {
            Name::Field(ref name) => name.as_ref(),
            Name::Authority => b":authority",
            Name::Method => b":method",
            Name::Scheme => b":scheme",
            Name::Path => b":path",
            Name::Protocol => b":protocol",
            Name::Status => b":status",
        }
    }
}
