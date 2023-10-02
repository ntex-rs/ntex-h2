use std::{fmt, task::Context, task::Poll, time::Duration};

use ntex_bytes::{ByteString, PoolId, PoolRef};
use ntex_connect::{self as connect, Address, Connect, Connector as DefaultConnector};
use ntex_io::IoBoxed;
use ntex_service::{IntoService, Pipeline, Service};
use ntex_util::time::{timeout_checked, Millis, Seconds};

use super::{connection::Connection, error::ConnectError, pool::ConnectionPool, Connect};

#[derive(Debug)]
/// Manages http client network connectivity.
///
/// The `Connector` type uses a builder-like combinator pattern for service
/// construction that finishes by calling the `.finish()` method.
///
/// ```rust,no_run
/// use ntex::http::client::Connector;
/// use ntex::time::Millis;
///
/// let connector = Connector::default()
///      .timeout(Millis(5_000))
///      .finish();
/// ```
pub struct Pool {
    timeout: Millis,
    maxconn: usize,
    conn_lifetime: Duration,
    disconnect_timeout: Millis,
    limit: usize,
    secure: bool,
    pool: PoolRef,
    config: crate::Config,
    authority: ByteString,
    connector: BoxedConnector,
}

impl Pool {
    pub fn new<A, T, F>(addr: A, connector: F) -> Pool
    where
        A: Address + Clone,
        F: IntoService<T, Connect<A>>,
        T: Service<Connect<A>, Error = connect::ConnectError>,
        IoBoxed: From<T::Response>,
    {
        let authority = ByteString::from(addr.host());

        Pool {
            authority,
            maxconn: 16,
            timeout: Millis(1_000),
            conn_lifetime: Duration::from_secs(0),
            disconnect_timeout: Millis(3_000),
            limit: 100,
            secure: false,
            config: crate::Config::client(),
            pool: PoolId::P5.pool_ref(),
        }
    }
}

impl Pool {
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
        self.pool = id.pool_ref();
        self
    }

    /// Connection timeout.
    ///
    /// i.e. max time to connect to remote host including dns name resolution.
    /// Set to 1 second by default.
    pub fn timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.timeout = timeout.into();
        self
    }

    /// Set total number of simultaneous streams per connection.
    ///
    /// If limit is 0, the connector uses "MAX_CONCURRENT_STREAMS" config from connection
    /// settings.
    /// The default limit size is 100.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set max lifetime period for connection.
    ///
    /// Connection lifetime is max lifetime of any opened connection
    /// until it is closed regardless of keep-alive period.
    ///
    /// Default lifetime period is not set.
    pub fn lifetime(mut self, dur: Seconds) -> Self {
        self.conn_lifetime = dur.into();
        self
    }

    /// Sets the maximum concurrent connections.
    ///
    /// By default max connections is set to a 16.
    pub fn maxconn(mut self, num: usize) -> Self {
        self.maxconn = num;
        self
    }

    /// Set server connection disconnect timeout.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the socket get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.disconnect_timeout = timeout.into();
        self
    }

    /// Configure http2 connection settings
    pub fn configure<O, R>(mut self, f: O) -> Self
    where
        O: FnOnce(&crate::Config) -> R,
    {
        let _ = f(&self.config);
        self
    }
}
