use std::marker::PhantomData;

use ntex_bytes::ByteString;
use ntex_error::Error;
use ntex_io::IoBoxed;
use ntex_net::connect::{Address, Connect, ConnectError, Connector as DefaultConnector};
use ntex_service::{Ctx, IntoService, Service, cfg::SharedCfg};
use ntex_util::{channel::pool, time::timeout_checked};

use crate::client::{ClientError, SimpleClient, stream::InflightStorage};
use crate::config::ServiceConfig;

#[derive(Debug)]
/// Service that establishes an HTTP/2 client connection.
pub struct Connector<A: Address, S> {
    svc: S,
    secure: bool,
    pool: pool::Pool<()>,
    _t: PhantomData<A>,
}

impl<A> Default for Connector<A, DefaultConnector<A>>
where
    A: Address,
{
    /// Creates a connector with default settings.
    fn default() -> Self {
        Self::new()
    }
}

impl<A> Connector<A, DefaultConnector<A>>
where
    A: Address,
{
    /// Creates a connector using the default TCP connector.
    pub fn new() -> Self {
        Connector {
            svc: DefaultConnector::new(),
            secure: false,
            pool: pool::new(),
            _t: PhantomData,
        }
    }
}

impl<A, S> Connector<A, S>
where
    A: Address,
{
    #[must_use]
    /// Sets whether requests of the resulting client use the `https` `:scheme`.
    ///
    /// Default is `false`, requests use the `http` `:scheme`.
    pub fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    #[must_use]
    /// Replaces the underlying transport connector.
    pub fn connector<U>(self, svc: impl IntoService<U, SharedCfg, Connect<A>>) -> Connector<A, U>
    where
        U: Service<SharedCfg, Connect<A>, Error = Error<ConnectError>>,
        IoBoxed: From<U::Res>,
    {
        Connector {
            svc: svc.into_service(),
            secure: self.secure,
            pool: self.pool,
            _t: PhantomData,
        }
    }
}

impl<A, S> Service<SharedCfg, A> for Connector<A, S>
where
    A: Address,
    S: Service<SharedCfg, Connect<A>, Error = Error<ConnectError>>,
    IoBoxed: From<S::Res>,
{
    type Res = SimpleClient;
    type Error = Error<ClientError>;

    /// Connects to an HTTP/2 server.
    async fn call(&self, req: A, ctx: Ctx<'_, Self, SharedCfg>) -> Result<Self::Res, Self::Error> {
        let authority = ByteString::from(req.host());

        let cfg = ctx.st().get::<ServiceConfig>();
        let timeout = cfg.handshake_timeout;

        let fut = async {
            let io = ctx
                .call(&self.svc, Connect::new(req))
                .await
                .map_err(|e| e.map(ClientError::from))?;

            Ok::<_, Error<ClientError>>(SimpleClient::with_params(
                io.into(),
                cfg.clone(),
                self.secure,
                authority,
                false,
                InflightStorage::default(),
                self.pool.clone(),
            ))
        };

        timeout_checked(timeout, fut)
            .await
            .map_err(|()| Error::from(ClientError::HandshakeTimeout).with_service(cfg.service()))
            .and_then(|item| item)
    }

    ntex_service::forward_ready!(SharedCfg, svc, |e| e.map(ClientError::from));
    ntex_service::forward_shutdown!(SharedCfg, svc);
}
