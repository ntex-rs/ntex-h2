//! Stream state and validation over an in-memory transport.
use std::{cell::RefCell, future::poll_fn, panic, pin::Pin};

use ntex::http::header::{self, HeaderValue};
use ntex::http::{HeaderMap, Method, StatusCode};
use ntex::io::{Io, testing::IoTest};
use ntex::service::cfg::SharedCfg;
use ntex::time::{Millis, sleep, timeout};
use ntex::util::{BytePages, Bytes, BytesMut};
use ntex_codec::{Decoder, Encoder};
use ntex_h2::client::{RecvStream, SendStream, SimpleClient};
use ntex_h2::frame::{self, Frame, Reason, StreamId};
use ntex_h2::{Codec, Message, MessageKind, OperationError, StreamEof, StreamError, server};

const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// A peer that writes raw frames to a client and decodes its output.
struct RawServer {
    io: IoTest,
    codec: Codec,
    buf: RefCell<BytesMut>,
}

impl RawServer {
    fn send(&self, frm: impl Into<Frame>) {
        let mut buf = BytePages::default();
        self.codec.encode(frm.into(), &mut buf).unwrap();
        self.io.write(buf.freeze());
    }

    fn headers(&self, id: u32, pseudo: frame::PseudoHeaders, hdrs: HeaderMap, eos: bool) {
        self.send(frame::Headers::new(id.into(), pseudo, hdrs, eos));
    }

    fn response(&self, id: u32, hdrs: HeaderMap, eos: bool) {
        self.headers(id, frame::PseudoHeaders::response(StatusCode::OK), hdrs, eos);
    }

    fn data(&self, id: u32, data: &'static str, eos: bool) {
        let mut frm = frame::Data::new(id.into(), Bytes::from_static(data.as_bytes()));
        if eos {
            frm.set_end_stream();
        }
        self.send(frm);
    }

    /// Frames sent by the client so far.
    async fn frames(&self) -> Vec<Frame> {
        sleep(Millis(50)).await;
        let mut buf = self.buf.borrow_mut();
        buf.extend_from_slice(&self.io.read_any());
        let mut frames = Vec::new();
        while let Some(frm) = self.codec.decode(&mut buf).unwrap() {
            frames.push(frm);
        }
        frames
    }

    async fn reset(&self, id: u32) -> Reason {
        self.frames()
            .await
            .into_iter()
            .find_map(|frm| match frm {
                Frame::Reset(rst) if rst.stream_id() == id => Some(rst.reason()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("stream {id} is not reset"))
    }
}

async fn client() -> (SimpleClient, RawServer) {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);
    let client = SimpleClient::new(
        Io::new(cli, SharedCfg::new("CLI").build()),
        false,
        "localhost".into(),
    );
    let srv = RawServer {
        io: srv,
        codec: Codec::default(),
        buf: RefCell::new(BytesMut::new()),
    };
    srv.send(frame::Settings::default());
    srv.send(frame::Settings::ack());
    sleep(Millis(20)).await;

    let preface = srv.io.read_any();
    assert_eq!(&preface[..24], PREFACE);
    srv.buf.borrow_mut().extend_from_slice(&preface[24..]);
    (client, srv)
}

async fn request(client: &SimpleClient, method: Method, eof: bool) -> (SendStream, RecvStream) {
    client
        .send(method, "/".into(), HeaderMap::new(), eof)
        .await
        .unwrap()
}

async fn next(rcv: &RecvStream) -> MessageKind {
    timeout(Millis(1_000), rcv.recv())
        .await
        .expect("no message")
        .expect("stream is closed")
        .kind
}

/// Waits for the stream error, skipping regular messages.
async fn stream_error(rcv: &RecvStream) -> StreamError {
    loop {
        match next(rcv).await {
            MessageKind::Eof(StreamEof::Error(err)) => return *err,
            MessageKind::Headers { .. } | MessageKind::Data(..) => {}
            kind => panic!("unexpected message: {kind:?}"),
        }
    }
}

fn content_length(val: &'static str) -> HeaderMap {
    let mut hdrs = HeaderMap::new();
    hdrs.insert(header::CONTENT_LENGTH, HeaderValue::from_static(val));
    hdrs
}

/// STATEMENT: Responses reject request pseudo-headers without closing the connection.
///
/// HOW:
/// - For each of method, scheme, authority, path, and protocol, inject that pseudo-header into
///   a response on a fresh client.
/// - Assert `StreamError::UnexpectedPseudo` names the header, stream 1 receives
///   `Reason::PROTOCOL_ERROR`, and the client stays open.
#[ntex::test]
async fn response_with_request_pseudo_headers() {
    type SetPseudo = fn(&mut frame::PseudoHeaders);
    let cases: [(&str, SetPseudo); 5] = [
        ("method", |p| p.method = Some(Method::GET)),
        ("scheme", |p| p.scheme = Some("http".into())),
        ("authority", |p| p.authority = Some("localhost".into())),
        ("path", |p| p.path = Some("/".into())),
        ("protocol", |p| {
            p.protocol = Some(frame::Protocol::from_static("websocket"))
        }),
    ];

    for (name, set) in cases {
        let (client, srv) = client().await;
        let (_snd, rcv) = request(&client, Method::GET, true).await;

        let mut pseudo = frame::PseudoHeaders::response(StatusCode::OK);
        set(&mut pseudo);
        srv.headers(1, pseudo, HeaderMap::new(), true);

        assert_eq!(stream_error(&rcv).await, StreamError::UnexpectedPseudo(name));
        assert_eq!(srv.reset(1).await, Reason::PROTOCOL_ERROR);
        assert!(!client.is_closed());
    }
}

/// STATEMENT: Response bodies are validated against `content-length` while valid lengths
/// preserve the connection.
///
/// HOW:
/// - Send nonnumeric and overlong lengths and assert `StreamError::InvalidContentLength`, plus
///   a protocol reset for the nonnumeric case.
/// - Send excess data, early end-of-stream, and early trailers; assert
///   `StreamError::WrongPayloadLength` for each.
/// - Send exactly three bytes over two data frames and assert headers, body data, final data,
///   and an open client connection.
#[ntex::test]
async fn response_content_length() {
    // not a number
    let (client, srv) = client().await;
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(1, content_length("abc"), false);
    assert_eq!(stream_error(&rcv).await, StreamError::InvalidContentLength);
    assert_eq!(srv.reset(1).await, Reason::PROTOCOL_ERROR);

    // too many digits
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(3, content_length("12345678901234567890"), false);
    assert_eq!(stream_error(&rcv).await, StreamError::InvalidContentLength);

    // more data than declared
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(5, content_length("3"), false);
    srv.data(5, "abcd", false);
    assert_eq!(stream_error(&rcv).await, StreamError::WrongPayloadLength);

    // less data than declared
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(7, content_length("3"), false);
    srv.data(7, "ab", true);
    assert_eq!(stream_error(&rcv).await, StreamError::WrongPayloadLength);

    // trailers before the declared length is received
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(9, content_length("3"), false);
    srv.data(9, "ab", false);
    srv.response(9, HeaderMap::new(), true);
    assert_eq!(stream_error(&rcv).await, StreamError::WrongPayloadLength);

    // exact length
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(11, content_length("3"), false);
    srv.data(11, "ab", false);
    srv.data(11, "c", true);
    assert!(matches!(
        next(&rcv).await,
        MessageKind::Headers { eof: false, .. }
    ));
    assert!(matches!(next(&rcv).await, MessageKind::Data(ref d, _) if d == "ab"));
    assert!(matches!(next(&rcv).await, MessageKind::Eof(StreamEof::Data(ref d, _)) if d == "c"));
    assert!(!client.is_closed());
}

/// STATEMENT: A response to `HEAD` must not contain body data.
///
/// HOW:
/// - Send a `HEAD` request, response headers declaring five bytes, and a five-byte final data
///   frame.
/// - Assert `StreamError::NonEmptyPayload` and a stream reset with `Reason::PROTOCOL_ERROR`.
#[ntex::test]
async fn head_response_with_body() {
    let (client, srv) = client().await;
    let (_snd, rcv) = request(&client, Method::HEAD, true).await;
    srv.response(1, content_length("5"), false);
    srv.data(1, "hello", true);
    assert_eq!(stream_error(&rcv).await, StreamError::NonEmptyPayload);
    assert_eq!(srv.reset(1).await, Reason::PROTOCOL_ERROR);
}

/// STATEMENT: Frames received in invalid stream states reset the stream without closing the
/// connection.
///
/// HOW:
/// - Send data before response headers and assert `StreamError::Idle` with a
///   `Reason::PROTOCOL_ERROR` reset.
/// - Finish response headers while keeping each request open, then send additional headers or
///   data and assert `Reason::STREAM_CLOSED` resets.
/// - Assert the client connection remains open.
#[ntex::test]
async fn frames_in_wrong_state() {
    let (client, srv) = client().await;

    // DATA before the response
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.data(1, "data", false);
    assert!(matches!(stream_error(&rcv).await, StreamError::Idle(_)));
    assert_eq!(srv.reset(1).await, Reason::PROTOCOL_ERROR);

    // HEADERS and DATA after the response is complete, the request is open
    for (id, frm) in [(3, "headers"), (5, "data")] {
        let (_snd, rcv) = request(&client, Method::POST, false).await;
        srv.response(id, HeaderMap::new(), true);
        assert!(matches!(next(&rcv).await, MessageKind::Headers { eof: true, .. }));
        if frm == "headers" {
            srv.response(id, HeaderMap::new(), true);
        } else {
            srv.data(id, "data", true);
        }
        assert_eq!(srv.reset(id).await, Reason::STREAM_CLOSED, "{frm}");
    }
    assert!(!client.is_closed());
}

/// STATEMENT: Response trailers must end the stream and must not contain pseudo-headers.
///
/// HOW:
/// - Send trailers containing the status pseudo-header and assert
///   `StreamError::UnexpectedPseudo("status")` with a protocol reset.
/// - Send trailers without end-of-stream and assert `StreamError::TrailersWithoutEos`.
/// - Send valid final trailers and assert headers, body data, and `StreamEof::Trailers`
///   containing `grpc-status: 0`, with the client still open.
#[ntex::test]
async fn response_trailers() {
    let (client, srv) = client().await;

    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(1, HeaderMap::new(), false);
    srv.data(1, "body", false);
    srv.response(1, HeaderMap::new(), true);
    assert_eq!(stream_error(&rcv).await, StreamError::UnexpectedPseudo("status"));
    assert_eq!(srv.reset(1).await, Reason::PROTOCOL_ERROR);

    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(3, HeaderMap::new(), false);
    srv.headers(3, frame::PseudoHeaders::default(), content_length("0"), false);
    assert_eq!(stream_error(&rcv).await, StreamError::TrailersWithoutEos);

    let (_snd, rcv) = request(&client, Method::GET, true).await;
    srv.response(5, HeaderMap::new(), false);
    srv.data(5, "body", false);
    let mut trailers = HeaderMap::new();
    trailers.insert(
        header::HeaderName::from_static("grpc-status"),
        HeaderValue::from_static("0"),
    );
    srv.headers(5, frame::PseudoHeaders::default(), trailers, true);
    assert!(matches!(next(&rcv).await, MessageKind::Headers { .. }));
    assert!(matches!(next(&rcv).await, MessageKind::Data(..)));
    match next(&rcv).await {
        MessageKind::Eof(StreamEof::Trailers(hdrs)) => assert_eq!(hdrs.get("grpc-status").unwrap(), "0"),
        kind => panic!("unexpected message: {kind:?}"),
    }
    assert!(!client.is_closed());
}

/// STATEMENT: Invalid stream-level `WindowUpdate` frames reset only the affected stream.
///
/// HOW:
/// - Send a zero increment and assert `StreamError::WindowZeroUpdateValue` with
///   `Reason::PROTOCOL_ERROR`.
/// - Overflow another stream window and assert `StreamError::WindowOverflowed` with
///   `Reason::FLOW_CONTROL_ERROR`.
/// - Assert the client connection stays open.
#[ntex::test]
async fn stream_window_update_errors() {
    let (client, srv) = client().await;

    let (_snd, rcv) = request(&client, Method::POST, false).await;
    srv.send(frame::WindowUpdate::new(1.into(), 0));
    assert_eq!(stream_error(&rcv).await, StreamError::WindowZeroUpdateValue);
    assert_eq!(srv.reset(1).await, Reason::PROTOCOL_ERROR);

    let (_snd, rcv) = request(&client, Method::POST, false).await;
    srv.send(frame::WindowUpdate::new(3.into(), 0x7fff_ffff));
    assert_eq!(stream_error(&rcv).await, StreamError::WindowOverflowed);
    assert_eq!(srv.reset(3).await, Reason::FLOW_CONTROL_ERROR);
    assert!(!client.is_closed());
}

/// STATEMENT: Paged payloads are split at frame and page boundaries without changing their
/// contents.
///
/// HOW:
/// - Send three 10,000-byte pages through `send_pages` with end-of-stream enabled.
/// - Decode data frames and assert stream ID 1, payload lengths at most 16,384 bytes, and no
///   data after end-of-stream.
/// - Assert an end-of-stream frame was seen and concatenated payloads equal the original pages.
#[ntex::test]
async fn send_pages_are_split() {
    let (client, srv) = client().await;
    let (snd, _rcv) = request(&client, Method::POST, false).await;

    let mut expected = Vec::new();
    let mut pages = BytePages::default();
    for ch in *b"abc" {
        let page = vec![ch; 10_000];
        expected.extend_from_slice(&page);
        pages.append(Bytes::from(page));
    }
    snd.send_pages(pages, true).await.unwrap();

    let mut received = Vec::new();
    let mut eos = false;
    for frm in srv.frames().await {
        if let Frame::Data(data) = frm {
            assert_eq!(data.stream_id(), 1);
            assert!(data.payload().len() <= 16_384);
            assert!(!eos);
            eos = data.is_end_stream();
            received.extend_from_slice(data.payload());
        }
    }
    assert!(eos);
    assert_eq!(received, expected);
}

/// STATEMENT: Client stream accessors, `Stream` polling, and reset notifications expose the
/// stream state.
///
/// HOW:
/// - Open a request and check matching send/receive IDs and tags, local nonfailed state, and
///   initial send capacity.
/// - Send response headers and final body data, then poll the receive stream for headers, EOF
///   data, and termination.
/// - Reset the stream from the peer, await `on_reset`, and assert reset and failure state.
#[ntex::test]
async fn client_stream_api() {
    let (client, srv) = client().await;
    let (snd, mut rcv) = request(&client, Method::POST, false).await;

    assert_eq!(snd.id(), StreamId::CLIENT);
    assert_eq!(rcv.id(), StreamId::CLIENT);
    assert_eq!(snd.tag(), "CLI");
    assert_eq!(rcv.tag(), "CLI");
    assert_eq!(snd.stream().id(), rcv.stream().id());
    assert!(!snd.stream().is_remote());
    assert!(!snd.stream().is_failed());
    assert_eq!(snd.available_send_capacity(), 65_535);

    srv.response(1, HeaderMap::new(), false);
    srv.data(1, "body", true);
    let msg = poll_fn(|cx| ntex::util::Stream::poll_next(Pin::new(&mut rcv), cx))
        .await
        .unwrap();
    assert!(matches!(msg.kind, MessageKind::Headers { eof: false, .. }));
    let msg = poll_fn(|cx| ntex::util::Stream::poll_next(Pin::new(&mut rcv), cx))
        .await
        .unwrap();
    assert!(matches!(msg.kind, MessageKind::Eof(StreamEof::Data(..))));
    assert!(
        poll_fn(|cx| ntex::util::Stream::poll_next(Pin::new(&mut rcv), cx))
            .await
            .is_none()
    );

    // reset by the peer
    srv.send(frame::Reset::new(1.into(), Reason::CANCEL));
    timeout(Millis(1_000), snd.on_reset())
        .await
        .expect("reset is not observed");
    assert!(snd.is_reset());
    assert!(snd.stream().is_failed());
}

/// STATEMENT: Capacity values from different streams cannot be combined.
///
/// HOW:
/// - Open two request streams and obtain an empty capacity value from each.
/// - Use `catch_unwind` to assert both addition and add-assignment panic when combining those
///   values.
#[ntex::test]
async fn capacity_from_different_streams() {
    let (client, _srv) = client().await;
    let (snd1, _rcv1) = request(&client, Method::POST, false).await;
    let (snd2, _rcv2) = request(&client, Method::POST, false).await;

    let cap1 = snd1.stream().empty_capacity();
    let cap2 = snd2.stream().empty_capacity();
    let res = panic::catch_unwind(panic::AssertUnwindSafe(|| cap1 + cap2));
    assert!(res.is_err());

    let mut cap1 = snd1.stream().empty_capacity();
    let cap2 = snd2.stream().empty_capacity();
    let res = panic::catch_unwind(panic::AssertUnwindSafe(move || cap1 += cap2));
    assert!(res.is_err());
}

/// STATEMENT: Dropping a stream marked with `disconnect_on_drop` closes the connection when all
/// streams finish.
///
/// HOW:
/// - Mark either the send or receive half with `disconnect_on_drop` in separate runs, then drop
///   both halves.
/// - Wait for the client to close and assert the peer receives a reset with `Reason::CANCEL`.
#[ntex::test]
async fn disconnect_on_drop() {
    for recv_side in [false, true] {
        let (client, srv) = client().await;
        let (snd, rcv) = request(&client, Method::POST, false).await;
        if recv_side {
            rcv.disconnect_on_drop();
            drop(snd);
            drop(rcv);
        } else {
            snd.disconnect_on_drop();
            drop(rcv);
            drop(snd);
        }

        timeout(Millis(1_000), async {
            while !client.is_closed() {
                sleep(Millis(10)).await;
            }
        })
        .await
        .expect("connection is not closed");
        let frames = srv.frames().await;
        assert!(
            frames
                .iter()
                .any(|frm| matches!(frm, Frame::Reset(rst) if rst.reason() == Reason::CANCEL)),
            "{frames:?}"
        );
    }
}

/// STATEMENT: Server response methods reject calls after the send side has advanced beyond
/// response headers.
///
/// HOW:
/// - Send response headers and assert a second response fails with `OperationError::Payload`.
/// - Send final body data and assert subsequent response and informational calls fail with
///   `OperationError::Closed(None)`.
/// - Assert the client receives the original headers and final body `done`.
#[ntex::test]
async fn server_response_state_errors() {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);

    ntex::rt::spawn(async move {
        let _ = server::Server::new(async |msg: Message| {
            if !matches!(msg.kind, MessageKind::Headers { .. }) {
                return Ok(());
            }
            let stream = msg.stream();
            stream
                .send_response(StatusCode::OK, HeaderMap::new(), false)
                .unwrap();
            let err = stream
                .send_response(StatusCode::OK, HeaderMap::new(), false)
                .unwrap_err();
            assert!(matches!(*err, OperationError::Payload));

            stream.send_payload("done", true).await.unwrap();
            let err = stream
                .send_response(StatusCode::OK, HeaderMap::new(), true)
                .unwrap_err();
            assert!(matches!(*err, OperationError::Closed(None)), "{err:?}");
            let err = stream
                .send_informational(StatusCode::CONTINUE, HeaderMap::new())
                .unwrap_err();
            assert!(matches!(*err, OperationError::Closed(None)), "{err:?}");
            Ok::<_, ()>(())
        })
        .run(Io::new(srv, SharedCfg::default()))
        .await;
    });

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    assert!(matches!(
        next(&rcv).await,
        MessageKind::Headers { eof: false, .. }
    ));
    assert!(matches!(
        next(&rcv).await,
        MessageKind::Eof(StreamEof::Data(ref d, _)) if d == "done"
    ));
}

/// STATEMENT: Response headers can be sent from a borrowed map without modifying it.
///
/// HOW:
/// - Build an `x-test: borrowed` header map and send a final `StatusCode::ACCEPTED` response
///   using a reference to it.
/// - Assert the original map retains its value and the client receives that status, header, and
///   end-of-stream.
#[ntex::test]
async fn server_response_borrowed_headers() {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);

    ntex::rt::spawn(async move {
        let _ = server::Server::new(async |msg: Message| {
            if !matches!(msg.kind, MessageKind::Headers { .. }) {
                return Ok(());
            }
            let mut hdrs = HeaderMap::new();
            hdrs.insert(
                header::HeaderName::from_static("x-test"),
                HeaderValue::from_static("borrowed"),
            );
            msg.stream()
                .send_response(StatusCode::ACCEPTED, &hdrs, true)
                .unwrap();
            assert_eq!(hdrs.get("x-test").unwrap(), "borrowed");
            Ok::<_, ()>(())
        })
        .run(Io::new(srv, SharedCfg::default()))
        .await;
    });

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());
    let (_snd, rcv) = request(&client, Method::GET, true).await;
    match next(&rcv).await {
        MessageKind::Headers {
            pseudo,
            headers,
            eof: true,
        } => {
            assert_eq!(pseudo.status, Some(StatusCode::ACCEPTED));
            assert_eq!(headers.get("x-test").unwrap(), "borrowed");
        }
        kind => panic!("unexpected message: {kind:?}"),
    }
}
/// STATEMENT: Acknowledging local settings applies the configured initial window to existing
/// streams.
///
/// HOW:
/// - Open a stream before the settings acknowledgment with a configured window of 1,000,000 or
///   1,000 bytes.
/// - Acknowledge settings and send 80,000 or 2,000 bytes respectively; assert no stream
///   `WindowUpdate` is emitted because the peer applies the new initial window itself.
/// - Assert only the smaller window produces `Reason::FLOW_CONTROL_ERROR`; otherwise receive
///   all data and consume its capacity.
/// - Assert the client remains open in both cases.
#[ntex::test]
async fn settings_ack_updates_stream_windows() {
    for (window, sent, reset) in [
        (1_000_000, 80_000, None),
        (1_000, 2_000, Some(Reason::FLOW_CONTROL_ERROR)),
    ] {
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1024 * 1024);
        srv.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("CLI")
            .add(ntex_h2::ServiceConfig::new().set_initial_window_size(window))
            .build();
        let client = SimpleClient::new(Io::new(cli, cfg), false, "localhost".into());
        let srv = RawServer {
            io: srv,
            codec: Codec::default(),
            buf: RefCell::new(BytesMut::new()),
        };
        srv.send(frame::Settings::default());
        sleep(Millis(20)).await;
        let preface = srv.io.read_any();
        srv.buf.borrow_mut().extend_from_slice(&preface[24..]);

        let (_snd, rcv) = request(&client, Method::POST, false).await;
        let _ = srv.frames().await;

        srv.send(frame::Settings::ack());
        srv.response(1, HeaderMap::new(), false);
        for chunk in vec![0u8; sent].chunks(16_000) {
            srv.send(frame::Data::new(1.into(), Bytes::copy_from_slice(chunk)));
        }
        let frames = srv.frames().await;
        assert!(
            !frames
                .iter()
                .any(|frm| matches!(frm, Frame::WindowUpdate(upd) if upd.stream_id() == 1)),
            "{window}: {frames:?}"
        );
        let rst = frames.iter().find_map(|frm| match frm {
            Frame::Reset(rst) if rst.stream_id() == 1 => Some(rst.reason()),
            _ => None,
        });
        assert_eq!(rst, reset, "{window}");
        if reset.is_none() {
            let mut received = 0;
            while received < sent {
                match next(&rcv).await {
                    MessageKind::Data(data, cap) => {
                        received += data.len();
                        cap.consume(data.len() as u32);
                    }
                    MessageKind::Headers { .. } => {}
                    kind => panic!("unexpected message: {kind:?}"),
                }
            }
        }
        assert!(!client.is_closed());
    }
}

/// STATEMENT: Applying acknowledged settings resets a stream whose previously increased window
/// would overflow.
///
/// HOW:
/// - Configure an initial window of `i32::MAX`, open a stream before acknowledgment, and
///   consume received data to trigger a stream `WindowUpdate`.
/// - Acknowledge settings and assert a reset with `Reason::FLOW_CONTROL_ERROR` and
///   `StreamError::WindowOverflowed`.
/// - Assert the client connection stays open.
#[ntex::test]
async fn settings_ack_overflows_stream_window() {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ntex_h2::ServiceConfig::new().set_initial_window_size(i32::MAX))
        .build();
    let client = SimpleClient::new(Io::new(cli, cfg), false, "localhost".into());
    let srv = RawServer {
        io: srv,
        codec: Codec::default(),
        buf: RefCell::new(BytesMut::new()),
    };
    srv.send(frame::Settings::default());
    sleep(Millis(20)).await;
    let preface = srv.io.read_any();
    srv.buf.borrow_mut().extend_from_slice(&preface[24..]);

    let (_snd, rcv) = request(&client, Method::POST, false).await;
    srv.response(1, HeaderMap::new(), false);
    srv.data(1, "data", false);
    assert!(matches!(next(&rcv).await, MessageKind::Headers { .. }));
    let MessageKind::Data(data, cap) = next(&rcv).await else {
        panic!("expected data")
    };
    cap.consume(data.len() as u32);
    assert!(
        srv.frames()
            .await
            .iter()
            .any(|frm| matches!(frm, Frame::WindowUpdate(upd) if upd.stream_id() == 1))
    );

    srv.send(frame::Settings::ack());
    assert_eq!(srv.reset(1).await, Reason::FLOW_CONTROL_ERROR);
    assert!(matches!(stream_error(&rcv).await, StreamError::WindowOverflowed));
    assert!(!client.is_closed());
}
