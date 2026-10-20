//! Client pool behavior over in-memory connections.
use std::{
    cell::RefCell,
    future::{Future, poll_fn},
    rc::Rc,
    task::Poll,
};

use ntex::http::{HeaderMap, Method};
use ntex::io::{Io, IoBoxed, testing::IoTest};
use ntex::service::cfg::SharedCfg;
use ntex::time::{Millis, sleep, timeout};
use ntex::util::{BytePages, Bytes, BytesMut};
use ntex_codec::{Decoder, Encoder};
use ntex_h2::client::{Client, ClientError, SimpleClient};
use ntex_h2::frame::{self, Frame, Reason};
use ntex_h2::{Codec, ConnectionError};
use ntex_net::connect::ConnectError;

type Peers = Rc<RefCell<Vec<IoTest>>>;

fn encode(frm: impl Into<Frame>) -> Bytes {
    let mut buf = BytePages::default();
    Codec::default().encode(frm.into(), &mut buf).unwrap();
    buf.freeze()
}

/// Creates an in-memory connection, the raw peer sends `settings`.
fn connect(
    peers: &Peers,
    settings: &frame::Settings,
) -> Result<IoBoxed, ntex_error::Error<ConnectError>> {
    let (cli, srv) = IoTest::create();
    cli.remote_buffer_cap(1024 * 1024);
    srv.remote_buffer_cap(1024 * 1024);
    srv.write(encode(*settings));
    peers.borrow_mut().push(srv);
    Ok(Io::new(cli, SharedCfg::default()).into())
}

fn settings(max_streams: Option<u32>) -> frame::Settings {
    let mut settings = frame::Settings::default();
    settings.set_max_concurrent_streams(max_streams);
    settings
}

async fn wait_for_stream_limit(client: &SimpleClient, limit: u32) {
    timeout(Millis(1_000), async {
        while client.max_streams() != Some(limit) {
            sleep(Millis(1)).await;
        }
    })
    .await
    .expect("peer stream limit was not applied");
}

/// Frames sent by the client to a raw peer.
fn client_frames(peer: &IoTest) -> Vec<Frame> {
    let out = peer.read_any();
    let mut buf = BytesMut::copy_from_slice(&out[24..]);
    let codec = Codec::default();
    let mut frames = Vec::new();
    while let Some(frm) = codec.decode(&mut buf).unwrap() {
        frames.push(frm);
    }
    frames
}

#[test]
fn client_error_conversions() {
    let err = ClientError::from(ConnectionError::MissingSettings);
    assert!(matches!(
        err,
        ClientError::Protocol(ConnectionError::MissingSettings)
    ));

    let err = ClientError::from(ntex::channel::Canceled);
    assert!(matches!(err, ClientError::Disconnected(_)));

    let _ = ntex_h2::client::Connector::<&'static str, _>::default();
}

/// A disconnecting connection is replaced by a new one.
#[ntex::test]
async fn disconnecting_connection_is_replaced() {
    let peers = Peers::default();
    let (p, s) = (peers.clone(), settings(None));
    let pool = Client::builder("localhost")
        .disconnect_timeout(Millis(100))
        .connector(async move |_| connect(&p, &s))
        .build(SharedCfg::default());

    let client1 = pool.client().await.unwrap();
    client1.ready().await.unwrap();
    assert_eq!(pool.stat_active_connections(), 1);
    let (_snd, _rcv) = client1
        .send(Method::POST, "/".into(), HeaderMap::new(), false)
        .await
        .unwrap();

    // the open stream keeps the connection
    peers.borrow()[0].write(encode(
        frame::GoAway::new(Reason::NO_ERROR).set_last_stream_id(1.into()),
    ));
    sleep(Millis(50)).await;
    assert!(client1.is_disconnecting());

    let client2 = pool.client().await.unwrap();
    assert_ne!(client1.id(), client2.id());
    assert_eq!(pool.stat_active_connections(), 1);
    assert_eq!(pool.stat_total_connections(), 2);
    pool.stat_connections(|cons| {
        assert_eq!(cons.len(), 1);
        assert_eq!(cons[0].id(), client2.id());
    });

    // the replaced connection is closed gracefully
    ntex::time::timeout(Millis(1_000), async {
        while !client1.is_closed() {
            sleep(Millis(10)).await;
        }
    })
    .await
    .expect("connection is not closed");
}

/// Connections over half of their stream capacity are used once the
/// connection limit is reached.
#[ntex::test]
async fn busy_connections_are_shared() {
    let peers = Peers::default();
    let (p, s) = (peers.clone(), settings(Some(4)));
    let pool = Client::builder("localhost")
        .connection_limit(2)
        .connector(async move |_| connect(&p, &s))
        .build(SharedCfg::default());

    let mut streams = Vec::new();
    for _ in 0..7 {
        let client = pool.client().await.unwrap();
        sleep(Millis(20)).await;
        streams.push(
            client
                .send(Method::POST, "/".into(), HeaderMap::new(), false)
                .await
                .unwrap(),
        );
    }
    assert_eq!(pool.stat_total_connections(), 2);
    let active: u32 = pool.stat_connections(|cons| cons.iter().map(|c| c.active_streams()).sum());
    assert_eq!(active, 7);
}

#[ntex::test]
async fn zero_capacity_connection_is_skipped() {
    let peers = Peers::default();
    let (p, s) = (peers.clone(), settings(Some(3)));
    let pool = Client::builder("localhost")
        .minconn(2)
        .connection_limit(2)
        .connector(async move |_| connect(&p, &s))
        .build(SharedCfg::default());

    let client1 = pool.client().await.unwrap();
    let client2 = pool.stat_connections(|cons| cons[1].clone());
    peers.borrow()[0].write(encode(settings(Some(0))));
    wait_for_stream_limit(&client1, 0).await;
    wait_for_stream_limit(&client2, 3).await;
    assert!(!client1.is_ready());

    let mut reservations = Vec::new();
    for active in [0, 2] {
        // Exercise both the preferred path and the ready-connection fallback.
        while reservations.len() < active {
            reservations.push(client2.reserve().unwrap());
        }
        let selected = timeout(Millis(1_000), pool.client()).await.unwrap().unwrap();
        assert_eq!(selected.id(), client2.id());
        assert_eq!(selected.active_streams() as usize, active);
        assert_eq!(client1.active_streams(), 0);
        assert_eq!(pool.stat_total_connections(), 2);
    }
    drop(reservations);
    assert_eq!(client2.active_streams(), 0);
}

#[ntex::test]
async fn zero_capacity_connection_opens_another() {
    let peers = Peers::default();
    let (p, s) = (peers.clone(), settings(Some(1)));
    let pool = Client::builder("localhost")
        .connection_limit(2)
        .connector(async move |_| connect(&p, &s))
        .build(SharedCfg::default());

    let client1 = pool.client().await.unwrap();
    peers.borrow()[0].write(encode(settings(Some(0))));
    wait_for_stream_limit(&client1, 0).await;

    let client2 = timeout(Millis(1_000), pool.client()).await.unwrap().unwrap();
    assert_ne!(client1.id(), client2.id());
    assert_eq!(pool.stat_total_connections(), 2);
    assert_eq!(pool.stat_active_connections(), 2);
    wait_for_stream_limit(&client2, 1).await;
    assert!(client2.is_ready());
}

#[ntex::test]
async fn zero_capacity_connection_waits_for_settings() {
    let peers = Peers::default();
    let (p, s) = (peers.clone(), settings(Some(1)));
    let pool = Client::builder("localhost")
        .connection_limit(1)
        .connector(async move |_| connect(&p, &s))
        .build(SharedCfg::default());

    let client = pool.client().await.unwrap();
    peers.borrow()[0].write(encode(settings(Some(0))));
    wait_for_stream_limit(&client, 0).await;
    assert!(!pool.is_ready());

    let mut waiting = Box::pin(pool.client());
    poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(pool.stat_total_connections(), 1);

    peers.borrow()[0].write(encode(settings(Some(1))));
    let selected = timeout(Millis(1_000), waiting).await.unwrap().unwrap();
    assert_eq!(selected.id(), client.id());
    assert_eq!(pool.stat_total_connections(), 1);
    let reservation = selected.reserve().unwrap();
    assert_eq!(client.active_streams(), 1);
    assert!(!pool.is_ready());
    drop(reservation);
    assert_eq!(client.active_streams(), 0);
    assert!(pool.is_ready());
}

/// Without the peer's limit the pool stream limit decides which
/// connection is less loaded.
#[ntex::test]
async fn pool_max_streams() {
    for (limit, same) in [(None, true), (Some(2), false)] {
        let peers = Peers::default();
        let (p, s) = (peers.clone(), settings(None));
        let builder = Client::builder("localhost").minconn(2);
        let builder = if let Some(limit) = limit {
            builder.max_streams(limit)
        } else {
            builder
        };
        let pool = builder
            .connector(async move |_| connect(&p, &s))
            .build(SharedCfg::default());

        let client1 = pool.client().await.unwrap();
        sleep(Millis(20)).await;
        assert_eq!(pool.stat_total_connections(), 2);
        assert_eq!(client1.max_streams(), None);
        let _s1 = client1
            .send(Method::POST, "/".into(), HeaderMap::new(), false)
            .await
            .unwrap();
        let _s2 = client1
            .send(Method::POST, "/".into(), HeaderMap::new(), false)
            .await
            .unwrap();

        let client2 = pool.client().await.unwrap();
        assert_eq!(client1.id() == client2.id(), same, "{limit:?}");
    }
}

/// Frames for unknown streams are not a connection error.
#[ntex::test]
async fn skip_unknown_streams() {
    for skip in [false, true] {
        let peers = Peers::default();
        let (p, s) = (peers.clone(), settings(None));
        let builder = Client::builder("localhost");
        let builder = if skip { builder.skip_unknown_streams() } else { builder };
        let pool = builder
            .connector(async move |_| connect(&p, &s))
            .build(SharedCfg::default());

        let client = pool.client().await.unwrap();
        sleep(Millis(20)).await;
        peers.borrow()[0].write(encode(frame::WindowUpdate::new(11.into(), 1)));
        sleep(Millis(50)).await;

        let frames = client_frames(&peers.borrow()[0]);
        if skip {
            assert!(!client.is_closed());
            assert!(
                frames.iter().any(|frm| matches!(
                    frm,
                    Frame::Reset(rst) if rst.stream_id() == 11 && rst.reason() == Reason::STREAM_CLOSED
                )),
                "{frames:?}"
            );
        } else {
            assert!(client.is_closed());
            assert!(
                frames.iter().any(|frm| matches!(
                    frm,
                    Frame::GoAway(frm) if frm.reason() == Reason::PROTOCOL_ERROR
                )),
                "{frames:?}"
            );
        }
    }
}
