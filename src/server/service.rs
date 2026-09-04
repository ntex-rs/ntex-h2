use std::{error::Error, fmt, future::Future, future::poll_fn, marker, pin::Pin, rc::Rc};

use ntex_dispatcher::Dispatcher as IoDispatcher;
use ntex_io::IoBoxed;
use ntex_service::cfg::Cfg;
use ntex_service::pipeline::{Pipeline, PipelineBinding, PipelineState};
use ntex_service::{Ctx, IntoService, IntoServiceFactory, RequestState, Service, ServiceFactory};
use ntex_util::{channel::pool, time::timeout_checked};

use crate::control::{Control, ControlAck};
use crate::{codec::Codec, connection::Connection, default::DefaultControlService};
use crate::{config::ServiceConfig, consts, dispatcher::Dispatcher, frame, message::Message};

use super::ServerError;

#[derive(Debug)]
/// Http/2 server factory
pub struct Server<Req, Pub, Err>
where
    Req: RequestState<IoBoxed>,
    Pub: ServiceFactory<Req::State, Message, Res = ()>,
{
    publish: Pub,
    control: PipelineState<Req::State, Control<Pub::Error>, ControlAck, Rc<dyn Error>>,
    pool: pool::Pool<()>,
    err: marker::PhantomData<Err>,
}

impl<Req, Pub, Err> Server<Req, Pub, Err>
where
    Req: RequestState<IoBoxed>,
    Req::State: Clone,
    Pub: ServiceFactory<Req::State, Message, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
    Err: 'static,
{
    /// Create new instance of Server factory
    pub fn new(publish: impl IntoServiceFactory<Pub, Req::State, Message>) -> Self {
        Self {
            publish: publish.into_factory(),
            control: PipelineState::new(DefaultControlService),
            pool: pool::new(),
            err: marker::PhantomData,
        }
    }
}

impl<Req, Pub, Err> Server<Req, Pub, Err>
where
    Req: RequestState<IoBoxed>,
    Req::State: Clone,
    Pub: ServiceFactory<Req::State, Message, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
    Err: 'static,
{
    /// Service to handle control frames
    #[must_use]
    pub fn control<S>(
        self,
        f: impl IntoService<S, Req::State, Control<Pub::Error>>,
    ) -> Server<Req, Pub, Err>
    where
        S: Service<Req::State, Control<Pub::Error>, Res = ControlAck> + 'static,
        S::Error: Into<Rc<dyn Error>>,
    {
        Server {
            publish: self.publish,
            control: PipelineState::new(f.into_service().map_err(Into::into)),
            pool: self.pool,
            err: marker::PhantomData,
        }
    }
}

impl<Req, Pub, Err> Server<Req, Pub, Err>
where
    Req: RequestState<IoBoxed>,
    Req::State: Clone,
    Pub: ServiceFactory<Req::State, Message, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
    Err: 'static,
{
    pub async fn run<St>(&self, st: Req::State, io: IoBoxed) -> Result<(), ServerError<Err>> {
        let shared = io.shared();
        let cfg = shared.get::<ServiceConfig>();

        let pub_svc = timeout_checked(cfg.handshake_timeout, async {
            read_preface(&io).await?;

            // create publish service
            self.publish
                .create(&st)
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
            Pipeline::new(
                (),
                Dispatcher::new(
                    con,
                    Pipeline::new(st.clone(), pub_svc),
                    self.control.bind_state(st),
                ),
            ),
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

impl<St, Req, Pub, Err> Service<St, Req> for Server<Req, Pub, Err>
where
    St: 'static,
    Req: RequestState<IoBoxed>,
    Req::State: Clone,
    Pub: ServiceFactory<Req::State, Message, Res = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: Into<Box<dyn Error>>,
    Err: 'static,
{
    type Res = ();
    type Error = ServerError<Err>;

    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let (st, io) = req.unpack();
        self.run::<St>(st, io).await
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
    let mut fut = IoDispatcher::new(
        io,
        codec,
        Pipeline::new((), Dispatcher::new(con, pub_svc, ctl_svc)),
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
