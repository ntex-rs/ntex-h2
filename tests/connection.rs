#![recursion_limit = "256"]
use std::{cell::Cell, io, net, rc::Rc};

use ::openssl::ssl::{AlpnError, SslAcceptor, SslConnector, SslFiletype, SslMethod, SslVerifyMode};
use ntex::http::{self, HeaderMap, HttpService, Method, Response, openssl, test, uri::Scheme};
use ntex::service::{Pipeline, cfg::SharedCfg, fn_service};
use ntex::time::{Millis, Seconds, sleep};
use ntex::{Service, channel::oneshot, connect::openssl, io::IoBoxed, util::Bytes};
use ntex_h2::client::{self, Client, SimpleClient};
use ntex_h2::{Codec, MessageKind, ServiceConfig, frame, frame::Reason};

fn ssl_acceptor() -> SslAcceptor {
    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder.set_certificate_chain_file("./tests/cert.pem").unwrap();
    builder.set_alpn_select_callback(|_, protos| {
        const H2: &[u8] = b"\x02h2";
        const H11: &[u8] = b"\x08http/1.1";
        if protos.windows(3).any(|window| window == H2) {
            Ok(b"h2")
        } else if protos.windows(9).any(|window| window == H11) {
            Ok(b"http/1.1")
        } else {
            Err(AlpnError::NOACK)
        }
    });
    builder
        .set_alpn_protos(b"\x08http/1.1\x02h2")
        .expect("Cannot contrust SslAcceptor");

    builder.build()
}

async fn start_server() -> test::TestServer {
    test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(async move |mut req: http::Request| {
                    let mut pl = req.take_payload();
                    pl.recv().await;
                    Ok::<_, io::Error>(Response::Ok().body("test body"))
                }),
            )
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(
            ServiceConfig::new()
                .set_capacity_timeout(Seconds(1))
                .set_max_concurrent_streams(1),
        ),
    )
}

async fn connect(addr: net::SocketAddr) -> IoBoxed {
    // disable ssl verification
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder
        .set_alpn_protos(b"\x02h2\x08http/1.1")
        .map_err(|e| log::error!("Cannot set alpn protocol: {:?}", e));

    let addr = ntex::connect::Connect::new("localhost").set_addr(Some(addr));
    Pipeline::new(SharedCfg::default(), openssl::SslConnector::new(builder.build()))
        .call(addr)
        .await
        .unwrap()
        .into()
}

fn get_reset(frm: frame::Frame) -> frame::Reset {
    match frm {
        frame::Frame::Reset(rst) => rst,
        _ => panic!("Expect Reset frame: {:?}", frm),
    }
}

fn goaway(frm: frame::Frame) -> frame::GoAway {
    match frm {
        frame::Frame::GoAway(f) => f,
        _ => panic!("Expect Reset frame: {:?}", frm),
    }
}

#[ntex::test]
async fn test_max_concurrent_streams() {
    let srv = start_server().await;
    let addr = srv.addr();
    let client = Pipeline::new(
        SharedCfg::default(),
        client::Connector::new()
            .scheme(Scheme::HTTP)
            .connector(fn_service(move |_| async move { Ok(connect(addr).await) })),
    )
    .call("localhost")
    .await
    .unwrap();
    assert!(format!("{:?}", client).contains("SimpleClient"));
    assert_eq!(client.authority(), "localhost");

    loop {
        sleep(Millis(150)).await; // we need to get settings frame from server
        if client.max_streams() == Some(1) {
            break;
        }
    }

    let (stream, recv_stream) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    assert!(!client.is_ready());
    assert!(client.active_streams() == 1);
    assert_eq!(stream.id(), recv_stream.id());
    assert_eq!(stream.stream(), recv_stream.stream());
    assert!(format!("{:?}", stream).contains("SendStream"));
    assert!(format!("{:?}", recv_stream).contains("RecvStream"));

    let client2 = client.clone();
    let opened = Rc::new(Cell::new(false));
    let opened2 = opened.clone();
    ntex::rt::spawn(async move {
        let _stream = client2
            .send(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        opened2.set(true);
    });

    stream.send_payload(Bytes::new(), true).await.unwrap();
    sleep(Millis(50)).await;
    assert!(client.is_ready());
    assert!(opened.get());
}

#[ntex::test]
async fn test_max_concurrent_streams_pool() {
    let srv = start_server().await;
    let addr = srv.addr();
    let client = Client::builder("localhost");
    assert!(format!("{:?}", client).contains("ClientBuilder"));

    let client = client
        .connection_limit(1)
        .scheme(Scheme::HTTPS)
        .connector(fn_service(move |_| async move { Ok(connect(addr).await) }))
        .build(SharedCfg::default());
    assert!(format!("{:?}", client).contains("Client"));
    assert!(client.is_ready());

    let (stream, _recv_stream) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    sleep(Millis(500)).await;
    assert!(!client.is_ready());

    let client2 = client.clone();
    let opened = Rc::new(Cell::new(false));
    let opened2 = opened.clone();
    ntex::rt::spawn(async move {
        let _stream = client2
            .send(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        opened2.set(true);
    });

    stream.send_payload(Bytes::new(), true).await.unwrap();
    client.ready().await;
    sleep(Millis(150)).await;
    assert!(client.is_ready());
    assert!(opened.get());
}

#[ntex::test]
async fn test_max_concurrent_streams_pool2() {
    let srv = start_server().await;
    let addr = srv.addr();

    let cnt = Rc::new(Cell::new(0));
    let cnt2 = cnt.clone();
    let client = Client::builder("localhost")
        .connection_limit(2)
        .connector(async move |_| {
            cnt2.set(cnt2.get() + 1);
            Ok(connect(addr).await)
        })
        .build(SharedCfg::default());
    assert!(client.is_ready());

    let (stream, _recv_stream) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    sleep(Millis(500)).await;
    assert!(client.is_ready());

    let client2 = client.clone();
    let opened = Rc::new(Cell::new(false));
    let opened2 = opened.clone();
    ntex::rt::spawn(async move {
        let _stream = client2
            .send(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        opened2.set(true);
    });

    stream.send_payload(Bytes::new(), true).await.unwrap();
    sleep(Millis(250)).await;
    assert!(client.is_ready());
    assert!(opened.get());
    assert!(cnt.get() == 2);
}

#[ntex::test]
async fn test_max_concurrent_streams_reset() {
    let srv = start_server().await;
    let io = connect(srv.addr()).await;
    let client = SimpleClient::new(io, Scheme::HTTP, "localhost".into());
    sleep(Millis(150)).await;

    let (stream, _recv_stream) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    assert!(!client.is_ready());

    let opened = Rc::new(Cell::new(0));

    let client2 = client.clone();
    let opened2 = opened.clone();
    ntex::rt::spawn(async move {
        let (_stream, _recv_stream) = client2
            .send(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        _stream.reset(Reason::NO_ERROR);
        opened2.set(opened2.get() + 1);
    });
    let client2 = client.clone();
    let opened2 = opened.clone();
    ntex::rt::spawn(async move {
        let _stream = client2
            .send(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        drop(_stream);
        opened2.set(opened2.get() + 1);
    });
    let client2 = client.clone();
    let opened2 = opened.clone();
    let (tx, rx) = oneshot::channel();
    ntex::rt::spawn(async move {
        let _stream = client2
            .send(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        opened2.set(opened2.get() + 1);
        let _ = tx.send(());
    });
    sleep(Millis(50)).await;

    stream.send_payload("chunk", false).await.unwrap();
    sleep(Millis(25)).await;
    stream.reset(Reason::NO_ERROR);
    let _ = rx.await;
    assert!(client.is_ready());
    assert_eq!(opened.get(), 3);
}

#[ntex::test]
async fn test_on_capacity() {
    let srv = start_server().await;
    let io = connect(srv.addr()).await;
    let client = SimpleClient::new(io, Scheme::HTTP, "localhost".into());
    let cnt = Rc::new(Cell::new(0));
    let cnt2 = cnt.clone();
    client.on_capacity(move || cnt2.set(cnt2.get() + 1));

    // peer settings
    sleep(Millis(150)).await;
    assert_eq!(client.max_streams(), Some(1));
    assert_eq!(cnt.get(), 1);

    let (stream, recv_stream) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    assert_eq!(cnt.get(), 1);

    // stream is released after response
    stream.send_payload(Bytes::new(), true).await.unwrap();
    while recv_stream.recv().await.is_some() {}
    sleep(Millis(50)).await;
    assert_eq!(client.active_streams(), 0);
    assert_eq!(cnt.get(), 2);

    // connection is closed
    client.force_close();
    sleep(Millis(50)).await;
    assert!(client.is_closed());
    assert!(cnt.get() > 2);
}

#[ntex::test]
async fn test_stream_reservation() {
    let srv = start_server().await;
    let io = connect(srv.addr()).await;
    let client = SimpleClient::new(io, Scheme::HTTP, "localhost".into());
    let cnt = Rc::new(Cell::new(0));
    let cnt2 = cnt.clone();
    client.on_capacity(move || cnt2.set(cnt2.get() + 1));
    sleep(Millis(150)).await;
    assert_eq!(client.max_streams(), Some(1));
    let base = cnt.get();

    // reserved stream is counted
    let reservation = client.reserve().unwrap();
    assert!(format!("{reservation:?}").contains("StreamReservation"));
    assert_eq!(client.active_streams(), 1);
    assert!(!client.is_ready());
    assert!(client.reserve().is_none());

    // dropped reservation releases the stream
    drop(reservation);
    assert_eq!(client.active_streams(), 0);
    assert!(client.is_ready());
    assert_eq!(cnt.get(), base + 1);

    // reserved stream is used by the request
    let reservation = client.reserve().unwrap();
    let (stream, recv_stream) = reservation
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .unwrap();
    assert_eq!(client.active_streams(), 1);
    assert_eq!(cnt.get(), base + 1);
    stream.send_payload(Bytes::new(), true).await.unwrap();
    while recv_stream.recv().await.is_some() {}
    sleep(Millis(50)).await;
    assert_eq!(client.active_streams(), 0);
    assert_eq!(cnt.get(), base + 2);

    // failed request releases the reservation
    let reservation = client.reserve().unwrap();
    client.force_close();
    sleep(Millis(50)).await;
    assert!(
        reservation
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .is_err()
    );
    assert_eq!(client.active_streams(), 0);
    assert!(client.reserve().is_none());
}

#[ntex::test]
async fn test_stream_reservation_graceful_disconnect() {
    let srv = start_server().await;
    let client = SimpleClient::new(connect(srv.addr()).await, Scheme::HTTP, "localhost".into());
    sleep(Millis(150)).await;

    // reserved stream can be used during graceful disconnect
    let reservation = client.reserve().unwrap();
    client.close();
    sleep(Millis(50)).await;
    assert!(client.is_disconnecting());
    assert!(client.reserve().is_none());

    let (stream, recv_stream) = reservation
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .unwrap();
    stream.send_payload(Bytes::new(), true).await.unwrap();
    let msg = recv_stream.recv().await.unwrap();
    assert!(matches!(msg.kind, MessageKind::Headers { .. }));
    while recv_stream.recv().await.is_some() {}
    sleep(Millis(50)).await;
    assert!(client.is_closed());

    // dropped reservation completes graceful disconnect
    let client = SimpleClient::new(connect(srv.addr()).await, Scheme::HTTP, "localhost".into());
    sleep(Millis(150)).await;
    let reservation = client.reserve().unwrap();
    client.close();
    sleep(Millis(50)).await;
    assert!(!client.is_closed());
    drop(reservation);
    sleep(Millis(50)).await;
    assert!(client.is_closed());

    // completed stream does not close connection with reserved stream
    let srv = test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(async move |mut req: http::Request| {
                    let mut pl = req.take_payload();
                    pl.recv().await;
                    Ok::<_, io::Error>(Response::Ok().body("test body"))
                }),
            )
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(ServiceConfig::new().set_max_concurrent_streams(2)),
    );
    let client = SimpleClient::new(connect(srv.addr()).await, Scheme::HTTP, "localhost".into());
    sleep(Millis(150)).await;
    let (stream, recv_stream) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    let reservation = client.reserve().unwrap();
    client.close();
    stream.send_payload(Bytes::new(), true).await.unwrap();
    while recv_stream.recv().await.is_some() {}
    sleep(Millis(50)).await;
    assert!(!client.is_closed());
    let (stream, recv_stream) = reservation
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .unwrap();
    stream.send_payload(Bytes::new(), true).await.unwrap();
    while recv_stream.recv().await.is_some() {}
    sleep(Millis(50)).await;
    assert!(client.is_closed());
}

#[ntex::test]
async fn test_client_send_capacity_timeout() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
        .build();
    let client = SimpleClient::new(ntex::io::Io::new(io, cfg), Scheme::HTTP, "localhost".into());

    // peer allows 1 byte per stream and never updates the window
    srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 1]);
    sleep(Millis(50)).await;

    let (stream, _recv) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    let res = ntex::time::timeout(Millis(3000), stream.send_payload("test", true))
        .await
        .unwrap();
    assert!(matches!(
        &*res.unwrap_err(),
        ntex_h2::OperationError::Stream(ntex_h2::StreamError::CapacityTimeout)
    ));
}

#[ntex::test]
async fn test_client_send_capacity_wait_timeout() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
        .build();
    let client = SimpleClient::new(ntex::io::Io::new(io, cfg), Scheme::HTTP, "localhost".into());

    // peer sets zero stream window and never updates it
    srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0]);
    sleep(Millis(50)).await;

    let (stream, _recv) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    let res = ntex::time::timeout(Millis(3000), stream.send_capacity())
        .await
        .unwrap();
    assert!(matches!(
        &*res.unwrap_err(),
        ntex_h2::OperationError::Stream(ntex_h2::StreamError::CapacityTimeout)
    ));
}

#[ntex::test]
async fn test_recv_woken_by_local_reset() {
    let srv = start_server().await;
    let client = SimpleClient::new(connect(srv.addr()).await, Scheme::HTTP, "localhost".into());
    sleep(Millis(150)).await;

    let (stream, recv_stream) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    let recv = ntex::rt::spawn(async move { recv_stream.recv().await.is_none() });
    sleep(Millis(100)).await;

    // dropping unfinished send stream resets the stream
    drop(stream);
    let res = ntex::time::timeout(Millis(1000), recv).await;
    assert!(res.unwrap().unwrap());
}

#[ntex::test]
async fn test_recv_woken_by_capacity_timeout() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
        .build();
    let client = SimpleClient::new(ntex::io::Io::new(io, cfg), Scheme::HTTP, "localhost".into());

    // peer sets zero stream window and never updates it
    srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0]);
    sleep(Millis(50)).await;

    let (stream, recv_stream) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    let recv = ntex::rt::spawn(async move { recv_stream.recv().await.is_none() });

    assert!(stream.send_payload("test", true).await.is_err());
    let res = ntex::time::timeout(Millis(500), recv).await;
    assert!(res.unwrap().unwrap());
}

#[ntex::test]
async fn test_stale_capacity_timeout_ignored() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
        .build();
    let client = SimpleClient::new(ntex::io::Io::new(io, cfg), Scheme::HTTP, "localhost".into());

    // peer sets zero stream window and never updates it
    srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0]);
    sleep(Millis(50)).await;

    let (stream, _recv) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();

    // start capacity timer, then close the stream
    assert!(
        ntex::time::timeout(Millis(100), stream.send_capacity())
            .await
            .is_err()
    );
    assert!(stream.reset(Reason::CANCEL));

    // capacity timer fires for the closed stream
    sleep(Millis(2500)).await;
    assert!(matches!(
        &*stream.send_capacity().await.unwrap_err(),
        ntex_h2::OperationError::LocalReset(Reason::CANCEL)
    ));
    assert!(!client.is_closed());
}

#[ntex::test]
async fn test_send_after_response() {
    let (client, srv) = limited_client(10);
    sleep(Millis(50)).await;

    let (stream, recv_stream) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    stream.send_payload("chunk", false).await.unwrap();

    // complete response, HEADERS(:status 200, END_STREAM)
    srv.write([0, 0, 1, 1, 5, 0, 0, 0, 1, 0x88]);
    while recv_stream.recv().await.is_some() {}
    drop(recv_stream);
    sleep(Millis(50)).await;

    // request body is not cancelled
    assert_eq!(client.active_streams(), 1);
    stream.send_payload("chunk", true).await.unwrap();
    sleep(Millis(50)).await;
    assert_eq!(client.active_streams(), 0);
    drop(stream);
    assert!(!client.is_closed());
}

const PREFACE: [u8; 24] = *b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// The first frame after the preface must be SETTINGS.
#[ntex::test]
async fn test_first_frame_not_settings() {
    let srv = start_server().await;

    for first in [frame::Ping::new([1; 8]).into(), frame::Settings::ack().into()] {
        let io = connect(srv.addr()).await;
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
        io.send(first, &codec).await.unwrap();

        loop {
            match io.recv(&codec).await.unwrap().unwrap() {
                frame::Frame::Settings(_) | frame::Frame::WindowUpdate(_) => {}
                frame::Frame::GoAway(res) => {
                    assert_eq!(res.reason(), Reason::PROTOCOL_ERROR);
                    assert_eq!(res.data().as_ref(), b"First frame is not SETTINGS");
                    break;
                }
                frm => panic!("unexpected frame: {frm:?}"),
            }
        }
        assert!(io.recv(&codec).await.unwrap().is_none());
    }
}

/// Invalid SETTINGS frames close the connection with the RFC 9113 error code.
#[ntex::test]
async fn test_invalid_settings_reason() {
    let srv = start_server().await;

    fn settings(flags: u8, stream: u8, payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u32;
        let mut buf = len.to_be_bytes()[1..].to_vec();
        buf.extend_from_slice(&[4, flags, 0, 0, 0, stream]);
        buf.extend_from_slice(payload);
        buf
    }

    let cases = [
        // INITIAL_WINDOW_SIZE = 2^31
        (
            settings(0, 0, &[0, 4, 0x80, 0, 0, 0]),
            Reason::FLOW_CONTROL_ERROR,
            "Initial window size is above the maximum window size",
        ),
        // payload is not a multiple of 6
        (settings(0, 0, &[0, 4, 0, 0]), Reason::FRAME_SIZE_ERROR, ""),
        // ACK with payload
        (
            settings(1, 0, &[0, 4, 0, 0, 0, 1]),
            Reason::FRAME_SIZE_ERROR,
            "Received a payload with an ACK settings frame",
        ),
        // ENABLE_PUSH = 2
        (
            settings(0, 0, &[0, 2, 0, 0, 0, 2]),
            Reason::PROTOCOL_ERROR,
            "An invalid setting value was provided",
        ),
        // non-zero stream id
        (
            settings(0, 1, &[]),
            Reason::PROTOCOL_ERROR,
            "An invalid stream identifier was provided",
        ),
    ];

    for (frm, reason, data) in cases {
        let io = connect(srv.addr()).await;
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| {
            buf.extend_from_slice(&PREFACE);
            buf.extend_from_slice(&frm);
        });

        loop {
            match io.recv(&codec).await.unwrap().unwrap() {
                frame::Frame::Settings(_) | frame::Frame::WindowUpdate(_) => {}
                frame::Frame::GoAway(res) => {
                    assert_eq!(res.reason(), reason, "{frm:?}");
                    if !data.is_empty() {
                        assert_eq!(res.data().as_ref(), data.as_bytes());
                    }
                    break;
                }
                frm => panic!("unexpected frame: {frm:?}"),
            }
        }
        assert!(io.recv(&codec).await.unwrap().is_none());
    }
}

/// Streams over the concurrency limit are refused, the connection stays
/// usable until the peer keeps opening streams over the limit.
#[ntex::test]
async fn test_refuse_on_overflow() {
    let srv = start_server().await;
    let addr = srv.addr();

    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));

    let settings = frame::Settings::default();
    io.encode(settings.into(), &codec).unwrap();

    // settings & window
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;

    let id = frame::StreamId::CLIENT;
    let pseudo = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
    io.send(hdrs.clone().into(), &codec).await.unwrap();

    let id = id.next_id().unwrap();
    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
    io.send(hdrs.clone().into(), &codec).await.unwrap();

    let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::REFUSED_STREAM);

    // data sent before the reset is seen is ignored
    io.send(frame::Data::new(id, Bytes::from_static(b"data")).into(), &codec)
        .await
        .unwrap();

    let mut id = id.next_id().unwrap();
    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
    io.send(hdrs.clone().into(), &codec).await.unwrap();
    let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::REFUSED_STREAM);

    io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
    match io.recv(&codec).await.unwrap().unwrap() {
        frame::Frame::Ping(ping) => assert!(ping.is_ack()),
        frm => panic!("unexpected frame: {frm:?}"),
    }

    // continuous overflow hits the reset limit
    loop {
        id = id.next_id().unwrap();
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
        io.send(hdrs.into(), &codec).await.unwrap();
        match io.recv(&codec).await.unwrap().unwrap() {
            frame::Frame::Reset(rst) => assert_eq!(rst.reason(), Reason::REFUSED_STREAM),
            frame::Frame::GoAway(res) => {
                assert_eq!(res.reason(), Reason::FLOW_CONTROL_ERROR);
                assert_eq!(res.data().as_ref(), b"Stream rapid reset count achieved");
                break;
            }
            frm => panic!("unexpected frame: {frm:?}"),
        }
        assert!(u32::from(id) < 64, "reset limit is not reached");
    }
    assert!(io.recv(&codec).await.unwrap().is_none());
}

/// Padding counts toward flow control, the peer gets its window back even
/// though the padding is not delivered.
#[ntex::test]
async fn test_padding_is_flow_controlled() {
    let srv = start_idle_server(ServiceConfig::new()).await;
    let io = connect(srv.addr()).await;
    let codec = Codec::default();
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
    io.encode(frame::Settings::default().into(), &codec).unwrap();

    // settings & window
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;

    let id = frame::StreamId::CLIENT;
    let pseudo = frame::PseudoHeaders {
        method: Some(Method::POST),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    let hdrs = frame::Headers::new(id, pseudo, HeaderMap::new(), false);
    io.send(hdrs.into(), &codec).await.unwrap();

    // 255 frames of 256 flow-controlled bytes, 1 data byte each, fit into
    // the initial stream window
    let mut frm = vec![0, 1, 0, 0, 0x8, 0, 0, 0, 1, 254, b'x'];
    frm.resize(9 + 256, 0);
    for _ in 0..255 {
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&frm));
    }
    io.flush(true).await.unwrap();

    // the discarded padding is returned to the peer, the delivered data
    // alone stays below the window update threshold
    let upd = ntex::time::timeout(Millis(2000), async {
        loop {
            if let frame::Frame::WindowUpdate(upd) = io.recv(&codec).await.unwrap().unwrap()
                && upd.stream_id() == id
            {
                return upd;
            }
        }
    })
    .await
    .expect("stream window is not updated");
    assert!(upd.size_increment() > 255);
}

/// Starts a server that never reads request payloads.
async fn start_idle_server(cfg: ServiceConfig) -> test::TestServer {
    test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(async move |_: http::Request| {
                    sleep(Millis(10_000)).await;
                    Ok::<_, io::Error>(Response::Ok().build())
                }),
            )
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(cfg),
    )
}

/// Opens a raw connection and a POST stream, acknowledges the server settings.
async fn open_raw_stream(srv: &test::TestServer) -> (IoBoxed, Codec) {
    let io = connect(srv.addr()).await;
    let codec = Codec::default();
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
    io.encode(frame::Settings::default().into(), &codec).unwrap();

    // wait for the server settings and the ack of ours
    let mut settings = false;
    let mut ack = false;
    while !(settings && ack) {
        if let frame::Frame::Settings(s) = io.recv(&codec).await.unwrap().unwrap() {
            if s.is_ack() {
                ack = true;
            } else {
                settings = true;
                io.encode(frame::Settings::ack().into(), &codec).unwrap();
            }
        }
    }

    let pseudo = frame::PseudoHeaders {
        method: Some(Method::POST),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    let hdrs = frame::Headers::new(frame::StreamId::CLIENT, pseudo, HeaderMap::new(), false);
    io.send(hdrs.into(), &codec).await.unwrap();
    // let the server process the settings ack and the stream
    sleep(Millis(50)).await;
    (io, codec)
}

/// SETTINGS_INITIAL_WINDOW_SIZE that overflows a stream window is
/// a connection error, the settings are not acknowledged.
#[ntex::test]
async fn test_settings_window_overflow() {
    let srv = start_idle_server(ServiceConfig::new()).await;
    let (io, codec) = open_raw_stream(&srv).await;

    // grow the server's send window of the stream to the maximum
    let upd = frame::WindowUpdate::new(
        frame::StreamId::CLIENT,
        frame::MAX_INITIAL_WINDOW_SIZE as u32 - frame::DEFAULT_INITIAL_WINDOW_SIZE as u32,
    );
    io.send(upd.into(), &codec).await.unwrap();

    let mut settings = frame::Settings::default();
    settings.set_initial_window_size(Some(frame::DEFAULT_INITIAL_WINDOW_SIZE as u32 + 1));
    io.send(settings.into(), &codec).await.unwrap();

    let res = goaway(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::FLOW_CONTROL_ERROR);
    assert!(io.recv(&codec).await.unwrap().is_none());
}

/// Sends `count` DATA frames of `size` bytes on the client stream.
async fn send_data(io: &IoBoxed, codec: &Codec, count: usize, size: usize) {
    for _ in 0..count {
        let data = frame::Data::new(frame::StreamId::CLIENT, Bytes::from(vec![b'x'; size]));
        io.encode(data.into(), codec).unwrap();
    }
    io.flush(true).await.unwrap();
}

/// Data beyond the stream receive window resets the stream.
#[ntex::test]
async fn test_stream_recv_window_exceeded() {
    let srv = start_idle_server(ServiceConfig::new()).await;
    let (io, codec) = open_raw_stream(&srv).await;

    // 80 KiB exceeds the 64 KiB stream window, not the 1 MiB connection window
    send_data(&io, &codec, 5, 16_384).await;

    let rst = ntex::time::timeout(Millis(2000), async {
        loop {
            if let frame::Frame::Reset(rst) = io.recv(&codec).await.unwrap().unwrap() {
                return rst;
            }
        }
    })
    .await
    .expect("stream is not reset");
    assert_eq!(rst.stream_id(), frame::StreamId::CLIENT);
    assert_eq!(rst.reason(), Reason::FLOW_CONTROL_ERROR);
}

/// The frame that exceeds the stream receive window does not update the
/// stream window, the stream is reset.
#[ntex::test]
async fn test_stream_recv_window_exceeded_no_window_update() {
    // the window update threshold is a third of the window
    let srv = start_idle_server(ServiceConfig::new().set_initial_window_size(30_000)).await;
    let (io, codec) = open_raw_stream(&srv).await;

    send_data(&io, &codec, 2, 16_384).await;

    let rst = ntex::time::timeout(Millis(2000), async {
        loop {
            match io.recv(&codec).await.unwrap().unwrap() {
                frame::Frame::Reset(rst) => return rst,
                frame::Frame::WindowUpdate(upd) if upd.stream_id() == frame::StreamId::CLIENT => {
                    panic!("unexpected {upd:?}")
                }
                _ => (),
            }
        }
    })
    .await
    .expect("stream is not reset");
    assert_eq!(rst.stream_id(), frame::StreamId::CLIENT);
    assert_eq!(rst.reason(), Reason::FLOW_CONTROL_ERROR);
}

/// Data beyond the connection receive window closes the connection.
#[ntex::test]
async fn test_connection_recv_window_exceeded() {
    let srv = start_idle_server(
        ServiceConfig::new()
            .set_initial_window_size(1_048_576)
            .set_initial_connection_window_size(65_535)
            .set_max_frame_size(1_048_576),
    )
    .await;
    let (io, codec) = open_raw_stream(&srv).await;
    codec.set_send_frame_size(1_048_576);

    // the connection window is updated as data arrives, a single 80 KiB frame
    // exceeds the 64 KiB connection window, not the 1 MiB stream window
    send_data(&io, &codec, 1, 81_920).await;

    let res = ntex::time::timeout(Millis(2000), async {
        loop {
            match io.recv(&codec).await.unwrap().unwrap() {
                frame::Frame::GoAway(res) => return res,
                frame::Frame::Reset(rst) => panic!("unexpected {rst:?}"),
                _ => (),
            }
        }
    })
    .await
    .expect("connection is not closed");
    assert_eq!(res.reason(), Reason::FLOW_CONTROL_ERROR);
}

#[ntex::test]
async fn test_stream_cancel() {
    let srv = start_server().await;
    let addr = srv.addr();

    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));

    let settings = frame::Settings::default();
    io.encode(settings.into(), &codec).unwrap();

    // settings & window
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;

    let id = frame::StreamId::CLIENT;
    let pseudo = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };

    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
    io.send(hdrs.into(), &codec).await.unwrap();
    io.send(frame::Reset::new(id, frame::Reason::CANCEL).into(), &codec)
        .await
        .unwrap();

    let reset = get_reset(io.recv(&codec).await.unwrap().unwrap());
    assert!(reset.reason() == frame::Reason::CANCEL);
}

#[ntex::test]
async fn test_goaway_on_reset() {
    let srv = start_server().await;
    let addr = srv.addr();

    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));

    let settings = frame::Settings::default();
    io.encode(settings.into(), &codec).unwrap();

    // settings & window
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;

    let mut id = frame::StreamId::CLIENT;
    let pseudo = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    for _ in 0..5 {
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), true);
        id = id.next_id().unwrap();
        io.send(hdrs.into(), &codec).await.unwrap();
        io.recv(&codec).await.unwrap().unwrap(); // headers
        io.recv(&codec).await.unwrap().unwrap(); // data
        io.recv(&codec).await.unwrap().unwrap(); // data eof
    }
    for _ in 0..4 {
        let rst = frame::Reset::new(id, Reason::NO_ERROR);
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
        id = id.next_id().unwrap();
        io.encode(hdrs.into(), &codec).unwrap();
        io.send(rst.into(), &codec).await.unwrap();
        io.recv(&codec).await.unwrap().unwrap(); // headers
    }
    let rst = frame::Reset::new(id, Reason::NO_ERROR);
    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
    io.encode(hdrs.into(), &codec).unwrap();
    io.send(rst.into(), &codec).await.unwrap();

    let res = goaway(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::FLOW_CONTROL_ERROR);
    assert!(io.recv(&codec).await.unwrap().is_none());
}

#[ntex::test]
async fn test_goaway_on_reset2() {
    let srv = start_server().await;
    let addr = srv.addr();

    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));

    let settings = frame::Settings::default();
    io.encode(settings.into(), &codec).unwrap();

    // settings & window
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;

    let mut id = frame::StreamId::CLIENT;
    let pseudo = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    for _ in 0..5 {
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), true);
        id = id.next_id().unwrap();
        io.send(hdrs.into(), &codec).await.unwrap();
        io.recv(&codec).await.unwrap().unwrap(); // headers
        io.recv(&codec).await.unwrap().unwrap(); // data
        io.recv(&codec).await.unwrap().unwrap(); // data eof
    }

    for _ in 0..4 {
        let rst = frame::Reset::new(id, Reason::NO_ERROR);
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), true);
        id = id.next_id().unwrap();
        io.encode(hdrs.into(), &codec).unwrap();
        io.send(rst.into(), &codec).await.unwrap();
        io.recv(&codec).await.unwrap().unwrap(); // headers
        io.recv(&codec).await.unwrap().unwrap(); // data
        io.recv(&codec).await.unwrap().unwrap(); // data eof
    }
    let rst = frame::Reset::new(id, Reason::NO_ERROR);
    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), true);
    io.encode(hdrs.into(), &codec).unwrap();
    io.send(rst.into(), &codec).await.unwrap();
    io.recv(&codec).await.unwrap().unwrap(); // headers
    io.recv(&codec).await.unwrap().unwrap(); // data
    io.recv(&codec).await.unwrap().unwrap(); // data eof

    let res = goaway(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::FLOW_CONTROL_ERROR);
    assert!(io.recv(&codec).await.unwrap().is_none());
}

#[ntex::test]
async fn test_ping_timeout_on_idle() {
    let srv = test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(async move |mut req: http::Request| {
                    let mut pl = req.take_payload();
                    pl.recv().await;
                    Ok::<_, io::Error>(Response::Ok().body("test body"))
                }),
            )
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(
            ServiceConfig::new()
                .set_max_concurrent_streams(1)
                .set_ping_timeout(Seconds(1)),
        ),
    );

    let addr = srv.addr();
    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));

    let settings = frame::Settings::default();
    io.encode(settings.into(), &codec).unwrap();

    // settings & window
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;

    // ping & goaway
    let _ = io.recv(&codec).await;
    let _ = goaway(io.recv(&codec).await.unwrap().unwrap());

    sleep(Millis(1000)).await;
    assert!(io.is_closed());
}

#[ntex::test]
async fn test_max_headers() {
    let srv = test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(|_: http::Request| async move {
                    Ok::<_, io::Error>(Response::Ok().body("test body"))
                }),
            )
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(ServiceConfig::new().set_max_headers(5)),
    );

    let addr = srv.addr();
    let client = Client::builder("localhost")
        .scheme(Scheme::HTTPS)
        .connector(async move |_| Ok(connect(addr).await))
        .build(SharedCfg::default());
    assert!(client.is_ready());

    let mut hdrs = HeaderMap::new();
    for n in ["h1", "h2", "h3", "h4", "h5", "h6"] {
        hdrs.append(n.try_into().unwrap(), "123".try_into().unwrap());
    }

    let (_, rstream) = client.send(Method::GET, "/".into(), hdrs, true).await.unwrap();

    let r = rstream.recv().await.unwrap();
    let MessageKind::Eof(ntex_h2::StreamEof::Error(err)) = r.kind else {
        panic!()
    };
    assert_eq!(
        err.into_error(),
        ntex_h2::StreamError::Reset(frame::Reason::REFUSED_STREAM)
    );

    // the connection is still usable
    let (_, rstream) = client
        .send(Method::GET, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    let r = ntex::time::timeout(Millis(5_000), rstream.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(r.kind, MessageKind::Headers { .. }), "{r:?}");
}

/// A malformed request resets the stream, the connection stays usable.
#[ntex::test]
async fn test_malformed_headers_reset_stream() {
    let srv = start_idle_server(ServiceConfig::new()).await;
    let (io, codec) = open_raw_stream(&srv).await;

    let pseudo = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    let mut hdrs = HeaderMap::new();
    hdrs.insert(http::header::CONNECTION, "close".try_into().unwrap());
    let id = frame::StreamId::from(3);
    io.send(frame::Headers::new(id, pseudo, hdrs, true).into(), &codec)
        .await
        .unwrap();
    assert_eq!(
        io.recv(&codec).await.unwrap().unwrap(),
        frame::Frame::Reset(frame::Reset::new(id, Reason::PROTOCOL_ERROR))
    );

    io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
    match io.recv(&codec).await.unwrap().unwrap() {
        frame::Frame::Ping(ping) => assert!(ping.is_ack()),
        frm => panic!("unexpected frame: {frm:?}"),
    }
}

/// A request with missing or unexpected pseudo headers resets the stream,
/// the connection stays usable.
#[ntex::test]
async fn test_invalid_pseudo_reset_stream() {
    let srv = start_idle_server(ServiceConfig::new()).await;
    let (io, codec) = open_raw_stream(&srv).await;

    let valid = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    let cases = [
        frame::PseudoHeaders {
            path: None,
            ..valid.clone()
        },
        frame::PseudoHeaders {
            method: None,
            ..valid.clone()
        },
        frame::PseudoHeaders {
            scheme: None,
            ..valid.clone()
        },
        frame::PseudoHeaders {
            status: Some(http::StatusCode::OK),
            ..valid.clone()
        },
        frame::PseudoHeaders {
            protocol: Some("websocket".into()),
            ..valid.clone()
        },
    ];

    let mut id = frame::StreamId::from(3);
    for pseudo in cases {
        io.send(
            frame::Headers::new(id, pseudo, HeaderMap::new(), false).into(),
            &codec,
        )
        .await
        .unwrap();
        assert_eq!(
            io.recv(&codec).await.unwrap().unwrap(),
            frame::Frame::Reset(frame::Reset::new(id, Reason::PROTOCOL_ERROR))
        );

        // data sent before the reset is seen is ignored
        io.send(frame::Data::new(id, Bytes::from_static(b"data")).into(), &codec)
            .await
            .unwrap();
        id = id.next_id().unwrap();
    }

    io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
    match io.recv(&codec).await.unwrap().unwrap() {
        frame::Frame::Ping(ping) => assert!(ping.is_ack()),
        frm => panic!("unexpected frame: {frm:?}"),
    }
}

/// A response with too many headers fails the client stream, instead of
/// closing the connection.
#[ntex::test]
async fn test_client_max_headers() {
    let srv = test::server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(|req: http::Request| async move {
                let mut resp = Response::Ok();
                if req.path() == "/large" {
                    for n in ["h1", "h2", "h3", "h4", "h5", "h6"] {
                        resp.header(n, "123");
                    }
                }
                Ok::<_, io::Error>(resp.body("test body"))
            }),
        )
        .map_err(|_| ())
    });

    let addr = srv.addr();
    let client = Client::builder("localhost")
        .scheme(Scheme::HTTPS)
        .connector(async move |_| Ok(connect(addr).await))
        .build(SharedCfg::new("CLI").add(ServiceConfig::new().set_max_headers(5)));

    let (_, rstream) = client
        .send(Method::GET, "/large".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    let r = ntex::time::timeout(Millis(5_000), rstream.recv())
        .await
        .unwrap()
        .unwrap();
    let id = r.id();
    let MessageKind::Eof(ntex_h2::StreamEof::Error(err)) = r.kind else {
        panic!("unexpected message: {r:?}")
    };
    assert_eq!(
        err.into_error(),
        ntex_h2::StreamError::InvalidFrame(frame::FrameError::TooManyHeaders(id))
    );

    let (_, rstream) = client
        .send(Method::GET, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    let r = ntex::time::timeout(Millis(5_000), rstream.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(r.kind, MessageKind::Headers { .. }), "{r:?}");
}

#[ntex::test]
async fn test_capacity_timeout() {
    let srv = start_server().await;
    let addr = srv.addr();

    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));

    let mut settings = frame::Settings::default();
    settings.set_initial_window_size(Some(1));
    io.encode(settings.into(), &codec).unwrap();

    // settings & window
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;

    let pseudo = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    let hdrs = frame::Headers::new(frame::StreamId::CLIENT, pseudo.clone(), HeaderMap::new(), true);
    io.send(hdrs.into(), &codec).await.unwrap();
    io.recv(&codec).await.unwrap().unwrap(); // headers
    io.recv(&codec).await.unwrap().unwrap(); // data
    let res = io.recv(&codec).await.unwrap().unwrap(); // reset on timeout
    assert_eq!(
        res,
        frame::Frame::Reset(frame::Reset::new(frame::StreamId::CLIENT, Reason::CANCEL,))
    );

    // success
    let pseudo = frame::PseudoHeaders {
        method: Some(Method::GET),
        scheme: Some("HTTPS".into()),
        authority: Some("localhost".into()),
        path: Some("/".into()),
        ..Default::default()
    };
    let id = frame::StreamId::CLIENT.next_id().unwrap();
    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), true);
    io.send(hdrs.into(), &codec).await.unwrap();
    io.recv(&codec).await.unwrap().unwrap(); // headers
    let res = io.recv(&codec).await.unwrap().unwrap(); // data
    assert_eq!(
        res,
        frame::Frame::Data(frame::Data::new(id, Bytes::copy_from_slice(b"t")))
    );
    let _ = io.send(frame::WindowUpdate::new(id, 16).into(), &codec).await;
    let res = io.recv(&codec).await.unwrap().unwrap(); // rest
    assert_eq!(
        res,
        frame::Frame::Data(frame::Data::new(id, Bytes::copy_from_slice(b"est body")))
    );
}

#[ntex::test]
async fn test_con_lifetime() {
    let srv = test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(|_: http::Request| async move {
                    Ok::<_, io::Error>(Response::Ok().body("test body"))
                }),
            )
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(ServiceConfig::new()),
    );

    let addr = srv.addr();
    let pool = Client::builder("localhost")
        .scheme(Scheme::HTTPS)
        .lifetime(1)
        .connector(async move |_| Ok(connect(addr).await))
        .build(SharedCfg::default());
    assert!(pool.is_ready());

    let client = pool.client().await.unwrap();
    let id1 = client.id().clone();
    drop(client);
    let client = pool.client().await.unwrap();
    let id2 = client.id().clone();
    drop(client);
    assert_eq!(id1, id2);
    sleep(Seconds(2)).await;

    let client = pool.client().await.unwrap();
    let id3 = client.id().clone();
    assert_ne!(id1, id3);
}

/// PRIORITY with an invalid length is a stream error of type `FRAME_SIZE_ERROR`.
#[ntex::test]
async fn test_priority_invalid_length() {
    let srv = start_idle_server(ServiceConfig::new()).await;
    let (io, codec) = open_raw_stream(&srv).await;

    // 4 bytes payload on the open stream
    let _ = io.with_write_src(|buf| {
        buf.extend_from_slice(&[0, 0, 4, 2, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
    });
    let rst = loop {
        match io.recv(&codec).await.unwrap().unwrap() {
            frame::Frame::Reset(rst) => break rst,
            frame::Frame::WindowUpdate(_) => {}
            frm => panic!("unexpected frame: {frm:?}"),
        }
    };
    assert_eq!(rst.stream_id(), frame::StreamId::CLIENT);
    assert_eq!(rst.reason(), Reason::FRAME_SIZE_ERROR);

    // the connection is still usable
    io.send(frame::Ping::new([7; 8]).into(), &codec).await.unwrap();
    loop {
        match io.recv(&codec).await.unwrap().unwrap() {
            frame::Frame::Ping(ping) => {
                assert!(ping.is_ack());
                break;
            }
            frame::Frame::WindowUpdate(_) => {}
            frm => panic!("unexpected frame: {frm:?}"),
        }
    }
}

async fn start_echo_server() -> test::TestServer {
    test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(async move |req: http::Request| {
                    let body = format!("{} {}", req.method(), req.uri());
                    Ok::<_, io::Error>(Response::Ok().body(body))
                }),
            )
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(ServiceConfig::new()),
    )
}

/// CONNECT omits `:scheme` and `:path` (RFC 9113 §8.5).
#[ntex::test]
async fn test_connect_request() {
    let srv = start_echo_server().await;
    let (io, codec) = open_raw_stream(&srv).await;
    let _ = io.recv(&codec).await; // response for the POST stream

    let connect =
        |authority: Option<&str>, scheme: Option<&str>, path: Option<&str>| frame::PseudoHeaders {
            method: Some(Method::CONNECT),
            authority: authority.map(Into::into),
            scheme: scheme.map(Into::into),
            path: path.map(Into::into),
            ..Default::default()
        };
    let cases = [
        (connect(Some("example.com:443"), None, None), None),
        (connect(None, None, None), Some(Reason::PROTOCOL_ERROR)),
        (
            connect(Some("example.com:443"), Some("https"), None),
            Some(Reason::PROTOCOL_ERROR),
        ),
        (
            connect(Some("example.com:443"), None, Some("/")),
            Some(Reason::PROTOCOL_ERROR),
        ),
        // extended CONNECT is not enabled (RFC 8441 §4)
        (
            frame::PseudoHeaders {
                protocol: Some("websocket".into()),
                ..connect(Some("example.com:443"), Some("https"), Some("/"))
            },
            Some(Reason::PROTOCOL_ERROR),
        ),
    ];

    let mut id = frame::StreamId::CLIENT;
    for (pseudo, expected) in cases {
        id = id.next_id().unwrap();
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), true);
        io.send(hdrs.into(), &codec).await.unwrap();

        loop {
            match io.recv(&codec).await.unwrap().unwrap() {
                frame::Frame::Reset(rst) if rst.stream_id() == id => {
                    assert_eq!(Some(rst.reason()), expected, "{pseudo:?}");
                    break;
                }
                frame::Frame::Data(data) if data.stream_id() == id => {
                    assert_eq!(expected, None, "{pseudo:?}");
                    assert_eq!(data.payload().as_ref(), b"CONNECT example.com:443");
                    break;
                }
                _ => {}
            }
        }
    }
}

/// The client omits `:scheme` and `:path` for CONNECT.
#[ntex::test]
async fn test_client_connect_request() {
    let srv = start_echo_server().await;
    let addr = srv.addr();
    let client = Pipeline::new(
        SharedCfg::default(),
        client::Connector::new()
            .scheme(Scheme::HTTP)
            .connector(fn_service(move |_| async move { Ok(connect(addr).await) })),
    )
    .call("localhost:8080")
    .await
    .unwrap();

    let (_snd, rcv) = client
        .send(Method::CONNECT, "/ignored".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    let msg = rcv.recv().await.unwrap();
    assert!(matches!(msg.kind(), MessageKind::Headers { .. }), "{msg:?}");
    let msg = rcv.recv().await.unwrap();
    match msg.kind() {
        MessageKind::Data(data, _) => assert_eq!(data.as_ref(), b"CONNECT localhost:8080"),
        kind => panic!("unexpected message: {kind:?}"),
    }
}

/// A control service failure must release the open streams (F9).
#[ntex::test]
async fn test_control_error_releases_streams() {
    use ntex::io::{Io, testing::IoTest};
    use ntex_h2::{Control, Message, server};

    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1_000_000);
    srv.remote_buffer_cap(1_000_000);

    let (tx, rx) = ntex::channel::mpsc::channel::<Message>();
    let (done_tx, done_rx) = oneshot::channel();
    ntex::rt::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            let fail =
                matches!(msg.kind, MessageKind::Headers { .. }) && msg.id() != frame::StreamId::CLIENT;
            let _ = tx.send(msg);
            if fail { Err(()) } else { Ok(()) }
        })
        .control(async move |msg: Control<()>| {
            Err::<ntex_h2::ControlAck, _>(io::Error::other(format!("{msg:?}")))
        })
        .run(Io::new(srv, SharedCfg::default()))
        .await;
        let _ = done_tx.send(());
    });

    let client = SimpleClient::new(
        Io::new(cli, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );

    // stream 1 stays open, the server keeps it
    let (_snd1, _rcv1) = client
        .send(Method::POST, "/1".into(), HeaderMap::new(), false)
        .await
        .unwrap();
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.id(), frame::StreamId::CLIENT);

    // stream 3 fails in the publish service, the control service fails too
    let _ = client
        .send(Method::GET, "/3".into(), HeaderMap::new(), true)
        .await;
    let _ = rx.recv().await.unwrap();

    // stream 1 gets a disconnect message, stream 3 can get one too
    ntex::time::timeout(Millis(1_000), async {
        loop {
            let msg = rx.recv().await.unwrap();
            if msg.id() == frame::StreamId::CLIENT {
                assert!(matches!(msg.kind, MessageKind::Disconnect(_)), "{msg:?}");
                break;
            }
        }
    })
    .await
    .expect("stream 1 is not released");

    // the dispatcher completes
    ntex::time::timeout(Millis(1_000), done_rx)
        .await
        .expect("dispatcher did not complete")
        .unwrap();
}

/// Client limited to `max` concurrent streams by the peer.
fn limited_client(max: u8) -> (Rc<SimpleClient>, ntex::io::testing::IoTest) {
    let (io, srv) = ntex::io::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let client = SimpleClient::new(
        ntex::io::Io::new(io, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );
    srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, max]);
    (Rc::new(client), srv)
}

/// A request waiting for a stream slot fails when the connection is closed (N1).
#[ntex::test]
async fn test_stream_waiter_fails_on_disconnect() {
    let (client, srv) = limited_client(1);
    sleep(Millis(50)).await;
    let (_stream, _recv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();

    let c = client.clone();
    let waiter = ntex::rt::spawn(async move {
        c.send(Method::GET, "/".into(), HeaderMap::new(), true)
            .await
            .map(|_| ())
    });
    sleep(Millis(50)).await;

    srv.close().await;
    let res = ntex::time::timeout(Millis(1_000), waiter)
        .await
        .expect("waiter is not woken")
        .unwrap();
    assert!(matches!(
        &*res.unwrap_err(),
        ntex_h2::OperationError::Disconnected
    ));
}

/// A request waiting for a stream slot fails on graceful disconnect (N1).
#[ntex::test]
async fn test_stream_waiter_fails_on_graceful_disconnect() {
    let (client, _srv) = limited_client(1);
    sleep(Millis(50)).await;
    let (_stream, _recv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();

    let c = client.clone();
    let waiter = ntex::rt::spawn(async move {
        c.send(Method::GET, "/".into(), HeaderMap::new(), true)
            .await
            .map(|_| ())
    });
    sleep(Millis(50)).await;

    client.close();
    let res = ntex::time::timeout(Millis(1_000), waiter)
        .await
        .expect("waiter is not woken")
        .unwrap();
    assert!(matches!(
        &*res.unwrap_err(),
        ntex_h2::OperationError::Disconnecting
    ));
}

/// Released stream slots wake the waiters, back to back releases included (N2).
#[ntex::test]
async fn test_stream_waiters_woken_on_release() {
    let (client, _srv) = limited_client(2);
    sleep(Millis(50)).await;
    let (s1, _r1) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();
    let (s2, _r2) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();

    let c = client.clone();
    let a = ntex::rt::spawn(async move {
        c.send(Method::POST, "/".into(), HeaderMap::new(), false)
            .await
            .unwrap()
    });
    let c = client.clone();
    let b = ntex::rt::spawn(async move {
        c.send(Method::POST, "/".into(), HeaderMap::new(), false)
            .await
            .unwrap()
    });
    sleep(Millis(50)).await;

    // both slots are released before the waiters run
    s1.reset(Reason::CANCEL);
    s2.reset(Reason::CANCEL);
    let _a = ntex::time::timeout(Millis(1_000), a)
        .await
        .expect("waiter a is not woken")
        .unwrap();
    let _b = ntex::time::timeout(Millis(1_000), b)
        .await
        .expect("waiter b is not woken")
        .unwrap();
}

/// A woken waiter that is dropped passes the wake up to the next waiter (N2).
#[ntex::test]
async fn test_dropped_stream_waiter_passes_wakeup() {
    use std::{future::Future, task::Poll};

    let (client, _srv) = limited_client(1);
    sleep(Millis(50)).await;
    let (s1, _r1) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();

    // first waiter is registered, but never polled again
    let mut a = Box::pin(client.send(Method::POST, "/".into(), HeaderMap::new(), false));
    std::future::poll_fn(|cx| {
        assert!(a.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;

    let c = client.clone();
    let b = ntex::rt::spawn(async move {
        c.send(Method::POST, "/".into(), HeaderMap::new(), false)
            .await
            .unwrap()
    });
    sleep(Millis(50)).await;

    // the slot wakes the first waiter, it is dropped
    s1.reset(Reason::CANCEL);
    drop(a);
    let _b = ntex::time::timeout(Millis(1_000), b)
        .await
        .expect("waiter b is not woken")
        .unwrap();
}

/// `RST_STREAM(NO_ERROR)` after a complete response stops the request body,
/// the response is not failed (N4).
#[ntex::test]
async fn test_no_error_reset_after_complete_response() {
    let (client, srv) = limited_client(10);
    sleep(Millis(50)).await;

    let (stream, recv_stream) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();

    // HEADERS(:status 200), DATA("ok", END_STREAM), RST_STREAM(NO_ERROR)
    srv.write([0, 0, 1, 1, 4, 0, 0, 0, 1, 0x88]);
    srv.write([0, 0, 2, 0, 1, 0, 0, 0, 1, b'o', b'k']);
    srv.write([0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
    sleep(Millis(50)).await;

    let msg = recv_stream.recv().await.unwrap();
    assert!(matches!(msg.kind, MessageKind::Headers { eof: false, .. }));
    let msg = recv_stream.recv().await.unwrap();
    assert!(
        matches!(msg.kind, MessageKind::Eof(ntex_h2::StreamEof::Data(ref d, _)) if d == "ok"),
        "{msg:?}"
    );
    assert!(recv_stream.recv().await.is_none());

    // request body is stopped, the connection is not affected
    assert_eq!(client.active_streams(), 0);
    assert!(stream.send_payload("chunk", true).await.is_err());
    assert!(!client.is_closed());
}

/// Request body sent after a complete response reaches the application.
#[ntex::test]
async fn test_request_body_after_response_is_published() {
    use ntex::io::{Io, testing::IoTest};
    use ntex_h2::{Message, server};

    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1_000_000);
    srv.remote_buffer_cap(1_000_000);

    let (tx, rx) = ntex::channel::mpsc::channel::<Message>();
    ntex::rt::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            if matches!(msg.kind, MessageKind::Headers { .. }) {
                // complete response, the request body is still open
                msg.stream()
                    .send_response(http::StatusCode::OK, HeaderMap::new(), true)
                    .unwrap();
            }
            let _ = tx.send(msg);
            Ok::<_, ()>(())
        })
        .run(Io::new(srv, SharedCfg::default()))
        .await;
    });

    let client = SimpleClient::new(
        Io::new(cli, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );
    let (snd, rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();
    let msg = rcv.recv().await.unwrap();
    assert!(matches!(msg.kind, MessageKind::Headers { eof: true, .. }));

    snd.send_payload("chunk", false).await.unwrap();
    snd.send_payload("", true).await.unwrap();

    let recv = async {
        let mut msgs = Vec::new();
        while msgs.len() < 3 {
            msgs.push(rx.recv().await.unwrap().kind);
        }
        msgs
    };
    let msgs = ntex::time::timeout(Millis(1_000), recv)
        .await
        .expect("request body is not published");
    assert!(matches!(msgs[1], MessageKind::Data(ref d, _) if d == "chunk"));
    assert!(matches!(msgs[2], MessageKind::Eof(_)));
}

/// Informational responses are sent before the final response and keep the
/// stream idle.
#[ntex::test]
async fn test_server_informational_responses() {
    use ntex::io::{Io, testing::IoTest};
    use ntex_h2::{Message, OperationError, server};

    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1_000_000);
    srv.remote_buffer_cap(1_000_000);

    ntex::rt::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            if matches!(msg.kind, MessageKind::Headers { .. }) {
                let stream = msg.stream();
                stream
                    .send_informational(http::StatusCode::EARLY_HINTS, HeaderMap::new())
                    .unwrap();
                stream
                    .send_informational(http::StatusCode::CONTINUE, HeaderMap::new())
                    .unwrap();
                stream
                    .send_response(http::StatusCode::OK, HeaderMap::new(), false)
                    .unwrap();
                let err = stream
                    .send_informational(http::StatusCode::CONTINUE, HeaderMap::new())
                    .unwrap_err();
                assert!(matches!(*err, OperationError::Payload));
                stream.send_payload("ok", true).await.unwrap();
            }
            Ok::<_, ()>(())
        })
        .run(Io::new(srv, SharedCfg::default()))
        .await;
    });

    let client = SimpleClient::new(
        Io::new(cli, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );
    let (_snd, rcv) = client
        .send(Method::GET, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    for status in [103, 100, 200] {
        let msg = rcv.recv().await.unwrap();
        let MessageKind::Headers { pseudo, eof, .. } = msg.kind else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(pseudo.status.unwrap().as_u16(), status);
        assert!(!eof);
    }
    let msg = rcv.recv().await.unwrap();
    assert!(
        matches!(msg.kind, MessageKind::Eof(ntex_h2::StreamEof::Data(ref data, _)) if data == "ok"),
        "{msg:?}"
    );
    assert!(!client.is_closed());
}

/// Remote reset of a server stream publishes the final error message.
#[ntex::test]
async fn test_remote_reset_is_published() {
    use ntex::io::{Io, testing::IoTest};
    use ntex_h2::{Message, StreamEof, server};

    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1_000_000);
    srv.remote_buffer_cap(1_000_000);

    let (tx, rx) = ntex::channel::mpsc::channel::<Message>();
    ntex::rt::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            let _ = tx.send(msg);
            Ok::<_, ()>(())
        })
        .run(Io::new(srv, SharedCfg::default()))
        .await;
    });

    let client = SimpleClient::new(
        Io::new(cli, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );
    let (snd, _rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();
    let msg = rx.recv().await.unwrap();
    assert!(matches!(msg.kind, MessageKind::Headers { .. }));

    snd.reset(Reason::CANCEL);
    let msg = ntex::time::timeout(Millis(1_000), rx.recv())
        .await
        .expect("reset is not published")
        .unwrap();
    assert!(
        matches!(msg.kind, MessageKind::Eof(StreamEof::Error(_))),
        "{msg:?}"
    );
}

/// Remote reset cancels the in-flight HEADERS publish call after DATA
/// frames are published for the stream.
#[ntex::test]
async fn test_remote_reset_cancels_handler_after_data() {
    use ntex::io::{Io, testing::IoTest};
    use ntex_h2::{Message, frame::StreamId, server};
    use std::{cell::RefCell, future::pending};

    struct Guard(StreamId, Rc<RefCell<Vec<StreamId>>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1_000_000);
    srv.remote_buffer_cap(1_000_000);

    let dropped = Rc::new(RefCell::new(Vec::new()));
    let (tx, rx) = ntex::channel::mpsc::channel::<()>();
    let dropped2 = dropped.clone();
    ntex::rt::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            let _ = tx.send(());
            if matches!(msg.kind, MessageKind::Headers { .. }) {
                let _guard = Guard(msg.id(), dropped2.clone());
                pending::<()>().await;
            }
            Ok::<_, ()>(())
        })
        .run(Io::new(srv, SharedCfg::default()))
        .await;
    });

    let client = SimpleClient::new(
        Io::new(cli, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );

    // the first pending call is polled by the dispatcher task itself,
    // the handler of the second stream runs in a spawned task
    let (_snd1, _rcv1) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();
    rx.recv().await.unwrap();

    let (snd, _rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();
    rx.recv().await.unwrap();
    snd.send_payload(Bytes::from_static(b"data"), false)
        .await
        .unwrap();
    rx.recv().await.unwrap();
    assert!(dropped.borrow().is_empty());

    snd.reset(Reason::CANCEL);
    sleep(Millis(100)).await;
    assert_eq!(&*dropped.borrow(), &[snd.id()], "handler is not cancelled");
}

/// Remote reset cancels all in-flight publish calls of the stream,
/// including DATA publish calls.
#[ntex::test]
async fn test_remote_reset_cancels_data_publish() {
    use ntex::io::{Io, testing::IoTest};
    use ntex_h2::{Message, server};
    use std::future::pending;

    struct Guard(Rc<Cell<usize>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1_000_000);
    srv.remote_buffer_cap(1_000_000);

    let dropped = Rc::new(Cell::new(0));
    let (tx, rx) = ntex::channel::mpsc::channel::<()>();
    let dropped2 = dropped.clone();
    ntex::rt::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            let _ = tx.send(());
            if matches!(msg.kind, MessageKind::Data(..)) {
                let _guard = Guard(dropped2.clone());
                pending::<()>().await;
            }
            Ok::<_, ()>(())
        })
        .run(Io::new(srv, SharedCfg::default()))
        .await;
    });

    let client = SimpleClient::new(
        Io::new(cli, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );

    let (snd, _rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();
    rx.recv().await.unwrap();
    for _ in 0..2 {
        snd.send_payload(Bytes::from_static(b"data"), false)
            .await
            .unwrap();
        rx.recv().await.unwrap();
    }
    assert_eq!(dropped.get(), 0);

    snd.reset(Reason::CANCEL);
    sleep(Millis(100)).await;
    assert_eq!(dropped.get(), 2, "DATA publish calls are not cancelled");
}

/// The connection stops processing frames once the in-flight publish
/// calls limit is reached.
#[ntex::test]
async fn test_max_inflight_messages() {
    use ntex::io::{Io, testing::IoTest};
    use ntex_h2::{Message, server};

    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1_000_000);
    srv.remote_buffer_cap(1_000_000);

    let (tx, rx) = ntex::channel::mpsc::channel::<()>();
    let (done_tx, done_rx) = ntex::channel::mpsc::channel::<()>();
    let done_rx = Rc::new(done_rx);
    ntex::rt::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            let _ = tx.send(());
            if matches!(msg.kind, MessageKind::Data(..)) {
                let _ = done_rx.recv().await;
            }
            Ok::<_, ()>(())
        })
        .run(Io::new(
            srv,
            SharedCfg::new("SRV").add(ServiceConfig::new().set_max_inflight_messages(2)),
        ))
        .await;
    });

    let client = SimpleClient::new(
        Io::new(cli, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );

    let (snd, _rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();
    rx.recv().await.unwrap();
    for _ in 0..3 {
        snd.send_payload(Bytes::from_static(b"data"), false)
            .await
            .unwrap();
    }
    sleep(Millis(100)).await;

    // the third DATA frame waits for a completed publish call
    rx.recv().await.unwrap();
    rx.recv().await.unwrap();
    assert!(
        ntex::time::timeout(Millis(100), rx.recv()).await.is_err(),
        "the in-flight limit is not applied"
    );

    done_tx.send(()).unwrap();
    rx.recv().await.unwrap();
}

/// Late HEADERS for closed client streams do not affect the connection,
/// HEADERS for an idle client stream is a connection error (N5).
#[ntex::test]
async fn test_client_late_headers_for_closed_streams() {
    let (client, srv) = limited_client(10);
    sleep(Millis(50)).await;

    // streams 1 and 3 are complete
    let mut streams = Vec::new();
    for _ in 0..2 {
        streams.push(
            client
                .send(Method::GET, "/".into(), HeaderMap::new(), true)
                .await
                .unwrap(),
        );
    }
    srv.write([0, 0, 1, 1, 5, 0, 0, 0, 1, 0x88]);
    srv.write([0, 0, 1, 1, 5, 0, 0, 0, 3, 0x88]);
    for (_, rcv) in &streams {
        let msg = rcv.recv().await.unwrap();
        assert!(matches!(msg.kind, MessageKind::Headers { eof: true, .. }));
    }
    drop(streams);
    sleep(Millis(50)).await;
    assert_eq!(client.active_streams(), 0);

    // late HEADERS for stream 3, then for stream 1
    srv.write([0, 0, 1, 1, 5, 0, 0, 0, 3, 0x88]);
    srv.write([0, 0, 1, 1, 5, 0, 0, 0, 1, 0x88]);
    sleep(Millis(50)).await;
    assert!(!client.is_closed());
    let (_snd, rcv) = client
        .send(Method::GET, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    srv.write([0, 0, 1, 1, 5, 0, 0, 0, 5, 0x88]);
    let msg = rcv.recv().await.unwrap();
    assert!(matches!(msg.kind, MessageKind::Headers { eof: true, .. }));

    // HEADERS for idle client stream
    srv.write([0, 0, 1, 1, 5, 0, 0, 0, 101, 0x88]);
    sleep(Millis(50)).await;
    assert!(client.is_closed());
}

/// Interim responses are delivered before the final response, an interim
/// response with `END_STREAM` and `101` are malformed.
#[ntex::test]
async fn test_client_interim_responses() {
    let (client, srv) = limited_client(10);
    sleep(Millis(50)).await;

    let (_snd, rcv) = client
        .send(Method::GET, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    // 103, 100, 200 and data
    srv.write([0, 0, 5, 1, 4, 0, 0, 0, 1, 0x08, 3, b'1', b'0', b'3']);
    srv.write([0, 0, 5, 1, 4, 0, 0, 0, 1, 0x08, 3, b'1', b'0', b'0']);
    srv.write([0, 0, 1, 1, 4, 0, 0, 0, 1, 0x88]);
    srv.write([0, 0, 2, 0, 1, 0, 0, 0, 1, b'o', b'k']);
    for status in [103, 100, 200] {
        let msg = rcv.recv().await.unwrap();
        let MessageKind::Headers { pseudo, eof, .. } = msg.kind else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(pseudo.status.unwrap().as_u16(), status);
        assert!(!eof);
    }
    let msg = rcv.recv().await.unwrap();
    assert!(
        matches!(msg.kind, MessageKind::Eof(ntex_h2::StreamEof::Data(ref data, _)) if data == "ok"),
        "{msg:?}"
    );

    // interim response with END_STREAM, `101`
    for (id, status, flags) in [(3, b'3', 5), (5, b'1', 4)] {
        let (_snd, rcv) = client
            .send(Method::GET, "/".into(), HeaderMap::new(), true)
            .await
            .unwrap();
        srv.write([0, 0, 5, 1, flags, 0, 0, 0, id, 0x08, 3, b'1', b'0', status]);
        let msg = rcv.recv().await.unwrap();
        let MessageKind::Eof(ntex_h2::StreamEof::Error(err)) = msg.kind else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(err.into_error(), ntex_h2::StreamError::InvalidInformational);
    }
    assert!(!client.is_closed());
}

/// A response to a HEAD request has no content, its `content-length`
/// describes the content of a GET response.
#[ntex::test]
async fn test_client_head_response() {
    let (client, srv) = limited_client(10);
    sleep(Millis(50)).await;

    // 200 with `content-length: 10`, then empty DATA with END_STREAM
    let (_snd, rcv) = client
        .send(Method::HEAD, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    srv.write([0, 0, 6, 1, 4, 0, 0, 0, 1, 0x88, 0x0f, 0x0d, 2, b'1', b'0']);
    srv.write([0, 0, 0, 0, 1, 0, 0, 0, 1]);
    let msg = rcv.recv().await.unwrap();
    let MessageKind::Headers { pseudo, headers, eof } = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(pseudo.status.unwrap().as_u16(), 200);
    assert_eq!(headers.get("content-length").unwrap(), "10");
    assert!(!eof);
    let msg = rcv.recv().await.unwrap();
    assert!(
        matches!(msg.kind, MessageKind::Eof(ntex_h2::StreamEof::Data(ref data, _)) if data.is_empty()),
        "{msg:?}"
    );

    // content in a HEAD response is an error
    let (_snd, rcv) = client
        .send(Method::HEAD, "/".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    srv.write([0, 0, 1, 1, 4, 0, 0, 0, 3, 0x88]);
    srv.write([0, 0, 2, 0, 1, 0, 0, 0, 3, b'o', b'k']);
    let msg = rcv.recv().await.unwrap();
    assert!(matches!(msg.kind, MessageKind::Headers { .. }), "{msg:?}");
    let msg = rcv.recv().await.unwrap();
    let MessageKind::Eof(ntex_h2::StreamEof::Error(err)) = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(err.into_error(), ntex_h2::StreamError::NonEmptyPayload);
    assert!(!client.is_closed());
}

/// A peer that does not acknowledge the local settings in time gets
/// `GOAWAY(SETTINGS_TIMEOUT)` (RFC 9113 §6.5.3).
#[ntex::test]
async fn test_settings_timeout() {
    for ack in [false, true] {
        let srv = start_idle_server(
            ServiceConfig::new()
                .set_ping_timeout(Seconds::ZERO)
                .set_settings_timeout(Seconds(1)),
        )
        .await;
        let io = connect(srv.addr()).await;
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
        io.encode(frame::Settings::default().into(), &codec).unwrap();

        loop {
            match io.recv(&codec).await.unwrap().unwrap() {
                frame::Frame::Settings(s) if !s.is_ack() => {
                    if ack {
                        io.encode(frame::Settings::ack().into(), &codec).unwrap();
                    }
                    break;
                }
                _ => {}
            }
        }

        if ack {
            // the connection stays usable
            sleep(Millis(1500)).await;
            io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
            loop {
                match io.recv(&codec).await.unwrap().unwrap() {
                    frame::Frame::Ping(ping) => {
                        assert!(ping.is_ack());
                        break;
                    }
                    frame::Frame::GoAway(frm) => panic!("unexpected goaway: {frm:?}"),
                    _ => {}
                }
            }
        } else {
            let frm = loop {
                if let frame::Frame::GoAway(frm) = io.recv(&codec).await.unwrap().unwrap() {
                    break frm;
                }
            };
            assert_eq!(frm.reason(), Reason::SETTINGS_TIMEOUT);
            sleep(Millis(100)).await;
            assert!(io.is_closed());
        }
    }
}
