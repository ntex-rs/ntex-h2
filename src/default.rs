use std::{error::Error, fmt, marker::PhantomData, rc::Rc};

use ntex_service::{Ctx, Service, ServiceFactory, cfg::SharedCfg};

use super::control::{Control, ControlAck};

#[derive(Copy, Clone, Debug)]
/// Default control service
pub struct DefaultControlService<E>(PhantomData<E>);

impl<E> DefaultControlService<E> {
    pub fn new() -> Self {
        DefaultControlService(PhantomData)
    }
}

impl<E: fmt::Debug> ServiceFactory<(), Control<E>> for DefaultControlService<E> {
    type Res = ControlAck;
    type Error = Rc<dyn Error>;
    type InitCfg = SharedCfg;
    type InitError = E;
    type Service = DefaultControlService<E>;

    async fn create(&self, _: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService::new())
    }
}

impl<E: fmt::Debug> Service<()> for DefaultControlService<E> {
    type Req = Control<E>;
    type Res = ControlAck;
    type Error = Rc<dyn Error>;

    async fn call(&self, msg: Control<E>, _: Ctx<'_, Self, ()>) -> Result<Self::Res, Self::Error> {
        log::trace!("Default control service is used: {msg:?}");
        Ok(msg.ack())
    }
}
