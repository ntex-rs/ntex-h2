use std::{cell::Cell, cell::RefCell, cmp, rc::Rc};

use ntex_bytes::{BytePages, Bytes, BytesMut};
use ntex_codec::{Decoder, Encoder};

mod error;
mod frame_decoder;

pub use self::error::EncoderError;

use self::frame_decoder::FrameDecoder;
use crate::{consts, frame, frame::Frame, frame::Kind, hpack};

// Push promise frame kind
const PUSH_PROMISE: u8 = 5;

/// Stateful HTTP/2 frame encoder and decoder.
///
/// Clones share HPACK and frame-size state.
#[derive(Clone, Debug)]
pub struct Codec(Rc<CodecInner>);

/// Partially loaded headers frame
#[derive(Debug)]
struct Partial {
    /// Stream of the header block
    stream_id: frame::StreamId,
    /// Flags of the HEADERS frame
    flags: frame::HeadersFlag,
    /// Partial header payload
    buf: BytesMut,
    /// Number of continuations
    count: usize,
    /// Stream-level error found before the block is complete
    error: Option<frame::FrameError>,
}

/// Loads the HPACK header block, stream-level errors are returned as `Ok(Some(_))`.
fn load_hpack(
    frame: &mut frame::Headers,
    src: &mut Bytes,
    inner: &CodecInner,
) -> Result<Option<frame::FrameError>, frame::FrameError> {
    let res = frame.load_hpack(
        src,
        &mut inner.decoder_hpack.borrow_mut(),
        inner.decoder_max_headers.get(),
        inner.decoder_max_header_list_size.get(),
    );
    match res {
        Ok(()) => Ok(None),
        Err(e @ (frame::FrameError::MalformedMessage | frame::FrameError::TooManyHeaders(_))) => {
            proto_err!(stream: "invalid header block; stream={:?}; err={:?}", frame.stream_id(), e);
            Ok(Some(e))
        }
        Err(e) => {
            proto_err!(conn: "failed HPACK decoding; err={:?}", e);
            Err(e)
        }
    }
}

#[derive(Debug)]
struct CodecInner {
    // encoder state
    encoder_hpack: RefCell<hpack::Encoder>,
    encoder_max_frame_size: Cell<frame::FrameSize>, // Max frame size, this is specified by the peer

    // decoder state
    decoder: FrameDecoder,
    decoder_hpack: RefCell<hpack::Decoder>,
    decoder_max_headers: Cell<usize>,
    decoder_max_header_list_size: Cell<usize>,
    decoder_max_header_continuations: Cell<usize>,
    partial: RefCell<Option<Partial>>, // Partially loaded headers frame
}

impl Default for Codec {
    #[inline]
    /// Creates a codec with default HTTP/2 limits.
    fn default() -> Self {
        let decoder = FrameDecoder::new(frame::DEFAULT_MAX_FRAME_SIZE as usize);

        Codec(Rc::new(CodecInner {
            decoder,
            decoder_hpack: RefCell::new(hpack::Decoder::new(frame::DEFAULT_SETTINGS_HEADER_TABLE_SIZE)),
            decoder_max_headers: Cell::new(consts::DEFAULT_MAX_HEADERS),
            decoder_max_header_list_size: Cell::new(
                consts::DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE as usize,
            ),
            decoder_max_header_continuations: Cell::new(consts::DEFAULT_MAX_COUNTINUATIONS),
            partial: RefCell::new(None),

            encoder_hpack: RefCell::new(hpack::Encoder::default()),
            encoder_max_frame_size: Cell::new(frame::DEFAULT_MAX_FRAME_SIZE),
        }))
    }
}

impl Codec {
    /// Updates the max received frame size.
    ///
    /// The change takes effect the next time a frame is decoded. In other
    /// words, if a frame is currently in process of being decoded with a frame
    /// size greater than `val` but less than the max frame size in effect
    /// before calling this function, then the frame will be allowed.
    ///
    /// # Panics
    ///
    /// Panics if size is greater than `16_777_215`.
    #[inline]
    pub fn set_recv_frame_size(&self, val: usize) {
        assert!(
            frame::DEFAULT_MAX_FRAME_SIZE as usize <= val && val <= frame::MAX_MAX_FRAME_SIZE as usize
        );
        self.0.decoder.set_max_frame_length(val);
    }

    /// Returns the maximum frame size accepted from the peer.
    pub fn recv_frame_size(&self) -> u32 {
        self.0.decoder.max_frame_length() as u32
    }

    /// Sets the maximum decoded header-list size.
    ///
    /// The default is 48 KiB.
    pub fn set_recv_header_list_size(&self, val: usize) {
        self.0.decoder_max_header_list_size.set(val);
    }

    /// Sets the maximum number of decoded headers.
    ///
    /// The default is 96.
    pub fn set_max_headers(&self, val: usize) {
        self.0.decoder_max_headers.set(val);
    }

    /// Sets the maximum continuation frames for one header block.
    ///
    /// The default is 5.
    pub fn set_max_header_continuations(&self, val: usize) {
        self.0.decoder_max_header_continuations.set(val);
    }

    /// Sets the maximum frame payload sent to the peer.
    ///
    /// # Panics
    ///
    /// Panics unless size is between `16_384` and `16_777_215`.
    pub fn set_send_frame_size(&self, val: usize) {
        assert!(
            (frame::DEFAULT_MAX_FRAME_SIZE as usize..=frame::MAX_MAX_FRAME_SIZE as usize).contains(&val),
            "frame size must be between 16384 and 16777215"
        );
        self.0.encoder_max_frame_size.set(val as frame::FrameSize);
    }

    /// Sets the peer's HPACK header table size.
    ///
    /// The encoder table is capped at 4096 bytes, larger peer values are ignored.
    pub fn set_send_header_table_size(&self, val: usize) {
        self.0
            .encoder_hpack
            .borrow_mut()
            .update_max_size(val.min(frame::DEFAULT_SETTINGS_HEADER_TABLE_SIZE));
    }

    /// Returns the maximum frame payload sent to the peer.
    pub fn send_frame_size(&self) -> u32 {
        self.0.encoder_max_frame_size.get()
    }

    /// Encodes a HEADERS frame from borrowed header fields.
    pub(crate) fn encode_headers_ref(
        &self,
        id: frame::StreamId,
        pseudo: frame::PseudoHeaders,
        fields: &ntex_http::HeaderMap,
        eof: bool,
        buf: &mut BytePages,
    ) {
        frame::encode_headers_ref(
            id,
            pseudo,
            fields,
            eof,
            &mut self.0.encoder_hpack.borrow_mut(),
            buf,
            self.0.encoder_max_frame_size.get() as usize,
        );
    }
}

impl Decoder for Codec {
    type Item = Frame;
    type Error = frame::FrameError;

    #[allow(clippy::too_many_lines)]
    /// Decodes a frame.
    ///
    /// This method is intentionally de-generified and outlined because it is very large.
    fn decode(&self, src: &mut BytesMut) -> Result<Option<Frame>, frame::FrameError> {
        let inner = &*self.0;
        loop {
            let Some(mut bytes) = inner.decoder.decode(src)? else {
                return Ok(None);
            };

            // check push promise, we do not support push
            if bytes[3] == PUSH_PROMISE {
                return Err(frame::FrameError::UnexpectedPushPromise);
            }

            // Parse the head
            let head = frame::Head::parse(&bytes);
            let kind = head.kind();

            if inner.partial.borrow().is_some() && kind != Kind::Continuation {
                proto_err!(conn: "expected CONTINUATION, got {:?}", kind);
                return Err(frame::FrameError::Continuation(
                    frame::FrameContinuationError::Expected,
                ));
            }

            log::trace!("decoding {:?} frame, frame buf len {}", kind, bytes.len());

            let frame = match kind {
                Kind::Settings => frame::Settings::load(head, &bytes[frame::HEADER_LEN..])
                    .inspect_err(|e| {
                        proto_err!(conn: "failed to load SETTINGS frame; err={:?}", e);
                    })?
                    .into(),
                Kind::Ping => frame::Ping::load(head, &bytes[frame::HEADER_LEN..])
                    .inspect_err(|e| {
                        proto_err!(conn: "failed to load PING frame; err={:?}", e);
                    })?
                    .into(),
                Kind::WindowUpdate => frame::WindowUpdate::load(head, &bytes[frame::HEADER_LEN..])
                    .inspect_err(|e| {
                        proto_err!(conn: "failed to load WINDOW_UPDATE frame; err={:?}", e);
                    })?
                    .into(),
                Kind::Data => {
                    bytes.advance_to(frame::HEADER_LEN);

                    frame::Data::load(head, bytes)
                        // TODO: Should this always be connection level? Probably not...
                        .inspect_err(|e| {
                            proto_err!(conn: "failed to load DATA frame; err={:?}", e);
                        })?
                        .into()
                }
                Kind::Headers => {
                    // Drop the frame header
                    bytes.advance_to(frame::HEADER_LEN);

                    // Parse the header frame w/o parsing the payload
                    let (mut frame, self_dependency) = frame::Headers::load_head(head, &mut bytes)
                        .inspect_err(|e| {
                            proto_err!(conn: "failed to load frame; err={:?}", e);
                        })?;

                    // A stream cannot depend on itself. An endpoint MUST treat this
                    // as a stream error (Section 5.4.2) of type `PROTOCOL_ERROR`.
                    let error = if self_dependency {
                        proto_err!(stream: "invalid HEADERS dependency ID");
                        Some(frame::FrameError::InvalidDependencyId)
                    } else {
                        None
                    };

                    if frame.is_end_headers() {
                        // The block is decoded even if the frame is invalid,
                        // the hpack state is connection level
                        let error = error.or(load_hpack(&mut frame, &mut bytes, inner)?);
                        if let Some(error) = error {
                            frame::InvalidFrame::new(kind, frame.stream_id(), error).into()
                        } else {
                            frame.into()
                        }
                    } else {
                        log::trace!("loaded partial header block");
                        // Defer returning the frame
                        let (stream_id, flags) = frame.into_head();
                        *inner.partial.borrow_mut() = Some(Partial {
                            stream_id,
                            flags,
                            buf: BytesMut::copy_from_slice(&bytes),
                            count: 0,
                            error,
                        });

                        continue;
                    }
                }
                Kind::Reset => frame::Reset::load(head, &bytes[frame::HEADER_LEN..])
                    .inspect_err(|e| {
                        proto_err!(conn: "failed to load RESET frame; err={:?}", e);
                    })?
                    .into(),
                Kind::GoAway => {
                    if head.stream_id() != 0 {
                        proto_err!(conn: "invalid GO_AWAY stream ID {:?}", head.stream_id());
                        return Err(frame::FrameError::InvalidStreamId);
                    }
                    bytes.advance_to(frame::HEADER_LEN);
                    frame::GoAway::load(bytes)
                        .inspect_err(|e| {
                            proto_err!(conn: "failed to load GO_AWAY frame; err={:?}", e);
                        })?
                        .into()
                }
                Kind::Priority => {
                    if head.stream_id() == 0 {
                        // Invalid stream identifier
                        proto_err!(conn: "invalid stream ID 0");
                        return Err(frame::FrameError::InvalidStreamId);
                    }

                    match frame::Priority::load(head, &bytes[frame::HEADER_LEN..]) {
                        Ok(frame) => frame.into(),
                        Err(
                            e @ (frame::FrameError::InvalidDependencyId
                            | frame::FrameError::InvalidPayloadLength),
                        ) => {
                            // A stream cannot depend on itself, a length other than 5
                            // octets is a stream error of type `FRAME_SIZE_ERROR`
                            // (RFC 9113 §6.3).
                            let id = head.stream_id();
                            proto_err!(stream: "invalid PRIORITY frame; stream={:?}; err={:?}", id, e);
                            frame::InvalidFrame::new(kind, id, e).into()
                        }
                        Err(e) => {
                            proto_err!(conn: "failed to load PRIORITY frame; err={:?};", e);
                            return Err(e);
                        }
                    }
                }
                Kind::Continuation => {
                    let mut partial = inner.partial.borrow_mut().take().ok_or_else(|| {
                        proto_err!(conn: "received unexpected CONTINUATION frame");
                        frame::FrameError::Continuation(frame::FrameContinuationError::Unexpected)
                    })?;

                    // The stream identifiers must match
                    if partial.stream_id != head.stream_id() {
                        proto_err!(conn: "CONTINUATION frame stream ID does not match previous frame stream ID");
                        return Err(frame::FrameError::Continuation(
                            frame::FrameContinuationError::UnknownStreamId,
                        ));
                    }

                    let max_continuations = inner.decoder_max_header_continuations.get();
                    if max_continuations > 0 {
                        // Check count of continuation frames
                        partial.count += 1;
                        if partial.count > max_continuations {
                            proto_err!(conn: "received excessive amount of CONTINUATION frames");
                            return Err(frame::FrameError::Continuation(
                                frame::FrameContinuationError::MaxContinuations,
                            ));
                        }
                    }

                    // Accumulate the header block, it is decoded once complete
                    let fragment = &bytes[frame::HEADER_LEN..];
                    if partial.buf.len() + fragment.len() > inner.decoder_max_header_list_size.get() {
                        proto_err!(conn: "CONTINUATION frame header block size over limit");
                        return Err(frame::FrameError::Continuation(
                            frame::FrameContinuationError::MaxLeftoverSize,
                        ));
                    }
                    if partial.buf.capacity() - partial.buf.len() < fragment.len() {
                        // `reserve` allocates the exact size, grow geometrically
                        // to keep the total copying linear
                        partial.buf.reserve(cmp::max(fragment.len(), partial.buf.len()));
                    }
                    partial.buf.extend_from_slice(fragment);

                    if (head.flag() & 0x4) == 0x4 {
                        let mut buf = partial.buf.take();
                        let mut frame = frame::Headers::from_head(partial.stream_id, partial.flags);
                        let error = partial.error.or(load_hpack(&mut frame, &mut buf, inner)?);
                        if let Some(error) = error {
                            frame::InvalidFrame::new(Kind::Headers, partial.stream_id, error).into()
                        } else {
                            frame.into()
                        }
                    } else {
                        *inner.partial.borrow_mut() = Some(partial);
                        continue;
                    }
                }
                Kind::Unknown => {
                    // Unknown frames are ignored
                    continue;
                }
            };

            return Ok(Some(frame));
        }
    }
}

impl Encoder for Codec {
    type Item = Frame;
    type Error = error::EncoderError;

    fn encode(&self, item: Frame, buf: &mut BytePages) -> Result<(), error::EncoderError> {
        // Ensure that we have enough capacity to accept the write.
        // log::debug!(frame = ?item, "send");

        let inner = &*self.0;

        match item {
            Frame::Data(v) => {
                // Ensure that the payload is not greater than the max frame.
                let len = v.payload().len();
                if len > inner.encoder_max_frame_size.get() as usize {
                    return Err(error::EncoderError::MaxSizeExceeded);
                }
                v.encode(buf);
            }
            Frame::Headers(v) => {
                let max_size = inner.encoder_max_frame_size.get() as usize;
                v.encode(&mut inner.encoder_hpack.borrow_mut(), buf, max_size);
            }
            Frame::Settings(v) => {
                v.encode(buf);
            }
            Frame::GoAway(v) => {
                v.encode(buf);
            }
            Frame::Ping(v) => {
                v.encode(buf);
            }
            Frame::WindowUpdate(v) => {
                v.encode(buf);
            }

            Frame::Priority(_) | Frame::Invalid(_) => (),
            Frame::Reset(v) => {
                v.encode(buf);
            }
        }

        Ok(())
    }
}
