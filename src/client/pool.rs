use std::{fmt, rc::Rc, time::Duration};

use ntex_bytes::{ByteString, PoolId, PoolRef};
use ntex_connect::{self as connect, Address, Connect, Connector as DefaultConnector};
use ntex_http::uri::Scheme;
use ntex_io::IoBoxed;
use ntex_service::{IntoService, Pipeline, Service};
use ntex_util::future::BoxFuture;
use ntex_util::time::{timeout_checked, Millis, Seconds};

type Fut = BoxFuture<'static, Result<IoBoxed, connect::ConnectError>>;
type CONNECTOR = Box<dyn Fn() -> BoxFuture<'static, Result<IoBoxed, connect::ConnectError>>>;

/// Manages http client network connectivity.
pub struct Pool(Rc<Inner>);

/// Manages http client network connectivity.
///
/// The `PoolBuilder` type uses a builder-like combinator pattern for service
/// construction that finishes by calling the `.finish()` method.
pub struct PoolBuilder(Inner);

struct Inner {
    maxconn: usize,
    conn_timeout: Millis,
    conn_lifetime: Duration,
    disconnect_timeout: Millis,
    limit: usize,
    scheme: Scheme,
    config: crate::Config,
    authority: ByteString,
    connector: CONNECTOR,
    pool: PoolRef,
}

impl PoolBuilder {
    pub fn new<A, U, T, F>(addr: U, connector: F) -> Self
    where
        A: Address + Clone,
        F: IntoService<T, Connect<A>>,
        T: Service<Connect<A>, Error = connect::ConnectError> + 'static,
        IoBoxed: From<T::Response>,
        Connect<A>: From<U>,
    {
        let connect = Connect::from(addr);
        let authority = ByteString::from(connect.host());
        let connector = Pipeline::new(connector.into_service());

        let connector = Box::new(move || {
            let connect = connect.clone();
            let svc = connector.clone();
            let f: Fut =
                Box::pin(async move { svc.call(connect).await.map(|io| IoBoxed::from(io)) });
            f
        });

        PoolBuilder(Inner {
            authority,
            connector,
            conn_timeout: Millis(1_000),
            conn_lifetime: Duration::from_secs(0),
            disconnect_timeout: Millis(3_000),
            limit: 100,
            maxconn: 16,
            scheme: Scheme::HTTP,
            config: crate::Config::client(),
            pool: PoolId::P5.pool_ref(),
        })
    }

    pub fn with_default<A, U>(addr: U) -> Self
    where
        A: Address + Clone,
        Connect<A>: From<U>,
    {
        Self::new(addr, DefaultConnector::default())
    }
}

impl PoolBuilder {
    #[inline]
    /// Set client's connection scheme
    pub fn scheme(mut self, scheme: Scheme) -> Self {
        self.0.scheme = scheme;
        self
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P5
    /// memory pool is used.
    pub fn memory_pool(mut self, id: PoolId) -> Self {
        self.0.pool = id.pool_ref();
        self
    }

    /// Connection timeout.
    ///
    /// i.e. max time to connect to remote host including dns name resolution.
    /// Set to 1 second by default.
    pub fn timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.0.conn_timeout = timeout.into();
        self
    }

    /// Set total number of simultaneous streams per connection.
    ///
    /// If limit is 0, the connector uses "MAX_CONCURRENT_STREAMS" config from connection
    /// settings.
    /// The default limit size is 100.
    pub fn limit(mut self, limit: usize) -> Self {
        self.0.limit = limit;
        self
    }

    /// Set max lifetime period for connection.
    ///
    /// Connection lifetime is max lifetime of any opened connection
    /// until it is closed regardless of keep-alive period.
    ///
    /// Default lifetime period is not set.
    pub fn lifetime(mut self, dur: Seconds) -> Self {
        self.0.conn_lifetime = dur.into();
        self
    }

    /// Sets the maximum concurrent connections.
    ///
    /// By default max connections is set to a 16.
    pub fn maxconn(mut self, num: usize) -> Self {
        self.0.maxconn = num;
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
        self.0.disconnect_timeout = timeout.into();
        self
    }

    /// Configure http2 connection settings
    pub fn configure<O, R>(self, f: O) -> Self
    where
        O: FnOnce(&crate::Config) -> R,
    {
        let _ = f(&self.0.config);
        self
    }

    /// Http/2 connection settings
    pub fn config(&self) -> &crate::Config {
        &self.0.config
    }

    /// Use custom connector
    pub fn connector<A, U, T, F>(mut self, addr: U, connector: F) -> Self
    where
        A: Address + Clone,
        F: IntoService<T, Connect<A>>,
        T: Service<Connect<A>, Error = connect::ConnectError> + 'static,
        IoBoxed: From<T::Response>,
        Connect<A>: From<U>,
    {
        let connect = Connect::from(addr);
        let authority = ByteString::from(connect.host());
        let connector = Pipeline::new(connector.into_service());

        let connector = Box::new(move || {
            let connect = connect.clone();
            let svc = connector.clone();
            let f: Fut =
                Box::pin(async move { svc.call(connect).await.map(|io| IoBoxed::from(io)) });
            f
        });

        self.0.authority = authority;
        self.0.connector = connector;
        self
    }

    /// Finish configuration process and create connections pool.
    pub fn finish(self) -> Pool {
        Pool(Rc::new(self.0))
    }
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("scheme", &self.0.scheme)
            .field("authority", &self.0.authority)
            .field("conn_timeout", &self.0.conn_timeout)
            .field("conn_lifetime", &self.0.conn_lifetime)
            .field("disconnect_timeout", &self.0.disconnect_timeout)
            .field("maxconn", &self.0.maxconn)
            .field("limit", &self.0.limit)
            .field("pool", &self.0.pool)
            .field("config", &self.0.config)
            .finish()
    }
}

impl fmt::Debug for PoolBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolBuilder")
            .field("scheme", &self.0.scheme)
            .field("authority", &self.0.authority)
            .field("conn_timeout", &self.0.conn_timeout)
            .field("conn_lifetime", &self.0.conn_lifetime)
            .field("disconnect_timeout", &self.0.disconnect_timeout)
            .field("maxconn", &self.0.maxconn)
            .field("limit", &self.0.limit)
            .field("pool", &self.0.pool)
            .field("config", &self.0.config)
            .finish()
    }
}
