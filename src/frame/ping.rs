use ntex_bytes::BufMut;

use crate::frame::{Frame, FrameError, Head, Kind, StreamId};

const ACK_FLAG: u8 = 0x1;

pub(super) type Payload = [u8; 8];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ping {
    ack: bool,
    payload: Payload,
}

impl Ping {
    pub fn new(payload: Payload) -> Ping {
        Ping {
            ack: false,
            payload,
        }
    }

    pub fn pong(payload: Payload) -> Ping {
        Ping { ack: true, payload }
    }

    pub fn is_ack(&self) -> bool {
        self.ack
    }

    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    pub fn into_payload(self) -> Payload {
        self.payload
    }

    /// Builds a `Ping` frame from a raw frame.
    pub fn load(head: Head, bytes: &[u8]) -> Result<Ping, FrameError> {
        debug_assert_eq!(head.kind(), crate::frame::Kind::Ping);

        // PING frames are not associated with any individual stream. If a PING
        // frame is received with a stream identifier field value other than
        // 0x0, the recipient MUST respond with a connection error
        // (Section 5.4.1) of type PROTOCOL_ERROR.
        if !head.stream_id().is_zero() {
            return Err(FrameError::InvalidStreamId);
        }

        // In addition to the frame header, PING frames MUST contain 8 octets of opaque
        // data in the payload.
        if bytes.len() != 8 {
            return Err(FrameError::BadFrameSize);
        }

        let mut payload = [0; 8];
        payload.copy_from_slice(bytes);

        // The PING frame defines the following flags:
        //
        // ACK (0x1): When set, bit 0 indicates that this PING frame is a PING
        //    response. An endpoint MUST set this flag in PING responses. An
        //    endpoint MUST NOT respond to PING frames containing this flag.
        let ack = head.flag() & ACK_FLAG != 0;

        Ok(Ping { ack, payload })
    }

    pub fn encode<B: BufMut>(&self, dst: &mut B) {
        let sz = self.payload.len();
        log::trace!("encoding PING; ack={} len={}", self.ack, sz);

        let flags = if self.ack { ACK_FLAG } else { 0 };
        let head = Head::new(Kind::Ping, flags, StreamId::zero());

        head.encode(sz, dst);
        dst.put_slice(&self.payload);
    }
}

impl From<Ping> for Frame {
    fn from(src: Ping) -> Frame {
        Frame::Ping(src)
    }
}
