use ntex_bytes::{BytePages, ByteString, Bytes, BytesMut};
use ntex_codec::{Decoder, Encoder};
use ntex_h2::frame::{self, Frame, FrameContinuationError, FrameError, Kind, Protocol, Reason, StreamId};
use ntex_h2::{Codec, EncoderError, hpack::DecoderError};
use ntex_http::HeaderMap;

const END_HEADERS: u8 = 0x4;

fn raw_frame(kind: u8, flags: u8, stream_id: u32, payload: &[u8]) -> BytesMut {
    let len = payload.len();
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags]);
    buf.extend_from_slice(&stream_id.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn setting(id: u16, val: u32) -> Vec<u8> {
    let mut buf = id.to_be_bytes().to_vec();
    buf.extend_from_slice(&val.to_be_bytes());
    buf
}

fn decode(mut buf: BytesMut) -> Result<Option<Frame>, FrameError> {
    Codec::default().decode(&mut buf)
}

fn encode(codec: &Codec, frm: Frame) -> BytesMut {
    let mut buf = BytePages::default();
    codec.encode(frm, &mut buf).unwrap();
    BytesMut::copy_from_slice(buf.freeze())
}

#[test]
fn malformed_frames_are_connection_errors() {
    let cases = [
        // SETTINGS
        (raw_frame(4, 0, 1, &[]), FrameError::InvalidStreamId),
        (
            raw_frame(4, 1, 0, &setting(1, 0)),
            FrameError::InvalidPayloadAckSettings,
        ),
        (raw_frame(4, 0, 0, &[0; 5]), FrameError::InvalidPayloadLength),
        (
            raw_frame(4, 0, 0, &setting(2, 2)),
            FrameError::InvalidSettingValue,
        ),
        (
            raw_frame(4, 0, 0, &setting(4, 1 << 31)),
            FrameError::InvalidInitialWindowSize,
        ),
        (
            raw_frame(4, 0, 0, &setting(5, 16_383)),
            FrameError::InvalidSettingValue,
        ),
        (
            raw_frame(4, 0, 0, &setting(5, 1 << 24)),
            FrameError::InvalidSettingValue,
        ),
        (
            raw_frame(4, 0, 0, &setting(8, 2)),
            FrameError::InvalidSettingValue,
        ),
        // PING
        (raw_frame(6, 0, 1, &[0; 8]), FrameError::InvalidStreamId),
        (raw_frame(6, 0, 0, &[0; 7]), FrameError::BadFrameSize),
        // WINDOW_UPDATE
        (raw_frame(8, 0, 0, &[0; 3]), FrameError::BadFrameSize),
        // RST_STREAM
        (raw_frame(3, 0, 1, &[0; 3]), FrameError::InvalidPayloadLength),
        (raw_frame(3, 0, 1, &[0; 5]), FrameError::InvalidPayloadLength),
        // GOAWAY
        (raw_frame(7, 0, 1, &[0; 8]), FrameError::InvalidStreamId),
        // PRIORITY
        (raw_frame(2, 0, 0, &[0, 0, 0, 3, 16]), FrameError::InvalidStreamId),
        // DATA, padded
        (raw_frame(0, 0x8, 1, &[]), FrameError::TooMuchPadding),
        (raw_frame(0, 0x8, 1, &[3, 0, 0]), FrameError::TooMuchPadding),
        // CONTINUATION without HEADERS
        (
            raw_frame(9, END_HEADERS, 1, &[0x82]),
            FrameError::Continuation(FrameContinuationError::Unexpected),
        ),
        // HEADERS, invalid HPACK is a connection error
        (
            raw_frame(1, END_HEADERS, 1, &[0xbe]),
            FrameError::Hpack(DecoderError::InvalidTableIndex),
        ),
        // HEADERS, framing
        (raw_frame(1, END_HEADERS, 0, &[0x82]), FrameError::InvalidStreamId),
        (
            raw_frame(1, END_HEADERS | 0x8, 1, &[]),
            FrameError::MalformedMessage,
        ),
        (
            raw_frame(1, END_HEADERS | 0x20, 1, &[0, 0, 0]),
            FrameError::MalformedMessage,
        ),
        (
            raw_frame(1, END_HEADERS | 0x8, 1, &[5, 0x82]),
            FrameError::TooMuchPadding,
        ),
        // HPACK, dynamic table size update above the maximum
        (
            raw_frame(1, END_HEADERS, 1, &[0x3f, 0xe2, 0x1f]),
            FrameError::Hpack(DecoderError::InvalidMaxDynamicSize),
        ),
        // HPACK, size update after a header field
        (
            raw_frame(1, END_HEADERS, 1, &[0x82, 0x20]),
            FrameError::Hpack(DecoderError::InvalidMaxDynamicSize),
        ),
        // HPACK, integer overflow
        (
            raw_frame(1, END_HEADERS, 1, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x0f]),
            FrameError::Hpack(DecoderError::IntegerOverflow),
        ),
        // HPACK, invalid huffman string
        (
            raw_frame(1, END_HEADERS, 1, &[0x00, 0x81, 0xff, 0x01, b'a']),
            FrameError::Hpack(DecoderError::InvalidHuffmanCode),
        ),
    ];

    for (idx, (buf, err)) in cases.into_iter().enumerate() {
        match decode(buf) {
            Err(e) => assert_eq!(e, err, "case {idx}"),
            res => panic!("case {idx}: expected {err:?}, got {res:?}"),
        }
    }
}

#[test]
fn continuation_sequence_errors() {
    // a non CONTINUATION frame interrupts the header block
    let codec = Codec::default();
    let mut buf = raw_frame(1, 0, 1, &[0x82]);
    buf.extend_from_slice(&raw_frame(6, 0, 0, &[0; 8]));
    assert_eq!(
        codec.decode(&mut buf).unwrap_err(),
        FrameError::Continuation(FrameContinuationError::Expected)
    );

    // CONTINUATION for a different stream
    let codec = Codec::default();
    let mut buf = raw_frame(1, 0, 1, &[0x82]);
    buf.extend_from_slice(&raw_frame(9, END_HEADERS, 3, &[0x84]));
    assert_eq!(
        codec.decode(&mut buf).unwrap_err(),
        FrameError::Continuation(FrameContinuationError::UnknownStreamId)
    );

    // a partial block waits for more data
    let codec = Codec::default();
    let mut buf = raw_frame(1, 0, 1, &[0x82, 0x86]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
    let mut buf = raw_frame(9, END_HEADERS, 1, &[0x84]);
    let Some(Frame::Headers(hdrs)) = codec.decode(&mut buf).unwrap() else {
        panic!("expected headers")
    };
    assert_eq!(hdrs.stream_id(), 1);
    assert!(hdrs.is_end_headers());
}

#[test]
fn priority_frames() {
    let frm = decode(raw_frame(2, 0, 3, &[0x80, 0, 0, 1, 16])).unwrap().unwrap();
    let Frame::Priority(prio) = frm else {
        panic!("expected priority frame: {frm:?}")
    };
    assert!(format!("{prio:?}").contains("Priority"));

    // PRIORITY frames are not encoded, the same for invalid frames
    let codec = Codec::default();
    assert!(encode(&codec, Frame::Priority(prio)).is_empty());

    // self dependency and bad length are stream errors
    for (payload, err) in [
        (&[0, 0, 0, 3, 16][..], FrameError::InvalidDependencyId),
        (&[0, 0, 0, 1][..], FrameError::InvalidPayloadLength),
    ] {
        match decode(raw_frame(2, 0, 3, payload)) {
            Ok(Some(Frame::Invalid(frm))) => {
                assert_eq!(frm.kind(), Kind::Priority);
                assert_eq!(frm.stream_id(), 3);
                assert_eq!(frm.error(), err);
                assert!(encode(&codec, frm.into()).is_empty());
            }
            res => panic!("expected invalid frame: {res:?}"),
        }
    }
}

#[test]
fn unknown_frames_are_skipped() {
    let mut buf = raw_frame(0xfa, 0xff, 5, b"ignored");
    buf.extend_from_slice(&raw_frame(6, 0, 0, b"12345678"));
    let codec = Codec::default();
    let Some(Frame::Ping(ping)) = codec.decode(&mut buf).unwrap() else {
        panic!("expected ping")
    };
    assert_eq!(ping.payload(), b"12345678");
    assert!(buf.is_empty());

    // only unknown frames
    let mut buf = raw_frame(0x20, 0, 0, b"x");
    assert!(codec.decode(&mut buf).unwrap().is_none());
    assert!(buf.is_empty());
}

#[test]
fn settings_roundtrip() {
    let mut settings = frame::Settings::default();
    settings.set_enable_push(false);
    settings.set_max_concurrent_streams(Some(10));
    settings.set_initial_window_size(Some(65_535));
    settings.set_max_frame_size(32_768);
    settings.set_max_header_list_size(Some(8192));
    settings.set_enable_connect_protocol(Some(1));

    let dbg = format!("{settings:?}");
    for field in [
        "enable_push",
        "max_concurrent_streams",
        "initial_window_size",
        "max_frame_size",
        "max_header_list_size",
        "enable_connect_protocol",
    ] {
        assert!(dbg.contains(field), "{field} is missing in {dbg}");
    }

    let buf = encode(&Codec::default(), settings.into());
    let Some(Frame::Settings(decoded)) = decode(buf).unwrap() else {
        panic!("expected settings")
    };
    assert!(!decoded.is_ack());
    assert_eq!(decoded.header_table_size(), None);
    assert_eq!(decoded.is_push_enabled(), Some(false));
    assert_eq!(decoded.max_concurrent_streams(), Some(10));
    assert_eq!(decoded.initial_window_size(), Some(65_535));
    assert_eq!(decoded.max_frame_size(), Some(32_768));
    assert_eq!(decoded.max_header_list_size(), Some(8192));
    assert_eq!(decoded.is_extended_connect_protocol_enabled(), Some(true));

    // unknown settings are ignored
    let mut payload = setting(0x99, 7);
    payload.extend_from_slice(&setting(8, 0));
    payload.extend_from_slice(&setting(1, 1024));
    let Some(Frame::Settings(decoded)) = decode(raw_frame(4, 0, 0, &payload)).unwrap() else {
        panic!("expected settings")
    };
    assert_eq!(decoded.is_extended_connect_protocol_enabled(), Some(false));
    assert_eq!(decoded.header_table_size(), Some(1024));
    assert!(format!("{decoded:?}").contains("header_table_size"));

    // ack
    let buf = encode(&Codec::default(), frame::Settings::ack().into());
    let Some(Frame::Settings(decoded)) = decode(buf).unwrap() else {
        panic!("expected settings")
    };
    assert!(decoded.is_ack());
    assert!(format!("{decoded:?}").contains("ACK"));
}

#[test]
fn control_frames_roundtrip() {
    let codec = Codec::default();

    let buf = encode(&codec, frame::Ping::new(*b"abcdefgh").into());
    let Some(Frame::Ping(ping)) = decode(buf).unwrap() else {
        panic!("expected ping")
    };
    assert!(!ping.is_ack());
    assert_eq!(ping.into_payload(), *b"abcdefgh");

    // the reserved bit is ignored
    let Some(Frame::WindowUpdate(upd)) = decode(raw_frame(8, 0, 3, &[0x80, 0, 1, 0])).unwrap() else {
        panic!("expected window update")
    };
    assert_eq!(upd.stream_id(), 3);
    assert_eq!(upd.size_increment(), 256);
    let buf = encode(&codec, upd.into());
    assert_eq!(&buf[9..], &[0, 0, 1, 0]);

    let rst = frame::Reset::new(5.into(), Reason::CANCEL).set_reason(Reason::REFUSED_STREAM);
    let buf = encode(&codec, rst.into());
    let Some(Frame::Reset(rst)) = decode(buf).unwrap() else {
        panic!("expected reset")
    };
    assert_eq!(rst.stream_id(), 5);
    assert_eq!(rst.reason(), Reason::REFUSED_STREAM);
}

#[test]
fn oversized_data_is_not_encoded() {
    let codec = Codec::default();
    let max = codec.send_frame_size() as usize;

    let data = frame::Data::new(1.into(), Bytes::from(vec![0; max]));
    assert!(!encode(&codec, data.into()).is_empty());

    let data = frame::Data::new(1.into(), Bytes::from(vec![0; max + 1]));
    let mut buf = BytePages::default();
    let err = codec.encode(data.into(), &mut buf).unwrap_err();
    assert_eq!(err, EncoderError::MaxSizeExceeded);
    assert_eq!(err.to_string(), "Max size exceeded");
    assert!(buf.freeze().is_empty());

    codec.set_send_frame_size(max + 1);
    let data = frame::Data::new(1.into(), Bytes::from(vec![0; max + 1]));
    assert!(!encode(&codec, data.into()).is_empty());
}

#[test]
fn frame_debug() {
    let hdrs = frame::Headers::new(
        1.into(),
        frame::PseudoHeaders::response(ntex_http::StatusCode::OK),
        HeaderMap::new(),
        false,
    );
    let frames: [(Frame, &str); 8] = [
        (frame::Data::new(1.into(), Bytes::new()).into(), "Data"),
        (hdrs.into(), "Headers"),
        (frame::Settings::default().into(), "Settings"),
        (frame::Ping::new([0; 8]).into(), "Ping"),
        (frame::GoAway::new(Reason::NO_ERROR).into(), "GoAway"),
        (frame::WindowUpdate::new(1.into(), 1).into(), "WindowUpdate"),
        (frame::Reset::new(1.into(), Reason::CANCEL).into(), "Reset"),
        (
            decode(raw_frame(2, 0, 1, &[0, 0, 0, 1, 0])).unwrap().unwrap(),
            "InvalidFrame",
        ),
    ];
    for (frm, name) in frames {
        let dbg = format!("{frm:?}");
        assert!(dbg.contains(name), "{dbg} does not contain {name}");
    }
}

#[test]
fn reason_codes() {
    let known = [
        (Reason::NO_ERROR, "NO_ERROR"),
        (Reason::PROTOCOL_ERROR, "PROTOCOL_ERROR"),
        (Reason::INTERNAL_ERROR, "INTERNAL_ERROR"),
        (Reason::FLOW_CONTROL_ERROR, "FLOW_CONTROL_ERROR"),
        (Reason::SETTINGS_TIMEOUT, "SETTINGS_TIMEOUT"),
        (Reason::STREAM_CLOSED, "STREAM_CLOSED"),
        (Reason::FRAME_SIZE_ERROR, "FRAME_SIZE_ERROR"),
        (Reason::REFUSED_STREAM, "REFUSED_STREAM"),
        (Reason::CANCEL, "CANCEL"),
        (Reason::COMPRESSION_ERROR, "COMPRESSION_ERROR"),
        (Reason::CONNECT_ERROR, "CONNECT_ERROR"),
        (Reason::ENHANCE_YOUR_CALM, "ENHANCE_YOUR_CALM"),
        (Reason::INADEQUATE_SECURITY, "INADEQUATE_SECURITY"),
        (Reason::HTTP_1_1_REQUIRED, "HTTP_1_1_REQUIRED"),
    ];
    let mut descriptions = std::collections::HashSet::new();
    for (code, (reason, name)) in known.into_iter().enumerate() {
        assert_eq!(u32::from(reason), code as u32);
        assert_eq!(Reason::from(code as u32), reason);
        assert_eq!(format!("{reason:?}"), name);
        assert_eq!(reason.to_string(), reason.description());
        assert_ne!(reason.description(), "unknown reason");
        assert!(descriptions.insert(reason.description().to_string()));
    }

    let unknown = Reason::from(0x1f);
    assert_eq!(format!("{unknown:?}"), "Reason(1f)");
    assert_eq!(unknown.to_string(), "unknown reason");
}

#[test]
fn frame_error_reasons() {
    let cases = [
        (FrameError::BadFrameSize, Reason::FRAME_SIZE_ERROR),
        (FrameError::MaxFrameSize, Reason::FRAME_SIZE_ERROR),
        (FrameError::InvalidPayloadLength, Reason::FRAME_SIZE_ERROR),
        (FrameError::InvalidPayloadAckSettings, Reason::FRAME_SIZE_ERROR),
        (FrameError::InvalidInitialWindowSize, Reason::FLOW_CONTROL_ERROR),
        (FrameError::TooManyHeaders(1.into()), Reason::REFUSED_STREAM),
        (
            FrameError::Hpack(DecoderError::InvalidUtf8),
            Reason::PROTOCOL_ERROR,
        ),
        (
            FrameError::Hpack(DecoderError::InvalidStatusCode),
            Reason::PROTOCOL_ERROR,
        ),
        (
            FrameError::Hpack(DecoderError::InvalidPseudoheader),
            Reason::PROTOCOL_ERROR,
        ),
        (
            FrameError::Hpack(DecoderError::InvalidTableIndex),
            Reason::COMPRESSION_ERROR,
        ),
        (
            FrameError::Hpack(DecoderError::IntegerOverflow),
            Reason::COMPRESSION_ERROR,
        ),
        (FrameError::TooMuchPadding, Reason::PROTOCOL_ERROR),
        (FrameError::InvalidSettingValue, Reason::PROTOCOL_ERROR),
        (FrameError::InvalidStreamId, Reason::PROTOCOL_ERROR),
        (FrameError::MalformedMessage, Reason::PROTOCOL_ERROR),
        (FrameError::InvalidDependencyId, Reason::PROTOCOL_ERROR),
        (FrameError::UnexpectedPushPromise, Reason::PROTOCOL_ERROR),
        (
            FrameError::Continuation(FrameContinuationError::MaxContinuations),
            Reason::PROTOCOL_ERROR,
        ),
    ];
    for (err, reason) in cases {
        assert_eq!(err.reason(), reason, "{err:?}");
        assert!(!err.to_string().is_empty());
    }
}

#[test]
fn protocol_conversions() {
    let proto = Protocol::from_static("websocket");
    assert_eq!(proto.as_str(), "websocket");
    assert_eq!(proto.as_ref(), b"websocket");
    assert_eq!(format!("{proto:?}"), "\"websocket\"");
    assert_eq!(Protocol::from("websocket"), proto);

    let s: ByteString = proto.clone().into();
    assert_eq!(s, "websocket");
    assert_eq!(Protocol::from(s), proto);
}

#[test]
fn stream_ids() {
    let id = StreamId::from(1);
    assert!(id.is_client_initiated());
    assert!(!id.is_server_initiated());
    assert_eq!(id.next_id().unwrap(), 3);

    let id = StreamId::from(2);
    assert!(id.is_server_initiated());
    assert!(!id.is_client_initiated());

    let zero = StreamId::zero();
    assert!(zero.is_zero());
    assert_eq!(zero, StreamId::CON);
    assert!(!zero.is_client_initiated());
    assert!(!zero.is_server_initiated());

    assert!(StreamId::MAX.next_id().is_err());
    assert_eq!(
        StreamId::from((u32::MAX >> 1) - 2).next_id().unwrap(),
        StreamId::MAX
    );

    // the reserved bit is reported and cleared
    assert_eq!(StreamId::parse(&[0x80, 0, 0, 5]), (StreamId::from(5), true));
    assert_eq!(StreamId::parse(&[0, 0, 0, 5]), (StreamId::from(5), false));
}

#[test]
fn frame_kinds() {
    let kinds = [
        Kind::Data,
        Kind::Headers,
        Kind::Priority,
        Kind::Reset,
        Kind::Settings,
        Kind::Unknown,
        Kind::Ping,
        Kind::GoAway,
        Kind::WindowUpdate,
        Kind::Continuation,
    ];
    for (byte, kind) in kinds.into_iter().enumerate() {
        assert_eq!(Kind::new(byte as u8), kind);
    }
    assert_eq!(Kind::new(0xff), Kind::Unknown);
}
