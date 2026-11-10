use std::fmt;

use ntex_h2::frame::{self, PseudoHeaders, StreamId};
use ntex_http::{self as http, HeaderMap};

pub const SETTINGS_ACK: &[u8] = &[0, 0, 0, 4, 1, 0, 0, 0, 0];

// ==== helper functions to easily construct h2 Frames ====

pub fn headers<T>(id: T) -> Mock<frame::Headers>
where
    T: Into<StreamId>,
{
    Mock(frame::Headers::new(
        id.into(),
        PseudoHeaders::default(),
        HeaderMap::default(),
        false,
    ))
}

pub struct Mock<T>(T);

impl Mock<frame::Headers> {
    pub fn response<S>(self, status: S) -> Self
    where
        S: TryInto<http::StatusCode>,
        S::Error: fmt::Debug,
    {
        let status = status.try_into().unwrap();
        let (id, _, fields) = self.into_parts();
        let frame = frame::Headers::new(id, PseudoHeaders::response(status), fields, false);
        Mock(frame)
    }

    pub fn field<K, V>(self, key: K, value: V) -> Self
    where
        K: TryInto<http::header::HeaderName>,
        K::Error: fmt::Debug,
        V: TryInto<http::header::HeaderValue>,
        V::Error: fmt::Debug,
    {
        let (id, pseudo, mut fields) = self.into_parts();
        fields.insert(key.try_into().unwrap(), value.try_into().unwrap());
        let frame = frame::Headers::new(id, pseudo, fields, false);
        Mock(frame)
    }

    pub fn eos(mut self) -> Self {
        self.0.set_end_stream();
        self
    }

    pub fn into_fields(self) -> HeaderMap {
        self.0.into_parts().1
    }

    pub fn into_parts(self) -> (StreamId, frame::PseudoHeaders, HeaderMap) {
        assert!(!self.0.is_end_stream(), "eos flag will be lost");
        assert!(self.0.is_end_headers(), "unset eoh will be lost");
        let id = self.0.stream_id();
        let parts = self.0.into_parts();
        (id, parts.0, parts.1)
    }
}
