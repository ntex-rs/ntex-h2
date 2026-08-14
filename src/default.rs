use std::{fmt, marker::PhantomData};

use ntex_service::{Service, ServiceCtx, cfg::SharedCfg};

use super::control::{Control, ControlAck};

#[derive(Copy, Clone, Debug)]
/// Default control service
pub struct DefaultControlService<E>(PhantomData<fn() -> E>);

impl<E> DefaultControlService<E> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<E: fmt::Debug + 'static> Service<SharedCfg> for DefaultControlService<E> {
    type Response = Self;
    type Error = E;
    type Data = ();

    async fn call(
        &self,
        _: SharedCfg,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(Self::new())
    }
}

impl<E: fmt::Debug + 'static> Service<Control<E>> for DefaultControlService<E> {
    type Response = ControlAck;
    type Error = E;
    type Data = ();

    async fn call(
        &self,
        msg: Control<E>,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        log::trace!("Default control service is used: {msg:?}");
        Ok(msg.ack())
    }
}
