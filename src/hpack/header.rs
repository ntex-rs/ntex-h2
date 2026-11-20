use ntex_bytes::{ByteString, Bytes};
use ntex_http::{HeaderName, HeaderValue, Method, StatusCode};

use super::{DecoderError, NeedMore};

/// Header field represented for HPACK encoding or decoding.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Header<T = HeaderName> {
    /// Regular header field.
    Field {
        /// Header name.
        name: T,
        /// Header value.
        value: HeaderValue,
    },
    /// `:authority` pseudo-header.
    Authority(ByteString),
    /// `:authority` pseudo-header with sensitive flag
    AuthoritySensitive(ByteString),
    /// `:method` pseudo-header.
    Method(Method),
    /// `:scheme` pseudo-header.
    Scheme(ByteString),
    /// `:path` pseudo-header.
    Path(ByteString),
    /// `:path` pseudo-header with sensitive flag
    PathSensitive(ByteString),
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
            Header::AuthoritySensitive(v) => Header::AuthoritySensitive(v),
            Header::Method(v) => Header::Method(v),
            Header::Scheme(v) => Header::Scheme(v),
            Header::Path(v) => Header::Path(v),
            Header::PathSensitive(v) => Header::PathSensitive(v),
            Header::Protocol(v) => Header::Protocol(v),
            Header::Status(v) => Header::Status(v),
        })
    }
}

impl Header {
    /// Compacts the header data, so it does not retain a larger source buffer.
    pub(super) fn detach(self) -> Header {
        match self {
            Header::Field { name, value } => {
                let mut bytes = value.as_shared().clone();
                bytes.trimdown();
                if bytes.as_ptr() == value.as_bytes().as_ptr() {
                    Header::Field { name, value }
                } else {
                    let sensitive = value.is_sensitive();
                    // the value is already valid, conversion cannot fail
                    let mut value = HeaderValue::from_shared(bytes).unwrap_or(value);
                    value.set_sensitive(sensitive);
                    Header::Field { name, value }
                }
            }
            Header::Authority(mut v) => {
                v.trimdown();
                Header::Authority(v)
            }
            Header::AuthoritySensitive(mut v) => {
                v.trimdown();
                Header::AuthoritySensitive(v)
            }
            Header::Scheme(mut v) => {
                v.trimdown();
                Header::Scheme(v)
            }
            Header::Path(mut v) => {
                v.trimdown();
                Header::Path(v)
            }
            Header::PathSensitive(mut v) => {
                v.trimdown();
                Header::PathSensitive(v)
            }
            Header::Protocol(mut v) => {
                v.trimdown();
                Header::Protocol(v)
            }
            h @ (Header::Method(_) | Header::Status(_)) => h,
        }
    }

    /// Parses a header name and value, marking it as sensitive if specified.
    ///
    /// - `name`: The raw bytes of the header name.
    /// - `value`: The raw bytes of the header value.
    /// - `sensitive`: A boolean flag indicating whether the header value should be treated as sensitive (never-indexed).
    pub fn new(name: &Bytes, value: Bytes, sensitive: bool) -> Result<Header, DecoderError> {
        if name.is_empty() {
            return Err(DecoderError::NeedMore(NeedMore::UnexpectedEndOfStream));
        }

        if name[0] == b':' {
            match &name[1..] {
                b"authority" => {
                    let val = ByteString::try_from(value)?;
                    Ok(if sensitive {
                        Header::AuthoritySensitive(val)
                    } else {
                        Header::Authority(val)
                    })
                }
                b"method" => Ok(Header::Method(Method::from_bytes(&value)?)),
                b"scheme" => Ok(Header::Scheme(ByteString::try_from(value)?)),
                b"path" => {
                    let val = ByteString::try_from(value)?;
                    Ok(if sensitive {
                        Header::PathSensitive(val)
                    } else {
                        Header::Path(val)
                    })
                }
                b"protocol" => Ok(Header::Protocol(ByteString::try_from(value)?)),
                b"status" => Ok(Header::Status(StatusCode::from_bytes(&value)?)),
                _ => Err(DecoderError::InvalidPseudoheader),
            }
        } else {
            // HTTP/2 requires lower case header names
            let name = HeaderName::from_lowercase(name)?;
            let mut value = HeaderValue::from_shared(value)?;
            value.set_sensitive(sensitive);

            Ok(Header::Field { name, value })
        }
    }
    /// Returns the HPACK table size of this header.
    #[inline]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    /// Returns the header name.
    #[inline]
    pub fn name(&self) -> Name<'_> {
        self.as_ref().name()
    }

    /// Returns the encoded header value.
    #[inline]
    pub fn value_slice(&self) -> &[u8] {
        self.as_ref().value_slice()
    }

    /// Returns whether both headers have the same kind and value.
    #[inline]
    pub fn value_eq(&self, other: &Header) -> bool {
        self.as_ref().value_eq(other)
    }

    /// Returns whether the header value is marked sensitive.
    pub fn is_sensitive(&self) -> bool {
        self.as_ref().is_sensitive()
    }

    /// Returns whether the value should be omitted from the dynamic table.
    pub fn skip_value_index(&self) -> bool {
        self.as_ref().skip_value_index()
    }

    /// Borrows the header.
    #[inline]
    pub(crate) fn as_ref(&self) -> HeaderRef<'_> {
        match self {
            Header::Field { name, value } => HeaderRef::Field { name, value },
            Header::Authority(v) => HeaderRef::Authority(v),
            Header::AuthoritySensitive(v) => HeaderRef::AuthoritySensitive(v),
            Header::Method(v) => HeaderRef::Method(v),
            Header::Scheme(v) => HeaderRef::Scheme(v),
            Header::Path(v) => HeaderRef::Path(v),
            Header::PathSensitive(v) => HeaderRef::PathSensitive(v),
            Header::Protocol(v) => HeaderRef::Protocol(v),
            Header::Status(v) => HeaderRef::Status(v),
        }
    }
}

/// Borrowed header field, encoded without cloning the name or value.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HeaderRef<'a> {
    Field {
        name: &'a HeaderName,
        value: &'a HeaderValue,
    },
    Authority(&'a ByteString),
    AuthoritySensitive(&'a ByteString),
    Method(&'a Method),
    Scheme(&'a ByteString),
    Path(&'a ByteString),
    PathSensitive(&'a ByteString),
    Protocol(&'a ByteString),
    Status(&'a StatusCode),
}

impl<'a> HeaderRef<'a> {
    /// Returns the HPACK table size of this header.
    #[inline]
    pub(crate) fn len(self) -> usize {
        match self {
            HeaderRef::Field { name, value } => len(name, value),
            HeaderRef::Authority(v) | HeaderRef::AuthoritySensitive(v) => 32 + 10 + v.len(),
            HeaderRef::Method(v) => 32 + 7 + v.as_ref().len(),
            HeaderRef::Scheme(v) => 32 + 7 + v.len(),
            HeaderRef::Path(v) | HeaderRef::PathSensitive(v) => 32 + 5 + v.len(),
            HeaderRef::Protocol(v) => 32 + 9 + v.len(),
            HeaderRef::Status(_) => 32 + 7 + 3,
        }
    }

    /// Returns the header name.
    #[inline]
    pub(crate) fn name(self) -> Name<'a> {
        match self {
            HeaderRef::Field { name, .. } => Name::Field(name),
            HeaderRef::Authority(..) | HeaderRef::AuthoritySensitive(..) => Name::Authority,
            HeaderRef::Method(..) => Name::Method,
            HeaderRef::Scheme(..) => Name::Scheme,
            HeaderRef::Path(..) | HeaderRef::PathSensitive(..) => Name::Path,
            HeaderRef::Protocol(..) => Name::Protocol,
            HeaderRef::Status(..) => Name::Status,
        }
    }

    /// Returns the encoded header value.
    #[inline]
    pub(crate) fn value_slice(self) -> &'a [u8] {
        match self {
            HeaderRef::Field { value, .. } => value.as_ref(),
            HeaderRef::Authority(v)
            | HeaderRef::AuthoritySensitive(v)
            | HeaderRef::Scheme(v)
            | HeaderRef::Path(v)
            | HeaderRef::PathSensitive(v)
            | HeaderRef::Protocol(v) => v.as_bytes(),
            HeaderRef::Method(v) => v.as_str().as_bytes(),
            HeaderRef::Status(v) => v.as_str().as_bytes(),
        }
    }

    /// Returns whether both headers have the same kind and value.
    #[inline]
    pub(crate) fn value_eq(self, other: &Header) -> bool {
        match (self, other) {
            (HeaderRef::Field { value: a, .. }, Header::Field { value: b, .. }) => a == b,
            (HeaderRef::Authority(a), Header::Authority(b))
            | (HeaderRef::Scheme(a), Header::Scheme(b))
            | (HeaderRef::Path(a), Header::Path(b))
            | (HeaderRef::Protocol(a), Header::Protocol(b)) => a == b,
            (HeaderRef::Method(a), Header::Method(b)) => a == b,
            _ => false,
        }
    }

    /// Returns whether the header value is marked sensitive.
    #[inline]
    pub(crate) fn is_sensitive(self) -> bool {
        match self {
            HeaderRef::Field { value, .. } => value.is_sensitive(),
            HeaderRef::AuthoritySensitive(..) | HeaderRef::PathSensitive(..) => true,
            _ => false,
        }
    }

    /// Returns whether the value should be omitted from the dynamic table.
    pub(crate) fn skip_value_index(self) -> bool {
        use ntex_http::header;

        match self {
            HeaderRef::Field { name, .. } => matches!(
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
            HeaderRef::AuthoritySensitive(..) | HeaderRef::Path(..) | HeaderRef::PathSensitive(..) => {
                true
            }
            _ => false,
        }
    }

    /// Clones the borrowed header.
    pub(crate) fn to_owned(self) -> Header {
        match self {
            HeaderRef::Field { name, value } => Header::Field {
                name: name.clone(),
                value: value.clone(),
            },
            HeaderRef::Authority(v) => Header::Authority(v.clone()),
            HeaderRef::AuthoritySensitive(v) => Header::AuthoritySensitive(v.clone()),
            HeaderRef::Method(v) => Header::Method(v.clone()),
            HeaderRef::Scheme(v) => Header::Scheme(v.clone()),
            HeaderRef::Path(v) => Header::Path(v.clone()),
            HeaderRef::PathSensitive(v) => Header::PathSensitive(v.clone()),
            HeaderRef::Protocol(v) => Header::Protocol(v.clone()),
            HeaderRef::Status(v) => Header::Status(*v),
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
            Header::AuthoritySensitive(v) => Header::AuthoritySensitive(v),
            Header::Method(v) => Header::Method(v),
            Header::Scheme(v) => Header::Scheme(v),
            Header::Path(v) => Header::Path(v),
            Header::PathSensitive(v) => Header::PathSensitive(v),
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
