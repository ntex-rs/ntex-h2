use std::{marker::PhantomData, ops};

use ntex_bytes::ByteString;
use ntex_http::uri::Scheme;
use ntex_io::IoBoxed;
use ntex_net::connect::{self as connect, Address, Connect, Connector as DefaultConnector};
use ntex_service::{IntoService, Pipeline, Service};
use ntex_util::time::timeout_checked;

use crate::{client::ClientError, client::SimpleClient, config::Config};

#[derive(Debug)]
/// Http2 client connector
pub struct Connector<A: Address, T> {
    connector: Pipeline<T>,
    config: Config,
    scheme: Scheme,

    _t: PhantomData<A>,
}

impl<A, T> Connector<A, T>
where
    A: Address,
    T: Service<Connect<A>, Error = connect::ConnectError>,
    IoBoxed: From<T::Response>,
{
    /// Create new http2 connector
    pub fn new<F>(connector: F) -> Connector<A, T>
    where
        F: IntoService<T, Connect<A>>,
    {
        Connector {
            connector: Pipeline::new(connector.into_service()),
            config: Config::client(),
            scheme: Scheme::HTTP,
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
        Connector {
            connector: DefaultConnector::default().into(),
            config: Config::client(),
            scheme: Scheme::HTTP,
            _t: PhantomData,
        }
    }
}

impl<A: Address, T> ops::Deref for Connector<A, T> {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl<A: Address, T> ops::DerefMut for Connector<A, T> {
    fn deref_mut(&mut self) -> &mut Config {
        &mut self.config
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
        F: IntoService<U, Connect<A>>,
        U: Service<Connect<A>, Error = connect::ConnectError>,
        IoBoxed: From<U::Response>,
    {
        Connector {
            connector: connector.into_service().into(),
            config: self.config.clone(),
            scheme: self.scheme.clone(),
            _t: PhantomData,
        }
    }
}

impl<A, T> Connector<A, T>
where
    A: Address,
    T: Service<Connect<A>, Error = connect::ConnectError>,
    IoBoxed: From<T::Response>,
{
    /// Connect to http2 server
    pub async fn connect(&self, address: A) -> Result<SimpleClient, ClientError> {
        let scheme = self.scheme.clone();
        let authority = ByteString::from(address.host());

        let fut = async {
            Ok::<_, ClientError>(SimpleClient::new(
                self.connector.call(Connect::new(address)).await?,
                self.config.clone(),
                scheme,
                authority,
            ))
        };

        timeout_checked(self.config.0.handshake_timeout.get(), fut)
            .await
            .map_err(|_| ClientError::HandshakeTimeout)
            .and_then(|item| item)
    }
}
