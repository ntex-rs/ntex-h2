use std::{fmt, future::poll_fn, future::Future, pin::Pin, rc::Rc};

use ntex_io::{Cfg, Dispatcher as IoDispatcher, Filter, Io, IoBoxed, SharedConfig};
use ntex_service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};
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

impl<Pub> Server<Pub, DefaultControlService>
where
    Pub: ServiceFactory<Message, SharedConfig, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    /// Create new instance of Server factory
    pub fn new(publish: Pub) -> Self {
        Self(ServerInner {
            publish: Rc::new(publish),
            control: Rc::new(DefaultControlService),
            pool: pool::new(),
        })
    }
}

impl<Pub, Ctl> Server<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, SharedConfig, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedConfig, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    /// Service to handle control frames
    pub fn control<S, F>(&self, service: F) -> Server<Pub, S>
    where
        F: IntoServiceFactory<S, Control<Pub::Error>, SharedConfig>,
        S: ServiceFactory<Control<Pub::Error>, SharedConfig, Response = ControlAck> + 'static,
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
    pub fn handler(&self, cfg: SharedConfig) -> ServerHandler<Pub, Ctl> {
        ServerHandler::new(cfg, self.0.clone())
    }
}

impl<Pub, Ctl> ServiceFactory<IoBoxed, SharedConfig> for Server<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, SharedConfig, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedConfig, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Service = ServerHandler<Pub, Ctl>;
    type InitError = ();

    async fn create(&self, cfg: SharedConfig) -> Result<Self::Service, Self::InitError> {
        Ok(ServerHandler::new(cfg, self.0.clone()))
    }
}

impl<F, Pub, Ctl> ServiceFactory<Io<F>, SharedConfig> for Server<Pub, Ctl>
where
    F: Filter,
    Ctl: ServiceFactory<Control<Pub::Error>, SharedConfig, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedConfig, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Service = ServerHandler<Pub, Ctl>;
    type InitError = ();

    async fn create(&self, cfg: SharedConfig) -> Result<Self::Service, Self::InitError> {
        Ok(ServerHandler::new(cfg, self.0.clone()))
    }
}

#[derive(Debug)]
/// Http2 connections handler
pub struct ServerHandler<Pub, Ctl> {
    inner: ServerInner<Pub, Ctl>,
    cfg: Cfg<ServiceConfig>,
    shared: SharedConfig,
}

impl<Pub, Ctl> ServerHandler<Pub, Ctl> {
    fn new(shared: SharedConfig, inner: ServerInner<Pub, Ctl>) -> Self {
        Self {
            cfg: shared.get(),
            shared,
            inner,
        }
    }
}

impl<Pub, Ctl> Clone for ServerHandler<Pub, Ctl> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            cfg: self.cfg,
            shared: self.shared,
        }
    }
}

impl<Pub, Ctl> ServerHandler<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, SharedConfig, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedConfig, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    pub async fn run(&self, io: IoBoxed) -> Result<(), ServerError<()>> {
        let inner = &self.inner;

        let (ctl_srv, pub_srv) = timeout_checked(self.cfg.handshake_timeout, async {
            read_preface(&io).await?;

            // create publish service
            let pub_srv = inner.publish.create(self.shared).await.map_err(|e| {
                log::error!("Publish service init error: {e:?}");
                ServerError::PublishServiceError
            })?;

            // create control service
            let ctl_srv = inner.control.create(self.shared).await.map_err(|e| {
                log::error!("Control service init error: {e:?}");
                ServerError::ControlServiceError
            })?;

            Ok::<_, ServerError<()>>((ctl_srv, pub_srv))
        })
        .await
        .map_err(|_| ServerError::HandshakeTimeout)??;

        // create h2 codec
        let codec = Codec::default();
        let con = Connection::new(
            true,
            io.get_ref(),
            codec.clone(),
            self.cfg,
            true,
            false,
            self.inner.pool.clone(),
        );
        let con2 = con.clone();

        // start protocol dispatcher
        let mut fut = IoDispatcher::new(io, codec, Dispatcher::new(con, ctl_srv, pub_srv));
        poll_fn(|cx| {
            if con2.config().is_shutdown() {
                con2.disconnect_when_ready();
            }
            Pin::new(&mut fut).poll(cx)
        })
        .await
        .map_err(|_| ServerError::Dispatcher)
    }
}

impl<Pub, Ctl> Service<IoBoxed> for ServerHandler<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, SharedConfig, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedConfig, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;

    async fn call(
        &self,
        io: IoBoxed,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.run(io).await
    }
}

impl<F, Pub, Ctl> Service<Io<F>> for ServerHandler<Pub, Ctl>
where
    F: Filter,
    Ctl: ServiceFactory<Control<Pub::Error>, SharedConfig, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedConfig, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;

    async fn call(
        &self,
        req: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.run(req.into()).await
    }
}

async fn read_preface(io: &IoBoxed) -> Result<(), ServerError<()>> {
    loop {
        let ready = io.with_read_buf(|buf| {
            if buf.len() >= consts::PREFACE.len() {
                if buf[..consts::PREFACE.len()] == consts::PREFACE {
                    buf.split_to(consts::PREFACE.len());
                    Ok(true)
                } else {
                    log::trace!("read_preface: invalid preface {buf:?}");
                    Err(ServerError::<()>::Frame(frame::FrameError::InvalidPreface))
                }
            } else {
                Ok(false)
            }
        })?;

        if ready {
            log::debug!("Preface has been received");
            return Ok::<_, ServerError<_>>(());
        } else {
            io.read_ready()
                .await?
                .ok_or(ServerError::Disconnected(None))?;
        }
    }
}

/// Handle io object.
pub async fn handle_one<Pub, Ctl>(
    io: IoBoxed,
    pub_svc: Pub,
    ctl_svc: Ctl,
) -> Result<(), ServerError<()>>
where
    Ctl: Service<Control<Pub::Error>, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
{
    let config: Cfg<ServiceConfig> = io.shared().get();

    // read preface
    timeout_checked(config.handshake_timeout, async { read_preface(&io).await })
        .await
        .map_err(|_| ServerError::HandshakeTimeout)??;

    // create h2 codec
    let codec = Codec::default();
    let con = Connection::new(
        true,
        io.get_ref(),
        codec.clone(),
        config.clone(),
        true,
        false,
        pool::new(),
    );
    let con2 = con.clone();

    // start protocol dispatcher
    let mut fut = IoDispatcher::new(io, codec, Dispatcher::new(con, ctl_svc, pub_svc));

    poll_fn(|cx| {
        if con2.config().is_shutdown() {
            con2.disconnect_when_ready();
        }
        Pin::new(&mut fut).poll(cx)
    })
    .await
    .map_err(|_| ServerError::Dispatcher)
}
