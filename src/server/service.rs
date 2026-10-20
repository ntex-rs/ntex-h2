use std::{convert::Infallible, error::Error, fmt, future::Future, future::poll_fn, pin::Pin};

use ntex_dispatcher::Dispatcher as IoDispatcher;
use ntex_io::IoBoxed;
use ntex_service::cfg::Cfg;
use ntex_service::pipeline::{Pipeline, PipelineFactory};
use ntex_service::{Ctx, IntoServiceFactory, RequestState, Service, ServiceFactory};
use ntex_util::{channel::pool, time::timeout_checked};

use crate::control::{Control, ControlAck};
use crate::{codec::Codec, connection::Connection, default::DefaultControlService};
use crate::{config::ServiceConfig, consts, dispatcher::Dispatcher, frame, message::Message};

use super::ServerError;

// The dispatcher service can outlive its driver through detached publish calls.
struct DisconnectGuard<'a>(&'a Connection);

impl Drop for DisconnectGuard<'_> {
    fn drop(&mut self) {
        self.0.recv_half().disconnect();
    }
}

#[derive(Debug)]
/// HTTP/2 server service.
///
/// Serves one transport per call, the transport must be ready for the
/// HTTP/2 connection preface, see the [crate docs](crate#server) for the
/// message flow.
pub struct Server<Req, PErr, Err>
where
    Req: RequestState<IoBoxed>,
{
    publish: PipelineFactory<Req::State, Message, (), PErr, Box<dyn Error>>,
    control: PipelineFactory<Req::State, Control<PErr>, ControlAck, Err, Box<dyn Error>>,
    pool: pool::Pool<()>,
}

impl<Req, PErr> Server<Req, PErr, Infallible>
where
    Req: RequestState<IoBoxed>,
    Req::State: Clone,
    PErr: fmt::Debug + 'static,
{
    /// Creates a server with the specified request service factory.
    ///
    /// The publish service is created once per connection and is called with
    /// a [`Message`] for every event of peer-initiated streams.
    pub fn new<Pub>(publish: impl IntoServiceFactory<Pub, Req::State, Message>) -> Self
    where
        Pub: ServiceFactory<Req::State, Message, Res = (), Error = PErr> + 'static,
        Pub::InitError: Into<Box<dyn Error>>,
    {
        Self {
            publish: PipelineFactory::new(publish.into_factory().map_init_err(Into::into)),
            control: PipelineFactory::new(DefaultControlService.map_init_err(Into::into)),
            pool: pool::new(),
        }
    }
}

impl<Req, PErr, Err> Server<Req, PErr, Err>
where
    Req: RequestState<IoBoxed>,
    Req::State: Clone,
    PErr: fmt::Debug + 'static,
    Err: 'static,
{
    /// Sets the service factory used to handle connection control events.
    ///
    /// The control service is created once per connection, after the publish
    /// service. The default control service acknowledges every event.
    #[must_use]
    pub fn control<S>(
        self,
        f: impl IntoServiceFactory<S, Req::State, Control<PErr>>,
    ) -> Server<Req, PErr, S::Error>
    where
        S: ServiceFactory<Req::State, Control<PErr>, Res = ControlAck> + 'static,
        S::InitError: Into<Box<dyn Error>>,
    {
        Server {
            publish: self.publish,
            control: PipelineFactory::new(f.into_factory().map_init_err(Into::into)),
            pool: self.pool,
        }
    }
}

impl<Req, PErr, Err> Server<Req, PErr, Err>
where
    Req: RequestState<IoBoxed>,
    Req::State: Clone,
    PErr: fmt::Debug + 'static,
    Err: 'static,
{
    /// Runs one HTTP/2 server connection.
    ///
    /// Reads the connection preface, creates the publish and control services
    /// and serves the connection until it is closed. The preface and the
    /// services creation are limited by
    /// [`ServiceConfig::set_handshake_timeout`](crate::ServiceConfig::set_handshake_timeout).
    ///
    /// Dropping this future disconnects open streams and wakes their waiters.
    /// Cancellation does not wait for asynchronous service shutdown.
    pub async fn run(&self, req: Req) -> Result<(), ServerError<Err>> {
        let (st, io) = req.unpack();

        let shared = io.shared();
        let cfg = shared.get::<ServiceConfig>();

        let (pub_svc, ctl_svc) = timeout_checked(cfg.handshake_timeout, async {
            read_preface(&io).await?;

            // create publish and control services
            let pub_svc = self
                .publish
                .create(st.clone())
                .await
                .map_err(ServerError::PublishService)?;
            let ctl_svc = self
                .control
                .create(st)
                .await
                .map_err(ServerError::ControlService)?;
            Ok::<_, ServerError<Err>>((pub_svc, ctl_svc))
        })
        .await
        .map_err(|()| ServerError::HandshakeTimeout)??;

        // create h2 codec
        let codec = Codec::default();
        codec.set_max_headers(cfg.max_headers);

        let con = Connection::new(
            true,
            io.get_ref(),
            codec.clone(),
            cfg.clone(),
            true,
            false,
            self.pool.clone(),
        );
        let con2 = con.clone();
        let _guard = DisconnectGuard(&con2);

        // start protocol dispatcher
        let max_inflight = u32::from(con.config().max_inflight);
        let mut fut = IoDispatcher::new(
            io,
            codec,
            Pipeline::new((), Dispatcher::new(con, pub_svc, ctl_svc)),
        )
        .max_inflight(max_inflight);

        poll_fn(|cx| {
            if con2.config().is_shutdown() {
                con2.disconnect_when_ready();
            }
            Pin::new(&mut fut).poll(cx)
        })
        .await
        .map_err(ServerError::Service)
    }
}

impl<St, Req, PErr, Err> Service<St, Req> for Server<Req, PErr, Err>
where
    St: 'static,
    Req: RequestState<IoBoxed>,
    Req::State: Clone,
    PErr: fmt::Debug + 'static,
    Err: 'static,
{
    type Res = ();
    type Error = ServerError<Err>;

    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        self.run(req).await
    }
}

async fn read_preface<Err>(io: &IoBoxed) -> Result<(), ServerError<Err>> {
    let mut buf = [0; consts::PREFACE_LEN];
    io.read_exact(&mut buf).await?;

    if buf == consts::PREFACE {
        log::debug!("Preface has been received");
        Ok(())
    } else {
        log::trace!("read_preface: invalid preface {buf:?}");
        Err(ServerError::Frame(frame::FrameError::InvalidPreface))
    }
}

/// Serves one established HTTP/2 transport with existing service pipelines.
///
/// Reads the connection preface within the handshake timeout, then serves
/// the connection like [`Server::run`] with already created publish and
/// control services. The configuration is taken from the transport's
/// shared configuration.
///
/// Dropping this future disconnects open streams and wakes their waiters.
/// Cancellation does not wait for asynchronous service shutdown.
pub async fn handle_one<Err: 'static, PErr: 'static>(
    io: IoBoxed,
    pub_svc: Pipeline<Message, (), PErr>,
    ctl_svc: Pipeline<Control<PErr>, ControlAck, Err>,
) -> Result<(), ServerError<Err>> {
    let config: Cfg<ServiceConfig> = io.shared().get();

    // read preface
    timeout_checked(config.handshake_timeout, async { read_preface(&io).await })
        .await
        .map_err(|()| ServerError::HandshakeTimeout)??;

    // create h2 codec
    let codec = Codec::default();
    codec.set_max_headers(config.max_headers);
    let con = Connection::new(
        true,
        io.get_ref(),
        codec.clone(),
        config,
        true,
        false,
        pool::new(),
    );
    let con2 = con.clone();
    let _guard = DisconnectGuard(&con2);

    // start protocol dispatcher
    let max_inflight = u32::from(con.config().max_inflight);
    let mut fut = IoDispatcher::new(
        io,
        codec,
        Pipeline::new((), Dispatcher::new(con, pub_svc, ctl_svc)),
    )
    .max_inflight(max_inflight);

    poll_fn(|cx| {
        if con2.config().is_shutdown() {
            con2.disconnect_when_ready();
        }
        Pin::new(&mut fut).poll(cx)
    })
    .await
    .map_err(ServerError::Service)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::future::pending;
    use std::rc::{Rc, Weak};
    use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use ntex_bytes::BytePages;
    use ntex_codec::Encoder;
    use ntex_http::{HeaderMap, Method};
    use ntex_io::{Io, testing::IoTest};
    use ntex_service::{cfg::SharedCfg, fn_factory};
    use ntex_util::task::LocalWaker;
    use ntex_util::time::{Millis, Seconds, sleep, timeout};

    use super::*;
    use crate::stream::{StreamRef, StreamState};
    use crate::{MessageKind, OperationError};

    #[derive(Default)]
    struct Observed {
        streams: RefCell<Vec<Weak<StreamState>>>,
        waker: LocalWaker,
        block_calls: bool,
        block_shutdown: bool,
        shutting_down: Cell<bool>,
    }

    struct Publish(Rc<Observed>);

    impl Service<(), Message> for Publish {
        type Res = ();
        type Error = ();

        async fn call(&self, msg: Message, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            if matches!(msg.kind(), MessageKind::Headers { .. }) {
                self.0.streams.borrow_mut().push(Rc::downgrade(&msg.stream().0));
                self.0.waker.wake();
                if self.0.block_calls {
                    pending::<()>().await;
                }
            }
            Ok(())
        }

        async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
            self.0.shutting_down.set(true);
            self.0.waker.wake();
            if self.0.block_shutdown {
                pending::<()>().await;
            }
        }
    }

    fn connection(streams: u32, eof: bool) -> (IoBoxed, IoTest) {
        let (wire, peer) = IoTest::create();
        wire.remote_buffer_cap(1024 * 1024);
        peer.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("SRV")
            .add(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_settings_timeout(Seconds::ZERO),
            )
            .build();
        let io = Io::new(wire, cfg).into();
        peer.write(consts::PREFACE);

        let codec = Codec::default();
        let mut buf = BytePages::default();
        let mut settings = frame::Settings::default();
        settings.set_initial_window_size(Some(0));
        codec.encode(settings.into(), &mut buf).unwrap();
        for i in 0..streams {
            let headers = frame::Headers::new(
                frame::StreamId::from(2 * i + 1),
                frame::PseudoHeaders {
                    method: Some(Method::GET),
                    scheme: Some("http".into()),
                    path: Some("/".into()),
                    ..Default::default()
                },
                HeaderMap::new(),
                eof,
            );
            codec.encode(headers.into(), &mut buf).unwrap();
        }
        peer.write(buf.freeze());
        (io, peer)
    }

    async fn serve(io: IoBoxed, observed: Rc<Observed>, use_server: bool) {
        if use_server {
            Server::new(fn_factory(async move |(): &()| {
                Ok::<_, Infallible>(Publish(observed.clone()))
            }))
            .run(io)
            .await
            .unwrap();
        } else {
            handle_one(
                io,
                Pipeline::new((), Publish(observed)),
                Pipeline::new((), DefaultControlService),
            )
            .await
            .unwrap();
        }
    }

    async fn drive_until(
        mut server: Pin<&mut impl Future<Output = ()>>,
        observed: &Observed,
        ready: impl Fn() -> bool,
    ) {
        timeout(
            Millis(1000),
            poll_fn(|cx| {
                observed.waker.register(cx.waker());
                assert!(server.as_mut().poll(cx).is_pending());
                if ready() { Poll::Ready(()) } else { Poll::Pending }
            }),
        )
        .await
        .expect("server did not reach the expected state");
    }

    async fn assert_released(observed: &Observed) {
        timeout(Millis(1000), async {
            while observed
                .streams
                .borrow()
                .iter()
                .any(|stream| stream.upgrade().is_some())
            {
                sleep(Millis(10)).await;
            }
        })
        .await
        .expect("cancelled server retained open streams");
    }

    #[ntex::test]
    async fn cancellation_releases_open_streams() {
        for use_server in [false, true] {
            for block_calls in [false, true] {
                let observed = Rc::new(Observed {
                    block_calls,
                    ..Observed::default()
                });
                // Two pending calls exercise the dispatcher-held and detached futures.
                let (io, _peer) = connection(2, !block_calls);
                let mut server = Box::pin(serve(io, observed.clone(), use_server));
                drive_until(server.as_mut(), &observed, || {
                    observed.streams.borrow().len() == 2
                })
                .await;
                drop(server);

                assert_released(&observed).await;
            }
        }
    }

    struct WakeFlag(AtomicBool);

    impl Wake for WakeFlag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[ntex::test]
    async fn cancellation_fails_and_wakes_stream_waiters() {
        for use_server in [false, true] {
            let observed = Rc::new(Observed::default());
            let (io, _peer) = connection(1, false);
            let mut server = Box::pin(serve(io, observed.clone(), use_server));
            drive_until(server.as_mut(), &observed, || {
                !observed.streams.borrow().is_empty()
            })
            .await;

            let stream = StreamRef(observed.streams.borrow()[0].upgrade().unwrap());
            let wake = Arc::new(WakeFlag(AtomicBool::new(false)));
            let waker = Waker::from(wake.clone());
            let mut cx = Context::from_waker(&waker);
            let mut capacity = Box::pin(stream.send_capacity());
            assert!(capacity.as_mut().poll(&mut cx).is_pending());
            let reset = stream.on_reset();
            assert!(
                reset
                    .poll_ready(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );

            drop(server);

            assert!(wake.0.load(Ordering::SeqCst), "capacity waiter was not woken");
            let Poll::Ready(Err(err)) = capacity.as_mut().poll(&mut cx) else {
                panic!("capacity waiter did not fail");
            };
            assert!(matches!(*err, OperationError::Disconnected));
            assert!(stream.is_failed());
            assert!(reset.poll_ready(&mut cx).is_ready());
            drop(reset);
            drop(capacity);
            drop(stream);
            assert!(observed.streams.borrow()[0].upgrade().is_none());
        }
    }

    #[ntex::test]
    async fn cancellation_during_service_shutdown() {
        for use_server in [false, true] {
            let observed = Rc::new(Observed {
                block_shutdown: true,
                ..Observed::default()
            });
            let (io, peer) = connection(1, false);
            let mut server = Box::pin(serve(io, observed.clone(), use_server));
            drive_until(server.as_mut(), &observed, || {
                !observed.streams.borrow().is_empty()
            })
            .await;
            let stream = StreamRef(observed.streams.borrow()[0].upgrade().unwrap());

            peer.close().await;
            drive_until(server.as_mut(), &observed, || observed.shutting_down.get()).await;
            drop(server);

            assert!(stream.is_failed());
            let err = stream.send_capacity().await.unwrap_err();
            assert!(matches!(*err, OperationError::Disconnected));
            drop(stream);
            assert_released(&observed).await;
        }
    }
}
