use ntex_bytes::{BytePages, Bytes};

use crate::frame::{Frame, FrameError, Head, Kind, StreamId, util};

/// Data frame
///
/// Data frames convey arbitrary, variable-length sequences of octets associated
/// with a stream. One or more DATA frames are used, for instance, to carry HTTP
/// request or response payloads.
#[derive(Clone, Eq, PartialEq)]
pub struct Data {
    stream_id: StreamId,
    payload: Bytes,
    flags: DataFlags,
    /// Frame payload length, including padding
    flow_len: u32,
}

#[derive(Default, Copy, Clone, Eq, PartialEq)]
struct DataFlags(u8);

const END_STREAM: u8 = 0x1;
const PADDED: u8 = 0x8;
const ALL: u8 = END_STREAM | PADDED;

impl Data {
    /// Creates a new DATA frame.
    ///
    /// # Panics
    ///
    /// Panics if stream id is zero
    pub fn new(stream_id: StreamId, payload: Bytes) -> Self {
        assert!(!stream_id.is_zero());

        Data {
            flow_len: payload.len() as u32,
            payload,
            stream_id,
            flags: DataFlags::default(),
        }
    }

    /// Returns the stream identifier that this frame is associated with.
    ///
    /// This cannot be a zero stream identifier.
    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    /// Gets the value of the `END_STREAM` flag for this frame.
    ///
    /// If true, this frame is the last that the endpoint will send for the
    /// identified stream.
    ///
    /// Setting this flag causes the stream to enter one of the "half-closed"
    /// states or the "closed" state (Section 5.1).
    pub fn is_end_stream(&self) -> bool {
        self.flags.is_end_stream()
    }

    /// Sets the value for the `END_STREAM` flag on this frame.
    pub fn set_end_stream(&mut self) {
        self.flags.set_end_stream();
    }

    /// Returns whether the `PADDED` flag is set on this frame.
    pub fn is_padded(&self) -> bool {
        self.flags.is_padded()
    }

    /// Sets the value for the `PADDED` flag on this frame.
    ///
    /// The frame is encoded with a pad length field and no padding, the
    /// padding of a received frame is not preserved.
    pub fn set_padded(&mut self) {
        if !self.flags.is_padded() {
            self.flags.set_padded();
            self.flow_len += 1;
        }
    }

    /// Returns a reference to this frame's payload.
    ///
    /// This does **not** include any padding that might have been originally
    /// included.
    pub fn payload(&self) -> &Bytes {
        &self.payload
    }

    /// Returns a mutable reference to this frame's payload.
    ///
    /// This does **not** include any padding that might have been originally
    /// included.
    pub fn payload_mut(&mut self) -> &mut Bytes {
        &mut self.payload
    }

    /// Returns the flow-controlled length of this frame.
    ///
    /// This is the length of the entire frame payload, including the pad
    /// length field and any padding that was originally included.
    pub fn flow_controlled_len(&self) -> u32 {
        self.flow_len
    }

    /// Consumes `self` and returns the frame's payload.
    ///
    /// This does **not** include any padding that might have been originally
    /// included.
    pub fn into_payload(self) -> Bytes {
        self.payload
    }

    pub(crate) fn head(&self) -> Head {
        Head::new(Kind::Data, self.flags.into(), self.stream_id)
    }

    pub(crate) fn load(head: Head, mut payload: Bytes) -> Result<Self, FrameError> {
        let flags = DataFlags::load(head.flag());

        // The stream identifier must not be zero
        if head.stream_id().is_zero() {
            return Err(FrameError::InvalidStreamId);
        }

        let flow_len = payload.len() as u32;
        if flags.is_padded() {
            util::strip_padding(&mut payload)?;
        }

        Ok(Data {
            flow_len,
            flags,
            payload,
            stream_id: head.stream_id(),
        })
    }

    /// Encode the data frame into the `dst` buffer.
    pub(crate) fn encode(self, dst: &mut BytePages) {
        if self.flags.is_padded() {
            self.head().encode(self.payload.len() + 1, dst);
            // pad length
            dst.extend_from_slice(&[0]);
        } else {
            self.head().encode(self.payload.len(), dst);
        }
        dst.append(self.payload);
    }
}

impl From<Data> for Frame {
    fn from(src: Data) -> Self {
        Frame::Data(src)
    }
}

impl std::fmt::Debug for Data {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = fmt.debug_struct("Data");
        f.field("stream_id", &self.stream_id);
        f.field("data_len", &self.payload.len());
        if !self.flags.is_empty() {
            f.field("flags", &self.flags);
        }
        // `data` bytes purposefully excluded
        f.finish()
    }
}

// ===== impl DataFlags =====

impl DataFlags {
    fn load(bits: u8) -> DataFlags {
        DataFlags(bits & ALL)
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn is_end_stream(self) -> bool {
        self.0 & END_STREAM == END_STREAM
    }

    fn set_end_stream(&mut self) {
        self.0 |= END_STREAM;
    }

    fn is_padded(self) -> bool {
        self.0 & PADDED == PADDED
    }

    fn set_padded(&mut self) {
        self.0 |= PADDED;
    }
}

impl From<DataFlags> for u8 {
    fn from(src: DataFlags) -> u8 {
        src.0
    }
}

impl std::fmt::Debug for DataFlags {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        util::debug_flags(fmt, self.0)
            .flag_if(self.is_end_stream(), "END_STREAM")
            .flag_if(self.is_padded(), "PADDED")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_is_flow_controlled() {
        let head = Head::new(Kind::Data, PADDED, StreamId::from(1));
        let frm = Data::load(head, Bytes::from_static(b"\x03data\0\0\0")).unwrap();
        assert_eq!(frm.payload(), &Bytes::from_static(b"data"));
        assert_eq!(frm.flow_controlled_len(), 8);

        let frm = Data::new(StreamId::from(1), Bytes::from_static(b"data"));
        assert_eq!(frm.flow_controlled_len(), 4);
    }

    #[test]
    fn padded_frame_encodes_pad_length() {
        let mut frm = Data::new(StreamId::from(1), Bytes::from_static(b"data"));
        frm.set_padded();
        frm.set_padded();
        assert_eq!(frm.flow_controlled_len(), 5);

        let mut dst = BytePages::default();
        frm.encode(&mut dst);
        let buf = dst.take().unwrap().freeze();
        assert_eq!(&buf[..], b"\0\0\x05\0\x08\0\0\0\x01\0data");

        let head = Head::new(Kind::Data, PADDED, StreamId::from(1));
        let frm = Data::load(head, buf.slice(9..)).unwrap();
        assert_eq!(frm.payload(), &Bytes::from_static(b"data"));
        assert_eq!(frm.flow_controlled_len(), 5);
    }

    #[test]
    fn data_debug() {
        let mut frm = Data::new(StreamId::from(1), Bytes::from_static(b"secret"));
        assert_eq!(format!("{frm:?}"), "Data { stream_id: StreamId(1), data_len: 6 }");

        frm.set_end_stream();
        assert_eq!(
            format!("{frm:?}"),
            "Data { stream_id: StreamId(1), data_len: 6, flags: (0x1: END_STREAM) }"
        );

        let head = Head::new(Kind::Data, PADDED | END_STREAM, StreamId::from(3));
        let frm = Data::load(head, Bytes::from_static(b"\x03data\0\0\0")).unwrap();
        assert_eq!(
            format!("{frm:?}"),
            "Data { stream_id: StreamId(3), data_len: 4, flags: (0x9: END_STREAM | PADDED) }"
        );
    }

    #[test]
    fn data_flags_debug() {
        assert_eq!(format!("{:?}", DataFlags::load(0)), "(0x0)");
        assert_eq!(format!("{:?}", DataFlags::load(END_STREAM)), "(0x1: END_STREAM)");
        assert_eq!(format!("{:?}", DataFlags::load(PADDED)), "(0x8: PADDED)");
        assert_eq!(
            format!("{:?}", DataFlags::load(ALL)),
            "(0x9: END_STREAM | PADDED)"
        );
        // unknown bits are dropped on load
        assert_eq!(
            format!("{:?}", DataFlags::load(0xff)),
            "(0x9: END_STREAM | PADDED)"
        );

        let mut flags = DataFlags::load(0);
        flags.set_padded();
        assert_eq!(format!("{flags:?}"), "(0x8: PADDED)");
    }
}
