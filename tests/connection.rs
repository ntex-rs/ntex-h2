use std::{cell::Cell, io, net, rc::Rc};

use ::openssl::ssl::{AlpnError, SslAcceptor, SslConnector, SslFiletype, SslMethod, SslVerifyMode};
use ntex::http::{self, HeaderMap, HttpService, Method, Response, test, uri::Scheme};
use ntex::service::{Pipeline, ServiceFactory, cfg::SharedCfg, fn_service};
use ntex::time::{Millis, Seconds, sleep};
use ntex::{channel::oneshot, connect::openssl, io::IoBoxed, util::Bytes};
use ntex_h2::{
    Codec, ServiceConfig, client, client::Client, client::SimpleClient, frame, frame::Reason,
};

fn ssl_acceptor() -> SslAcceptor {
    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./tests/cert.pem")
        .unwrap();
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
        move || {
            HttpService::h2(|mut req: http::Request| async move {
                let mut pl = req.take_payload();
                pl.recv().await;
                Ok::<_, io::Error>(Response::Ok().body("test body"))
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(ServiceConfig::new().max_concurrent_streams(1)),
    )
    .await
}

async fn connect(addr: net::SocketAddr) -> IoBoxed {
    // disable ssl verification
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder
        .set_alpn_protos(b"\x02h2\x08http/1.1")
        .map_err(|e| log::error!("Cannot set alpn protocol: {:?}", e));

    let addr = ntex::connect::Connect::new("localhost").set_addr(Some(addr));
    openssl::SslConnector::new(builder.build())
        .create(SharedCfg::default())
        .await
        .map(Pipeline::new)
        .unwrap()
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
    let client =
        client::Connector::new(fn_service(move |_| async move { Ok(connect(addr).await) }))
            .scheme(Scheme::HTTP)
            .connector(fn_service(move |_| async move { Ok(connect(addr).await) }))
            .create(SharedCfg::default())
            .await
            .map(Pipeline::new)
            .unwrap()
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
    let client = Client::build(
        "localhost",
        fn_service(move |_| async move { Ok(connect(addr).await) }),
    );
    assert!(format!("{:?}", client).contains("ClientBuilder"));
    let client = client
        .maxconn(1)
        .scheme(Scheme::HTTPS)
        .connector(fn_service(move |_| async move { Ok(connect(addr).await) }))
        .finish(SharedCfg::default())
        .await
        .unwrap();
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
    let client = Client::build(
        "localhost",
        fn_service(move |_| {
            cnt2.set(cnt2.get() + 1);
            async move { Ok(connect(addr).await) }
        }),
    )
    .maxconn(2)
    .finish(SharedCfg::default())
    .await
    .unwrap();
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

    stream.send_payload("chunk".into(), false).await.unwrap();
    sleep(Millis(25)).await;
    stream.reset(Reason::NO_ERROR);
    let _ = rx.await;
    assert!(client.is_ready());
    assert_eq!(opened.get(), 3);
}

const PREFACE: [u8; 24] = *b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[ntex::test]
async fn test_goaway_on_overflow() {
    let srv = start_server().await;
    let addr = srv.addr();

    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_buf(|buf| buf.extend_from_slice(&PREFACE));

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

    let id = id.next_id().unwrap();
    let hdrs = frame::Headers::new(id, pseudo, HeaderMap::new(), false);
    io.send(hdrs.clone().into(), &codec).await.unwrap();

    let res = goaway(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::FLOW_CONTROL_ERROR);
    assert!(io.recv(&codec).await.unwrap().is_none());
}

#[ntex::test]
async fn test_stream_cancel() {
    let srv = start_server().await;
    let addr = srv.addr();

    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_buf(|buf| buf.extend_from_slice(&PREFACE));

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
    let _ = io.with_write_buf(|buf| buf.extend_from_slice(&PREFACE));

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
    let _ = io.with_write_buf(|buf| buf.extend_from_slice(&PREFACE));

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
    let _srv = test::server_with_config(
        move || {
            HttpService::h2(|mut req: ntex::http::Request| async move {
                let mut pl = req.take_payload();
                pl.recv().await;
                Ok::<_, io::Error>(Response::Ok().body("test body"))
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(
            ServiceConfig::new()
                .max_concurrent_streams(1)
                .ping_timeout(Seconds(1)),
        ),
    );

    let srv = start_server().await;
    let addr = srv.addr();

    let io = connect(addr).await;
    let codec = Codec::default();
    let _ = io.with_write_buf(|buf| buf.extend_from_slice(&PREFACE));

    let settings = frame::Settings::default();
    io.encode(settings.into(), &codec).unwrap();

    // settings & window
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;
    let _ = io.recv(&codec).await;

    // ping & goaway
    let _ = io.recv(&codec).await;
    let _ = goaway(io.recv(&codec).await.unwrap().unwrap());
    sleep(Millis(500)).await;
    assert!(io.is_closed());
}
