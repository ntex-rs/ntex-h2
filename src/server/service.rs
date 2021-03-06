use std::{fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use ntex_io::{Dispatcher as IoDispatcher, Filter, Io, IoBoxed};
use ntex_service::{Service, ServiceFactory};
use ntex_util::{future::Ready, time::sleep, time::timeout_checked, time::Seconds};

use crate::connection::{Connection, ConnectionFlags};
use crate::control::{ControlMessage, ControlResult};
use crate::{
    codec::Codec, config::Config, consts, dispatcher::Dispatcher, frame, message::Message,
};

use super::{ServerBuilder, ServerError};

/// Http/2 server factory
pub struct Server<Ctl, Pub>(Rc<ServerInner<Ctl, Pub>>);

struct ServerInner<Ctl, Pub> {
    control: Ctl,
    publish: Pub,
    config: Config,
}

impl Server<(), ()> {
    /// Returns a new server builder instance initialized with default
    /// configuration values.
    pub fn build<E>() -> ServerBuilder<E> {
        ServerBuilder::new()
    }
}

impl<Ctl, Pub> Server<Ctl, Pub>
where
    Ctl: ServiceFactory<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    /// Create new instance of Server factory
    pub fn new(config: Config, control: Ctl, publish: Pub) -> Server<Ctl, Pub> {
        Self(Rc::new(ServerInner {
            control,
            publish,
            config,
        }))
    }

    /// Construct service handler
    pub fn handler(&self) -> ServerHandler<Ctl, Pub> {
        ServerHandler(self.0.clone())
    }
}

impl<Ctl, Pub> ServiceFactory<IoBoxed> for Server<Ctl, Pub>
where
    Ctl: ServiceFactory<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Service = ServerHandler<Ctl, Pub>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(ServerHandler(self.0.clone()))
    }
}

impl<F, Ctl, Pub> ServiceFactory<Io<F>> for Server<Ctl, Pub>
where
    F: Filter,
    Ctl: ServiceFactory<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Service = ServerHandler<Ctl, Pub>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(ServerHandler(self.0.clone()))
    }
}

/// Http2 connections handler
pub struct ServerHandler<Ctl, Pub>(Rc<ServerInner<Ctl, Pub>>);

impl<Ctl, Pub> Clone for ServerHandler<Ctl, Pub> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Ctl, Pub> ServerHandler<Ctl, Pub>
where
    Ctl: ServiceFactory<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    pub async fn run(&self, io: IoBoxed) -> Result<(), ServerError<()>> {
        let inner = &self.0;

        let (ctl_srv, pub_srv) = timeout_checked(inner.config.0.handshake_timeout.get(), async {
            read_preface(&io).await?;

            // create publish service
            let pub_srv = inner.publish.new_service(()).await.map_err(|e| {
                log::error!("Publish service init error: {:?}", e);
                ServerError::PublishServiceError
            })?;

            // create control service
            let ctl_srv = inner.control.new_service(()).await.map_err(|e| {
                log::error!("Control service init error: {:?}", e);
                ServerError::ControlServiceError
            })?;

            Ok::<_, ServerError<()>>((ctl_srv, pub_srv))
        })
        .await
        .map_err(|_| ServerError::HandshakeTimeout)??;

        // create h2 codec
        let (codec, con) = create_connection(&io, &inner.config);

        // start protocol dispatcher
        IoDispatcher::new(io, codec, Dispatcher::new(con, ctl_srv, pub_srv))
            .keepalive_timeout(Seconds::ZERO)
            .disconnect_timeout(inner.config.0.disconnect_timeout.get())
            .await
            .map_err(|_| ServerError::Dispatcher)
    }
}

impl<Ctl, Pub> Service<IoBoxed> for ServerHandler<Ctl, Pub>
where
    Ctl: ServiceFactory<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(&self, _: &mut Context<'_>, _is_error: bool) -> Poll<()> {
        Poll::Ready(())
    }

    fn call(&self, io: IoBoxed) -> Self::Future {
        let slf = ServerHandler(self.0.clone());
        Box::pin(async move { slf.run(io).await })
    }
}

impl<F, Ctl, Pub> Service<Io<F>> for ServerHandler<Ctl, Pub>
where
    F: Filter,
    Ctl: ServiceFactory<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    type Response = ();
    type Error = ServerError<()>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(&self, _: &mut Context<'_>, _is_error: bool) -> Poll<()> {
        Poll::Ready(())
    }

    fn call(&self, req: Io<F>) -> Self::Future {
        let slf = ServerHandler(self.0.clone());
        Box::pin(async move { slf.run(req.into()).await })
    }
}

fn create_connection(io: &IoBoxed, config: &Config) -> (Codec, Connection) {
    // create h2 codec
    let codec = Codec::default();
    let con = Connection::new(io.get_ref(), codec.clone(), config.clone(), true);

    // slow request timeout
    let timeout = config.0.client_timeout.get();
    if !timeout.is_zero() {
        con.state().set_flags(ConnectionFlags::SLOW_REQUEST_TIMEOUT);

        let state = con.get_state();
        ntex_rt::spawn(async move {
            sleep(timeout).await;

            if state
                .flags()
                .contains(ConnectionFlags::SLOW_REQUEST_TIMEOUT)
            {
                state.io.close()
            }
        });
    }

    (codec, con)
}

async fn read_preface(io: &IoBoxed) -> Result<(), ServerError<()>> {
    loop {
        let ready = io.with_read_buf(|buf| {
            if buf.len() >= consts::PREFACE.len() {
                if buf[..consts::PREFACE.len()] == consts::PREFACE {
                    buf.split_to(consts::PREFACE.len());
                    Ok(true)
                } else {
                    log::trace!("read_preface: invalid preface {:?}", buf);
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
pub async fn handle_one<Ctl, Pub>(
    io: IoBoxed,
    config: Config,
    ctl_svc: Ctl,
    pub_svc: Pub,
) -> Result<(), ServerError<()>>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
{
    // read preface
    timeout_checked(config.0.handshake_timeout.get(), async {
        read_preface(&io).await
    })
    .await
    .map_err(|_| ServerError::HandshakeTimeout)??;

    // create h2 codec
    let (codec, con) = create_connection(&io, &config);

    // start protocol dispatcher
    IoDispatcher::new(io, codec, Dispatcher::new(con, ctl_svc, pub_svc))
        .keepalive_timeout(Seconds::ZERO)
        .disconnect_timeout(config.0.disconnect_timeout.get())
        .await
        .map_err(|_| ServerError::Dispatcher)
}
