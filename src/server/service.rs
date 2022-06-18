use std::{fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use ntex_io::{Dispatcher as IoDispatcher, Filter, Io, IoBoxed};
use ntex_service::{Service, ServiceFactory};
use ntex_util::{future::Ready, time::timeout_checked, time::Seconds};

use crate::connection::{Config, Connection};
use crate::control::{ControlMessage, ControlResult};
use crate::{codec::Codec, consts, dispatcher::Dispatcher, frame, message::Message};

use super::{ServerBuilder, ServerError};

/// Http/2 server factory
pub struct Server<Ctl, Pub>(Rc<ServerInner<Ctl, Pub>>);

pub(crate) struct ServerInner<Ctl, Pub> {
    pub(super) control: Ctl,
    pub(super) publish: Pub,
    pub(super) config: Rc<Config>,
    pub(super) keepalive_timeout: Seconds,
    pub(super) handshake_timeout: Seconds,
    pub(super) disconnect_timeout: Seconds,
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
    pub(super) fn new(inner: ServerInner<Ctl, Pub>) -> Server<Ctl, Pub> {
        Self(Rc::new(inner))
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
    fn create(&self, req: IoBoxed) -> Pin<Box<dyn Future<Output = Result<(), ServerError<()>>>>> {
        let inner = self.0.clone();

        Box::pin(async move {
            let (ctl_srv, pub_srv) = timeout_checked(inner.handshake_timeout, async {
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
                .keepalive_timeout(inner.keepalive_timeout)
                .disconnect_timeout(inner.disconnect_timeout)
                .await
                .map_err(|_| ServerError::Dispatcher)
        })
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
        self.create(req)
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
        self.create(req.into())
    }
}
