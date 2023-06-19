use std::fmt;

use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::future::Ready;

use super::control::{ControlMessage, ControlResult};

/// Default control service
pub struct DefaultControlService;

impl<E: fmt::Debug + 'static> ServiceFactory<ControlMessage<E>> for DefaultControlService {
    type Response = ControlResult;
    type Error = E;
    type InitError = E;
    type Service = DefaultControlService;
    type Future<'f> = Ready<Self::Service, Self::InitError>;

    fn create(&self, _: ()) -> Self::Future<'_> {
        Ready::Ok(DefaultControlService)
    }
}

impl<E: fmt::Debug + 'static> Service<ControlMessage<E>> for DefaultControlService {
    type Response = ControlResult;
    type Error = E;
    type Future<'f> = Ready<Self::Response, Self::Error>;

    #[inline]
    fn call<'a>(&'a self, msg: ControlMessage<E>, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        log::trace!("Default control service is used: {:?}", msg);
        Ready::Ok(msg.ack())
    }
}
