mod support;

use ntex_bytes::BytesMut;
use ntex_codec::{Decoder, Encoder};
use ntex_h2::{Codec, frame, frame::FrameError};
use ntex_http::{HeaderMap, HeaderName, Method, StatusCode};
use ntex_io::testing::IoTest;
use ntex_util::future::join;
use ntex_util::time::{Millis, sleep};

use support::{build_large_headers, frames};

// ===== DATA =====

#[macro_export]
macro_rules! decode_frame {
    ($type: ident, $bytes: ident) => {{
        use ntex_h2::frame::Frame;

        match Codec::default().decode(&mut $bytes) {
            Ok(Some(Frame::$type(frame))) => frame,
            frame => panic!("unexpected frame; actual={:?}", frame),
        }
    }};
}

#[macro_export]
macro_rules! decode_err {
    ($bytes: ident, $type: expr) => {{
        match Codec::default().decode(&mut $bytes) {
            Err(e) => assert_eq!(e, $type),
            frame => panic!("expected error; actual={:?}", frame),
        }
    }};
}

/// STATEMENT: An unpadded `Data` frame preserves its payload and stream identifier without
/// ending the stream.
///
/// HOW:
/// - Decode raw bytes for stream 1 containing `hello` with no flags.
/// - Assert the stream identifier, unchanged payload, and unset end-of-stream flag.
#[test]
fn read_data_no_padding() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0, 0, 5, 0, 0, 0, 0, 0, 1]);
    buf.extend_from_slice(b"hello");

    let data = decode_frame!(Data, buf);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b"hello"[..]);
    assert!(!data.is_end_stream());
}

/// STATEMENT: A `Data` frame may carry an empty payload without ending its stream.
///
/// HOW:
/// - Decode a zero-length `Data` frame for stream 1 with no flags.
/// - Assert the stream identifier, empty payload, and unset end-of-stream flag.
#[test]
fn read_data_empty_payload() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);

    let data = decode_frame!(Data, buf);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b""[..]);
    assert!(!data.is_end_stream());
}

/// STATEMENT: Decoding a `Data` frame preserves its end-of-stream flag alongside its payload.
///
/// HOW:
/// - Decode raw bytes for stream 1 containing `hello` with the end-of-stream flag set.
/// - Assert the stream identifier, payload, and set end-of-stream flag.
#[test]
fn read_data_end_stream() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0, 0, 5, 0, 1, 0, 0, 0, 1]);
    buf.extend_from_slice(b"hello");

    let data = decode_frame!(Data, buf);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b"hello"[..]);
    assert!(data.is_end_stream());
}

/// STATEMENT: Decoding padded `Data` excludes the padding length and trailing padding from the
/// payload.
///
/// HOW:
/// - Build a padded frame for stream 1 containing `helloworld` and five padding bytes.
/// - Assert the decoded payload contains only the data and the stream remains open.
#[test]
fn read_data_padding() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0, 0, 16, 0, 0x8, 0, 0, 0, 1]);
    buf.extend_from_slice(&[5]); // Pad length
    buf.extend_from_slice(b"helloworld"); // Data
    buf.extend_from_slice(b"\0\0\0\0\0"); // Padding

    let data = decode_frame!(Data, buf);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b"helloworld"[..]);
    assert!(!data.is_end_stream());
}

/// STATEMENT: The codec rejects an incoming push promise with
/// `FrameError::UnexpectedPushPromise`.
///
/// HOW:
/// - Build raw push-promise bytes for stream 1 promising stream 2 with an HPACK method field.
/// - Decode the frame and assert the exact rejection error.
#[test]
fn read_push_promise() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[
        0, 0, 0x5, 0x5, 0x4, 0, 0, 0, 0x1, // stream id
        0, 0, 0, 0x2,  // promised id
        0x82, // HPACK :method="GET"
    ]);

    decode_err!(buf, FrameError::UnexpectedPushPromise);
}

/// STATEMENT: A `Data` frame cannot use connection-level stream identifier zero.
///
/// HOW:
/// - Build a frame carrying `hello` on stream zero.
/// - Assert decoding returns `FrameError::InvalidStreamId`.
#[test]
fn read_data_stream_id_zero() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0, 0, 5, 0, 0, 0, 0, 0, 0]);
    buf.extend_from_slice(b"hello"); // Data

    decode_err!(buf, FrameError::InvalidStreamId);
}

// ===== HEADERS =====

/// STATEMENT: A client reconstructs a large response header block carried across continuation
/// frames.
///
/// HOW:
/// - Connect an in-memory client and server, then send a `GET` request for `/index.html`.
/// - Have the server send a large header set with status `StatusCode::OK` and verify the
///   request path and end-of-stream flag.
/// - Assert the client receives the expected status and the complete header map.
#[ntex::test]
async fn read_continuation_frames() {
    let (cli, srv) = IoTest::create();

    let large = build_large_headers();
    let frame = large
        .iter()
        .fold(frames::headers(1).response(200), |frame, &(name, ref value)| {
            frame.field(name, &value[..])
        })
        .eos();

    let srv_rx = support::start_server(srv);
    let client = support::start_client(cli);

    let srv_fut = async move {
        let msg = srv_rx.recv().await.unwrap();

        let hdrs = frame.into_fields();
        msg.stream().send_response(StatusCode::OK, hdrs, true).unwrap();

        let (pseudo, _hdrs, eof) = get_headers!(msg);
        assert_eq!(pseudo.path, Some("/index.html".into()));
        assert!(eof);
    };

    let client_fut = async move {
        let (_snd, rcv) = client
            .send(Method::GET, "/index.html".into(), HeaderMap::new(), true)
            .await
            .expect("response");

        let msg = rcv.recv().await.unwrap();
        let (pseudo, hdrs, _eof) = get_headers!(msg);

        assert_eq!(pseudo.status, Some(StatusCode::OK));
        let expected = large
            .iter()
            .fold(HeaderMap::new(), |mut map, &(name, ref value)| {
                map.append(HeaderName::try_from(name).unwrap(), value.parse().unwrap());
                map
            });
        assert_eq!(hdrs, expected);
    };

    join(srv_fut, client_fut).await;
}

/// STATEMENT: The configured receive frame-size limit rejects a frame larger than the limit.
///
/// HOW:
/// - Buffer a five-byte `Data` frame followed by a 16,385-byte frame and decode `hello` first.
/// - Set a codec receive limit of 16,384, assert the accessor reports it, and verify decoding
///   the remaining frame reports "Frame size exceeded".
#[test]
fn update_max_frame_len_at_rest() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0, 0, 5, 0, 0, 0, 0, 0, 1]);
    buf.extend_from_slice(b"hello");
    buf.extend_from_slice(&[0, 64, 1, 0, 0, 0, 0, 0, 1]);
    buf.extend_from_slice(&vec![0; 16_385]);

    assert_eq!(decode_frame!(Data, buf).payload(), &b"hello"[..]);

    let codec = Codec::default();
    codec.set_recv_frame_size(16_384);
    assert_eq!(codec.recv_frame_size(), 16_384);
    assert_eq!(
        codec.decode(&mut buf).unwrap_err().to_string(),
        "Frame size exceeded"
    );
}

/// STATEMENT: Decoding `GoAway` preserves its reason, last stream identifier, and debug data.
///
/// HOW:
/// - Build raw `GoAway` bytes with last stream 1, error code 11, and `too_many_pings` debug
///   data.
/// - Assert the decoded reason is `Reason::ENHANCE_YOUR_CALM` and the identifier and debug
///   bytes match.
#[test]
fn read_goaway_with_debug_data() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[
        // head
        0, 0, 22, 7, 0, 0, 0, 0, 0, // last_stream_id
        0, 0, 0, 1, // error_code
        0, 0, 0, 11,
    ]);
    // debug_data
    buf.extend_from_slice(b"too_many_pings");

    let data = decode_frame!(GoAway, buf);
    assert_eq!(data.reason(), frame::Reason::ENHANCE_YOUR_CALM);
    assert_eq!(data.last_stream_id(), 1);
    assert_eq!(&**data.data(), b"too_many_pings");
}

/// STATEMENT: A large request header set survives encoding and decoding across continuation
/// frames.
///
/// HOW:
/// - Send a `GET` request for `/index.html` with a large header map over paired in-memory I/O.
/// - Assert the server receives the full map, request path, and end-of-stream flag before
///   sending a response.
/// - Assert the client receives `StatusCode::NO_CONTENT` with its end-of-stream flag set.
#[ntex::test]
async fn write_continuation_frames() {
    let (cli, srv) = IoTest::create();
    let large = build_large_headers();
    let expected = large
        .iter()
        .fold(HeaderMap::new(), |mut map, &(name, ref value)| {
            map.append(HeaderName::try_from(name).unwrap(), value.parse().unwrap());
            map
        });

    let srv_rx = support::start_server(srv);
    let client = support::start_client(cli);

    let server = async move {
        let msg = srv_rx.recv().await.unwrap();
        msg.stream()
            .send_response(StatusCode::NO_CONTENT, HeaderMap::new(), true)
            .unwrap();
        let (pseudo, headers, eof) = get_headers!(msg);
        assert_eq!(pseudo.path, Some("/index.html".into()));
        assert_eq!(headers, expected);
        assert!(eof);
    };

    let client = async move {
        let headers = large
            .iter()
            .fold(HeaderMap::new(), |mut map, &(name, ref value)| {
                map.append(HeaderName::try_from(name).unwrap(), value.parse().unwrap());
                map
            });
        let (_snd, rcv) = client
            .send(Method::GET, "/index.html".into(), headers, true)
            .await
            .unwrap();
        let msg = rcv.recv().await.unwrap();
        let (pseudo, _, eof) = get_headers!(msg);
        assert_eq!(pseudo.status, Some(StatusCode::NO_CONTENT));
        assert!(eof);
    };

    join(server, client).await;
}

fn frame_payload(buf: &[u8], kind: u8, stream_id: u32) -> &[u8] {
    let mut pos = 0;
    while pos + 9 <= buf.len() {
        let len =
            (usize::from(buf[pos]) << 16) | (usize::from(buf[pos + 1]) << 8) | usize::from(buf[pos + 2]);
        let end = pos + 9 + len;
        assert!(end <= buf.len(), "incomplete frame");
        let id = u32::from_be_bytes(buf[pos + 5..pos + 9].try_into().unwrap()) & 0x7fff_ffff;
        if buf[pos + 3] == kind && id == stream_id {
            return &buf[pos + 9..end];
        }
        pos = end;
    }
    panic!("frame type {kind} for stream {stream_id} not found");
}

/// STATEMENT: A client honors a peer setting that disables the HPACK dynamic table.
///
/// HOW:
/// - Send `SETTINGS_HEADER_TABLE_SIZE = 0` to an in-memory client and allow it to process the
///   setting.
/// - Send a request and inspect the stream-1 header payload for the leading zero-size update
///   byte `0x20`.
#[ntex::test]
async fn client_settings_header_table_size() {
    let (cli, srv) = IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let client = support::start_client(cli);

    // SETTINGS_HEADER_TABLE_SIZE = 0
    srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
    sleep(Millis(50)).await;
    let _ = srv.read_any();

    let _ = client
        .send(Method::GET, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    sleep(Millis(50)).await;

    let bytes = srv.read_any();
    let payload = frame_payload(&bytes, 1, 1);
    assert_eq!(payload.first(), Some(&0x20));
}

/// STATEMENT: A server honors a peer setting that disables the HPACK dynamic table.
///
/// HOW:
/// - Send the connection preface and `SETTINGS_HEADER_TABLE_SIZE = 0` to an in-memory server.
/// - Acknowledge its settings, send a request, and respond with an `a: b` header.
/// - Inspect the response header payload for the leading zero-size update byte `0x20`.
#[ntex::test]
async fn server_settings_header_table_size() {
    let (srv, cli) = IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    cli.remote_buffer_cap(1024 * 1024);

    let messages = support::start_server(srv);
    cli.write(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    // SETTINGS_HEADER_TABLE_SIZE = 0
    cli.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
    sleep(Millis(50)).await;
    let _ = cli.read_any();

    // acknowledge the server settings and send GET /
    cli.write(frames::SETTINGS_ACK);
    cli.write([
        0, 0, 0x10, 1, 5, 0, 0, 0, 1, 0x82, 0x86, 0x41, 0x8B, 0x9D, 0x29, 0xAC, 0x4B, 0x8F, 0xA8, 0xE9,
        0x19, 0x97, 0x21, 0xE9, 0x84,
    ]);

    let msg = messages.recv().await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("a".parse().unwrap(), "b".parse().unwrap());
    msg.stream().send_response(StatusCode::OK, headers, true).unwrap();
    sleep(Millis(50)).await;

    let bytes = cli.read_any();
    let payload = frame_payload(&bytes, 1, 1);
    assert_eq!(payload.first(), Some(&0x20));
}

// ===== SETTINGS =====

/// STATEMENT: A zero send frame-size limit is rejected with a bounds-check panic.
///
/// HOW:
/// - Call `Codec::set_send_frame_size` with zero.
/// - Require a panic stating that frame size must be between 16384 and 16777215.
#[test]
#[should_panic(expected = "frame size must be between 16384 and 16777215")]
fn send_frame_size_zero_is_rejected() {
    Codec::default().set_send_frame_size(0);
}

/// STATEMENT: A send frame-size limit below the protocol minimum is rejected with a bounds-
/// check panic.
///
/// HOW:
/// - Call `Codec::set_send_frame_size` with one less than `DEFAULT_MAX_FRAME_SIZE`.
/// - Require a panic stating that frame size must be between 16384 and 16777215.
#[test]
#[should_panic(expected = "frame size must be between 16384 and 16777215")]
fn send_frame_size_below_minimum_is_rejected() {
    Codec::default().set_send_frame_size(frame::DEFAULT_MAX_FRAME_SIZE as usize - 1);
}

/// STATEMENT: Both endpoints of the permitted send frame-size range are accepted.
///
/// HOW:
/// - Set the send frame size to `DEFAULT_MAX_FRAME_SIZE`, then to `MAX_MAX_FRAME_SIZE`.
/// - Assert `Codec::send_frame_size` returns each configured endpoint.
#[test]
fn send_frame_size_bounds_are_accepted() {
    let codec = Codec::default();
    codec.set_send_frame_size(frame::DEFAULT_MAX_FRAME_SIZE as usize);
    assert_eq!(codec.send_frame_size(), frame::DEFAULT_MAX_FRAME_SIZE);
    codec.set_send_frame_size(frame::MAX_MAX_FRAME_SIZE as usize);
    assert_eq!(codec.send_frame_size(), frame::MAX_MAX_FRAME_SIZE);
}

// ===== HEADERS limits =====

const END_HEADERS: u8 = 0x4;

fn raw_frame(kind: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut buf = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    buf.extend_from_slice(&stream_id.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// `:method: GET`, `:scheme: http`, `:path: /`
const REQUEST_PSEUDO: [u8; 3] = [0x82, 0x86, 0x84];

/// Literal header field without indexing, new name.
fn literal_field(name: &[u8], value: &[u8]) -> Vec<u8> {
    assert!(name.len() < 127 && value.len() < 127);
    let mut buf = vec![0x00, name.len() as u8];
    buf.extend_from_slice(name);
    buf.push(value.len() as u8);
    buf.extend_from_slice(value);
    buf
}

#[track_caller]
fn assert_invalid(
    res: Result<Option<frame::Frame>, FrameError>,
    kind: frame::Kind,
    id: u32,
    error: FrameError,
) {
    match res {
        Ok(Some(frame::Frame::Invalid(frm))) => {
            assert_eq!(frm.kind(), kind);
            assert_eq!(frm.stream_id(), id);
            assert_eq!(frm.error(), error);
        }
        res => panic!("expected invalid frame; actual={res:?}"),
    }
}

/// STATEMENT: Repeated header fields each count toward the configured header-count limit.
///
/// HOW:
/// - Set `Codec::set_max_headers` to three and decode a request with three `x: a` fields.
/// - Assert all three values survive, then decode four fields and require an invalid stream-1
///   `Headers` frame with `FrameError::TooManyHeaders`.
#[test]
fn repeated_header_names_count_toward_max_headers() {
    let block = |n| {
        let mut block = REQUEST_PSEUDO.to_vec();
        for _ in 0..n {
            block.extend(literal_field(b"x", b"a"));
        }
        BytesMut::from(&raw_frame(1, END_HEADERS, 1, &block)[..])
    };

    let codec = Codec::default();
    codec.set_max_headers(3);
    match codec.decode(&mut block(3)) {
        Ok(Some(frame::Frame::Headers(hdrs))) => {
            assert_eq!(hdrs.fields().get_all("x").count(), 3);
        }
        res => panic!("unexpected result: {res:?}"),
    }
    assert_invalid(
        codec.decode(&mut block(4)),
        frame::Kind::Headers,
        1,
        FrameError::TooManyHeaders(1.into()),
    );
}

/// STATEMENT: An oversized decoded header list is rejected while preserving HPACK dynamic-table
/// updates.
///
/// HOW:
/// - Insert a 4000-byte value into the dynamic table and reference it twenty times, expanding
///   roughly 4 KB of compressed data to 84 KB.
/// - Assert the first stream yields `FrameError::TooManyHeaders` in an invalid `Headers` frame.
/// - Decode a later stream referencing that entry and assert its `x` value still contains all
///   4000 bytes.
#[test]
fn decoded_header_list_size_is_limited() {
    // add a 4000 byte value to the dynamic table, then reference it repeatedly;
    // the block is ~4KB compressed but ~84KB decoded
    let mut block = REQUEST_PSEUDO.to_vec();
    block.extend_from_slice(&[0x40, 1, b'x', 0x7f, 0xa1, 0x1e]);
    block.extend_from_slice(&[b'a'; 4000]);
    block.extend_from_slice(&[0xbe; 20]);

    let codec = Codec::default();
    let mut buf = BytesMut::from(&raw_frame(1, END_HEADERS, 1, &block)[..]);
    assert_invalid(
        codec.decode(&mut buf),
        frame::Kind::Headers,
        1,
        FrameError::TooManyHeaders(1.into()),
    );

    // the rejected block still updated the hpack state
    let mut block = REQUEST_PSEUDO.to_vec();
    block.push(0xbe);
    let mut buf = BytesMut::from(&raw_frame(1, END_HEADERS, 3, &block)[..]);
    match codec.decode(&mut buf) {
        Ok(Some(frame::Frame::Headers(hdrs))) => {
            assert_eq!(hdrs.fields().get("x").unwrap().as_bytes(), &[b'a'; 4000][..]);
        }
        res => panic!("unexpected result: {res:?}"),
    }
}

/// STATEMENT: Disabling the continuation-count limit allows a header block split into single-
/// byte frames.
///
/// HOW:
/// - Build a request with 90 headers, placing its first byte in `Headers` and every remaining
///   byte in a separate `Continuation` frame.
/// - Set `Codec::set_max_header_continuations` to zero and decode the entire sequence.
/// - Assert the method is `Method::GET`, all 90 headers are present, `x89` is `a`, and the
///   input buffer is empty.
#[test]
fn many_small_continuation_frames() {
    let mut block = REQUEST_PSEUDO.to_vec();
    for i in 0..90 {
        block.extend(literal_field(format!("x{i}").as_bytes(), b"a"));
    }

    // HEADERS with the first byte, then a CONTINUATION per byte
    let mut buf = BytesMut::from(&raw_frame(1, 0, 1, &block[..1])[..]);
    for (idx, b) in block[1..].iter().enumerate() {
        let flags = if idx == block.len() - 2 { END_HEADERS } else { 0 };
        buf.extend_from_slice(&raw_frame(9, flags, 1, &[*b]));
    }

    let codec = Codec::default();
    codec.set_max_header_continuations(0);
    match codec.decode(&mut buf) {
        Ok(Some(frame::Frame::Headers(hdrs))) => {
            assert_eq!(hdrs.pseudo().method, Some(Method::GET));
            assert_eq!(hdrs.fields().len(), 90);
            assert_eq!(hdrs.fields().get("x89").unwrap(), "a");
        }
        res => panic!("unexpected result: {res:?}"),
    }
    assert!(buf.is_empty());
}

/// STATEMENT: Unknown header-frame flag bits do not appear in the decoded flags.
///
/// HOW:
/// - Decode request pseudo-headers with flag bits `0xc0` alongside `END_HEADERS`.
/// - Assert the decoded header debug output reports only the `END_HEADERS` flag.
#[test]
fn headers_unknown_flags_are_ignored() {
    let mut buf = BytesMut::from(&raw_frame(1, 0xc0 | END_HEADERS, 1, &REQUEST_PSEUDO)[..]);
    match Codec::default().decode(&mut buf) {
        Ok(Some(frame::Frame::Headers(hdrs))) => {
            assert!(format!("{hdrs:?}").contains("flags: (0x4: END_HEADERS)"));
        }
        res => panic!("unexpected result: {res:?}"),
    }
}

/// STATEMENT: The `te` header accepts case-insensitive `trailers` values but rejects other
/// values.
///
/// HOW:
/// - Decode requests containing lowercase, title-case, and uppercase `trailers` values and
///   assert each original value is preserved.
/// - Decode `te: gzip` and require an invalid stream-1 `Headers` frame with
///   `FrameError::MalformedMessage`.
#[test]
fn te_trailers_is_case_insensitive() {
    let block = |value: &[u8]| {
        let mut block = REQUEST_PSEUDO.to_vec();
        block.extend(literal_field(b"te", value));
        BytesMut::from(&raw_frame(1, END_HEADERS, 1, &block)[..])
    };

    for value in [&b"trailers"[..], b"Trailers", b"TRAILERS"] {
        match Codec::default().decode(&mut block(value)) {
            Ok(Some(frame::Frame::Headers(hdrs))) => {
                assert_eq!(hdrs.fields().get("te").unwrap().as_bytes(), value);
            }
            res => panic!("unexpected result: {res:?}"),
        }
    }
    assert_invalid(
        Codec::default().decode(&mut block(b"gzip")),
        frame::Kind::Headers,
        1,
        FrameError::MalformedMessage,
    );
}

/// STATEMENT: Completing a header block with a continuation preserves the original end-of-
/// stream flag.
///
/// HOW:
/// - Split request pseudo-headers between a `Headers` frame with the end-of-stream flag and a
///   final `Continuation` frame.
/// - Assert the result belongs to stream 1, has both completion flags set, and retains the `/`
///   path.
#[test]
fn continuation_frames_keep_headers_flags() {
    let block = REQUEST_PSEUDO;
    // HEADERS with END_STREAM, the block ends in a CONTINUATION
    let mut buf = BytesMut::from(&raw_frame(1, 0x1, 1, &block[..1])[..]);
    buf.extend_from_slice(&raw_frame(9, END_HEADERS, 1, &block[1..]));

    match Codec::default().decode(&mut buf) {
        Ok(Some(frame::Frame::Headers(hdrs))) => {
            assert_eq!(hdrs.stream_id(), 1);
            assert!(hdrs.is_end_stream());
            assert!(hdrs.is_end_headers());
            assert_eq!(hdrs.pseudo().path, Some("/".into()));
        }
        res => panic!("unexpected result: {res:?}"),
    }
}

/// STATEMENT: Stream-level header and priority errors leave the HPACK dynamic table
/// synchronized.
///
/// HOW:
/// - Insert `x: a` while sending a forbidden `connection` header, then repeat the malformed
///   request across continuation frames using that dynamic entry.
/// - Assert both header blocks are invalid, and self-dependent `Headers` and `Priority` frames
///   report `FrameError::InvalidDependencyId`.
/// - Decode a valid later request referencing the same entry and assert its `x` value is `a`.
#[test]
fn stream_level_header_errors_keep_hpack_state() {
    // `connection` header is not allowed, the block also adds `x: a` to the
    // dynamic table
    let mut block = REQUEST_PSEUDO.to_vec();
    block.extend_from_slice(&[0x40, 1, b'x', 1, b'a']);
    block.extend(literal_field(b"connection", b"close"));

    let codec = Codec::default();
    let mut buf = BytesMut::from(&raw_frame(1, END_HEADERS, 1, &block)[..]);
    assert_invalid(
        codec.decode(&mut buf),
        frame::Kind::Headers,
        1,
        FrameError::MalformedMessage,
    );

    // same block split into CONTINUATION frames, referencing the dynamic table
    let mut block = REQUEST_PSEUDO.to_vec();
    block.push(0xbe);
    block.extend(literal_field(b"connection", b"close"));
    let mut buf = BytesMut::from(&raw_frame(1, 0, 3, &block[..2])[..]);
    buf.extend_from_slice(&raw_frame(9, END_HEADERS, 3, &block[2..]));
    assert_invalid(
        codec.decode(&mut buf),
        frame::Kind::Headers,
        3,
        FrameError::MalformedMessage,
    );

    // HEADERS depending on itself, with a valid block
    let mut payload = vec![0, 0, 0, 5, 0];
    payload.extend_from_slice(&REQUEST_PSEUDO);
    payload.push(0xbe);
    let mut buf = BytesMut::from(&raw_frame(1, END_HEADERS | 0x20, 5, &payload)[..]);
    assert_invalid(
        codec.decode(&mut buf),
        frame::Kind::Headers,
        5,
        FrameError::InvalidDependencyId,
    );

    // PRIORITY depending on itself
    let mut buf = BytesMut::from(&raw_frame(2, 0, 7, &[0, 0, 0, 7, 0])[..]);
    assert_invalid(
        codec.decode(&mut buf),
        frame::Kind::Priority,
        7,
        FrameError::InvalidDependencyId,
    );

    // the hpack state is still in sync
    let mut block = REQUEST_PSEUDO.to_vec();
    block.push(0xbe);
    let mut buf = BytesMut::from(&raw_frame(1, END_HEADERS, 9, &block)[..]);
    match codec.decode(&mut buf) {
        Ok(Some(frame::Frame::Headers(hdrs))) => {
            assert_eq!(hdrs.fields().get("x").unwrap(), "a");
        }
        res => panic!("unexpected result: {res:?}"),
    }
}

/// STATEMENT: The send-side HPACK table size remains capped at 4096 even when the peer allows
/// more.
///
/// HOW:
/// - Set the table size to `1 << 30`, encode a response, and assert only the indexed status
///   byte is emitted.
/// - Shrink the table to 100 and request a large increase, then assert the next block emits
///   size updates for 100 and 4096 before the status byte.
#[test]
fn send_header_table_size_is_capped() {
    fn encode(codec: &Codec) -> ntex_bytes::Bytes {
        let hdrs = frame::Headers::new(
            1.into(),
            frame::PseudoHeaders::response(StatusCode::OK),
            HeaderMap::new(),
            true,
        );
        let mut buf = ntex_bytes::BytePages::default();
        codec.encode(hdrs.into(), &mut buf).unwrap();
        buf.freeze().slice(9..)
    }

    // larger than default, the table stays at 4096 without a size update
    let codec = Codec::default();
    codec.set_send_header_table_size(1 << 30);
    assert_eq!(&encode(&codec)[..], &[0x88]);

    // shrink, then grow back up to 4096 only
    codec.set_send_header_table_size(100);
    codec.set_send_header_table_size(1 << 30);
    assert_eq!(&encode(&codec)[..], &[0x3f, 0x45, 0x3f, 0xe1, 0x1f, 0x88]);
}
