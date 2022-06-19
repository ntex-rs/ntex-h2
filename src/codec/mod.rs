use std::cell::RefCell;

use ntex_bytes::BytesMut;
use ntex_codec::{Decoder, Encoder};

mod error;
mod length_delimited;
mod partial;

pub use self::error::EncoderError;

use self::{length_delimited::LengthDelimitedCodec, partial::Partial};
use crate::{frame, frame::Frame, frame::Kind, hpack};

// 16 MB "sane default" taken from golang http2
const DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE: usize = 16 << 20;

// Push promise frame kind
const PUSH_PROMISE: u8 = 5;

#[derive(Debug)]
pub struct Codec(RefCell<CodecInner>);

#[derive(Debug)]
struct CodecInner {
    // encoder state
    encoder_hpack: hpack::Encoder,
    encoder_last_data_frame: Option<frame::Data>,
    encoder_max_frame_size: frame::FrameSize, // Max frame size, this is specified by the peer

    // decoder state
    decoder: LengthDelimitedCodec,
    decoder_hpack: hpack::Decoder,
    decoder_max_header_list_size: usize,
    partial: Option<Partial>, // Partially loaded headers frame
}

impl Default for Codec {
    #[inline]
    /// Returns a new `Codec` with the default max frame size
    fn default() -> Self {
        // Delimit the frames
        let decoder = self::length_delimited::Builder::new()
            .length_field_length(3)
            .length_adjustment(9)
            .max_frame_length(frame::DEFAULT_MAX_FRAME_SIZE as usize)
            .num_skip(0) // Don't skip the header
            .new_codec();

        Codec(RefCell::new(CodecInner {
            decoder,
            decoder_hpack: hpack::Decoder::new(frame::DEFAULT_SETTINGS_HEADER_TABLE_SIZE),
            decoder_max_header_list_size: DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE,
            partial: None,

            encoder_hpack: hpack::Encoder::default(),
            encoder_last_data_frame: None,
            encoder_max_frame_size: frame::DEFAULT_MAX_FRAME_SIZE,
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
    #[inline]
    pub fn set_recv_frame_size(&self, val: usize) {
        assert!(
            frame::DEFAULT_MAX_FRAME_SIZE as usize <= val
                && val <= frame::MAX_MAX_FRAME_SIZE as usize
        );
        self.0.borrow_mut().decoder.set_max_frame_length(val);
    }

    /// Set the max header list size that can be received.
    pub fn set_recv_header_list_size(&self, val: usize) {
        self.0.borrow_mut().decoder_max_header_list_size = val;
    }

    /// Set the peer's max frame size.
    pub fn set_send_frame_size(&self, val: usize) {
        assert!(val <= frame::MAX_MAX_FRAME_SIZE as usize);
        self.0.borrow_mut().encoder_max_frame_size = val as frame::FrameSize;
    }

    /// Set the peer's header table size size.
    pub fn set_send_header_list_size(&self, val: usize) {
        self.0.borrow_mut().encoder_hpack.update_max_size(val);
    }
}

macro_rules! header_block {
    ($slf:ident, $frame:ident, $head:ident, $bytes:ident) => ({
        // Drop the frame header
        let _ = $bytes.split_to(frame::HEADER_LEN);

        // Parse the header frame w/o parsing the payload
        let mut frame = match frame::$frame::load($head, &mut $bytes) {
            Ok(res) => Ok(res),
            Err(frame::FrameError::InvalidDependencyId) => {
                proto_err!(stream: "invalid HEADERS dependency ID");
                // A stream cannot depend on itself. An endpoint MUST
                // treat this as a stream error (Section 5.4.2) of type `PROTOCOL_ERROR`.
                Err(frame::FrameError::InvalidDependencyId)
            },
            Err(e) => {
                proto_err!(conn: "failed to load frame; err={:?}", e);
                Err(e)
            }
        }?;

        let is_end_headers = frame.is_end_headers();

        // Load the HPACK encoded headers
        match frame.load_hpack(&mut $bytes, $slf.decoder_max_header_list_size, &mut $slf.decoder_hpack) {
            Ok(_) => {},
            Err(frame::FrameError::Hpack(hpack::DecoderError::NeedMore(_))) if !is_end_headers => {},
            Err(frame::FrameError::MalformedMessage) => {
                let id = $head.stream_id();
                proto_err!(stream: "malformed header block; stream={:?}", id);
                return Err(frame::FrameError::MalformedMessage)
            },
            Err(e) => {
                proto_err!(conn: "failed HPACK decoding; err={:?}", e);
                return Err(e);
            }
        }

        if is_end_headers {
            frame.into()
        } else {
            log::trace!("loaded partial header block");
            // Defer returning the frame
            $slf.partial = Some(Partial {
                frame,
                buf: $bytes.split(),
            });

            return Ok(None);
        }
    });
}

impl Decoder for Codec {
    type Item = Frame;
    type Error = frame::FrameError;

    /// Decodes a frame.
    ///
    /// This method is intentionally de-generified and outlined because it is very large.
    fn decode(&self, src: &mut BytesMut) -> Result<Option<Frame>, frame::FrameError> {
        log::trace!("decoding frame from {}B", src.len());

        let mut inner = self.0.borrow_mut();
        let mut bytes = if let Some(bytes) = inner.decoder.decode(src)? {
            bytes
        } else {
            return Ok(None);
        };

        // check push promise, we do not support push
        if bytes[3] == PUSH_PROMISE {
            return Err(frame::FrameError::UnexpectedPushPromise);
        }

        // Parse the head
        let head = frame::Head::parse(&bytes);
        let kind = head.kind();

        if inner.partial.is_some() && kind != Kind::Continuation {
            proto_err!(conn: "expected CONTINUATION, got {:?}", kind);
            return Err(frame::FrameError::Continuation(
                frame::FrameContinuationError::Expected,
            ));
        }

        // log::trace!(frame.kind = ?kind);
        let frame = match kind {
            Kind::Settings => frame::Settings::load(head, &bytes[frame::HEADER_LEN..])
                .map_err(|e| {
                    proto_err!(conn: "failed to load SETTINGS frame; err={:?}", e);
                    e
                })?
                .into(),
            Kind::Ping => frame::Ping::load(head, &bytes[frame::HEADER_LEN..])
                .map_err(|e| {
                    proto_err!(conn: "failed to load PING frame; err={:?}", e);
                    e
                })?
                .into(),
            Kind::WindowUpdate => frame::WindowUpdate::load(head, &bytes[frame::HEADER_LEN..])
                .map_err(|e| {
                    proto_err!(conn: "failed to load WINDOW_UPDATE frame; err={:?}", e);
                    e
                })?
                .into(),
            Kind::Data => {
                let _ = bytes.split_to(frame::HEADER_LEN);

                frame::Data::load(head, bytes.freeze())
                    // TODO: Should this always be connection level? Probably not...
                    .map_err(|e| {
                        proto_err!(conn: "failed to load DATA frame; err={:?}", e);
                        e
                    })?
                    .into()
            }
            Kind::Headers => header_block!(inner, Headers, head, bytes),
            Kind::Reset => frame::Reset::load(head, &bytes[frame::HEADER_LEN..])
                .map_err(|e| {
                    proto_err!(conn: "failed to load RESET frame; err={:?}", e);
                    e
                })?
                .into(),
            Kind::GoAway => frame::GoAway::load(&bytes[frame::HEADER_LEN..])
                .map_err(|e| {
                    proto_err!(conn: "failed to load GO_AWAY frame; err={:?}", e);
                    e
                })?
                .into(),
            Kind::Priority => {
                if head.stream_id() == 0 {
                    // Invalid stream identifier
                    proto_err!(conn: "invalid stream ID 0");
                    return Err(frame::FrameError::InvalidStreamId);
                }

                match frame::Priority::load(head, &bytes[frame::HEADER_LEN..]) {
                    Ok(frame) => frame.into(),
                    Err(frame::FrameError::InvalidDependencyId) => {
                        // A stream cannot depend on itself. An endpoint MUST
                        // treat this as a stream error (Section 5.4.2) of type
                        // `PROTOCOL_ERROR`.
                        let id = head.stream_id();
                        proto_err!(stream: "PRIORITY invalid dependency ID; stream={:?}", id);
                        return Err(frame::FrameError::InvalidDependencyId);
                    }
                    Err(e) => {
                        proto_err!(conn: "failed to load PRIORITY frame; err={:?};", e);
                        return Err(e);
                    }
                }
            }
            Kind::Continuation => {
                let is_end_headers = (head.flag() & 0x4) == 0x4;

                // get partial frame
                let mut partial = inner.partial.take().ok_or_else(|| {
                    proto_err!(conn: "received unexpected CONTINUATION frame");
                    frame::FrameError::Continuation(frame::FrameContinuationError::Unexpected)
                })?;

                // The stream identifiers must match
                if partial.frame.stream_id() != head.stream_id() {
                    proto_err!(conn: "CONTINUATION frame stream ID does not match previous frame stream ID");
                    return Err(frame::FrameError::Continuation(
                        frame::FrameContinuationError::UnknownStreamId,
                    ));
                }

                // Extend the buf
                if partial.buf.is_empty() {
                    partial.buf = bytes.split_off(frame::HEADER_LEN);
                } else {
                    if partial.frame.is_over_size() {
                        // If there was left over bytes previously, they may be
                        // needed to continue decoding, even though we will
                        // be ignoring this frame. This is done to keep the HPACK
                        // decoder state up-to-date.
                        //
                        // Still, we need to be careful, because if a malicious
                        // attacker were to try to send a gigantic string, such
                        // that it fits over multiple header blocks.
                        //
                        // Instead, we use a simple heuristic to determine if
                        // we should continue to ignore decoding, or to tell
                        // the attacker to go away.
                        if partial.buf.len() + bytes.len() > inner.decoder_max_header_list_size {
                            proto_err!(conn: "CONTINUATION frame header block size over ignorable limit");
                            return Err(frame::FrameError::Continuation(
                                frame::FrameContinuationError::MaxLeftoverSize,
                            ));
                        }
                    }
                    partial.buf.extend_from_slice(&bytes[frame::HEADER_LEN..]);
                }

                match partial.frame.load_hpack(
                    &mut partial.buf,
                    inner.decoder_max_header_list_size,
                    &mut inner.decoder_hpack,
                ) {
                    Ok(_) => {}
                    Err(frame::FrameError::Hpack(hpack::DecoderError::NeedMore(_)))
                        if !is_end_headers => {}
                    Err(frame::FrameError::MalformedMessage) => {
                        let id = head.stream_id();
                        proto_err!(stream: "malformed CONTINUATION frame; stream={:?}", id);
                        return Err(frame::FrameContinuationError::Malformed.into());
                    }
                    Err(e) => {
                        proto_err!(conn: "failed HPACK decoding; err={:?}", e);
                        return Err(e);
                    }
                }

                if is_end_headers {
                    partial.frame.into()
                } else {
                    inner.partial = Some(partial);
                    return Ok(None);
                }
            }
            Kind::Unknown => {
                // Unknown frames are ignored
                return Ok(None);
            }
        };

        Ok(Some(frame))
    }
}

impl Encoder for Codec {
    type Item = Frame;
    type Error = error::EncoderError;

    fn encode(&self, item: Frame, buf: &mut BytesMut) -> Result<(), error::EncoderError> {
        // Ensure that we have enough capacity to accept the write.
        // log::debug!(frame = ?item, "send");

        let mut inner = self.0.borrow_mut();

        match item {
            Frame::Data(v) => {
                // Ensure that the payload is not greater than the max frame.
                let len = v.payload().len();
                if len > inner.encoder_max_frame_size as usize {
                    return Err(error::EncoderError::MaxSizeExceeded);
                }
                v.encode(buf);

                // Save off the last frame...
                inner.encoder_last_data_frame = Some(v);
            }
            Frame::Headers(v) => {
                v.encode(&mut inner.encoder_hpack, buf);
            }
            Frame::Settings(v) => {
                v.encode(buf);
                // log::trace!(rem = inner.buf.remaining(), "encoded settings");
            }
            Frame::GoAway(v) => {
                v.encode(buf);
                // log::trace!(rem = self.buf.remaining(), "encoded go_away");
            }
            Frame::Ping(v) => {
                v.encode(buf);
                // log::trace!(rem = self.buf.remaining(), "encoded ping");
            }
            Frame::WindowUpdate(v) => {
                v.encode(buf);
                // log::trace!(rem = self.buf.remaining(), "encoded window_update");
            }

            Frame::Priority(_) => {
                unimplemented!()
            }
            Frame::Reset(v) => {
                v.encode(buf);
                // log::trace!(rem = self.buf.remaining(), "encoded reset");
            }
        }

        Ok(())
    }
}
