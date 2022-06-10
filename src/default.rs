use std::{fmt, marker::PhantomData, task::Context, task::Poll};

use ntex_service::{Service, ServiceFactory};
use ntex_util::future::Ready;

use super::control::{ControlMessage, ControlResult};

/// Default control service
pub struct DefaultControlService;

impl<E: fmt::Debug> ServiceFactory<ControlMessage<E>> for DefaultControlService {
    type Response = ControlResult;
    type Error = E;
    type InitError = E;
    type Service = DefaultControlService;
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(DefaultControlService)
    }
}

impl<E: fmt::Debug> Service<ControlMessage<E>> for DefaultControlService {
    type Response = ControlResult;
    type Error = E;
    type Future = Ready<Self::Response, Self::Error>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, msg: ControlMessage<E>) -> Self::Future {
        log::trace!("Default control service is used: {:?}", msg);
        Ready::Ok(msg.ack())
    }
}
