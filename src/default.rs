use std::{convert::Infallible, error::Error, fmt, rc::Rc};

use ntex_service::{Ctx, Service, ServiceFactory, cfg::SharedCfg};

use super::control::{Control, ControlAck};

#[derive(Copy, Clone, Debug)]
/// Default control service
pub struct DefaultControlService;

impl<E: fmt::Debug> ServiceFactory<(), Control<E>, SharedCfg> for DefaultControlService {
    type Res = ControlAck;
    type Error = Rc<dyn Error>;

    type Service = DefaultControlService;
    type InitError = Infallible;

    async fn create(&self, _: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}

impl<E: fmt::Debug> Service<(), Control<E>> for DefaultControlService {
    type Res = ControlAck;
    type Error = Rc<dyn Error>;

    async fn call(&self, msg: Control<E>, _: Ctx<'_, Self, ()>) -> Result<Self::Res, Self::Error> {
        log::trace!("Default control service is used: {msg:?}");
        Ok(msg.ack())
    }
}
