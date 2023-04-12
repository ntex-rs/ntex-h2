use std::{cell::Cell, io, net, rc::Rc};

use ::openssl::ssl::{AlpnError, SslAcceptor, SslConnector, SslFiletype, SslMethod, SslVerifyMode};
use ntex::http::{test::server as test_server, HeaderMap, HttpService, Method, Response};
use ntex::time::{sleep, Millis};
use ntex::{
    connect::openssl, io::IoBoxed, service::fn_service, service::ServiceFactory, util::Bytes,
};
use ntex_h2::{client::ClientConnection, frame::Reason};

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

fn start_server() -> ntex::http::test::TestServer {
    test_server(move || {
        HttpService::build()
            .configure_http2(|cfg| {
                cfg.max_concurrent_streams(1);
            })
            .h2(|mut req: ntex::http::Request| async move {
                let mut pl = req.take_payload();
                pl.recv().await;
                Ok::<_, io::Error>(Response::Ok().finish())
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    })
}

async fn connect(addr: net::SocketAddr) -> IoBoxed {
    // disable ssl verification
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder
        .set_alpn_protos(b"\x02h2\x08http/1.1")
        .map_err(|e| log::error!("Cannot set alpn protocol: {:?}", e));

    let addr = ntex::connect::Connect::new("localhost").set_addr(Some(addr));
    openssl::Connector::new(builder.build())
        .connect(addr)
        .await
        .unwrap()
        .into()
}

#[ntex::test]
async fn test_max_concurrent_streams() {
    let srv = start_server();
    let io = connect(srv.addr()).await;
    let connection =
        ClientConnection::with_params(io, ntex_h2::Config::client(), true, "localhost".into());
    let client = connection.client();
    ntex::rt::spawn(async move {
        let _ = connection
            .start(fn_service(
                |_: ntex_h2::Message| async move { Ok::<_, ()>(()) },
            ))
            .await;
    });
    sleep(Millis(150)).await;

    let stream = client
        .send_request(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    assert!(!client.is_ready());

    let client2 = client.clone();
    let opened = Rc::new(Cell::new(false));
    let opened2 = opened.clone();
    ntex::rt::spawn(async move {
        let _stream = client2
            .send_request(Method::GET, "/".into(), HeaderMap::default(), false)
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
async fn test_max_concurrent_streams_reset() {
    let srv = start_server();
    let io = connect(srv.addr()).await;
    let connection =
        ClientConnection::with_params(io, ntex_h2::Config::client(), true, "localhost".into());
    let client = connection.client();
    ntex::rt::spawn(async move {
        let _ = connection
            .start(fn_service(
                |_: ntex_h2::Message| async move { Ok::<_, ()>(()) },
            ))
            .await;
    });
    sleep(Millis(150)).await;

    let stream = client
        .send_request(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    assert!(!client.is_ready());

    let client2 = client.clone();
    let opened = Rc::new(Cell::new(false));
    let opened2 = opened.clone();
    ntex::rt::spawn(async move {
        let _stream = client2
            .send_request(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        opened2.set(true);
    });

    stream.reset(Reason::NO_ERROR);
    sleep(Millis(50)).await;
    assert!(client.is_ready());
    assert!(opened.get());
}
