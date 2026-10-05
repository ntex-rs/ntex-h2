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
