//! Connection level protocol handling over an in-memory transport.
use ntex::channel::{mpsc, oneshot};
use ntex::http::{HeaderMap, Method, StatusCode};
use ntex::io::{Io, IoConfig, testing::IoTest};
use ntex::service::cfg::SharedCfg;
use ntex::time::{Millis, Seconds, sleep, timeout};
use ntex::util::{Bytes, BytesMut};
use ntex_codec::Decoder;
use ntex_h2::control::Reason as CtlReason;
use ntex_h2::frame::{self, Frame, Reason, StreamId};
use ntex_h2::{Codec, Control, ControlAck, Message, MessageKind, server};

const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const END_STREAM: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PRIORITY: u8 = 0x20;

/// `:method: GET`, `:scheme: http`, `:path: /`
const REQUEST: [u8; 3] = [0x82, 0x86, 0x84];

fn raw_frame(kind: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut buf = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    buf.extend_from_slice(&stream_id.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// HEADERS frame that depends on its own stream.
fn self_dependent_headers(id: u32) -> Vec<u8> {
    let mut payload = id.to_be_bytes().to_vec();
    payload.push(16);
    payload.extend_from_slice(&REQUEST);
    raw_frame(1, END_HEADERS | END_STREAM | PRIORITY, id, &payload)
}

fn request(id: u32, path: &str, eos: bool) -> Frame {
    let pseudo = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("http".into()),
        authority: Some("localhost".into()),
        path: Some(path.into()),
        ..Default::default()
    };
    frame::Headers::new(id.into(), pseudo, HeaderMap::new(), eos).into()
}

struct Peer {
    io: Io,
    codec: Codec,
    peer: IoTest,
    events: mpsc::Receiver<String>,
    disconnects: mpsc::Receiver<String>,
    done: oneshot::Receiver<()>,
}

impl Peer {
    fn write(&self, buf: &[u8]) {
        let _ = self.io.with_write_src(|dst| dst.extend_from_slice(buf));
    }

    async fn send(&self, frm: impl Into<Frame>) {
        self.io.send(frm.into(), &self.codec).await.unwrap();
    }

    /// Next frame that is not part of connection setup.
    async fn recv(&self) -> Option<Frame> {
        timeout(Millis(3_000), async {
            loop {
                match self.io.recv(&self.codec).await {
                    Ok(Some(Frame::Settings(_) | Frame::WindowUpdate(_))) => {}
                    Ok(frm) => return frm,
                    Err(_) => return None,
                }
            }
        })
        .await
        .expect("no frame")
    }

    async fn goaway(&self) -> frame::GoAway {
        match self.recv().await {
            Some(Frame::GoAway(frm)) => frm,
            frm => panic!("expected GOAWAY: {frm:?}"),
        }
    }

    async fn ping(&self) {
        self.send(frame::Ping::new(*b"pingpong")).await;
        match self.recv().await {
            Some(Frame::Ping(ping)) => {
                assert!(ping.is_ack());
                assert_eq!(ping.payload(), b"pingpong");
            }
            frm => panic!("expected PING ack: {frm:?}"),
        }
    }

    async fn event(&self) -> String {
        timeout(Millis(3_000), self.events.recv())
            .await
            .expect("no control event")
            .unwrap()
    }

    async fn stopped(self) {
        timeout(Millis(3_000), self.done)
            .await
            .expect("server did not stop")
            .unwrap();
    }
}

/// Starts a server, the peer is handshaked already.
///
/// The control service reports events, `over` replaces the GOAWAY reason
/// for errors.
async fn start(cfg: SharedCfg, over: Option<Reason>) -> Peer {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);

    let (tx, events) = mpsc::channel();
    let (dis_tx, disconnects) = mpsc::channel();
    let (done_tx, done) = oneshot::channel();
    ntex::rt::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            let pseudo = match &msg.kind {
                MessageKind::Headers { pseudo, .. } => pseudo,
                MessageKind::Disconnect(err) => {
                    let res = msg.stream().send_response(StatusCode::OK, HeaderMap::new(), true);
                    let _ = dis_tx.send(format!("{err:?} {res:?}"));
                    return Ok(());
                }
                _ => return Ok(()),
            };
            match pseudo.path.as_ref().map(AsRef::as_ref) {
                Some("/fail") => Err(()),
                Some("/big") => {
                    let stream = msg.stream();
                    stream
                        .send_response(StatusCode::OK, HeaderMap::new(), false)
                        .unwrap();
                    stream
                        .send_payload(Bytes::from(vec![b'x'; 40_000]), true)
                        .await
                        .unwrap();
                    Ok(())
                }
                Some("/open") => Ok(()),
                _ => {
                    msg.stream()
                        .send_response(StatusCode::OK, HeaderMap::new(), true)
                        .unwrap();
                    Ok(())
                }
            }
        })
        .control(async move |msg: Control<()>| {
            let Control::Disconnect(mut reason) = msg else {
                return Ok::<_, ()>(msg.ack());
            };
            let event = match &mut reason {
                CtlReason::Error(e) => format!("error: {:?}", e.get_ref()),
                CtlReason::ProtocolError(e) => format!("proto: {:?}", e.get_ref()),
                CtlReason::GoAway(g) => format!("goaway: {:?}", g.frame().reason()),
                CtlReason::PeerGone(p) => {
                    let has_err = p.err().is_some();
                    assert_eq!(p.take().is_some(), has_err);
                    assert!(p.err().is_none());
                    format!("gone: {has_err}")
                }
            };
            let _ = tx.send(event);
            Ok::<ControlAck, ()>(match (reason, over) {
                (CtlReason::Error(e), Some(r)) => e.reason(r).ack(),
                (CtlReason::ProtocolError(e), Some(r)) => e.reason(r).ack(),
                (reason, _) => reason.ack(),
            })
        })
        .run(Io::new(srv, cfg))
        .await;
        let _ = done_tx.send(());
    });

    let peer = Peer {
        io: Io::new(cli.clone(), SharedCfg::default()),
        codec: Codec::default(),
        peer: cli,
        events,
        disconnects,
        done,
    };
    peer.write(PREFACE);
    peer.send(frame::Settings::default()).await;
    peer.send(frame::Settings::ack()).await;
    peer
}

async fn start_default() -> Peer {
    start(SharedCfg::new("SRV").build(), None).await
}

/// An invalid HEADERS frame resets a new stream, the same frame for a
/// closed stream is a connection error.
#[ntex::test]
async fn invalid_headers_frame() {
    let peer = start_default().await;

    peer.write(&self_dependent_headers(1));
    match peer.recv().await {
        Some(Frame::Reset(rst)) => {
            assert_eq!(rst.stream_id(), 1);
            assert_eq!(rst.reason(), Reason::PROTOCOL_ERROR);
        }
        frm => panic!("expected RST_STREAM: {frm:?}"),
    }
    peer.ping().await;

    peer.write(&self_dependent_headers(1));
    let frm = peer.goaway().await;
    assert_eq!(frm.reason(), Reason::PROTOCOL_ERROR);
    assert_eq!(frm.last_stream_id(), 1);
    assert!(peer.event().await.contains("InvalidStreamId"));
    peer.stopped().await;
}

/// HEADERS for an old stream id is a connection error.
#[ntex::test]
async fn headers_for_closed_stream() {
    let peer = start_default().await;

    peer.send(request(3, "/", true)).await;
    assert!(matches!(peer.recv().await, Some(Frame::Headers(h)) if h.stream_id() == 3));

    peer.send(request(1, "/", true)).await;
    let frm = peer.goaway().await;
    assert_eq!(frm.reason(), Reason::PROTOCOL_ERROR);
    assert_eq!(frm.last_stream_id(), 3);
    assert!(peer.event().await.contains("InvalidStreamId"));
    peer.stopped().await;
}

/// Connection level flow control and stream zero errors.
#[ntex::test]
async fn connection_frame_errors() {
    let cases: [(&[u8], Reason, &str); 4] = [
        // connection window overflow
        (
            &raw_frame(8, 0, 0, &[0x7f, 0xff, 0xff, 0xff]),
            Reason::FLOW_CONTROL_ERROR,
            "WindowValueOverflow",
        ),
        // RST_STREAM for stream zero
        (
            &raw_frame(3, 0, 0, &[0, 0, 0, 8]),
            Reason::PROTOCOL_ERROR,
            "RST_STREAM-zero",
        ),
        // WINDOW_UPDATE for an idle stream
        (
            &raw_frame(8, 0, 11, &[0, 0, 0, 1]),
            Reason::PROTOCOL_ERROR,
            "WINDOW_UPDATE",
        ),
        // second SETTINGS ack
        (
            &raw_frame(4, 1, 0, &[]),
            Reason::PROTOCOL_ERROR,
            "UnexpectedSettingsAck",
        ),
    ];

    for (frm, reason, event) in cases {
        let peer = start_default().await;
        peer.ping().await;
        peer.write(frm);
        assert_eq!(peer.goaway().await.reason(), reason, "{event}");
        let ev = peer.event().await;
        assert!(ev.contains(event), "{ev} does not contain {event}");
        peer.stopped().await;
    }
}

/// PRIORITY frames are accepted and ignored.
#[ntex::test]
async fn priority_frames_are_ignored() {
    let peer = start_default().await;

    peer.send(request(1, "/open", false)).await;
    // valid PRIORITY for an open stream
    peer.write(&raw_frame(2, 0, 1, &[0, 0, 0, 0, 16]));
    // invalid PRIORITY for an open stream resets it
    peer.write(&raw_frame(2, 0, 1, &[0, 0, 0, 1, 16]));
    match peer.recv().await {
        Some(Frame::Reset(rst)) => {
            assert_eq!(rst.stream_id(), 1);
            assert_eq!(rst.reason(), Reason::PROTOCOL_ERROR);
        }
        frm => panic!("expected RST_STREAM: {frm:?}"),
    }
    // invalid PRIORITY for an idle stream is ignored
    peer.write(&raw_frame(2, 0, 9, &[0, 0, 0, 9, 16]));
    peer.write(&raw_frame(2, 0, 7, &[0, 0, 0, 1]));
    peer.ping().await;
}

/// The peer's SETTINGS_MAX_FRAME_SIZE is used for DATA frames.
#[ntex::test]
async fn peer_max_frame_size() {
    let mut peer = start_default().await;
    let mut settings = frame::Settings::default();
    settings.set_max_frame_size(32_768);
    peer.send(settings).await;
    peer.codec = Codec::default();
    peer.codec.set_recv_frame_size(32_768);

    peer.send(request(1, "/big", true)).await;
    let mut sizes = Vec::new();
    loop {
        match peer.recv().await {
            Some(Frame::Headers(_)) => {}
            Some(Frame::Data(data)) => {
                sizes.push(data.payload().len());
                if data.is_end_stream() {
                    break;
                }
            }
            frm => panic!("unexpected frame: {frm:?}"),
        }
    }
    assert_eq!(sizes.iter().sum::<usize>(), 40_000);
    assert!(sizes.iter().any(|sz| *sz > 16_384), "{sizes:?}");
    assert!(sizes.iter().all(|sz| *sz <= 32_768), "{sizes:?}");
}

/// The control service can replace the GOAWAY reason.
#[ntex::test]
async fn control_overrides_goaway_reason() {
    // publish service error
    let peer = start(SharedCfg::new("SRV").build(), Some(Reason::CANCEL)).await;
    peer.send(request(1, "/fail", true)).await;
    loop {
        match peer.recv().await {
            Some(Frame::Reset(_)) => {}
            Some(Frame::GoAway(frm)) => {
                assert_eq!(frm.reason(), Reason::CANCEL);
                assert_eq!(frm.last_stream_id(), 1);
                break;
            }
            frm => panic!("unexpected frame: {frm:?}"),
        }
    }
    assert_eq!(peer.event().await, "error: ()");
    peer.stopped().await;

    // protocol error
    let peer = start(SharedCfg::new("SRV").build(), Some(Reason::ENHANCE_YOUR_CALM)).await;
    peer.write(&raw_frame(3, 0, 0, &[0, 0, 0, 8]));
    assert_eq!(peer.goaway().await.reason(), Reason::ENHANCE_YOUR_CALM);
    assert!(peer.event().await.starts_with("proto: "));
    peer.stopped().await;
}

/// Remote GOAWAY and peer disconnect are reported to the control service.
#[ntex::test]
async fn control_remote_events() {
    let peer = start_default().await;
    peer.ping().await;
    peer.send(frame::GoAway::new(Reason::ENHANCE_YOUR_CALM)).await;
    assert_eq!(peer.event().await, "goaway: ENHANCE_YOUR_CALM");
    peer.stopped().await;

    let peer = start_default().await;
    peer.ping().await;
    peer.peer.close().await;
    assert!(peer.event().await.starts_with("gone: "));
    peer.stopped().await;
}

/// A peer that stops in the middle of a frame is disconnected.
#[ntex::test]
async fn read_timeout() {
    let cfg = SharedCfg::new("SRV")
        .add(IoConfig::new().set_frame_read_rate(Seconds(1), Seconds(1), 256))
        .build();
    let peer = start(cfg, None).await;
    peer.ping().await;

    // partial frame header
    peer.write(&[0, 0, 8, 6, 0]);
    let _ = peer.io.flush(true).await;
    let frm = peer.goaway().await;
    assert_eq!(frm.reason(), Reason::NO_ERROR);
    assert!(peer.event().await.contains("ReadTimeout"));
    peer.stopped().await;
}

/// A peer that does not read is disconnected.
#[ntex::test]
async fn write_timeout() {
    let cfg = SharedCfg::new("SRV")
        .add(
            IoConfig::new()
                .set_write_timeout(Seconds(1))
                .set_write_backpressure(64),
        )
        .build();
    let peer = start(cfg, None).await;
    peer.send(request(1, "/open", false)).await;
    peer.ping().await;

    // the peer stops reading, PING acks enable write backpressure
    peer.peer.remote_buffer_cap(0);
    sleep(Millis(50)).await;
    for _ in 0..8 {
        peer.send(frame::Ping::new([0; 8])).await;
    }
    assert!(peer.event().await.contains("WriteTimeout"));
    // open streams fail with the same error
    let err = timeout(Millis(1_000), peer.disconnects.recv())
        .await
        .expect("stream is not disconnected")
        .unwrap();
    assert!(!err.contains("ReadTimeout"), "{err}");
    assert_eq!(err.matches("WriteTimeout").count(), 2, "{err}");
    peer.stopped().await;
}

/// Frames sent to a client by a raw server, the client replies are decoded.
fn client_output(srv: &IoTest) -> Vec<Frame> {
    let out = srv.read_any();
    assert_eq!(&out[..24], PREFACE);
    let mut buf = BytesMut::copy_from_slice(&out[24..]);
    let codec = Codec::default();
    let mut frames = Vec::new();
    while let Some(frm) = codec.decode(&mut buf).unwrap() {
        frames.push(frm);
    }
    frames
}

fn simple_client() -> (ntex_h2::client::SimpleClient, IoTest) {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);
    let client =
        ntex_h2::client::SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());
    srv.write(raw_frame(4, 0, 0, &[]));
    (client, srv)
}

/// A client rejects an invalid HEADERS frame for a server initiated stream.
#[ntex::test]
async fn client_invalid_headers_for_server_stream() {
    let (client, srv) = simple_client();
    sleep(Millis(50)).await;

    srv.write(self_dependent_headers(2));
    timeout(Millis(1_000), async {
        while !client.is_closed() {
            sleep(Millis(10)).await;
        }
    })
    .await
    .expect("client is not closed");

    let goaway = client_output(&srv).into_iter().find_map(|frm| match frm {
        Frame::GoAway(frm) => Some(frm),
        _ => None,
    });
    assert_eq!(goaway.unwrap().reason(), Reason::PROTOCOL_ERROR);
}

/// A client does not open streams after GOAWAY.
#[ntex::test]
async fn client_send_after_goaway() {
    let (client, srv) = simple_client();
    let (_snd, _rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();

    let goaway = frame::GoAway::new(Reason::NO_ERROR).set_last_stream_id(StreamId::CLIENT);
    let mut buf = ntex_bytes::BytePages::default();
    ntex_codec::Encoder::encode(&Codec::default(), goaway.into(), &mut buf).unwrap();
    srv.write(buf.freeze());
    sleep(Millis(50)).await;

    let err = client
        .send(Method::GET, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap_err();
    assert!(
        matches!(
            *err,
            ntex_h2::OperationError::Connection(ntex_h2::ConnectionError::GoAway(Reason::NO_ERROR))
        ),
        "{err}"
    );
}

#[derive(Debug)]
struct NotReady;

impl ntex::service::Service<(), Message> for NotReady {
    type Res = ();
    type Error = &'static str;

    async fn ready(&self, _: ntex::service::Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        Err("not ready")
    }

    async fn call(&self, _: Message, _: ntex::service::Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct NotReadyControl;

impl ntex::service::Service<(), Control<&'static str>> for NotReadyControl {
    type Res = ControlAck;
    type Error = &'static str;

    async fn ready(&self, _: ntex::service::Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        Err("control not ready")
    }

    async fn call(
        &self,
        msg: Control<&'static str>,
        _: ntex::service::Ctx<'_, Self, ()>,
    ) -> Result<ControlAck, Self::Error> {
        Ok(msg.ack())
    }
}

impl ntex::service::ServiceFactory<(), Control<&'static str>> for NotReadyControl {
    type Res = ControlAck;
    type Error = &'static str;
    type Service = NotReadyControl;
    type InitError = std::io::Error;

    async fn create(&self, _: &()) -> Result<NotReadyControl, std::io::Error> {
        Ok(NotReadyControl)
    }
}

#[derive(Debug)]
struct NotReadyFactory;

impl ntex::service::ServiceFactory<(), Message> for NotReadyFactory {
    type Res = ();
    type Error = &'static str;
    type Service = NotReady;
    type InitError = std::io::Error;

    async fn create(&self, _: &()) -> Result<NotReady, std::io::Error> {
        Ok(NotReady)
    }
}

/// A publish service readiness failure is reported to the control service,
/// a control service failure stops the server.
#[ntex::test]
async fn publish_service_not_ready() {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);

    let (tx, rx) = oneshot::channel();
    ntex::rt::spawn(async move {
        let res = server::Server::new(NotReadyFactory)
            .control(async move |msg: Control<&'static str>| {
                let Control::Disconnect(CtlReason::Error(err)) = msg else {
                    panic!("unexpected control message: {msg:?}")
                };
                Err::<ControlAck, _>(*err.get_ref())
            })
            .run(Io::new(srv, SharedCfg::default()))
            .await;
        let _ = tx.send(res);
    });

    let io = Io::new(cli, SharedCfg::default());
    let codec = Codec::default();
    let _ = io.with_write_src(|dst| dst.extend_from_slice(PREFACE));
    io.send(frame::Settings::default().into(), &codec).await.unwrap();

    let res = timeout(Millis(3_000), rx)
        .await
        .expect("server did not stop")
        .unwrap();
    assert!(
        matches!(res, Err(server::ServerError::Service("not ready"))),
        "{res:?}"
    );
}

/// Control ack for a failed publish service sends GOAWAY and closes the
/// connection.
#[ntex::test]
async fn publish_service_not_ready_ack() {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);

    let (tx, rx) = oneshot::channel();
    ntex::rt::spawn(async move {
        let res = server::Server::new(NotReadyFactory)
            .control(async move |msg: Control<&'static str>| {
                let Control::Disconnect(CtlReason::Error(err)) = msg else {
                    panic!("unexpected control message: {msg:?}")
                };
                Ok::<_, &'static str>(err.reason(Reason::ENHANCE_YOUR_CALM).ack())
            })
            .run(Io::new(srv, SharedCfg::default()))
            .await;
        let _ = tx.send(res);
    });

    let io = Io::new(cli, SharedCfg::default());
    let codec = Codec::default();
    let _ = io.with_write_src(|dst| dst.extend_from_slice(PREFACE));
    io.send(frame::Settings::default().into(), &codec).await.unwrap();

    let reason = timeout(Millis(3_000), async {
        loop {
            match io.recv(&codec).await {
                Ok(Some(Frame::GoAway(frm))) => return frm.reason(),
                Ok(Some(_)) => {}
                res => panic!("unexpected: {res:?}"),
            }
        }
    })
    .await
    .expect("no GOAWAY");
    assert_eq!(reason, Reason::ENHANCE_YOUR_CALM);

    let res = timeout(Millis(3_000), rx)
        .await
        .expect("server did not stop")
        .unwrap();
    assert!(res.is_ok(), "{res:?}");
}

/// Errors after the control service is notified of a disconnect are
/// handled with the default response.
#[ntex::test]
async fn error_after_goaway() {
    let peer = start(SharedCfg::new("SRV").build(), Some(Reason::ENHANCE_YOUR_CALM)).await;
    peer.send(request(1, "/open", false)).await;
    peer.ping().await;

    // graceful GOAWAY, stream 1 keeps the connection open
    peer.send(frame::GoAway::new(Reason::NO_ERROR).set_last_stream_id(0.into()))
        .await;
    assert!(peer.event().await.starts_with("goaway: "));
    let frm = peer.goaway().await;
    assert_eq!(frm.reason(), Reason::NO_ERROR);
    assert_eq!(frm.last_stream_id(), StreamId::from(1));
    peer.ping().await;

    // repeated GOAWAY is not reported
    peer.send(frame::GoAway::new(Reason::NO_ERROR).set_last_stream_id(0.into()))
        .await;
    peer.ping().await;

    // RST_STREAM on stream 0, the control service is not called again
    peer.write(&raw_frame(3, 0, 0, &[0, 0, 0, 0]));
    let _ = peer.io.flush(true).await;
    assert_eq!(peer.goaway().await.reason(), Reason::PROTOCOL_ERROR);
    let ev = timeout(Millis(3_000), peer.events.recv()).await;
    assert!(matches!(ev, Ok(None)), "{ev:?}");
    peer.stopped().await;
}

/// Both services failing readiness stops the server with the control error.
#[ntex::test]
async fn publish_and_control_not_ready() {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);

    let (tx, rx) = oneshot::channel();
    ntex::rt::spawn(async move {
        let res = server::Server::new(NotReadyFactory)
            .control(NotReadyControl)
            .run(Io::new(srv, SharedCfg::default()))
            .await;
        let _ = tx.send(res);
    });

    let io = Io::new(cli, SharedCfg::default());
    let codec = Codec::default();
    let _ = io.with_write_src(|dst| dst.extend_from_slice(PREFACE));
    io.send(frame::Settings::default().into(), &codec).await.unwrap();

    let res = timeout(Millis(3_000), rx)
        .await
        .expect("server did not stop")
        .unwrap();
    assert!(
        matches!(res, Err(server::ServerError::Service("control not ready"))),
        "{res:?}"
    );
}
