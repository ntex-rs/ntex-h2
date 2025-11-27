use ntex_bytes::{Bytes, BytesMut};

use crate::frame::{Frame, FrameError, Head, Kind, StreamId, util};

/// Data frame
///
/// Data frames convey arbitrary, variable-length sequences of octets associated
/// with a stream. One or more DATA frames are used, for instance, to carry HTTP
/// request or response payloads.
#[derive(Clone, Eq, PartialEq)]
pub struct Data {
    stream_id: StreamId,
    data: Bytes,
    flags: DataFlags,
}

#[derive(Default, Copy, Clone, Eq, PartialEq)]
struct DataFlags(u8);

const END_STREAM: u8 = 0x1;
const PADDED: u8 = 0x8;
const ALL: u8 = END_STREAM | PADDED;

impl Data {
    /// Creates a new DATA frame.
    pub fn new(stream_id: StreamId, payload: Bytes) -> Self {
        assert!(!stream_id.is_zero());

        Data {
            stream_id,
            data: payload,
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
    pub fn set_padded(&mut self) {
        self.flags.set_padded();
    }

    /// Returns a reference to this frame's payload.
    ///
    /// This does **not** include any padding that might have been originally
    /// included.
    pub fn payload(&self) -> &Bytes {
        &self.data
    }

    /// Returns a mutable reference to this frame's payload.
    ///
    /// This does **not** include any padding that might have been originally
    /// included.
    pub fn payload_mut(&mut self) -> &mut Bytes {
        &mut self.data
    }

    /// Consumes `self` and returns the frame's payload.
    ///
    /// This does **not** include any padding that might have been originally
    /// included.
    pub fn into_payload(self) -> Bytes {
        self.data
    }

    pub(crate) fn head(&self) -> Head {
        Head::new(Kind::Data, self.flags.into(), self.stream_id)
    }

    pub(crate) fn load(head: Head, mut data: Bytes) -> Result<Self, FrameError> {
        let flags = DataFlags::load(head.flag());

        // The stream identifier must not be zero
        if head.stream_id().is_zero() {
            return Err(FrameError::InvalidStreamId);
        }

        if flags.is_padded() {
            util::strip_padding(&mut data)?;
        }

        Ok(Data {
            data,
            flags,
            stream_id: head.stream_id(),
        })
    }

    /// Encode the data frame into the `dst` buffer.
    pub(crate) fn encode(&self, dst: &mut BytesMut) {
        // Encode the frame head to the buffer
        self.head().encode(self.data.len(), dst);
        // Encode payload
        dst.extend_from_slice(&self.data);
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
        f.field("data_len", &self.data.len());
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

    fn is_empty(&self) -> bool {
        self.0 == 0
    }

    fn is_end_stream(&self) -> bool {
        self.0 & END_STREAM == END_STREAM
    }

    fn set_end_stream(&mut self) {
        self.0 |= END_STREAM
    }

    fn is_padded(&self) -> bool {
        self.0 & PADDED == PADDED
    }

    fn set_padded(&mut self) {
        self.0 |= PADDED
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
