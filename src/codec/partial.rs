use ntex_bytes::BytesMut;

use crate::{frame, hpack};

/// Partially loaded headers frame
#[derive(Debug)]
pub(crate) struct Partial {
    /// Empty frame
    pub(crate) frame: frame::Headers,

    /// Partial header payload
    pub(crate) buf: BytesMut,
}

// ===== impl Continuable =====

impl Partial {
    pub(crate) fn stream_id(&self) -> frame::StreamId {
        self.frame.stream_id()
    }

    pub(crate) fn is_over_size(&self) -> bool {
        self.frame.is_over_size()
    }

    pub(crate) fn load_hpack(
        &mut self,
        src: &mut BytesMut,
        max_header_list_size: usize,
        decoder: &mut hpack::Decoder,
    ) -> Result<(), frame::FrameError> {
        self.frame.load_hpack(src, max_header_list_size, decoder)
    }
}
