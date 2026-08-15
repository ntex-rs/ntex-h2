use std::{fmt, future::Future, future::poll_fn, marker::PhantomData, pin::Pin, rc::Rc};

use ntex_dispatcher::Dispatcher as IoDispatcher;
use ntex_io::{Filter, Io, IoBoxed};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Ctx, IntoServiceFactory, Pipeline, Service, ServiceFactory};
use ntex_util::{channel::pool, time::timeout_checked};

use crate::control::{Control, ControlAck};
use crate::{codec::Codec, connection::Connection, default::DefaultControlService};
use crate::{config::ServiceConfig, consts, dispatcher::Dispatcher, frame, message::Message};

use super::ServerError;

#[derive(Debug)]
/// Http/2 server factory
pub struct Server<Pub, Ctl>(ServerInner<Pub, Ctl>);

#[derive(Debug)]
struct ServerInner<Pub, Ctl> {
    control: Rc<Ctl>,
    publish: Rc<Pub>,
    pool: pool::Pool<()>,
}

impl<Pub, Ctl> Clone for ServerInner<Pub, Ctl> {
    fn clone(&self) -> Self {
        Self {
            control: self.control.clone(),
            publish: self.publish.clone(),
            pool: self.pool.clone(),
        }
    }
}

impl<Pub> Server<Pub, DefaultControlService<Pub::Error>>
where
    Pub: ServiceFactory<Message, St = (), InitCfg = SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    /// Create new instance of Server factory
    pub fn new(publish: Pub) -> Self {
        Self(ServerInner {
            publish: Rc::new(publish),
            control: Rc::new(DefaultControlService::new()),
            pool: pool::new(),
        })
    }
}

impl<Pub, Ctl> Server<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, St = (), InitCfg = SharedCfg, Res = ControlAck>
        + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, St = (), InitCfg = SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    /// Service to handle control frames
    pub fn control<S, F>(&self, service: F) -> Server<Pub, S>
    where
        F: IntoServiceFactory<S, Control<Pub::Error>>,
        S: ServiceFactory<Control<Pub::Error>, St = (), InitCfg = SharedCfg, Res = ControlAck>
            + 'static,
        S::Error: fmt::Debug,
        S::InitError: fmt::Debug,
    {
        Server(ServerInner {
            control: Rc::new(service.into_factory()),
            publish: self.0.publish.clone(),
            pool: self.0.pool.clone(),
        })
    }

    /// Construct service handler
    pub fn handler(&self, cfg: SharedCfg) -> ServerHandler<Pub, Ctl> {
        ServerHandler::new(cfg, self.0.clone())
    }
}

impl<Pub, Ctl> ServiceFactory<IoBoxed> for Server<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, St = (), InitCfg = SharedCfg, Res = ControlAck>
        + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, St = (), InitCfg = SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type St = ();
    type Res = ();
    type Error = ServerError<()>;
    type Service = ServerHandler<Pub, Ctl>;
    type InitCfg = SharedCfg;
    type InitError = ();

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(ServerHandler::new(cfg.clone(), self.0.clone()))
    }
}

impl<F: Filter, Pub, Ctl> ServiceFactory<Io<F>> for Server<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, St = (), InitCfg = SharedCfg, Res = ControlAck>
        + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, St = (), InitCfg = SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type St = ();
    type Res = ();
    type Error = ServerError<()>;
    type Service = ServerHandlerF<Pub, Ctl, F>;
    type InitCfg = SharedCfg;
    type InitError = ();

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(ServerHandlerF::new(cfg.clone(), self.0.clone()))
    }
}

#[derive(Debug)]
/// Http2 connections handler
pub struct ServerHandler<Pub, Ctl> {
    cfg: Cfg<ServiceConfig>,
    inner: ServerInner<Pub, Ctl>,
    shared: SharedCfg,
}

#[derive(Debug)]
/// Http2 connections handler
pub struct ServerHandlerF<Pub, Ctl, F> {
    hnd: ServerHandler<Pub, Ctl>,
    f: PhantomData<F>,
}

impl<Pub, Ctl> ServerHandler<Pub, Ctl> {
    fn new(shared: SharedCfg, inner: ServerInner<Pub, Ctl>) -> Self {
        let cfg = shared.get();
        Self { cfg, inner, shared }
    }
}

impl<Pub, Ctl> Clone for ServerHandler<Pub, Ctl> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            cfg: self.cfg.clone(),
            shared: self.shared.clone(),
        }
    }
}

impl<Pub, Ctl, F> ServerHandlerF<Pub, Ctl, F> {
    fn new(shared: SharedCfg, inner: ServerInner<Pub, Ctl>) -> Self {
        Self {
            hnd: ServerHandler::new(shared, inner),
            f: PhantomData,
        }
    }
}

impl<Pub, Ctl, F> Clone for ServerHandlerF<Pub, Ctl, F> {
    fn clone(&self) -> Self {
        Self {
            hnd: self.hnd.clone(),
            f: PhantomData,
        }
    }
}

impl<Pub, Ctl> ServerHandler<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, St = (), InitCfg = SharedCfg, Res = ControlAck>
        + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, St = (), InitCfg = SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    pub async fn run(&self, io: IoBoxed) -> Result<(), ServerError<()>> {
        let inner = &self.inner;

        let (ctl_srv, pub_srv) = timeout_checked(self.cfg.handshake_timeout, async {
            read_preface(&io).await?;

            // create publish service
            let pub_srv = inner.publish.create(&self.shared).await.map_err(|e| {
                log::error!("Publish service init error: {e:?}");
                ServerError::PublishServiceError
            })?;

            // create control service
            let ctl_srv = inner.control.create(&self.shared).await.map_err(|e| {
                log::error!("Control service init error: {e:?}");
                ServerError::ControlServiceError
            })?;

            Ok::<_, ServerError<()>>((ctl_srv, pub_srv))
        })
        .await
        .map_err(|()| ServerError::HandshakeTimeout)??;

        // create h2 codec
        let codec = Codec::default();
        codec.set_max_headers(self.cfg.max_headers);

        let con = Connection::new(
            true,
            io.get_ref(),
            codec.clone(),
            self.cfg.clone(),
            true,
            false,
            self.inner.pool.clone(),
        );
        let con2 = con.clone();

        // start protocol dispatcher
        let mut fut = IoDispatcher::new(
            io,
            codec,
            Pipeline::new(Dispatcher::new(con, ctl_srv, pub_srv)).bind(),
        );
        poll_fn(|cx| {
            if con2.config().is_shutdown() {
                con2.disconnect_when_ready();
            }
            Pin::new(&mut fut).poll(cx)
        })
        .await
        .map_err(|()| ServerError::Dispatcher)
    }
}

impl<Pub, Ctl> Service for ServerHandler<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, St = (), Res = ControlAck, InitCfg = SharedCfg>
        + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, St = (), Res = (), InitCfg = SharedCfg> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type St = ();
    type Req = IoBoxed;
    type Res = ();
    type Error = ServerError<()>;

    async fn call(&self, io: IoBoxed, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        self.run(io).await
    }
}

impl<F: Filter, Pub, Ctl> Service for ServerHandlerF<Pub, Ctl, F>
where
    Ctl: ServiceFactory<Control<Pub::Error>, St = (), Res = ControlAck, InitCfg = SharedCfg>
        + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, St = (), Res = (), InitCfg = SharedCfg> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type St = ();
    type Req = Io<F>;
    type Res = ();
    type Error = ServerError<()>;

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        self.hnd.run(io.boxed()).await
    }
}

async fn read_preface(io: &IoBoxed) -> Result<(), ServerError<()>> {
    let mut buf = [0; consts::PREFACE_LEN];
    io.read(&mut buf).await?;

    if buf == consts::PREFACE {
        log::debug!("Preface has been received");
        Ok(())
    } else {
        log::trace!("read_preface: invalid preface {buf:?}");
        Err(ServerError::<()>::Frame(frame::FrameError::InvalidPreface))
    }
}

/// Handle io object.
pub async fn handle_one<Pub, Ctl>(
    io: IoBoxed,
    pub_svc: Pub,
    ctl_svc: Ctl,
) -> Result<(), ServerError<()>>
where
    Ctl: Service<St = (), Req = Control<Pub::Error>, Res = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<St = (), Req = Message, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
{
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

    // start protocol dispatcher
    let mut fut = IoDispatcher::new(
        io,
        codec,
        Pipeline::new(Dispatcher::new(con, ctl_svc, pub_svc)).bind(),
    );

    poll_fn(|cx| {
        if con2.config().is_shutdown() {
            con2.disconnect_when_ready();
        }
        Pin::new(&mut fut).poll(cx)
    })
    .await
    .map_err(|()| ServerError::Dispatcher)
}
