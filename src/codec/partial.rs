use ntex_bytes::BytesMut;

use crate::{frame, hpack};

/// Partially loaded headers frame
#[derive(Debug)]
pub(crate) struct Partial {
    /// Empty frame
    pub(crate) frame: Continuable,

    /// Partial header payload
    pub(crate) buf: BytesMut,
}

#[derive(Debug)]
pub(crate) enum Continuable {
    Headers(frame::Headers),
    PushPromise(frame::PushPromise),
}

// ===== impl Continuable =====

impl Continuable {
    pub(crate) fn stream_id(&self) -> frame::StreamId {
        match *self {
            Continuable::Headers(ref h) => h.stream_id(),
            Continuable::PushPromise(ref p) => p.stream_id(),
        }
    }

    pub(crate) fn is_over_size(&self) -> bool {
        match *self {
            Continuable::Headers(ref h) => h.is_over_size(),
            Continuable::PushPromise(ref p) => p.is_over_size(),
        }
    }

    pub(crate) fn load_hpack(
        &mut self,
        src: &mut BytesMut,
        max_header_list_size: usize,
        decoder: &mut hpack::Decoder,
    ) -> Result<(), frame::Error> {
        match *self {
            Continuable::Headers(ref mut h) => h.load_hpack(src, max_header_list_size, decoder),
            Continuable::PushPromise(ref mut p) => p.load_hpack(src, max_header_list_size, decoder),
        }
    }
}

impl From<Continuable> for frame::Frame {
    fn from(cont: Continuable) -> Self {
        match cont {
            Continuable::Headers(mut headers) => {
                headers.set_end_headers();
                headers.into()
            }
            Continuable::PushPromise(mut push) => {
                push.set_end_headers();
                push.into()
            }
        }
    }
}
