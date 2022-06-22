use std::{fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use ntex_io::{Dispatcher as IoDispatcher, Filter, Io, IoBoxed};
use ntex_service::{Service, ServiceFactory};
use ntex_util::{future::Ready, time::timeout_checked};

use crate::connection::Connection;
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
    config: Rc<Config>,
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
        config.server();
        Self(Rc::new(ServerInner {
            control,
            publish,
            config: Rc::new(config),
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

impl<Ctl, Pub> ServerHandler<Ctl, Pub>
where
    Ctl: ServiceFactory<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
    Pub: ServiceFactory<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
    Pub::InitError: fmt::Debug,
{
    pub async fn run(&self, req: IoBoxed) -> Result<(), ServerError<()>> {
        let inner = &self.0;

        let (ctl_srv, pub_srv) = timeout_checked(inner.config.handshake_timeout.get(), async {
            // read preface
            loop {
                let ready = req.with_read_buf(|buf| {
                    if buf.len() >= consts::PREFACE.len() {
                        if buf[..consts::PREFACE.len()] == consts::PREFACE {
                            buf.split_to(consts::PREFACE.len());
                            Ok(true)
                        } else {
                            log::trace!("read_preface: invalid preface");
                            Err(ServerError::<()>::Frame(frame::FrameError::InvalidPreface))
                        }
                    } else {
                        Ok(false)
                    }
                })?;

                if ready {
                    break;
                } else {
                    req.read_ready()
                        .await?
                        .ok_or(ServerError::Disconnected(None))?;
                }
            }

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
        let codec = Rc::new(Codec::default());
        let con = Connection::new(req.get_ref(), codec.clone(), inner.config.clone());

        // start protocol dispatcher
        IoDispatcher::new(req, codec, Dispatcher::new(con, ctl_srv, pub_srv))
            .keepalive_timeout(inner.config.keepalive_timeout.get())
            .disconnect_timeout(inner.config.disconnect_timeout.get())
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

    fn call(&self, req: IoBoxed) -> Self::Future {
        let slf = ServerHandler(self.0.clone());
        Box::pin(async move { slf.run(req).await })
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
