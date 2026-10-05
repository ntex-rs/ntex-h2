use std::{cell::Cell, io, rc::Rc};

use ntex::http::{HeaderMap, Method};
use ntex::io::{Io, IoBoxed, testing::IoTest};
use ntex::service::{Ctx, Pipeline, Service, ServiceFactory, cfg::SharedCfg};
use ntex::time::{Millis, Seconds};
use ntex_h2::client::SimpleClient;
use ntex_h2::server::{Server, ServerError, handle_one};
use ntex_h2::{Control, ControlAck, Message, ServiceConfig, frame};

const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[test]
fn server_error_display_and_conversion() {
    let err = ServerError::<()>::Frame(frame::FrameError::InvalidPreface);
    assert_eq!(err.to_string(), "Http/2 codec error: An invalid preface");

    let err = ServerError::<()>::PublishService(Box::new(io::Error::other("publish")));
    assert_eq!(err.to_string(), "Publish service init error");

    let err = ServerError::<()>::ControlService(Box::new(io::Error::other("control")));
    assert_eq!(err.to_string(), "Control service init error");

    let err = ServerError::<()>::HandshakeTimeout;
    assert_eq!(err.to_string(), "Handshake timeout");

    let err = ServerError::Service("control");
    assert_eq!(err.to_string(), "Control service error");

    let err = ServerError::<()>::Disconnected(None);
    assert_eq!(err.to_string(), "Peer is disconnected, error: None");

    let err: ServerError<()> = io::Error::new(io::ErrorKind::ConnectionReset, "gone").into();
    assert!(matches!(
        err,
        ServerError::Disconnected(Some(ref source))
            if source.kind() == io::ErrorKind::ConnectionReset
    ));
}

#[ntex::test]
async fn server_handshake_failures() {
    let (io, peer) = IoTest::create();
    peer.remote_buffer_cap(1024 * 1024);
    peer.write([0; 24]);
    let server = Pipeline::new((), Server::new(async |_msg: Message| Ok::<_, ()>(())));
    let err = server.call(Io::new(io, SharedCfg::default())).await.unwrap_err();
    assert!(matches!(
        err,
        ServerError::Frame(frame::FrameError::InvalidPreface)
    ));

    let (io, peer) = IoTest::create();
    peer.close().await;
    let err = Server::new(async |_msg: Message| Ok::<_, ()>(()))
        .run(Io::new(io, SharedCfg::default()))
        .await
        .unwrap_err();
    assert!(matches!(err, ServerError::Disconnected(_)));

    let (io, _peer) = IoTest::create();
    let cfg = SharedCfg::new("SRV")
        .add(ServiceConfig::new().set_handshake_timeout(Seconds(1)))
        .build();
    let err = Server::new(async |_msg: Message| Ok::<_, ()>(()))
        .run(Io::new(io, cfg))
        .await
        .unwrap_err();
    assert!(matches!(err, ServerError::HandshakeTimeout));
}

#[derive(Debug)]
struct PublishService;

impl Service<(), Message> for PublishService {
    type Res = ();
    type Error = ();

    async fn call(&self, _: Message, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        Ok(())
    }
}

#[derive(Debug)]
struct FailingPublishFactory;

impl ServiceFactory<(), Message> for FailingPublishFactory {
    type Res = ();
    type Error = ();
    type Service = PublishService;
    type InitError = io::Error;

    async fn create(&self, _: &()) -> Result<Self::Service, Self::InitError> {
        Err(io::Error::other("publish init"))
    }
}

#[ntex::test]
async fn server_publish_service_init_error() {
    let (io, peer) = IoTest::create();
    peer.remote_buffer_cap(1024 * 1024);
    peer.write(PREFACE);

    let err = Server::new(FailingPublishFactory)
        .run(Io::new(io, SharedCfg::default()))
        .await
        .unwrap_err();
    let ServerError::PublishService(source) = err else {
        panic!("unexpected server error: {err:?}")
    };
    assert_eq!(source.to_string(), "publish init");
}

#[derive(Debug, Default)]
struct ControlCounters {
    created: Cell<usize>,
    shutdown: Cell<usize>,
}

#[derive(Debug)]
struct ControlService(Rc<ControlCounters>);

impl Service<(), Control<()>> for ControlService {
    type Res = ControlAck;
    type Error = ();

    async fn call(&self, msg: Control<()>, _: Ctx<'_, Self, ()>) -> Result<ControlAck, ()> {
        Ok(msg.ack())
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.0.shutdown.set(self.0.shutdown.get() + 1);
    }
}

#[derive(Debug, Clone)]
struct ControlFactory(Option<Rc<ControlCounters>>);

impl ServiceFactory<(), Control<()>> for ControlFactory {
    type Res = ControlAck;
    type Error = ();
    type Service = ControlService;
    type InitError = io::Error;

    async fn create(&self, _: &()) -> Result<Self::Service, Self::InitError> {
        if let Some(ref counters) = self.0 {
            counters.created.set(counters.created.get() + 1);
            Ok(ControlService(counters.clone()))
        } else {
            Err(io::Error::other("control init"))
        }
    }
}

#[ntex::test]
async fn server_control_service_init_error() {
    let (io, peer) = IoTest::create();
    peer.remote_buffer_cap(1024 * 1024);
    peer.write(PREFACE);

    let err = Server::new(async |_msg: Message| Ok::<_, ()>(()))
        .control(ControlFactory(None))
        .run(Io::new(io, SharedCfg::default()))
        .await
        .unwrap_err();
    let ServerError::ControlService(source) = err else {
        panic!("unexpected server error: {err:?}")
    };
    assert_eq!(source.to_string(), "control init");
}

#[ntex::test]
async fn server_control_service_per_connection() {
    let counters = Rc::new(ControlCounters::default());
    let server = Rc::new(
        Server::new(async |_msg: Message| Ok::<_, ()>(()))
            .control(ControlFactory(Some(counters.clone()))),
    );

    let mut peers = Vec::new();
    for _ in 0..2 {
        let (io, peer) = IoTest::create();
        peer.remote_buffer_cap(1024 * 1024);
        peer.write(PREFACE);
        let server = server.clone();
        ntex::rt::spawn(async move {
            let _ = server.run(Io::new(io, SharedCfg::default())).await;
        });
        peers.push(peer);
    }
    ntex::time::sleep(Millis(100)).await;
    assert_eq!(counters.created.get(), 2);
    assert_eq!(counters.shutdown.get(), 0);

    // control services are shut down with their connections
    for peer in peers {
        peer.close().await;
    }
    ntex::time::sleep(Millis(100)).await;
    assert_eq!(counters.shutdown.get(), 2);
}

#[ntex::test]
async fn server_control_error_is_returned() {
    let (client_io, server_io) = IoTest::create();
    client_io.remote_buffer_cap(1024 * 1024);
    server_io.remote_buffer_cap(1024 * 1024);

    let server = Server::new(async |_msg: Message| Err::<(), ()>(()))
        .control(async |_msg: Control<()>| Err::<ControlAck, _>("control failed"));
    let (tx, rx) = ntex::channel::oneshot::channel();
    ntex::rt::spawn(async move {
        let result = server.run(Io::new(server_io, SharedCfg::default())).await;
        let _ = tx.send(result);
    });

    let client = SimpleClient::new(
        Io::new(client_io, SharedCfg::default()),
        false,
        "localhost".into(),
    );
    let _ = client.send(Method::GET, "/".into(), HeaderMap::new(), true).await;

    let err = ntex::time::timeout(Millis(1_000), rx)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, ServerError::Service("control failed")));
}

#[ntex::test]
async fn handle_one_rejects_invalid_preface() {
    let (io, peer) = IoTest::create();
    peer.remote_buffer_cap(1024 * 1024);
    peer.write([0; 24]);

    let publish = Pipeline::new((), async |_msg: Message| Ok::<_, ()>(()));
    let control = Pipeline::new((), async |msg: Control<()>| Ok::<_, ()>(msg.ack()));
    let io: IoBoxed = Io::new(io, SharedCfg::default()).into();

    let err = handle_one(io, publish, control).await.unwrap_err();
    assert!(matches!(
        err,
        ServerError::Frame(frame::FrameError::InvalidPreface)
    ));
}
