use std::{error::Error, fmt, future::Future, future::poll_fn, marker, pin::Pin, rc::Rc};

use ntex_dispatcher::Dispatcher as IoDispatcher;
use ntex_io::{Filter, Io, IoBoxed};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::pipeline::{Pipeline, PipelineBinding};
use ntex_service::{Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory};
use ntex_util::{channel::pool, time::timeout_checked};

use crate::control::{Control, ControlAck};
use crate::{codec::Codec, connection::Connection, default::DefaultControlService};
use crate::{config::ServiceConfig, consts, dispatcher::Dispatcher, frame, message::Message};

use super::ServerError;

#[derive(Debug)]
/// Http/2 server factory
pub struct Server<Pub, Err>
where
    Pub: ServiceFactory<(), Message, SharedCfg, Res = ()>,
{
    publish: Pub,
    control: Pipeline<Control<Pub::Error>, ControlAck, Rc<dyn Error>>,
    pool: pool::Pool<()>,
    err: marker::PhantomData<Err>,
}

impl<Pub, Err> Server<Pub, Err>
where
    Err: 'static,
    Pub: ServiceFactory<(), Message, SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
{
    /// Create new instance of Server factory
    pub fn new(publish: impl IntoServiceFactory<Pub, (), Message, SharedCfg>) -> Self {
        Self {
            publish: publish.into_factory(),
            control: Pipeline::new(DefaultControlService),
            pool: pool::new(),
            err: marker::PhantomData,
        }
    }
}

impl<Pub, Err> Server<Pub, Err>
where
    Err: 'static,
    Pub: ServiceFactory<(), Message, SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
{
    /// Service to handle control frames
    #[must_use]
    pub fn control<S>(self, f: impl IntoService<S, (), Control<Pub::Error>>) -> Server<Pub, Err>
    where
        S: Service<(), Control<Pub::Error>, Res = ControlAck> + 'static,
        S::Error: Into<Rc<dyn Error>>,
    {
        Server {
            publish: self.publish,
            control: Pipeline::new(f.into_service().map_err(Into::into)),
            pool: self.pool,
            err: marker::PhantomData,
        }
    }
}

impl<Pub, Err> Server<Pub, Err>
where
    Err: 'static,
    Pub: ServiceFactory<(), Message, SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
{
    pub async fn run(&self, io: IoBoxed) -> Result<(), ServerError<Err>> {
        let shared = io.shared();
        let cfg = shared.get::<ServiceConfig>();

        let pub_svc = timeout_checked(cfg.handshake_timeout, async {
            read_preface(&io).await?;

            // create publish service
            self.publish
                .create(&shared)
                .await
                .map_err(|e| ServerError::PublishService(e.into()))
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

        // start protocol dispatcher
        let mut fut = IoDispatcher::new(
            io,
            codec,
            Pipeline::new(Dispatcher::new(con, Pipeline::new(pub_svc), self.control.bind())),
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

impl<St, Pub, Err> Service<St, IoBoxed> for Server<Pub, Err>
where
    Err: 'static,
    Pub: ServiceFactory<(), Message, SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
{
    type Res = ();
    type Error = ServerError<Err>;

    async fn call(&self, io: IoBoxed, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        self.run(io).await
    }
}

impl<F: Filter, St, Pub, Err> Service<St, Io<F>> for Server<Pub, Err>
where
    Err: 'static,
    Pub: ServiceFactory<(), Message, SharedCfg, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
{
    type Res = ();
    type Error = ServerError<Err>;

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        self.run(io.boxed()).await
    }
}

async fn read_preface<Err>(io: &IoBoxed) -> Result<(), ServerError<Err>> {
    let mut buf = [0; consts::PREFACE_LEN];
    io.read(&mut buf).await?;

    if buf == consts::PREFACE {
        log::debug!("Preface has been received");
        Ok(())
    } else {
        log::trace!("read_preface: invalid preface {buf:?}");
        Err(ServerError::Frame(frame::FrameError::InvalidPreface))
    }
}

/// Handle io object.
pub async fn handle_one<Err: 'static>(
    io: IoBoxed,
    pub_svc: Pipeline<Message, (), Err>,
    ctl_svc: PipelineBinding<Control<Err>, ControlAck, Rc<dyn Error>>,
) -> Result<(), ServerError<()>> {
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
    let mut fut = IoDispatcher::new(io, codec, Pipeline::new(Dispatcher::new(con, pub_svc, ctl_svc)));

    poll_fn(|cx| {
        if con2.config().is_shutdown() {
            con2.disconnect_when_ready();
        }
        Pin::new(&mut fut).poll(cx)
    })
    .await
    .map_err(|()| ServerError::Dispatcher)
}
