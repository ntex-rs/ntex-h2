use std::{cell::Cell, cell::RefCell, collections::VecDeque, fmt, rc::Rc, time::Duration};

use nanorand::{Rng, WyRand};
use ntex_bytes::{ByteString, PoolId, PoolRef};
use ntex_connect::{self as connect, Address, Connect, Connector as DefaultConnector};
use ntex_http::{uri::Scheme, HeaderMap, Method};
use ntex_io::IoBoxed;
use ntex_service::{IntoService, Pipeline, Service};
use ntex_util::time::{timeout_checked, Millis, Seconds};
use ntex_util::{channel::oneshot, future::BoxFuture};

use super::stream::{InflightStorage, RecvStream, SendStream};
use super::{client::Client, ClientError};

type Fut = BoxFuture<'static, Result<IoBoxed, connect::ConnectError>>;
type Connector = Box<dyn Fn() -> BoxFuture<'static, Result<IoBoxed, connect::ConnectError>>>;

#[derive(Clone)]
/// Manages http client network connectivity.
pub struct Pool {
    inner: Rc<Inner>,
    waiters: Rc<RefCell<VecDeque<oneshot::Sender<()>>>>,
}

/// Notify one active waiter
fn notify(waiters: &mut VecDeque<oneshot::Sender<()>>) {
    log::debug!("Notify waiter, total {:?}", waiters.len());
    while let Some(waiter) = waiters.pop_front() {
        if waiter.send(()).is_ok() {
            break;
        }
    }
}

impl Pool {
    #[inline]
    /// Configure and build connection pool
    pub fn build<A, U, T, F>(addr: U, connector: F) -> PoolBuilder
    where
        A: Address + Clone,
        F: IntoService<T, Connect<A>>,
        T: Service<Connect<A>, Error = connect::ConnectError> + 'static,
        IoBoxed: From<T::Response>,
        Connect<A>: From<U>,
    {
        PoolBuilder::new(addr, connector)
    }

    #[inline]
    /// Send request to the peer
    pub async fn send(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<(SendStream, RecvStream), ClientError> {
        loop {
            let (client, num) = {
                let mut connections = self.inner.connections.borrow_mut();

                // cleanup connections
                let mut idx = 0;
                while idx < connections.len() {
                    if connections[idx].is_closed() {
                        connections.remove(idx);
                    } else {
                        idx += 1;
                    }
                }

                // check existing connections
                let available = connections.iter().filter(|item| item.is_ready()).count();
                let client = if available > 0 {
                    let idx = WyRand::new().generate_range(0_usize..available);
                    connections
                        .iter()
                        .filter(|item| item.is_ready())
                        .nth(idx)
                        .cloned()
                } else {
                    None
                };

                (client, connections.len())
            };

            if let Some(client) = client {
                return client
                    .send(method, path, headers, eof)
                    .await
                    .map_err(From::from);
            }

            // can create new connection
            if !self.inner.connecting.get() && num < self.inner.maxconn {
                // create new connection
                self.inner.connecting.set(true);
                let (tx, rx) = oneshot::channel();
                let inner = self.inner.clone();
                let waiters = self.waiters.clone();
                ntex_rt::spawn(async move {
                    let res = match timeout_checked(inner.conn_timeout, (*inner.connector)()).await
                    {
                        Ok(Ok(io)) => {
                            // callbacks for end of stream
                            let waiters2 = waiters.clone();
                            let storage = InflightStorage::new(move |_| {
                                notify(&mut waiters2.borrow_mut());
                            });
                            // construct client
                            io.set_memory_pool(inner.pool);
                            let client = Client::with_params(
                                io,
                                inner.config.clone(),
                                inner.scheme.clone(),
                                inner.authority.clone(),
                                storage,
                            );
                            inner.connections.borrow_mut().push(client.clone());
                            Ok(client)
                        }
                        Ok(Err(err)) => Err(ClientError::from(err)),
                        Err(_) => Err(ClientError::HandshakeTimeout),
                    };
                    inner.connecting.set(false);
                    for waiter in waiters.borrow_mut().drain(..) {
                        let _ = waiter.send(());
                    }
                    let _ = tx.send(res);
                });
                return rx
                    .await??
                    .send(method, path, headers, eof)
                    .await
                    .map_err(From::from);
            } else {
                log::debug!(
                    "New connection is being established {:?} or number of existing cons {} greater than allowed {}",
                    self.inner.connecting.get(), num, self.inner.maxconn);

                // wait for available connection
                let (tx, rx) = oneshot::channel();
                self.waiters.borrow_mut().push_back(tx);
                let _ = rx.await;
            }
        }
    }

    #[inline]
    /// Check if client is allowed to send new request
    ///
    /// Readiness depends on number of opened streams and max concurrency setting
    pub fn is_ready(&self) -> bool {
        let connections = self.inner.connections.borrow();
        for client in &*connections {
            if client.is_ready() {
                return true;
            }
        }

        !self.inner.connecting.get() && connections.len() < self.inner.maxconn
    }

    #[inline]
    /// Check client readiness
    ///
    /// Client is ready when it is possible to start new stream
    pub async fn ready(&self) {
        loop {
            if !self.is_ready() {
                // add waiter
                let (tx, rx) = oneshot::channel();
                self.waiters.borrow_mut().push_back(tx);
                let _ = rx.await;
                'inner: while let Some(tx) = self.waiters.borrow_mut().pop_front() {
                    if tx.send(()).is_ok() {
                        break 'inner;
                    }
                }
            } else {
                break;
            }
        }
    }
}

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
    connector: Connector,
    pool: PoolRef,
    connecting: Cell<bool>,
    connections: RefCell<Vec<Client>>,
}

impl PoolBuilder {
    fn new<A, U, T, F>(addr: U, connector: F) -> Self
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
            let f: Fut = Box::pin(async move { svc.call(connect).await.map(IoBoxed::from) });
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
            connecting: Cell::new(false),
            connections: Default::default(),
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
            let f: Fut = Box::pin(async move { svc.call(connect).await.map(IoBoxed::from) });
            f
        });

        self.0.authority = authority;
        self.0.connector = connector;
        self
    }

    /// Finish configuration process and create connections pool.
    pub fn finish(self) -> Pool {
        Pool {
            inner: Rc::new(self.0),
            waiters: Default::default(),
        }
    }
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("scheme", &self.inner.scheme)
            .field("authority", &self.inner.authority)
            .field("conn_timeout", &self.inner.conn_timeout)
            .field("conn_lifetime", &self.inner.conn_lifetime)
            .field("disconnect_timeout", &self.inner.disconnect_timeout)
            .field("maxconn", &self.inner.maxconn)
            .field("limit", &self.inner.limit)
            .field("pool", &self.inner.pool)
            .field("config", &self.inner.config)
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
