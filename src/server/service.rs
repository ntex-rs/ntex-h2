use std::{fmt, future::Future, future::poll_fn, pin::Pin, rc::Rc};

use ntex_dispatcher::Dispatcher as IoDispatcher;
use ntex_io::{Filter, Io, IoBoxed};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{IntoServiceFactory, Pipeline, Service, ServiceCtx, ServiceFactory};
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
    Pub: ServiceFactory<Message, SharedCfg, Response = (), Data = ()> + 'static,
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
    Ctl: ServiceFactory<Control<Pub::Error>, SharedCfg, Response = ControlAck, Data = ()> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedCfg, Response = (), Data = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    /// Service to handle control frames
    pub fn control<S, F>(&self, service: F) -> Server<Pub, S>
    where
        F: IntoServiceFactory<S, Control<Pub::Error>, SharedCfg>,
        S: ServiceFactory<Control<Pub::Error>, SharedCfg, Response = ControlAck, Data = ()>
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

impl<Pub, Ctl> ServiceFactory<IoBoxed, SharedCfg> for Server<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, SharedCfg, Response = ControlAck, Data = ()> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedCfg, Response = (), Data = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Service = ServerHandler<Pub, Ctl>;
    type InitError = ();
    type Data = ();

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(ServerHandler::new(cfg, self.0.clone()))
    }

    async fn map_data(&self, _: &SharedCfg, _: &Self::Data) -> Result<(), Self::InitError> {
        Ok(())
    }
}

impl<F, Pub, Ctl> ServiceFactory<Io<F>, SharedCfg> for Server<Pub, Ctl>
where
    F: Filter,
    Ctl: ServiceFactory<Control<Pub::Error>, SharedCfg, Response = ControlAck, Data = ()> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedCfg, Response = (), Data = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Service = ServerHandler<Pub, Ctl>;
    type InitError = ();
    type Data = ();

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(ServerHandler::new(cfg, self.0.clone()))
    }

    async fn map_data(&self, _: &SharedCfg, _: &Self::Data) -> Result<(), Self::InitError> {
        Ok(())
    }
}

#[derive(Debug)]
/// Http2 connections handler
pub struct ServerHandler<Pub, Ctl> {
    cfg: Cfg<ServiceConfig>,
    inner: ServerInner<Pub, Ctl>,
    shared: SharedCfg,
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

impl<Pub, Ctl> ServerHandler<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, SharedCfg, Response = ControlAck, Data = ()> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedCfg, Response = (), Data = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    async fn run_inner(&self, io: IoBoxed) -> Result<(), ServerError<()>> {
        let inner = &self.inner;

        let (ctl_srv, ctl_data, pub_srv, pub_data) =
            timeout_checked(self.cfg.handshake_timeout, async {
                read_preface(&io).await?;

                let pub_data = inner
                    .publish
                    .map_data(&self.shared, &())
                    .await
                    .map_err(|e| {
                        log::error!("Cannot construct publish service data: {e:?}");
                        ServerError::PublishServiceError
                    })?;

                // create publish service
                let pub_srv = inner
                    .publish
                    .create(self.shared.clone())
                    .await
                    .map_err(|e| {
                        log::error!("Publish service init error: {e:?}");
                        ServerError::PublishServiceError
                    })?;

                let ctl_data = inner
                    .control
                    .map_data(&self.shared, &())
                    .await
                    .map_err(|e| {
                        log::error!("Cannot construct control service data: {e:?}");
                        ServerError::ControlServiceError
                    })?;

                // create control service
                let ctl_srv = inner
                    .control
                    .create(self.shared.clone())
                    .await
                    .map_err(|e| {
                        log::error!("Control service init error: {e:?}");
                        ServerError::ControlServiceError
                    })?;

                Ok::<_, ServerError<()>>((ctl_srv, ctl_data, pub_srv, pub_data))
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
            Dispatcher::new(con, ctl_srv, ctl_data, pub_srv, pub_data),
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

    /// Run the HTTP/2 dispatcher.
    pub async fn run(&self, io: IoBoxed) -> Result<(), ServerError<()>> {
        Pipeline::new(self.clone(), ()).call(io).await
    }
}

impl<Pub, Ctl> Service<IoBoxed> for ServerHandler<Pub, Ctl>
where
    Ctl: ServiceFactory<Control<Pub::Error>, SharedCfg, Response = ControlAck, Data = ()> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedCfg, Response = (), Data = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Data = ();

    async fn call(
        &self,
        io: IoBoxed,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.run_inner(io).await
    }
}

impl<F, Pub, Ctl> Service<Io<F>> for ServerHandler<Pub, Ctl>
where
    F: Filter,
    Ctl: ServiceFactory<Control<Pub::Error>, SharedCfg, Response = ControlAck, Data = ()> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, SharedCfg, Response = (), Data = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Data = ();

    async fn call(
        &self,
        req: Io<F>,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.run_inner(req.into()).await
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
    Ctl: Service<Control<Pub::Error>, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::Data: Default,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::Data: Default,
{
    handle_one_with_data(
        io,
        pub_svc,
        Pub::Data::default(),
        ctl_svc,
        Ctl::Data::default(),
    )
    .await
}

/// Handle one I/O object with explicit publish and control service data.
pub async fn handle_one_with_data<Pub, Ctl>(
    io: IoBoxed,
    pub_svc: Pub,
    pub_data: Pub::Data,
    ctl_svc: Ctl,
    ctl_data: Ctl::Data,
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
        Dispatcher::new(con, ctl_svc, ctl_data, pub_svc, pub_data),
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
