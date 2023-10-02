#![allow(dead_code, unused_variables)]
use std::convert::TryFrom;

mod support;

use ntex_bytes::BytesMut;
use ntex_codec::Decoder;
use ntex_h2::{frame, frame::FrameError, Codec};
use ntex_http::{HeaderMap, HeaderName, Method, StatusCode};
use ntex_io::testing::IoTest;
use ntex_util::future::join;

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

#[test]
fn read_data_empty_payload() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);

    let data = decode_frame!(Data, buf);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b""[..]);
    assert!(!data.is_end_stream());
}

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

#[test]
fn read_data_stream_id_zero() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0, 0, 5, 0, 0, 0, 0, 0, 0]);
    buf.extend_from_slice(b"hello"); // Data

    decode_err!(buf, FrameError::InvalidStreamId);
}

// ===== HEADERS =====

#[ntex::test]
async fn read_continuation_frames() {
    let _ = env_logger::try_init();

    let (cli, srv) = IoTest::create();

    let large = build_large_headers();
    let frame = large
        .iter()
        .fold(
            frames::headers(1).response(200),
            |frame, &(name, ref value)| frame.field(name, &value[..]),
        )
        .eos();

    let srv_rx = support::start_server(srv);
    let client = support::start_client(cli);

    let srv_fut = async move {
        let msg = srv_rx.recv().await.unwrap();

        let hdrs = frame.into_fields();
        msg.stream()
            .send_response(StatusCode::OK, hdrs, true)
            .unwrap();

        let (pseudo, hdrs, eof) = get_headers!(msg);
        assert_eq!(pseudo.path, Some("/index.html".into()));
        assert!(eof);
    };

    let client_fut = async move {
        let (snd, rcv) = client
            .send(Method::GET, "/index.html".into(), HeaderMap::new(), true)
            .await
            .expect("response");

        let msg = rcv.recv().await.unwrap();
        let (pseudo, hdrs, eof) = get_headers!(msg);

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

// #[tokio::test]
// async fn write_continuation_frames() {
//     // An invalid dependency ID results in a stream level error. The hpack
//     // payload should still be decoded.
//     h2_support::trace_init!();
//     let (io, mut srv) = mock::new();

//     let large = build_large_headers();

//     // Build the large request frame
//     let frame = large.iter().fold(
//         frames::headers(1).request("GET", "https://http2.akamai.com/"),
//         |frame, &(name, ref value)| frame.field(name, &value[..]),
//     );

//     let srv = async move {
//         let settings = srv.assert_client_handshake().await;
//         assert_default_settings!(settings);
//         srv.recv_frame(frame.eos()).await;
//         srv.send_frame(frames::headers(1).response(204).eos()).await;
//     };

//     let client = async move {
//         let (mut client, mut conn) = client::handshake(io).await.expect("handshake");

//         let mut request = Request::builder();
//         request = request.uri("https://http2.akamai.com/");

//         for &(name, ref value) in &large {
//             request = request.header(name, &value[..]);
//         }

//         let request = request.body(()).unwrap();

//         let req = async {
//             let res = client
//                 .send_request(request, true)
//                 .expect("send_request1")
//                 .0
//                 .await;
//             let response = res.unwrap();
//             assert_eq!(response.status(), StatusCode::NO_CONTENT);
//         };

//         conn.drive(req).await;
//         conn.await.unwrap();
//     };

//     join(srv, client).await;
// }

// #[tokio::test]
// async fn client_settings_header_table_size() {
//     // A server sets the SETTINGS_HEADER_TABLE_SIZE to 0, test that the
//     // client doesn't send indexed headers.
//     h2_support::trace_init!();

//     let io = mock_io::Builder::new()
//         // Read SETTINGS_HEADER_TABLE_SIZE = 0
//         .handshake_read_settings(&[
//             0, 0, 6, // len
//             4, // type
//             0, // flags
//             0, 0, 0, 0, // stream id
//             0, 0x1, // id = SETTINGS_HEADER_TABLE_SIZE
//             0, 0, 0, 0, // value = 0
//         ])
//         // Write GET / (1st)
//         .write(&[
//             0, 0, 0x10, 1, 5, 0, 0, 0, 1, 0x82, 0x87, 0x41, 0x8B, 0x9D, 0x29, 0xAC, 0x4B, 0x8F,
//             0xA8, 0xE9, 0x19, 0x97, 0x21, 0xE9, 0x84,
//         ])
//         .write(frames::SETTINGS_ACK)
//         // Read response
//         .read(&[0, 0, 1, 1, 5, 0, 0, 0, 1, 137])
//         // Write GET / (2nd, doesn't use indexed headers)
//         // - Sends 0x20 about size change
//         // - Sends :authority as literal instead of indexed
//         .write(&[
//             0, 0, 0x11, 1, 5, 0, 0, 0, 3, 0x20, 0x82, 0x87, 0x1, 0x8B, 0x9D, 0x29, 0xAC, 0x4B,
//             0x8F, 0xA8, 0xE9, 0x19, 0x97, 0x21, 0xE9, 0x84,
//         ])
//         .read(&[0, 0, 1, 1, 5, 0, 0, 0, 3, 137])
//         .build();

//     let (mut client, mut conn) = client::handshake(io).await.expect("handshake");

//     let req1 = client.get("https://http2.akamai.com");
//     conn.drive(req1).await.expect("req1");

//     let req2 = client.get("https://http2.akamai.com");
//     conn.drive(req2).await.expect("req1");
// }

// #[tokio::test]
// async fn server_settings_header_table_size() {
//     // A client sets the SETTINGS_HEADER_TABLE_SIZE to 0, test that the
//     // server doesn't send indexed headers.
//     h2_support::trace_init!();

//     let io = mock_io::Builder::new()
//         .read(MAGIC_PREFACE)
//         // Read SETTINGS_HEADER_TABLE_SIZE = 0
//         .read(&[
//             0, 0, 6, // len
//             4, // type
//             0, // flags
//             0, 0, 0, 0, // stream id
//             0, 0x1, // id = SETTINGS_HEADER_TABLE_SIZE
//             0, 0, 0, 0, // value = 0
//         ])
//         .write(frames::SETTINGS)
//         .write(frames::SETTINGS_ACK)
//         .read(frames::SETTINGS_ACK)
//         // Write GET /
//         .read(&[
//             0, 0, 0x10, 1, 5, 0, 0, 0, 1, 0x82, 0x87, 0x41, 0x8B, 0x9D, 0x29, 0xAC, 0x4B, 0x8F,
//             0xA8, 0xE9, 0x19, 0x97, 0x21, 0xE9, 0x84,
//         ])
//         // Read response
//         //.write(&[0, 0, 6, 1, 5, 0, 0, 0, 1, 136, 64, 129, 31, 129, 143])
//         .write(&[0, 0, 7, 1, 5, 0, 0, 0, 1, 32, 136, 0, 129, 31, 129, 143])
//         .build();

//     let mut srv = server::handshake(io).await.expect("handshake");

//     let (_req, mut stream) = srv.accept().await.unwrap().unwrap();

//     let rsp = http::Response::builder()
//         .status(200)
//         .header("a", "b")
//         .body(())
//         .unwrap();
//     stream.send_response(rsp, true).unwrap();

//     assert!(srv.accept().await.is_none());
// }
