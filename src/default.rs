use std::fmt;

use ntex_service::{Service, ServiceCtx, ServiceFactory};

use super::control::{Control, ControlAck};

#[derive(Copy, Clone, Debug)]
/// Default control service
pub struct DefaultControlService;

impl<E: fmt::Debug + 'static> ServiceFactory<Control<E>> for DefaultControlService {
    type Response = ControlAck;
    type Error = E;
    type InitError = E;
    type Service = DefaultControlService;

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}

impl<E: fmt::Debug + 'static> Service<Control<E>> for DefaultControlService {
    type Response = ControlAck;
    type Error = E;

    async fn call(
        &self,
        msg: Control<E>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        log::trace!("Default control service is used: {msg:?}");
        Ok(msg.ack())
    }
}
