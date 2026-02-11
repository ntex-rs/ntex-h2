use crate::frame::{self, FrameError, Head, Kind, StreamId};

use ntex_bytes::BufMut;

const SIZE_INCREMENT_MASK: u32 = 1 << 31;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct WindowUpdate {
    stream_id: StreamId,
    size_increment: u32,
}

impl WindowUpdate {
    pub fn new(stream_id: StreamId, size_increment: u32) -> WindowUpdate {
        WindowUpdate {
            stream_id,
            size_increment,
        }
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    #[allow(clippy::cast_possible_wrap)]
    pub fn size_increment(&self) -> i32 {
        self.size_increment as i32
    }

    /// Builds a `WindowUpdate` frame from a raw frame.
    pub fn load(head: Head, payload: &[u8]) -> Result<WindowUpdate, FrameError> {
        debug_assert_eq!(head.kind(), crate::frame::Kind::WindowUpdate);
        if payload.len() != 4 {
            return Err(FrameError::BadFrameSize);
        }

        // Clear the most significant bit, as that is reserved and MUST be ignored
        // when received.
        let size_increment = unpack_octets_4!(payload, 0, u32) & !SIZE_INCREMENT_MASK;

        Ok(WindowUpdate {
            stream_id: head.stream_id(),
            size_increment,
        })
    }

    pub fn encode<B: BufMut>(&self, dst: &mut B) {
        log::trace!(
            "encoding WINDOW_UPDATE; id={:?}, inc={}",
            self.stream_id,
            self.size_increment
        );
        let head = Head::new(Kind::WindowUpdate, 0, self.stream_id);
        head.encode(4, dst);
        dst.put_u32(self.size_increment);
    }
}

impl From<WindowUpdate> for frame::Frame {
    fn from(src: WindowUpdate) -> Self {
        frame::Frame::WindowUpdate(src)
    }
}
