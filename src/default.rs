use std::{fmt, marker::PhantomData};

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

impl<E: fmt::Debug> ServiceFactory<Control<E>> for DefaultControlService<E> {
    type St = ();
    type Res = ControlAck;
    type Error = E;
    type InitCfg = SharedCfg;
    type InitError = E;
    type Service = DefaultControlService<E>;

    async fn create(&self, _: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService::new())
    }
}

impl<E: fmt::Debug> Service for DefaultControlService<E> {
    type St = ();
    type Req = Control<E>;
    type Res = ControlAck;
    type Error = E;

    async fn call(&self, msg: Control<E>, _: Ctx<'_, Self>) -> Result<ControlAck, E> {
        log::trace!("Default control service is used: {msg:?}");
        Ok(msg.ack())
    }
}
