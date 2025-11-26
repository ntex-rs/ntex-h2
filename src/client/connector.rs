use std::marker::PhantomData;

use ntex_bytes::ByteString;
use ntex_http::uri::Scheme;
use ntex_io::IoBoxed;
use ntex_net::connect::{Address, Connect, ConnectError, Connector as DefaultConnector};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};
use ntex_util::{channel::pool, time::timeout_checked};

use crate::client::{stream::InflightStorage, SimpleClient};
use crate::{client::ClientError, config::ServiceConfig};

#[derive(Debug)]
/// Http2 client connector
pub struct Connector<A: Address, T> {
    connector: T,
    scheme: Scheme,
    pool: pool::Pool<()>,

    _t: PhantomData<A>,
}

impl<A, T> Connector<A, T>
where
    A: Address,
    T: ServiceFactory<Connect<A>, SharedCfg, Error = ConnectError>,
    IoBoxed: From<T::Response>,
{
    /// Create new http2 connector
    pub fn new<F>(connector: F) -> Connector<A, T>
    where
        F: IntoServiceFactory<T, Connect<A>, SharedCfg>,
    {
        Connector {
            connector: connector.into_factory(),
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

impl<A, T> Connector<A, T>
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
    pub fn connector<U, F>(&self, connector: F) -> Connector<A, U>
    where
        F: IntoServiceFactory<U, Connect<A>, SharedCfg>,
        U: ServiceFactory<Connect<A>, SharedCfg, Error = ConnectError>,
        IoBoxed: From<U::Response>,
    {
        Connector {
            connector: connector.into_factory(),
            scheme: self.scheme.clone(),
            pool: self.pool.clone(),
            _t: PhantomData,
        }
    }
}

impl<A, T> ServiceFactory<A, SharedCfg> for Connector<A, T>
where
    A: Address,
    T: ServiceFactory<Connect<A>, SharedCfg, Error = ConnectError>,
    IoBoxed: From<T::Response>,
{
    type Response = SimpleClient;
    type Error = ClientError;
    type InitError = T::InitError;
    type Service = ConnectorService<A, T::Service>;

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let svc = self.connector.create(cfg).await?;
        Ok(ConnectorService {
            svc,
            scheme: self.scheme.clone(),
            config: cfg.get(),
            pool: self.pool.clone(),
            _t: PhantomData,
        })
    }
}

#[derive(Debug)]
pub struct ConnectorService<A, T> {
    svc: T,
    scheme: Scheme,
    config: Cfg<ServiceConfig>,
    pool: pool::Pool<()>,
    _t: PhantomData<A>,
}

impl<A, T> Service<A> for ConnectorService<A, T>
where
    A: Address,
    T: Service<Connect<A>, Error = ConnectError>,
    IoBoxed: From<T::Response>,
{
    type Response = SimpleClient;
    type Error = ClientError;

    /// Connect to http2 server
    async fn call(&self, req: A, ctx: ServiceCtx<'_, Self>) -> Result<SimpleClient, ClientError> {
        let authority = ByteString::from(req.host());

        let fut = async {
            Ok::<_, ClientError>(SimpleClient::with_params(
                ctx.call(&self.svc, Connect::new(req)).await?.into(),
                self.config.clone(),
                self.scheme.clone(),
                authority,
                false,
                InflightStorage::default(),
                self.pool.clone(),
            ))
        };

        timeout_checked(self.config.handshake_timeout, fut)
            .await
            .map_err(|_| ClientError::HandshakeTimeout)
            .and_then(|item| item)
    }

    ntex_service::forward_ready!(svc);
    ntex_service::forward_poll!(svc);
    ntex_service::forward_shutdown!(svc);
}
