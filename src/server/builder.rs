use std::{fmt, marker};

use ntex_service::{IntoServiceFactory, ServiceFactory};

use crate::control::{Control, ControlAck};
use crate::{config::Config, default::DefaultControlService, message::Message, server::Server};

/// Builds server with custom configuration values.
///
/// Methods can be chained in order to set the configuration values.
///
/// New instances of `Builder` are obtained via [`Builder::new`].
///
/// See function level documentation for details on the various server
/// configuration settings.
///
/// [`Builder::new`]: struct.ServerBuilder.html#method.new
#[derive(Clone, Debug)]
pub struct ServerBuilder<E, Ctl = DefaultControlService> {
    control: Ctl,
    config: Config,
    _t: marker::PhantomData<E>,
}

// ===== impl Builder =====

impl<E> ServerBuilder<E> {
    /// Returns a new server builder instance initialized with default
    /// configuration values.
    ///
    /// Configuration methods can be chained on the return value.
    pub fn new() -> ServerBuilder<E> {
        ServerBuilder {
            config: Config::server(),
            control: DefaultControlService,
            _t: marker::PhantomData,
        }
    }

    /// Configure connection settings
    pub fn configure<'a, F, R>(&'a self, f: F) -> &'a Self
    where
        F: FnOnce(&'a Config) -> R + 'a,
    {
        let _ = f(&self.config);
        self
    }
}

impl<E: fmt::Debug, Ctl> ServerBuilder<E, Ctl> {
    /// Service to call with control frames
    pub fn control<S, F>(&self, service: F) -> ServerBuilder<E, S>
    where
        F: IntoServiceFactory<S, Control<E>>,
        S: ServiceFactory<Control<E>, Response = ControlAck> + 'static,
        S::Error: fmt::Debug,
        S::InitError: fmt::Debug,
    {
        ServerBuilder {
            control: service.into_factory(),
            config: self.config.clone(),
            _t: marker::PhantomData,
        }
    }
}

impl<E, Ctl> ServerBuilder<E, Ctl>
where
    E: fmt::Debug,
    Ctl: ServiceFactory<Control<E>, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
{
    /// Creates a new configured HTTP/2 server.
    pub fn finish<F, Pub>(self, service: F) -> Server<Ctl, Pub>
    where
        F: IntoServiceFactory<Pub, Message>,
        Pub: ServiceFactory<Message, Response = (), Error = E> + 'static,
        Pub::InitError: fmt::Debug,
    {
        Server::new(self.config, self.control, service.into_factory())
    }
}

impl<E> Default for ServerBuilder<E> {
    fn default() -> ServerBuilder<E> {
        ServerBuilder::new()
    }
}
