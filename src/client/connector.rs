use std::{cell::Cell, marker::PhantomData, ops};

use ntex_bytes::{PoolId, PoolRef};
use ntex_connect::{self as connect, Address, Connect, Connector as DefaultConnector};
use ntex_io::IoBoxed;
use ntex_service::{IntoService, Service};
use ntex_util::time::timeout_checked;

use crate::{client::ClientConnection, client::ClientError, config::Config};

/// Mqtt client connector
pub struct Connector<A, T> {
    connector: T,
    config: Config,
    pub(super) pool: Cell<PoolRef>,

    _t: PhantomData<A>,
}

impl<A> Connector<A, ()>
where
    A: Address,
{
    #[allow(clippy::new_ret_no_self)]
    /// Create new h2 connector
    pub fn new() -> Connector<A, DefaultConnector<A>> {
        Connector {
            connector: DefaultConnector::default(),
            config: Config::default(),
            pool: Cell::new(PoolId::P5.pool_ref()),
            _t: PhantomData,
        }
    }
}

impl<A, T> ops::Deref for Connector<A, T> {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl<A, T> ops::DerefMut for Connector<A, T> {
    fn deref_mut(&mut self) -> &mut Config {
        &mut self.config
    }
}

impl<A, T> Connector<A, T>
where
    A: Address,
{
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
            connector: connector.into_service(),
            config: self.config.clone(),
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
        let fut = async {
            Ok::<_, ClientError>(ClientConnection::new(
                self.connector.call(Connect::new(address)).await?,
                self.config.clone(),
            ))
        };

        match timeout_checked(self.config.handshake_timeout.get(), fut).await {
            Ok(res) => res.map_err(From::from),
            Err(_) => Err(ClientError::HandshakeTimeout),
        }
    }
}
