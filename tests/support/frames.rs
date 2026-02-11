#![allow(dead_code)]
use std::fmt;

use ntex_bytes::Bytes;
use ntex_h2::frame::{self, Frame, Protocol, PseudoHeaders, StreamId};
use ntex_http::{self as http, HeaderMap, StatusCode};

pub const SETTINGS: &[u8] = &[0, 0, 0, 4, 0, 0, 0, 0, 0];
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

pub fn data<T, B>(id: T, buf: B) -> Mock<frame::Data>
where
    T: Into<StreamId>,
    B: AsRef<[u8]>,
{
    let buf = Bytes::copy_from_slice(buf.as_ref());
    Mock(frame::Data::new(id.into(), buf))
}

pub fn window_update<T>(id: T, sz: u32) -> frame::WindowUpdate
where
    T: Into<StreamId>,
{
    frame::WindowUpdate::new(id.into(), sz)
}

pub fn go_away<T>(id: T) -> Mock<frame::GoAway>
where
    T: Into<StreamId>,
{
    Mock(frame::GoAway::new(frame::Reason::NO_ERROR).set_last_stream_id(id.into()))
}

pub fn reset<T>(id: T) -> Mock<frame::Reset>
where
    T: Into<StreamId>,
{
    Mock(frame::Reset::new(id.into(), frame::Reason::NO_ERROR))
}

pub fn settings() -> Mock<frame::Settings> {
    Mock(frame::Settings::default())
}

pub fn settings_ack() -> Mock<frame::Settings> {
    Mock(frame::Settings::ack())
}

pub fn ping(payload: [u8; 8]) -> Mock<frame::Ping> {
    Mock(frame::Ping::new(payload))
}

// === Generic helpers of all frame types

pub struct Mock<T>(T);

impl<T: fmt::Debug> fmt::Debug for Mock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl<T> From<Mock<T>> for Frame
where
    T: Into<Frame>,
{
    fn from(src: Mock<T>) -> Self {
        src.0.into()
    }
}

// Headers helpers

impl Mock<frame::Headers> {
    pub fn request<M, U>(self, method: M, uri: U) -> Self
    where
        M: TryInto<http::Method>,
        M::Error: fmt::Debug,
        U: TryInto<http::Uri>,
        U::Error: fmt::Debug,
    {
        let method = method.try_into().unwrap();
        let uri = uri.try_into().unwrap();
        let (id, _, fields) = self.into_parts();
        let extensions = Default::default();
        let pseudo = PseudoHeaders::request(method, uri, extensions);
        let frame = frame::Headers::new(id, pseudo, fields, false);
        Mock(frame)
    }

    pub fn method<M>(self, method: M) -> Self
    where
        M: TryInto<http::Method>,
        M::Error: fmt::Debug,
    {
        let method = method.try_into().unwrap();
        let (id, _, fields) = self.into_parts();
        let frame = frame::Headers::new(
            id,
            frame::PseudoHeaders {
                scheme: None,
                method: Some(method),
                ..Default::default()
            },
            fields,
            false,
        );
        Mock(frame)
    }

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

    pub fn fields(self, fields: HeaderMap) -> Self {
        let (id, pseudo, _) = self.into_parts();
        let frame = frame::Headers::new(id, pseudo, fields, false);
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

    pub fn status(self, value: StatusCode) -> Self {
        let (id, mut pseudo, fields) = self.into_parts();

        pseudo.set_status(value);

        Mock(frame::Headers::new(id, pseudo, fields, false))
    }

    pub fn scheme(self, value: &str) -> Self {
        let (id, mut pseudo, fields) = self.into_parts();
        let value = value.parse().unwrap();

        pseudo.set_scheme(&value);

        Mock(frame::Headers::new(id, pseudo, fields, false))
    }

    pub fn protocol(self, value: &str) -> Self {
        let (id, mut pseudo, fields) = self.into_parts();
        let value = Protocol::from(value);

        pseudo.set_protocol(value);

        Mock(frame::Headers::new(id, pseudo, fields, false))
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

impl From<Mock<frame::Headers>> for frame::Headers {
    fn from(src: Mock<frame::Headers>) -> Self {
        src.0
    }
}

// Data helpers

impl Mock<frame::Data> {
    pub fn padded(mut self) -> Self {
        self.0.set_padded();
        self
    }

    pub fn eos(mut self) -> Self {
        self.0.set_end_stream();
        self
    }
}

// GoAway helpers

impl Mock<frame::GoAway> {
    pub fn protocol_error(self) -> Self {
        self.reason(frame::Reason::PROTOCOL_ERROR)
    }

    pub fn internal_error(self) -> Self {
        self.reason(frame::Reason::INTERNAL_ERROR)
    }

    pub fn flow_control(self) -> Self {
        self.reason(frame::Reason::FLOW_CONTROL_ERROR)
    }

    pub fn frame_size(self) -> Self {
        self.reason(frame::Reason::FRAME_SIZE_ERROR)
    }

    pub fn no_error(self) -> Self {
        self.reason(frame::Reason::NO_ERROR)
    }

    pub fn reason(self, reason: frame::Reason) -> Self {
        Mock(frame::GoAway::new(reason).set_last_stream_id(self.0.last_stream_id()))
    }
}

// ==== Reset helpers

impl Mock<frame::Reset> {
    pub fn protocol_error(self) -> Self {
        let id = self.0.stream_id();
        Mock(frame::Reset::new(id, frame::Reason::PROTOCOL_ERROR))
    }

    pub fn flow_control(self) -> Self {
        let id = self.0.stream_id();
        Mock(frame::Reset::new(id, frame::Reason::FLOW_CONTROL_ERROR))
    }

    pub fn refused(self) -> Self {
        let id = self.0.stream_id();
        Mock(frame::Reset::new(id, frame::Reason::REFUSED_STREAM))
    }

    pub fn cancel(self) -> Self {
        let id = self.0.stream_id();
        Mock(frame::Reset::new(id, frame::Reason::CANCEL))
    }

    pub fn stream_closed(self) -> Self {
        let id = self.0.stream_id();
        Mock(frame::Reset::new(id, frame::Reason::STREAM_CLOSED))
    }

    pub fn internal_error(self) -> Self {
        let id = self.0.stream_id();
        Mock(frame::Reset::new(id, frame::Reason::INTERNAL_ERROR))
    }

    pub fn reason(self, reason: frame::Reason) -> Self {
        let id = self.0.stream_id();
        Mock(frame::Reset::new(id, reason))
    }
}

// ==== Settings helpers

impl Mock<frame::Settings> {
    pub fn max_concurrent_streams(mut self, max: u32) -> Self {
        self.0.set_max_concurrent_streams(Some(max));
        self
    }

    pub fn initial_window_size(mut self, val: u32) -> Self {
        self.0.set_initial_window_size(Some(val));
        self
    }

    pub fn max_header_list_size(mut self, val: u32) -> Self {
        self.0.set_max_header_list_size(Some(val));
        self
    }

    pub fn disable_push(mut self) -> Self {
        self.0.set_enable_push(false);
        self
    }

    pub fn enable_connect_protocol(mut self, val: u32) -> Self {
        self.0.set_enable_connect_protocol(Some(val));
        self
    }
}

impl From<Mock<frame::Settings>> for frame::Settings {
    fn from(src: Mock<frame::Settings>) -> Self {
        src.0
    }
}

// ==== Ping helpers

impl Mock<frame::Ping> {
    pub fn pong(self) -> Self {
        let payload = self.0.into_payload();
        Mock(frame::Ping::pong(payload))
    }
}
