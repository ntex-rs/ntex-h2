use std::{convert::Infallible, fmt};

use ntex_service::{Ctx, Service, ServiceFactory};

use super::control::{Control, ControlAck};

#[derive(Copy, Clone, Debug)]
/// Default control service
pub struct DefaultControlService;

impl<St, E: fmt::Debug> ServiceFactory<St, Control<E>> for DefaultControlService {
    type Res = ControlAck;
    type Error = Infallible;

    type Service = DefaultControlService;
    type InitError = Infallible;

    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}

impl<St, E: fmt::Debug> Service<St, Control<E>> for DefaultControlService {
    type Res = ControlAck;
    type Error = Infallible;

    async fn call(&self, msg: Control<E>, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        log::trace!("Default control service is used: {msg:?}");
        Ok(msg.ack())
    }
}
