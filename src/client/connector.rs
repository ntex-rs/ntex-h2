use std::marker::PhantomData;

use ntex_bytes::ByteString;
use ntex_error::Error;
use ntex_http::uri::Scheme;
use ntex_io::IoBoxed;
use ntex_net::connect::{Address, Connect, ConnectError, Connector as DefaultConnector};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Ctx, IntoServiceFactory, Service, ServiceFactory};
use ntex_util::{channel::pool, time::timeout_checked};

use crate::client::{SimpleClient, stream::InflightStorage};
use crate::{client::ClientError, config::ServiceConfig};

#[derive(Debug)]
/// Http2 client connector
pub struct Connector<A: Address, T> {
    svc: T,
    scheme: Scheme,
    pool: pool::Pool<()>,

    _t: PhantomData<A>,
}

impl<A, Sf> Connector<A, Sf>
where
    A: Address,
    Sf: ServiceFactory<Connect<A>, St = (), Error = Error<ConnectError>, InitCfg = SharedCfg>,
    IoBoxed: From<Sf::Res>,
{
    /// Create new http2 connector
    pub fn new<F>(svc: F) -> Connector<A, Sf>
    where
        F: IntoServiceFactory<Sf, Connect<A>>,
    {
        Connector {
            svc: svc.into_factory(),
            scheme: Scheme::HTTP,
            pool: pool::new(),
            _t: PhantomData,
        }
    }
}

impl<A> Default for Connector<A, DefaultConnector<A>>
where
    A: Address,
{
    /// Create new h2 connector
    fn default() -> Self {
        Self::new(DefaultConnector::default())
    }
}

impl<A, Sf> Connector<A, Sf>
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
    pub fn connector<U, F>(&self, svc: F) -> Connector<A, U>
    where
        F: IntoServiceFactory<U, Connect<A>>,
        U: ServiceFactory<Connect<A>, St = (), InitCfg = SharedCfg, Error = Error<ConnectError>>,
        IoBoxed: From<U::Res>,
    {
        Connector {
            svc: svc.into_factory(),
            scheme: self.scheme.clone(),
            pool: self.pool.clone(),
            _t: PhantomData,
        }
    }
}

impl<A, Sf> ServiceFactory<A> for Connector<A, Sf>
where
    A: Address,
    Sf: ServiceFactory<Connect<A>, St = (), Error = Error<ConnectError>, InitCfg = SharedCfg>,
    IoBoxed: From<Sf::Res>,
{
    type St = Sf::St;
    type Res = SimpleClient;
    type Error = Error<ClientError>;
    type InitCfg = SharedCfg;
    type InitError = Sf::InitError;
    type Service = ConnectorService<A, Sf::Service>;

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        let config = cfg.get();
        let svc = self.svc.create(cfg).await?;
        Ok(ConnectorService {
            svc,
            config,
            scheme: self.scheme.clone(),
            pool: self.pool.clone(),
            _t: PhantomData,
        })
    }
}

#[derive(Debug)]
pub struct ConnectorService<A, S> {
    svc: S,
    scheme: Scheme,
    config: Cfg<ServiceConfig>,
    pool: pool::Pool<()>,
    _t: PhantomData<A>,
}

impl<A, S,> Service for ConnectorService<A, S>
where
    A: Address,
    S: Service<Req = Connect<A>, Error = Error<ConnectError>>,
    IoBoxed: From<S::Res>,
{
    type St = S::St;
    type Req = A;
    type Res = SimpleClient;
    type Error = Error<ClientError>;

    /// Connect to http2 server
    async fn call(&self, req: A, ctx: Ctx<'_, Self>) -> Result<SimpleClient, Self::Error> {
        let authority = ByteString::from(req.host());

        let fut = async {
            let io = ctx
                .call(&self.svc, Connect::new(req))
                .await
                .map_err(|e| e.map(ClientError::from))?;

            Ok::<_, Error<ClientError>>(SimpleClient::with_params(
                io.into(),
                self.config.clone(),
                &self.scheme,
                authority,
                false,
                InflightStorage::default(),
                self.pool.clone(),
            ))
        };

        timeout_checked(self.config.handshake_timeout, fut)
            .await
            .map_err(|()| {
                Error::from(ClientError::HandshakeTimeout).set_service(self.config.service())
            })
            .and_then(|item| item)
    }

    ntex_service::forward_ready!(svc, |e| e.map(ClientError::from));
    ntex_service::forward_poll!(svc, |e| e.map(ClientError::from));
    ntex_service::forward_shutdown!(svc);
}
