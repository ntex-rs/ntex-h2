use std::marker::PhantomData;

use ntex_bytes::ByteString;
use ntex_error::Error;
use ntex_http::uri::Scheme;
use ntex_io::IoBoxed;
use ntex_net::connect::{Address, Connect, ConnectError, Connector as DefaultConnector};
use ntex_service::{Ctx, IntoService, Service, cfg::SharedCfg};
use ntex_util::{channel::pool, time::timeout_checked};

use crate::client::{ClientError, SimpleClient, stream::InflightStorage};
use crate::config::ServiceConfig;

#[derive(Debug)]
/// Http2 client connector
pub struct Connector<A: Address, S, St> {
    svc: S,
    scheme: Scheme,
    pool: pool::Pool<()>,
    cfg: SharedCfg,
    _t: PhantomData<(A, St)>,
}

impl<A> Default for Connector<A, DefaultConnector<A, ()>, ()>
where
    A: Address,
{
    /// Create new h2 connector
    fn default() -> Self {
        Self::new(SharedCfg::default())
    }
}

impl<A, St> Connector<A, DefaultConnector<A, St>, St>
where
    A: Address,
{
    /// Create new http2 connector
    pub fn new(cfg: impl Into<SharedCfg>) -> Self {
        let cfg = cfg.into();
        let svc = DefaultConnector::with(cfg.clone());

        Connector {
            cfg,
            svc,
            scheme: Scheme::HTTP,
            pool: pool::new(),
            _t: PhantomData,
        }
    }
}

impl<A, S, St> Connector<A, S, St>
where
    A: Address,
{
    #[inline]
    /// Set scheme
    pub fn scheme(&mut self, scheme: Scheme) -> &mut Self {
        self.scheme = scheme;
        self
    }

    /// Use custom connector
    pub fn connector<U>(self, svc: impl IntoService<U, St, Connect<A>>) -> Connector<A, U, St>
    where
        U: Service<St, Connect<A>, Error = Error<ConnectError>>,
        IoBoxed: From<U::Res>,
    {
        Connector {
            cfg: self.cfg,
            svc: svc.into_service(),
            scheme: self.scheme,
            pool: self.pool,
            _t: PhantomData,
        }
    }
}

impl<A, S, St> Service<St, A> for Connector<A, S, St>
where
    A: Address,
    S: Service<St, Connect<A>, Error = Error<ConnectError>>,
    IoBoxed: From<S::Res>,
{
    type Res = SimpleClient;
    type Error = Error<ClientError>;

    /// Connect to http2 server
    async fn call(&self, req: A, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let authority = ByteString::from(req.host());

        let cfg = self.cfg.get::<ServiceConfig>();
        let timeout = cfg.handshake_timeout;

        let fut = async {
            let io = ctx
                .call(&self.svc, Connect::new(req))
                .await
                .map_err(|e| e.map(ClientError::from))?;

            Ok::<_, Error<ClientError>>(SimpleClient::with_params(
                io.into(),
                cfg,
                &self.scheme,
                authority,
                false,
                InflightStorage::default(),
                self.pool.clone(),
            ))
        };

        timeout_checked(timeout, fut)
            .await
            .map_err(|()| {
                Error::from(ClientError::HandshakeTimeout).set_service(self.cfg.service())
            })
            .and_then(|item| item)
    }

    ntex_service::forward_ready!(St, svc, |e| e.map(ClientError::from));
    ntex_service::forward_shutdown!(St, svc);
}
