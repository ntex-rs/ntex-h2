use std::fmt;

use ntex_service::{Service, ServiceCtx, ServiceFactory, cfg::SharedCfg};

use super::control::{Control, ControlAck};

#[derive(Copy, Clone, Debug)]
/// Default control service
pub struct DefaultControlService;

impl<E: fmt::Debug + 'static> ServiceFactory<Control<E>, SharedCfg> for DefaultControlService {
    type Response = ControlAck;
    type Error = E;
    type Service = DefaultControlService;
    type InitError = E;
    type Data = ();

    async fn create(&self, _: SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }

    async fn map_data(&self, _: &SharedCfg, _: &Self::Data) -> Result<(), Self::InitError> {
        Ok(())
    }
}

impl<E: fmt::Debug + 'static> Service<Control<E>> for DefaultControlService {
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
