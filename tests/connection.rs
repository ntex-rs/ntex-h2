#![recursion_limit = "256"]
use std::{cell::Cell, io, net, rc::Rc, time::SystemTime};

use ::openssl::ssl::{AlpnError, SslAcceptor, SslConnector, SslFiletype, SslMethod, SslVerifyMode};
use ntex::http::{self, HeaderMap, HttpService, Method, Response, openssl, test};
use ntex::service::{Pipeline, cfg::SharedCfg, fn_service};
use ntex::time::{Millis, Seconds, sleep};
use ntex::{Service, channel::oneshot, connect::openssl, io::IoBoxed, util::Bytes};
use ntex_error::ErrorDiagnostic;
use ntex_h2::client::{self, Client, ClientError, SimpleClient};
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

/// Receives response body, the last chunk of a sized body may end the stream,
/// otherwise an empty frame follows
async fn recv_body(io: &IoBoxed, codec: &Codec) {
    match io.recv(codec).await.unwrap().unwrap() {
        frame::Frame::Data(data) if data.is_end_stream() => (),
        frame::Frame::Data(_) => match io.recv(codec).await.unwrap().unwrap() {
            frame::Frame::Data(data) if data.is_end_stream() && data.payload().is_empty() => (),
            frm => panic!("Expect empty Data frame with END_STREAM: {:?}", frm),
        },
        frm => panic!("Expect Data frame: {:?}", frm),
    }
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

/// STATEMENT: Cloning each `ClientError` variant preserves its diagnostic signature and display
/// text.
///
/// HOW:
/// - Construct protocol, operation, frame, handshake timeout, connector, and I/O errors.
/// - Clone each error and compare its signature with the expected value and its display text
///   with the original.
#[test]
fn test_client_error_clone_and_diagnostic() {
    let errors = [
        (
            ClientError::Protocol(ntex_h2::ConnectionError::MissingSettings),
            "h2-conn-MissingSettings",
        ),
        (
            ClientError::Operation(ntex_h2::OperationError::Disconnected),
            "h2-oper-Disconnected",
        ),
        (
            ClientError::Frame(frame::FrameError::BadFrameSize),
            "h2-client-Frame",
        ),
        (ClientError::HandshakeTimeout, "h2-client-HandshakeTimeout"),
        (
            ClientError::Connect(ntex_net::connect::ConnectError::InvalidInput),
            "ntex-connect-InvalidInput",
        ),
        (
            ClientError::Disconnected(io::Error::new(io::ErrorKind::ConnectionReset, "gone")),
            "std-io-ConnectionReset",
        ),
    ];

    for (err, signature) in errors {
        let cloned = err.clone();
        assert_eq!(cloned.signature(), signature);
        assert_eq!(cloned.to_string(), err.to_string());
    }
}

/// STATEMENT: A new `SimpleClient` exposes its connection metadata and notifies listeners on
/// forced closure.
///
/// HOW:
/// - Create a tagged in-memory client with authority `example.com` and check its 16-character
///   base32 identifier, tag, service, authority, creation time, and initial ping count.
/// - Verify the I/O is open, register `on_disconnect`, and call `force_close`.
/// - Await the notification and assert that the client is closed.
#[ntex::test]
async fn test_simple_client_metadata_and_disconnect() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let client = SimpleClient::new(
        ntex::io::Io::new(io, SharedCfg::new("CLI").build()),
        true,
        "example.com".into(),
    );

    assert_eq!(client.id().len(), 16);
    assert!(
        client
            .id()
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || (b'2'..=b'7').contains(&ch))
    );
    assert_eq!(client.tag(), "CLI");
    assert!(!client.service().is_empty());
    assert_eq!(client.authority(), "example.com");
    assert!(client.created() <= SystemTime::now());
    assert_eq!(client.pings_count(), 0);
    assert!(!client.io_ref().is_closed());

    let disconnected = client.on_disconnect();
    client.force_close();
    disconnected.await;
    assert!(client.is_closed());
}

/// STATEMENT: Timed-out, dropped, and unreserved disconnect futures close the client and wake
/// disconnect listeners.
///
/// HOW:
/// - Hold a stream reservation, await a disconnect with a short timeout, and check
///   `OperationError::Disconnected`, listener completion, and closure.
/// - On a second client, hold a reservation and drop the disconnect future after checking the
///   disconnecting state; verify the listener wakes and the client closes.
/// - Disconnect a third client without reservations and verify it returns
///   `OperationError::Disconnected` and closes within the outer timeout.
#[ntex::test]
async fn test_client_disconnect_future() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let client = SimpleClient::new(
        ntex::io::Io::new(io, SharedCfg::default()),
        false,
        "localhost".into(),
    );
    let reservation = client.reserve().unwrap();
    let disconnected = client.on_disconnect();

    let err = client
        .disconnect()
        .disconnect_timeout(Millis(10))
        .await
        .unwrap_err();
    assert!(matches!(&*err, ntex_h2::OperationError::Disconnected));
    disconnected.await;
    assert!(client.is_closed());
    drop(reservation);

    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let client = SimpleClient::new(
        ntex::io::Io::new(io, SharedCfg::default()),
        false,
        "localhost".into(),
    );
    let reservation = client.reserve().unwrap();
    let disconnected = client.on_disconnect();
    let disconnect = client.disconnect();
    assert!(client.is_disconnecting());
    assert!(!client.is_closed());
    drop(disconnect);
    disconnected.await;
    assert!(client.is_closed());
    drop(reservation);

    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let client = SimpleClient::new(
        ntex::io::Io::new(io, SharedCfg::default()),
        false,
        "localhost".into(),
    );
    let err = ntex::time::timeout(Millis(1_000), client.disconnect())
        .await
        .expect("client did not disconnect")
        .unwrap_err();
    assert!(matches!(&*err, ntex_h2::OperationError::Disconnected));
    assert!(client.is_closed());
}

/// STATEMENT: Pool connection timeouts and connector errors leave no connections and increment
/// the error count.
///
/// HOW:
/// - Use a connector slower than the configured connection timeout and assert
///   `ClientError::HandshakeTimeout` and pool readiness.
/// - Use an immediately failing connector and assert `ClientError::Connect` with
///   `ConnectError::InvalidInput`.
/// - For both failures, verify zero active and total connections and exactly one connection
///   error.
#[ntex::test]
async fn test_client_pool_connect_failures() {
    let client = Client::builder("localhost")
        .minconn(0)
        .connect_timeout(Millis(10))
        .connector(async |_| {
            sleep(Seconds(1)).await;
            Err::<IoBoxed, _>(ntex_error::Error::from(
                ntex_net::connect::ConnectError::InvalidInput,
            ))
        })
        .build(SharedCfg::default());

    let err = client.client().await.unwrap_err();
    assert!(matches!(&*err, ClientError::HandshakeTimeout));
    assert_eq!(client.stat_active_connections(), 0);
    assert_eq!(client.stat_total_connections(), 0);
    assert_eq!(client.stat_connect_errors(), 1);
    assert!(client.is_ready());

    let client = Client::builder("localhost")
        .minconn(0)
        .connector(async |_| {
            Err::<IoBoxed, _>(ntex_error::Error::from(
                ntex_net::connect::ConnectError::InvalidInput,
            ))
        })
        .build(SharedCfg::default());

    let err = client.client().await.unwrap_err();
    assert!(matches!(
        &*err,
        ClientError::Connect(ntex_net::connect::ConnectError::InvalidInput)
    ));
    assert_eq!(client.stat_active_connections(), 0);
    assert_eq!(client.stat_total_connections(), 0);
    assert_eq!(client.stat_connect_errors(), 1);
}

/// STATEMENT: A `SimpleClient` honors the peer stream limit and opens a waiting request when
/// capacity returns.
///
/// HOW:
/// - Connect to a server allowing one concurrent stream, wait for its settings, and check the
///   client authority and debug output.
/// - Open a request and verify the client is not ready, has one active stream, and exposes
///   matching send/receive stream identities and debug output.
/// - Spawn another request, finish the first request body, and verify the waiting request opens
///   and readiness returns.
#[ntex::test]
async fn test_max_concurrent_streams() {
    let srv = start_server().await;
    let addr = srv.addr();
    let client = Pipeline::new(
        SharedCfg::default(),
        client::Connector::new()
            .secure(false)
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

/// STATEMENT: A single-connection pool regains readiness and serves a waiter after its stream
/// slot is released.
///
/// HOW:
/// - Build a secure pool limited to one connection and check builder/client debug output and
///   initial readiness.
/// - Open a request against a server allowing one concurrent stream and assert that the pool
///   becomes unready.
/// - Spawn another request, finish the first request body, await pool readiness, and verify the
///   waiting request opens.
#[ntex::test]
async fn test_max_concurrent_streams_pool() {
    let srv = start_server().await;
    let addr = srv.addr();
    let client = Client::builder("localhost");
    assert!(format!("{:?}", client).contains("ClientBuilder"));

    let client = client
        .connection_limit(1)
        .secure(true)
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

/// STATEMENT: A pool with a two-connection limit can open another connection when a peer stream
/// slot is occupied.
///
/// HOW:
/// - Count connector calls in a pool limited to two connections, using a server allowing one
///   stream per connection.
/// - Open the first request and verify the pool remains ready, then spawn another request and
///   finish the first request body.
/// - Assert that the second request opens, the pool remains ready, and exactly two connections
///   were created.
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

/// STATEMENT: Resetting and dropping streams successively release capacity for queued client
/// requests.
///
/// HOW:
/// - Fill a one-stream client and queue three requests, arranging for the first waiter to reset
///   its stream and the second to drop its stream handles.
/// - Send a partial body on the original stream and reset it with `Reason::NO_ERROR`.
/// - Wait for the final queued request and assert all three requests opened and the client is
///   ready.
#[ntex::test]
async fn test_max_concurrent_streams_reset() {
    let srv = start_server().await;
    let io = connect(srv.addr()).await;
    let client = SimpleClient::new(io, false, "localhost".into());
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

/// STATEMENT: Capacity callbacks report peer settings, released stream slots, and connection
/// closure.
///
/// HOW:
/// - Register an `on_capacity` counter and verify the peer one-stream setting invokes it once
///   while opening a request does not.
/// - Finish the request body and drain its response, then assert zero active streams and a
///   second callback.
/// - Force-close the connection and verify closure and further callback activity.
#[ntex::test]
async fn test_on_capacity() {
    let srv = start_server().await;
    let io = connect(srv.addr()).await;
    let client = SimpleClient::new(io, false, "localhost".into());
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

/// STATEMENT: A stream reservation consumes one slot and releases it on drop, request
/// completion, or send failure.
///
/// HOW:
/// - Reserve the only stream slot and check its debug output, active count, lack of readiness,
///   and refusal of another reservation.
/// - Drop the reservation and verify capacity and its callback return; use another reservation
///   to send a request and verify completion releases its slot with one additional callback.
/// - Reserve again, force-close the client, and verify the reserved send fails, the active
///   count is zero, and no new reservation is available.
#[ntex::test]
async fn test_stream_reservation() {
    let srv = start_server().await;
    let io = connect(srv.addr()).await;
    let client = SimpleClient::new(io, false, "localhost".into());
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

/// STATEMENT: Forced disconnection clears active stream accounting without a second decrement
/// on handle drop.
///
/// HOW:
/// - Wait for a one-stream limit, open a request, and assert one active stream.
/// - Force-close the client and verify it is closed with zero active streams.
/// - Drop both stream handles and assert the active count remains zero.
#[ntex::test]
async fn test_disconnect_resets_active_streams() {
    let srv = start_server().await;
    let io = connect(srv.addr()).await;
    let client = SimpleClient::new(io, false, "localhost".into());
    sleep(Millis(150)).await;
    assert_eq!(client.max_streams(), Some(1));

    let (stream, recv_stream) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    assert_eq!(client.active_streams(), 1);

    // streams failed by the disconnect are no longer active
    client.force_close();
    sleep(Millis(50)).await;
    assert!(client.is_closed());
    assert_eq!(client.active_streams(), 0);
    drop((stream, recv_stream));
    assert_eq!(client.active_streams(), 0);
}

/// STATEMENT: Graceful disconnection honors existing reservations and waits until all reserved
/// work is released.
///
/// HOW:
/// - Reserve a stream and call `close`; verify new reservations are refused but the existing
///   one can send and receive a response before closure.
/// - On another client, verify a held reservation delays closure until it is dropped.
/// - With two slots, complete an active request during graceful shutdown and verify the
///   remaining reservation keeps the client open until its own request completes.
#[ntex::test]
async fn test_stream_reservation_graceful_disconnect() {
    let srv = start_server().await;
    let client = SimpleClient::new(connect(srv.addr()).await, false, "localhost".into());
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
    let client = SimpleClient::new(connect(srv.addr()).await, false, "localhost".into());
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
    let client = SimpleClient::new(connect(srv.addr()).await, false, "localhost".into());
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

/// STATEMENT: A payload send blocked by the peer stream window fails with
/// `StreamError::CapacityTimeout`.
///
/// HOW:
/// - Configure a one-second capacity timeout and an in-memory peer advertising a one-byte
///   stream window without updates.
/// - Send a four-byte POST body and verify it completes within the outer timeout with
///   `OperationError::Stream(StreamError::CapacityTimeout)`.
#[ntex::test]
async fn test_client_send_capacity_timeout() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
        .build();
    let client = SimpleClient::new(ntex::io::Io::new(io, cfg), false, "localhost".into());

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

/// STATEMENT: Waiting for send capacity on a zero-window stream fails with
/// `StreamError::CapacityTimeout`.
///
/// HOW:
/// - Configure a one-second capacity timeout and an in-memory peer advertising a zero stream
///   window without updates.
/// - Open a POST stream, await `send_capacity` within an outer timeout, and assert
///   `OperationError::Stream(StreamError::CapacityTimeout)`.
#[ntex::test]
async fn test_client_send_capacity_wait_timeout() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
        .build();
    let client = SimpleClient::new(ntex::io::Io::new(io, cfg), false, "localhost".into());

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

/// STATEMENT: Dropping an unfinished send stream wakes a pending receive and ends it without a
/// message.
///
/// HOW:
/// - Open a POST request to a server waiting for its body and spawn a task blocked in
///   `RecvStream::recv`.
/// - Drop the unfinished send stream to reset it locally and verify the receive returns `None`
///   within the timeout.
#[ntex::test]
async fn test_recv_woken_by_local_reset() {
    let srv = start_server().await;
    let client = SimpleClient::new(connect(srv.addr()).await, false, "localhost".into());
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

/// STATEMENT: A send-capacity timeout wakes a pending receive and ends it without a message.
///
/// HOW:
/// - Create an in-memory client with a one-second capacity timeout and a peer advertising a
///   zero stream window.
/// - Open a POST stream and spawn a pending receive, then verify sending its body fails.
/// - Assert the receive task returns `None` within the timeout.
#[ntex::test]
async fn test_recv_woken_by_capacity_timeout() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
        .build();
    let client = SimpleClient::new(ntex::io::Io::new(io, cfg), false, "localhost".into());

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

/// STATEMENT: An expired capacity timer does not replace a closed stream reset or close the
/// connection.
///
/// HOW:
/// - Open a POST stream against a zero-window peer with a one-second capacity timeout.
/// - Poll `send_capacity` until an outer timeout expires, then reset the stream with
///   `Reason::CANCEL`.
/// - Wait beyond the capacity timeout and assert `send_capacity` still returns
///   `OperationError::LocalReset(Reason::CANCEL)` while the client remains open.
#[ntex::test]
async fn test_stale_capacity_timeout_ignored() {
    let (io, srv) = ntex::testing::IoTest::create();
    srv.remote_buffer_cap(1024 * 1024);
    let cfg = SharedCfg::new("CLI")
        .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
        .build();
    let client = SimpleClient::new(ntex::io::Io::new(io, cfg), false, "localhost".into());

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

/// STATEMENT: A completed response does not cancel an unfinished request body or prematurely
/// release its slot.
///
/// HOW:
/// - Send a partial POST body and inject response `Headers` carrying status 200 and
///   `END_STREAM`.
/// - Drain and drop the receive stream, then verify the request still counts as one active
///   stream.
/// - Finish the request body successfully, verify zero active streams, and confirm the client
///   remains open after dropping the send handle.
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

/// STATEMENT: The first frame after the client preface must be a non-acknowledgement `Settings`
/// frame.
///
/// HOW:
/// - Open raw connections and send either a `Ping` or a settings acknowledgement immediately
///   after the preface.
/// - Skip initial settings and window updates, then assert `GoAway` has
///   `Reason::PROTOCOL_ERROR` and the expected first-frame diagnostic.
/// - Verify each connection then reaches EOF.
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

/// STATEMENT: Invalid `Settings` frames close the connection with the appropriate RFC 9113
/// error code.
///
/// HOW:
/// - Send raw settings with an oversized initial window, non-multiple-of-six payload,
///   acknowledgement payload, invalid push value, or nonzero stream identifier.
/// - Assert `GoAway` carries the expected flow-control, frame-size, or protocol error and,
///   where specified, the expected diagnostic text.
/// - Verify each connection then reaches EOF.
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

/// STATEMENT: Excess concurrent streams are refused without closing the connection until
/// repeated overflow hits the reset limit.
///
/// HOW:
/// - Keep one request open, open excess streams, and assert `Reason::REFUSED_STREAM` resets
///   even when data follows a refused stream.
/// - Send a `Ping` and verify its acknowledgement shows the connection remains usable.
/// - Continue opening excess streams until `GoAway` reports `Reason::ENHANCE_YOUR_CALM` and the
///   rapid-reset diagnostic, then verify EOF.
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
                assert_eq!(res.reason(), Reason::ENHANCE_YOUR_CALM);
                assert_eq!(res.data().as_ref(), b"Stream rapid reset count achieved");
                break;
            }
            frm => panic!("unexpected frame: {frm:?}"),
        }
        assert!(u32::from(id) < 64, "reset limit is not reached");
    }
    assert!(io.recv(&codec).await.unwrap().is_none());
}

/// STATEMENT: Discarded padding returns flow-control capacity even when delivered data stays
/// below the update threshold.
///
/// HOW:
/// - Open a POST stream against a server that does not read payloads.
/// - Send 255 padded `Data` frames containing one application byte and 256 flow-controlled
///   bytes each.
/// - Wait for a stream `WindowUpdate` and assert its increment exceeds the 255 delivered data
///   bytes.
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

/// STATEMENT: A settings-induced stream-window overflow is a connection error and is not
/// acknowledged.
///
/// HOW:
/// - Open a raw stream and grow the server send window to `MAX_INITIAL_WINDOW_SIZE` with a
///   `WindowUpdate`.
/// - Increase the initial stream window by one byte via `Settings`.
/// - Assert the next frame is `GoAway` with `Reason::FLOW_CONTROL_ERROR`, rather than a
///   settings acknowledgement, and verify EOF.
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

/// STATEMENT: Receiving data beyond a stream receive window resets that stream with a
/// flow-control error.
///
/// HOW:
/// - Open a POST stream on a server that does not consume payloads.
/// - Send five 16-KiB `Data` frames, exceeding the default stream window but not the connection
///   window.
/// - Await a `Reset` within the timeout and verify the client stream identifier and
///   `Reason::FLOW_CONTROL_ERROR`.
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

/// STATEMENT: A frame exceeding the stream receive window triggers a reset without returning
/// stream capacity.
///
/// HOW:
/// - Configure a 30,000-byte stream window on a server that does not consume payloads and open
///   a raw POST stream.
/// - Send two 16-KiB `Data` frames and reject any stream `WindowUpdate` received before the
///   reset.
/// - Verify the reset arrives within the timeout for the client stream with
///   `Reason::FLOW_CONTROL_ERROR`.
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

/// STATEMENT: A frame exceeding the connection receive window causes a connection-level
/// flow-control error.
///
/// HOW:
/// - Configure a 65,535-byte connection window, a 1-MiB stream window, and a 1-MiB maximum
///   frame size.
/// - Open a raw POST stream and send one 80-KiB `Data` frame, exceeding only the connection
///   window.
/// - Await `GoAway` within the timeout, reject any stream reset, and assert
///   `Reason::FLOW_CONTROL_ERROR`.
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

/// STATEMENT: A received stream cancellation is not answered with another reset, as required by
/// RFC 9113 §5.4.2.
///
/// HOW:
/// - Complete the raw connection setup, open a GET stream, and immediately send a `Reset` with
///   `Reason::CANCEL`.
/// - Send a `Ping` and assert the next received frame is its acknowledgement rather than a
///   reset.
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

    // a received RST_STREAM is not answered with RST_STREAM (RFC 9113 §5.4.2)
    io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
    match io.recv(&codec).await.unwrap().unwrap() {
        frame::Frame::Ping(ping) => assert!(ping.is_ack()),
        frm => panic!("unexpected frame {frm:?}"),
    }
}

/// STATEMENT: Repeated resets of unfinished requests close the connection and report the last
/// peer stream.
///
/// HOW:
/// - Complete five raw GET requests and consume their response bodies.
/// - Open and immediately reset five more requests with `Reason::NO_ERROR` before sending their
///   bodies.
/// - Assert `GoAway` carries `Reason::ENHANCE_YOUR_CALM` and the last opened stream identifier,
///   then verify EOF.
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
        recv_body(&io, &codec).await;
    }
    for _ in 0..4 {
        let rst = frame::Reset::new(id, Reason::NO_ERROR);
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
        id = id.next_id().unwrap();
        io.encode(hdrs.into(), &codec).unwrap();
        io.send(rst.into(), &codec).await.unwrap();
    }
    let rst = frame::Reset::new(id, Reason::NO_ERROR);
    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
    io.encode(hdrs.into(), &codec).unwrap();
    io.send(rst.into(), &codec).await.unwrap();

    let res = goaway(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::ENHANCE_YOUR_CALM);
    // the last stream opened by the peer is reported
    assert_eq!(res.last_stream_id(), id);
    assert!(io.recv(&codec).await.unwrap().is_none());
}

/// STATEMENT: Repeated resets count toward the rapid-reset limit even for requests whose bodies
/// are already ended.
///
/// HOW:
/// - Complete five raw GET requests and consume their response bodies.
/// - Send five more GET requests with `END_STREAM`, immediately follow each with a
///   `Reason::NO_ERROR` reset, and consume their responses.
/// - Assert `GoAway` carries `Reason::ENHANCE_YOUR_CALM` and the connection reaches EOF.
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
        recv_body(&io, &codec).await;
    }

    for _ in 0..4 {
        let rst = frame::Reset::new(id, Reason::NO_ERROR);
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), true);
        id = id.next_id().unwrap();
        io.encode(hdrs.into(), &codec).unwrap();
        io.send(rst.into(), &codec).await.unwrap();
        io.recv(&codec).await.unwrap().unwrap(); // headers
        recv_body(&io, &codec).await;
    }
    let rst = frame::Reset::new(id, Reason::NO_ERROR);
    let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), true);
    io.encode(hdrs.into(), &codec).unwrap();
    io.send(rst.into(), &codec).await.unwrap();
    io.recv(&codec).await.unwrap().unwrap(); // headers
    recv_body(&io, &codec).await;

    let res = goaway(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::ENHANCE_YOUR_CALM);
    assert!(io.recv(&codec).await.unwrap().is_none());
}

/// STATEMENT: An idle connection closes when the peer does not acknowledge the keep-alive ping.
///
/// HOW:
/// - Configure a one-second ping timeout and complete the raw connection setup without opening
///   a stream.
/// - Leave the subsequent ping unanswered, receive a `GoAway`, and assert the I/O closes after
///   a delay.
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

/// STATEMENT: Only an acknowledgement matching the keep-alive ping payload prevents its
/// timeout.
///
/// HOW:
/// - Configure a one-second ping timeout and, on separate connections, receive a
///   non-acknowledgement `Ping`.
/// - Reply with either its actual payload or an unrelated payload of eight `0xff` bytes.
/// - Verify a matching reply permits another non-acknowledgement `Ping`, while the unrelated
///   reply is followed by `GoAway`.
#[ntex::test]
async fn test_ping_timeout_unexpected_ack() {
    let srv = test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(async move |_: http::Request| {
                    Ok::<_, io::Error>(Response::Ok().body("test body"))
                }),
            )
            .map_err(|_| ())
        },
        SharedCfg::new("SRV").add(ServiceConfig::new().set_ping_timeout(Seconds(1))),
    );

    for valid in [true, false] {
        let io = connect(srv.addr()).await;
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
        io.encode(frame::Settings::default().into(), &codec).unwrap();

        // settings & window
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;

        let frame::Frame::Ping(ping) = io.recv(&codec).await.unwrap().unwrap() else {
            panic!("expected ping")
        };
        assert!(!ping.is_ack());
        let payload = if valid { *ping.payload() } else { [0xff; 8] };
        io.send(frame::Ping::pong(payload).into(), &codec).await.unwrap();

        match io.recv(&codec).await.unwrap().unwrap() {
            frame::Frame::Ping(ping) if valid => assert!(!ping.is_ack()),
            frame::Frame::GoAway(_) if !valid => (),
            frm => panic!("unexpected frame: {frm:?}"),
        }
    }
}

/// STATEMENT: Exceeding the server request-header limit refuses only that stream and leaves the
/// connection usable.
///
/// HOW:
/// - Configure a five-header server limit and send a GET request containing six custom headers.
/// - Assert the receive stream ends with `StreamError::Reset(Reason::REFUSED_STREAM)`.
/// - Send a request without custom headers and verify response headers arrive within the
///   timeout.
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
        .secure(true)
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

/// STATEMENT: A malformed request resets only its stream and leaves the connection usable.
///
/// HOW:
/// - Open a raw connection and send another request containing the forbidden `CONNECTION`
///   header.
/// - Assert the exact `Reset` identifies that stream and carries `Reason::PROTOCOL_ERROR`.
/// - Send a `Ping` and verify its acknowledgement.
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

/// STATEMENT: Missing or unexpected request pseudo-headers reset the stream without making the
/// connection unusable.
///
/// HOW:
/// - Send requests missing `:path`, `:method`, or `:scheme`, or containing unexpected `:status`
///   or `:protocol` pseudo-headers.
/// - Assert a `Reason::PROTOCOL_ERROR` reset for each stream and send trailing data to exercise
///   handling of frames arriving after reset.
/// - Send a `Ping` and verify its acknowledgement rather than another error frame.
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

/// STATEMENT: A response exceeding the client header limit fails only its stream and preserves
/// the connection.
///
/// HOW:
/// - Configure the client to accept five headers and have the server send six custom headers
///   for `/large`.
/// - Assert the receive stream ends with
///   `StreamError::InvalidFrame(FrameError::TooManyHeaders)` carrying the message stream
///   identifier.
/// - Request `/` on the same client and verify response headers arrive within the timeout.
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
        .secure(true)
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

/// STATEMENT: A stalled server response is cancelled on capacity timeout while later responses
/// can resume on window updates.
///
/// HOW:
/// - Advertise a one-byte initial stream window, request a response, and verify a
///   `Reason::CANCEL` reset follows its initial headers and data when no update arrives.
/// - Open another request and assert its first `Data` payload is `t`, then send a stream
///   `WindowUpdate`.
/// - Verify the remaining payload is `est body` on the same stream and accept either an ending
///   data frame or a following empty `END_STREAM` frame.
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
    let frame::Frame::Data(data) = res else {
        panic!("Expect Data frame: {:?}", res);
    };
    assert_eq!(data.stream_id(), id);
    assert_eq!(data.payload(), &Bytes::copy_from_slice(b"est body"));

    // the last chunk of a sized body may end the stream, otherwise an empty
    // frame follows
    if !data.is_end_stream() {
        let mut eof = frame::Data::new(id, Bytes::new());
        eof.set_end_stream();
        assert_eq!(io.recv(&codec).await.unwrap().unwrap(), frame::Frame::Data(eof));
    }
}

/// STATEMENT: A connection pool reuses a live connection but replaces it after its configured
/// lifetime expires.
///
/// HOW:
/// - Build a ready pool with a one-second connection lifetime and acquire and drop two client
///   handles.
/// - Assert both handles have the same connection identifier.
/// - Wait two seconds, acquire another handle, and assert its identifier differs.
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
        .secure(true)
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

/// STATEMENT: An invalid-length `Priority` frame on an open stream is a stream-level frame-size
/// error.
///
/// HOW:
/// - Open a raw stream and write a `Priority` frame with a four-byte payload instead of five.
/// - Assert a `Reset` for that stream carries `Reason::FRAME_SIZE_ERROR`.
/// - Send a `Ping` and verify its acknowledgement to confirm the connection remains usable.
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

/// STATEMENT: A basic CONNECT requires only `:authority`, omits `:scheme` and `:path`, and
/// rejects unsupported extended CONNECT.
///
/// HOW:
/// - Use an echo server and send CONNECT requests with valid authority-only pseudo-headers,
///   missing authority, unexpected scheme/path, and an extended CONNECT protocol.
/// - Verify the valid request returns `CONNECT //example.com:443`, matching RFC 9113 §8.5.
/// - Verify every invalid variant resets its stream with `Reason::PROTOCOL_ERROR`, including
///   extended CONNECT because RFC 8441 §4 support is not enabled.
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
                    assert_eq!(data.payload().as_ref(), b"CONNECT //example.com:443");
                    break;
                }
                _ => {}
            }
        }
    }
}

/// STATEMENT: The client omits `:scheme` and `:path` when encoding a CONNECT request.
///
/// HOW:
/// - Connect to an echo server with client authority `localhost:8080` and send CONNECT using
///   the otherwise ignored path `/ignored`.
/// - Verify response headers arrive and the response body is `CONNECT //localhost:8080`,
///   accepting either data or ending-data delivery.
#[ntex::test]
async fn test_client_connect_request() {
    let srv = start_echo_server().await;
    let addr = srv.addr();
    let client = Pipeline::new(
        SharedCfg::default(),
        client::Connector::new()
            .secure(false)
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
        // the last chunk of a sized body may end the stream
        MessageKind::Data(data, _) | MessageKind::Eof(ntex_h2::StreamEof::Data(data, _)) => {
            assert_eq!(data.as_ref(), b"CONNECT //localhost:8080")
        }
        kind => panic!("unexpected message: {kind:?}"),
    }
}

/// STATEMENT: A control service failure releases open streams and allows the dispatcher to
/// complete (F9).
///
/// HOW:
/// - Run an in-memory server whose publish service fails on the second stream and whose control
///   service also fails.
/// - Keep stream 1 open, trigger the failure with stream 3, and assert stream 1 receives
///   `MessageKind::Disconnect` within the timeout.
/// - Verify the server dispatcher completes within the timeout.
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

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());

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
        false,
        "localhost".into(),
    );
    srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, max]);
    (Rc::new(client), srv)
}

/// STATEMENT: A request waiting for a stream slot fails when the connection is closed (N1).
///
/// HOW:
/// - Fill a one-stream client and spawn another request waiting for capacity.
/// - Close the in-memory peer and verify the waiter completes within the timeout with
///   `OperationError::Disconnected`.
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

/// STATEMENT: A request waiting for a stream slot fails when graceful disconnection begins
/// (N1).
///
/// HOW:
/// - Fill a one-stream client and spawn another request waiting for capacity.
/// - Call `close` and verify the waiter completes within the timeout with
///   `OperationError::Disconnecting`.
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

/// STATEMENT: Released stream slots wake waiting requests even when releases occur back to back
/// (N2).
///
/// HOW:
/// - Fill a two-stream client and spawn two requests waiting for stream slots.
/// - Reset both active streams before either waiter runs.
/// - Verify both waiting requests successfully open streams within their timeouts.
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

/// STATEMENT: Dropping a woken stream waiter passes its capacity notification to the next
/// waiter (N2).
///
/// HOW:
/// - Fill a one-stream client, manually poll the first waiting send to `Poll::Pending`, and
///   spawn a second waiting send.
/// - Reset the active stream, then drop the first waiter without polling it again.
/// - Verify the second waiting request opens a stream within the timeout.
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

/// STATEMENT: A `Reason::NO_ERROR` reset after a completed response stops the request body
/// without failing the response (N4).
///
/// HOW:
/// - Open a POST request and inject status-200 `Headers`, ending `Data` containing `ok`, and a
///   `Reason::NO_ERROR` reset.
/// - Verify normal response headers and ending data are delivered, followed by receive EOF
///   rather than an error.
/// - Assert the active stream count is zero, further request-body sending fails, and the
///   connection stays open.
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

/// STATEMENT: Request-body data sent after a complete response still reaches the server
/// application.
///
/// HOW:
/// - Run an in-memory server that sends a complete response as soon as request headers arrive
///   and forwards published messages to a channel.
/// - Verify response headers carry EOF, then send `chunk` and an empty ending request-body
///   frame.
/// - Collect three server messages within the timeout and verify the latter two contain the
///   request data and EOF.
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

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());
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

/// STATEMENT: Informational responses leave the stream awaiting a final response and cannot
/// follow final headers.
///
/// HOW:
/// - Have an in-memory server send statuses 103 and 100 before final status 200 and an ending
///   `ok` body.
/// - Assert attempting another informational response after final headers fails with
///   `OperationError::Payload`.
/// - Verify the client receives statuses 103, 100, and 200 in order without EOF on their
///   headers, then ending data `ok`, and remains connected.
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

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());
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

/// STATEMENT: A remote reset of a server stream publishes a final error message.
///
/// HOW:
/// - Forward in-memory server messages to a channel and verify an open POST request publishes
///   headers.
/// - Reset the client send stream with `Reason::CANCEL`.
/// - Assert the server publishes `MessageKind::Eof(StreamEof::Error)` within the timeout.
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

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());
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

/// STATEMENT: A remote reset cancels its pending request-header handler even after data has
/// been published.
///
/// HOW:
/// - Keep two server header handlers pending with drop guards recording their stream
///   identifiers; the second runs in a spawned task.
/// - Send data on the second stream, wait for publication, and verify neither handler guard has
///   dropped.
/// - Reset the second stream with `Reason::CANCEL` and verify only its handler guard drops.
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

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());

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

/// STATEMENT: A remote reset cancels all pending publish calls for the stream, including data
/// handlers.
///
/// HOW:
/// - Run an in-memory server whose `MessageKind::Data` handlers stay pending with counted drop
///   guards.
/// - Send two data chunks on one POST stream and verify both handlers have started without
///   dropping their guards.
/// - Reset the stream with `Reason::CANCEL` and assert both data-handler guards drop.
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

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());

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

/// STATEMENT: The connection stops processing frames at the in-flight publish-call limit and
/// resumes when a call completes.
///
/// HOW:
/// - Configure a two-message in-flight limit and block server data handlers on a completion
///   channel.
/// - Send three data frames and observe two handler notifications; verify the third
///   notification times out while both calls remain pending.
/// - Unblock one handler and verify the third frame is then published.
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

    let client = SimpleClient::new(Io::new(cli, SharedCfg::default()), false, "localhost".into());

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

/// STATEMENT: Late headers for closed client streams are harmless, but headers for an idle
/// stream close the connection (N5).
///
/// HOW:
/// - Complete streams 1 and 3 with ending response headers, drop their handles, and verify zero
///   active streams.
/// - Inject late headers for streams 3 and 1, verify the client stays open, and successfully
///   complete a request on stream 5.
/// - Inject headers for never-opened stream 101 and assert the client closes.
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

/// STATEMENT: The client delivers interim responses before final headers but rejects status 101
/// and interim EOF.
///
/// HOW:
/// - Inject statuses 103, 100, and 200 followed by an ending `ok` body, then verify their
///   ordered headers without EOF and the ending data.
/// - On separate streams, inject status 103 with `END_STREAM` and status 101 without it.
/// - Assert both streams fail with `StreamError::InvalidInformational` while the connection
///   stays open.
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

/// STATEMENT: A HEAD response accepts the GET representation `content-length` without content
/// and rejects a nonempty payload.
///
/// HOW:
/// - Reply to HEAD with status 200, `content-length: 10`, and empty ending data; verify the
///   headers retain that length and the body ends empty.
/// - Reply to another HEAD request with status 200 and nonempty ending data.
/// - Verify the second stream fails with `StreamError::NonEmptyPayload` while the connection
///   remains open.
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

/// STATEMENT: Unacknowledged settings trigger `Reason::SETTINGS_TIMEOUT`, while acknowledged
/// settings keep the connection usable (RFC 9113 §6.5.3).
///
/// HOW:
/// - Disable keep-alive pings, configure a one-second settings timeout, and test peers that
///   acknowledge or ignore the server settings.
/// - For the acknowledging peer, wait beyond the timeout and verify a `Ping` is acknowledged
///   without `GoAway`.
/// - For the non-acknowledging peer, assert `GoAway` carries `Reason::SETTINGS_TIMEOUT` and the
///   I/O closes.
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

/// STATEMENT: A pooled request waiting for capacity continues when another stream is cancelled
/// without a final response.
///
/// HOW:
/// - Limit a pool to one connection, wait for the peer one-stream limit, and open a POST
///   request without sending its body.
/// - Spawn a GET request and verify the pool is not ready while its stream slot remains
///   occupied.
/// - Drop both POST stream handles and verify the waiting GET send succeeds within the timeout.
#[ntex::test]
async fn test_pool_waiter_woken_on_stream_cancel() {
    let srv = start_server().await;
    let addr = srv.addr();
    let client = Client::builder("localhost")
        .secure(true)
        .connection_limit(1)
        .connector(async move |_| Ok(connect(addr).await))
        .build(SharedCfg::default());

    // wait for the peer's stream limit
    let con = client.client().await.unwrap();
    while con.max_streams() != Some(1) {
        sleep(Millis(50)).await;
    }

    // the request body is never sent, the server waits for it
    let (snd, rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();

    let (tx, rx) = oneshot::channel();
    let client2 = client.clone();
    ntex::rt::spawn(async move {
        let res = client2
            .send(Method::GET, "/".into(), HeaderMap::new(), true)
            .await;
        let _ = tx.send(res.is_ok());
    });
    sleep(Millis(150)).await;
    assert!(!client.is_ready());

    drop(snd);
    drop(rcv);
    let res = ntex::time::timeout(Millis(2_000), rx).await;
    assert_eq!(res, Ok(Ok(true)));
}

/// STATEMENT: An invalid-length `Priority` frame on an idle stream is ignored without resetting
/// it (RFC 9113 §5.1).
///
/// HOW:
/// - Open a raw stream, then write a four-byte `Priority` payload for never-opened stream 5.
/// - Send a `Ping` and assert the next frame is its acknowledgement rather than a stream reset.
#[ntex::test]
async fn test_invalid_priority_for_idle_stream_ignored() {
    let srv = start_idle_server(ServiceConfig::new()).await;
    let (io, codec) = open_raw_stream(&srv).await;

    // PRIORITY with a 4 byte payload for idle stream 5
    let _ = io.with_write_src(|buf| buf.extend_from_slice(&[0, 0, 4, 2, 0, 0, 0, 0, 5, 0, 0, 0, 0]));
    io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
    match io.recv(&codec).await.unwrap().unwrap() {
        frame::Frame::Ping(ping) => assert!(ping.is_ack()),
        frm => panic!("unexpected frame: {frm:?}"),
    }
}

/// STATEMENT: A `GoAway` with a nonzero frame stream identifier is a connection error reporting
/// the last peer stream (RFC 9113 §6.8).
///
/// HOW:
/// - Open client stream 1 and write a raw `GoAway` whose frame header incorrectly uses stream
///   identifier 1.
/// - Send a `Ping` to flush the frame and assert the server replies with `GoAway` carrying
///   `Reason::PROTOCOL_ERROR` and last stream identifier 1.
#[ntex::test]
async fn test_goaway_with_stream_id() {
    let srv = start_idle_server(ServiceConfig::new()).await;
    let (io, codec) = open_raw_stream(&srv).await;

    let _ = io.with_write_src(|buf| {
        buf.extend_from_slice(&[0, 0, 8, 7, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    });
    io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
    let res = goaway(io.recv(&codec).await.unwrap().unwrap());
    assert_eq!(res.reason(), Reason::PROTOCOL_ERROR);
    assert_eq!(res.last_stream_id(), frame::StreamId::from(1));
}
