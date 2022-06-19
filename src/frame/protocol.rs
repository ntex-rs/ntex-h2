//! Extensions specific to the HTTP/2 protocol.
use std::fmt;

use ntex_bytes::ByteString;

/// Represents the `:protocol` pseudo-header used by
/// the [Extended CONNECT Protocol].
///
/// [Extended CONNECT Protocol]: https://datatracker.ietf.org/doc/html/rfc8441#section-4
#[derive(Clone, Eq, PartialEq)]
pub struct Protocol {
    value: ByteString,
}

impl Protocol {
    /// Converts a static string to a protocol name.
    pub const fn from_static(value: &'static str) -> Self {
        Self {
            value: ByteString::from_static(value),
        }
    }

    /// Returns a str representation of the header.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.value.as_ref()
    }
}

impl<'a> From<&'a str> for Protocol {
    fn from(value: &'a str) -> Self {
        Self {
            value: ByteString::from(value),
        }
    }
}

impl From<ByteString> for Protocol {
    fn from(value: ByteString) -> Self {
        Protocol { value }
    }
}

impl From<Protocol> for ByteString {
    fn from(proto: Protocol) -> Self {
        proto.value
    }
}

impl AsRef<[u8]> for Protocol {
    fn as_ref(&self) -> &[u8] {
        self.value.as_bytes()
    }
}

impl fmt::Debug for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.value.fmt(f)
    }
}
