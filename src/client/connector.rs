use std::{cell::Cell, marker::PhantomData, ops};

use ntex_bytes::{ByteString, PoolId, PoolRef};
use ntex_connect::{self as connect, Address, Connect, Connector as DefaultConnector};
use ntex_io::IoBoxed;
use ntex_service::{Container, IntoService, Service};
use ntex_util::time::timeout_checked;

use crate::{client::ClientConnection, client::ClientError, config::Config};

/// Mqtt client connector
pub struct Connector<A: Address, T> {
    connector: Container<T>,
    config: Config,
    secure: bool,
    pub(super) pool: Cell<PoolRef>,

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
            connector: Container::new(connector.into_service()),
            config: Config::client(),
            secure: false,
            pool: Cell::new(PoolId::P5.pool_ref()),
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
            secure: false,
            pool: Cell::new(PoolId::P5.pool_ref()),
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
    pub fn secure(&mut self) -> &mut Self {
        self.secure = true;
        self
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P5
    /// memory pool is used.
    pub fn memory_pool(&mut self, id: PoolId) -> &mut Self {
        self.pool.set(id.pool_ref());
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
            secure: self.secure,
            pool: self.pool.clone(),
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
    pub async fn connect(&self, address: A) -> Result<ClientConnection, ClientError> {
        let secure = self.secure;
        let authority = ByteString::from(address.host());

        let fut = async {
            Ok::<_, ClientError>(ClientConnection::with_params(
                self.connector.call(Connect::new(address)).await?,
                self.config.clone(),
                secure,
                authority,
            ))
        };

        match timeout_checked(self.config.0.handshake_timeout.get(), fut).await {
            Ok(res) => res.map_err(From::from),
            Err(_) => Err(ClientError::HandshakeTimeout),
        }
    }
}
