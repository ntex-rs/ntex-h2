use std::fmt;

use ntex_bytes::{BufMut, BytePages, Bytes};

use crate::frame::{self, FrameError, Head, Kind, Reason, StreamId};

#[derive(Clone, Eq, PartialEq)]
pub struct GoAway {
    last_stream_id: StreamId,
    error_code: Reason,
    data: Bytes,
}

impl GoAway {
    pub fn new(reason: Reason) -> Self {
        GoAway {
            last_stream_id: 0.into(),
            data: Bytes::new(),
            error_code: reason,
        }
    }

    #[must_use]
    pub fn set_last_stream_id(mut self, id: StreamId) -> Self {
        self.last_stream_id = id;
        self
    }

    #[must_use]
    pub fn set_data<T>(mut self, data: T) -> Self
    where
        Bytes: From<T>,
    {
        self.data = data.into();
        self
    }

    #[must_use]
    pub fn set_reason(mut self, error_code: Reason) -> Self {
        self.error_code = error_code;
        self
    }

    pub fn last_stream_id(&self) -> StreamId {
        self.last_stream_id
    }

    pub fn reason(&self) -> Reason {
        self.error_code
    }

    pub fn data(&self) -> &Bytes {
        &self.data
    }

    pub fn load(mut payload: Bytes) -> Result<GoAway, FrameError> {
        if payload.len() < 8 {
            return Err(FrameError::BadFrameSize);
        }

        let (last_stream_id, _) = StreamId::parse(&payload[..4]);
        let error_code = unpack_octets_4!(payload, 4, u32);
        let data = if payload.len() > 8 {
            payload.advance_to(8);
            payload
        } else {
            Bytes::new()
        };

        Ok(GoAway {
            last_stream_id,
            data,
            error_code: error_code.into(),
        })
    }

    pub fn encode(self, dst: &mut BytePages) {
        log::trace!("encoding GO_AWAY; code={:?}", self.error_code);
        let head = Head::new(Kind::GoAway, 0, StreamId::zero());
        head.encode(8 + self.data.len(), dst);
        dst.put_u32(self.last_stream_id.into());
        dst.put_u32(self.error_code.into());
        dst.append(self.data);
    }
}

impl From<GoAway> for frame::Frame {
    fn from(src: GoAway) -> Self {
        frame::Frame::GoAway(src)
    }
}

impl fmt::Debug for GoAway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("GoAway");
        builder.field("error_code", &self.error_code);
        builder.field("last_stream_id", &self.last_stream_id);

        if !self.data.is_empty() {
            builder.field("data", &self.data);
        }

        builder.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_does_not_copy_data() {
        let mut buf = b"\x00\x00\x00\x05\x00\x00\x00\x02".to_vec();
        buf.extend_from_slice(&[b'a'; 1024]);
        let payload = Bytes::from(buf);
        let frm = GoAway::load(payload.clone()).unwrap();
        assert_eq!(frm.last_stream_id(), StreamId::from(5));
        assert_eq!(frm.reason(), Reason::INTERNAL_ERROR);
        assert_eq!(frm.data(), &[b'a'; 1024][..]);
        assert_eq!(frm.data().as_ptr(), payload[8..].as_ptr());

        let frm = GoAway::load(payload.slice(..8)).unwrap();
        assert!(frm.data().is_empty());

        assert!(GoAway::load(payload.slice(..7)).is_err());
    }
}
