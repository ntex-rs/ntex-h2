use std::collections::VecDeque;
use std::{cell::Cell, cell::RefCell, fmt, marker::PhantomData, rc::Rc, time::Duration};

use nanorand::{Rng, WyRand};
use ntex_bytes::ByteString;
use ntex_http::{HeaderMap, Method, uri::Scheme};
use ntex_io::IoBoxed;
use ntex_net::connect::{Address, Connect, ConnectError, Connector as DefaultConnector};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{IntoServiceFactory, Pipeline, ServiceFactory};
use ntex_util::time::{Millis, Seconds, timeout_checked};
use ntex_util::{channel::oneshot, channel::pool, future::BoxFuture};

use super::stream::{InflightStorage, RecvStream, SendStream};
use super::{ClientError, simple::SimpleClient};
use crate::ServiceConfig;

type Fut = BoxFuture<'static, Result<IoBoxed, ConnectError>>;
type Connector = Box<dyn Fn() -> BoxFuture<'static, Result<IoBoxed, ConnectError>>>;

#[derive(Clone)]
/// Manages http client network connectivity.
pub struct Client {
    inner: Rc<Inner>,
    waiters: Rc<RefCell<VecDeque<pool::Sender<()>>>>,
}

struct Inner {
    cfg: Cfg<ServiceConfig>,
    config: InnerConfig,
    connector: Connector,
}

/// Notify one active waiter
fn notify(waiters: &mut VecDeque<pool::Sender<()>>) {
    log::debug!("Notify waiter, total {:?}", waiters.len());
    while let Some(waiter) = waiters.pop_front() {
        if waiter.send(()).is_ok() {
            break;
        }
    }
}

impl Client {
    #[inline]
    /// Configure and build client
    pub fn builder<A, U, T, F>(addr: U, connector: F) -> ClientBuilder<A, T>
    where
        A: Address + Clone,
        F: IntoServiceFactory<T, Connect<A>, SharedCfg>,
        T: ServiceFactory<Connect<A>, SharedCfg, Error = ConnectError> + 'static,
        IoBoxed: From<T::Response>,
        Connect<A>: From<U>,
    {
        ClientBuilder::new(addr, connector)
    }

    /// Send request to the peer
    pub async fn send(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<(SendStream, RecvStream), ClientError> {
        self.client()
            .await?
            .send(method, path, headers, eof)
            .await
            .map_err(From::from)
    }

    /// Get client from the pool
    pub async fn client(&self) -> Result<SimpleClient, ClientError> {
        loop {
            let (client, num) = self.get_client();

            if let Some(client) = client {
                return Ok(client);
            } else {
                self.connect(num).await?;
            }
        }
    }

    async fn connect(&self, num: usize) -> Result<(), ClientError> {
        let cfg = &self.inner.config;

        // can create new connection
        if !cfg.connecting.get() && (num < cfg.maxconn || (cfg.minconn > 0 && num < cfg.minconn)) {
            // create new connection
            cfg.connecting.set(true);

            self.create_connection().await?;
        } else {
            log::debug!(
                "New connection is being established {:?} or number of existing cons {} greater than allowed {}",
                cfg.connecting.get(),
                num,
                cfg.maxconn
            );

            // wait for available connection
            let (tx, rx) = cfg.pool.channel();
            self.waiters.borrow_mut().push_back(tx);
            rx.await?;
        }
        Ok(())
    }

    fn get_client(&self) -> (Option<SimpleClient>, usize) {
        let cfg = &self.inner.config;
        let mut connections = cfg.connections.borrow_mut();

        // cleanup connections
        let mut idx = 0;
        while idx < connections.len() {
            if connections[idx].is_closed() {
                connections.remove(idx);
            } else if connections[idx].is_disconnecting() {
                let con = connections.remove(idx);
                let timeout = cfg.disconnect_timeout;
                let _ = ntex_util::spawn(async move {
                    let _ = con.disconnect().disconnect_timeout(timeout).await;
                });
            } else {
                idx += 1;
            }
        }
        let num = connections.len();
        if cfg.minconn > 0 && num < cfg.minconn {
            // create new connection
            (None, num)
        } else {
            // first search for connections with less than 50% capacity usage
            let client = connections.iter().find(|item| {
                let cap = item.max_streams().unwrap_or(cfg.max_streams) >> 1;
                item.active_streams() <= cap
            });
            if let Some(client) = client {
                (Some(client.clone()), num)
            } else {
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

                (client, num)
            }
        }
    }

    async fn create_connection(&self) -> Result<(), ClientError> {
        let (tx, rx) = oneshot::channel();

        let inner = self.inner.clone();
        let waiters = self.waiters.clone();

        let _ = ntex_util::spawn(async move {
            let res = match timeout_checked(inner.config.conn_timeout, (*inner.connector)()).await {
                Ok(Ok(io)) => {
                    // callbacks for end of stream
                    let waiters2 = waiters.clone();
                    let storage = InflightStorage::new(move |_| {
                        notify(&mut waiters2.borrow_mut());
                    });
                    // construct client
                    let client = SimpleClient::with_params(
                        io,
                        inner.cfg,
                        inner.config.scheme.clone(),
                        inner.config.authority.clone(),
                        inner.config.skip_unknown_streams,
                        storage,
                        inner.config.pool.clone(),
                    );
                    inner.config.connections.borrow_mut().push(client);
                    inner
                        .config
                        .total_connections
                        .set(inner.config.total_connections.get() + 1);
                    Ok(())
                }
                Ok(Err(err)) => Err(ClientError::from(err)),
                Err(_) => Err(ClientError::HandshakeTimeout),
            };
            inner.config.connecting.set(false);
            for waiter in waiters.borrow_mut().drain(..) {
                let _ = waiter.send(());
            }

            if res.is_err() {
                inner
                    .config
                    .connect_errors
                    .set(inner.config.connect_errors.get() + 1);
            }
            let _ = tx.send(res);
        });

        rx.await?
    }

    #[inline]
    /// Check if client is allowed to send new request
    ///
    /// Readiness depends on number of opened streams and max concurrency setting
    pub fn is_ready(&self) -> bool {
        let connections = self.inner.config.connections.borrow();
        for client in &*connections {
            if client.is_ready() {
                return true;
            }
        }

        !self.inner.config.connecting.get() && connections.len() < self.inner.config.maxconn
    }

    #[inline]
    /// Check client readiness
    ///
    /// Client is ready when it is possible to start new stream
    pub async fn ready(&self) {
        loop {
            if !self.is_ready() {
                // add waiter
                let (tx, rx) = self.inner.config.pool.channel();
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

#[doc(hidden)]
impl Client {
    pub fn stat_active_connections(&self) -> usize {
        self.inner.config.connections.borrow().len()
    }

    pub fn stat_total_connections(&self) -> usize {
        self.inner.config.total_connections.get()
    }

    pub fn stat_connect_errors(&self) -> usize {
        self.inner.config.connect_errors.get()
    }

    pub fn stat_connections<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&[SimpleClient]) -> R,
    {
        f(&self.inner.config.connections.borrow())
    }
}

/// Manages http client network connectivity.
///
/// The `ClientBuilder` type uses a builder-like combinator pattern for service
/// construction that finishes by calling the `.finish()` method.
pub struct ClientBuilder<A, T> {
    connect: Connect<A>,
    inner: InnerConfig,
    connector: T,
    _t: PhantomData<A>,
}

struct InnerConfig {
    minconn: usize,
    maxconn: usize,
    conn_timeout: Millis,
    conn_lifetime: Duration,
    disconnect_timeout: Millis,
    max_streams: u32,
    skip_unknown_streams: bool,
    scheme: Scheme,
    authority: ByteString,
    connecting: Cell<bool>,
    connections: RefCell<Vec<SimpleClient>>,
    total_connections: Cell<usize>,
    connect_errors: Cell<usize>,
    pool: pool::Pool<()>,
}

impl<A, T> ClientBuilder<A, T>
where
    A: Address + Clone,
    T: ServiceFactory<Connect<A>, SharedCfg, Error = ConnectError>,
    IoBoxed: From<T::Response>,
{
    fn new<U, F>(addr: U, connector: F) -> Self
    where
        Connect<A>: From<U>,
        F: IntoServiceFactory<T, Connect<A>, SharedCfg>,
    {
        let connect = Connect::from(addr);
        let authority = ByteString::from(connect.host());
        let connector = connector.into_factory();

        ClientBuilder {
            connect,
            connector,
            inner: InnerConfig {
                authority,
                conn_timeout: Millis(1_000),
                conn_lifetime: Duration::from_secs(0),
                disconnect_timeout: Millis(15_000),
                max_streams: 100,
                skip_unknown_streams: false,
                minconn: 1,
                maxconn: 16,
                scheme: Scheme::HTTP,
                connecting: Cell::new(false),
                connections: Default::default(),
                total_connections: Cell::new(0),
                connect_errors: Cell::new(0),
                pool: pool::new(),
            },
            _t: PhantomData,
        }
    }
}

impl<A> ClientBuilder<A, DefaultConnector<A>>
where
    A: Address + Clone,
{
    pub fn with_default<U>(addr: U) -> Self
    where
        Connect<A>: From<U>,
    {
        Self::new(addr, DefaultConnector::default())
    }
}

impl<A, T> ClientBuilder<A, T>
where
    A: Address + Clone,
{
    #[inline]
    /// Set client's connection scheme
    pub fn scheme(mut self, scheme: Scheme) -> Self {
        self.inner.scheme = scheme;
        self
    }

    /// Set total number of simultaneous streams per connection.
    ///
    /// If limit is 0, the connector uses "MAX_CONCURRENT_STREAMS" config from connection
    /// settings.
    /// The default limit size is 100.
    pub fn max_streams(mut self, limit: u32) -> Self {
        self.inner.max_streams = limit;
        self
    }

    /// Do not return error for frames for unknown streams.
    ///
    /// This includes pending resets, data and window update frames.
    pub fn skip_unknown_streams(mut self) -> Self {
        self.inner.skip_unknown_streams = true;
        self
    }

    /// Set max lifetime period for connection.
    ///
    /// Connection lifetime is max lifetime of any opened connection
    /// until it is closed regardless of keep-alive period.
    ///
    /// Default lifetime period is not set.
    pub fn lifetime(mut self, dur: Seconds) -> Self {
        self.inner.conn_lifetime = dur.into();
        self
    }

    /// Sets the minimum concurrent connections.
    ///
    /// By default min connections is set to a 1.
    pub fn minconn(mut self, num: usize) -> Self {
        self.inner.minconn = num;
        self
    }

    /// Sets the maximum concurrent connections.
    ///
    /// By default max connections is set to a 16.
    pub fn maxconn(mut self, num: usize) -> Self {
        self.inner.maxconn = num;
        self
    }

    /// Use custom connector
    pub fn connector<U, F>(self, connector: F) -> ClientBuilder<A, U>
    where
        F: IntoServiceFactory<U, Connect<A>, SharedCfg>,
        U: ServiceFactory<Connect<A>, SharedCfg, Error = ConnectError> + 'static,
        IoBoxed: From<U::Response>,
    {
        ClientBuilder {
            connect: self.connect,
            connector: connector.into_factory(),
            inner: self.inner,
            _t: PhantomData,
        }
    }
}

impl<A, T> ClientBuilder<A, T>
where
    A: Address + Clone,
    T: ServiceFactory<Connect<A>, SharedCfg, Error = ConnectError> + 'static,
    IoBoxed: From<T::Response>,
{
    /// Finish configuration process and create connections pool.
    pub async fn build(self, cfg: SharedCfg) -> Result<Client, T::InitError> {
        let connect = self.connect;
        let svc = Pipeline::new(self.connector.create(cfg).await?);

        let connector = Box::new(move || {
            log::trace!(
                "{}: Opening http/2 connection to {}",
                cfg.tag(),
                connect.host()
            );
            let fut = svc.call_static(connect.clone());
            Box::pin(async move { fut.await.map(IoBoxed::from) }) as Fut
        });

        Ok(Client {
            inner: Rc::new(Inner {
                connector,
                cfg: cfg.get(),
                config: self.inner,
            }),
            waiters: Default::default(),
        })
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("scheme", &self.inner.config.scheme)
            .field("authority", &self.inner.config.authority)
            .field("conn_timeout", &self.inner.config.conn_timeout)
            .field("conn_lifetime", &self.inner.config.conn_lifetime)
            .field("disconnect_timeout", &self.inner.config.disconnect_timeout)
            .field("minconn", &self.inner.config.minconn)
            .field("maxconn", &self.inner.config.maxconn)
            .field("max-streams", &self.inner.config.max_streams)
            .finish()
    }
}

impl<A, T> fmt::Debug for ClientBuilder<A, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("scheme", &self.inner.scheme)
            .field("authority", &self.inner.authority)
            .field("conn_timeout", &self.inner.conn_timeout)
            .field("conn_lifetime", &self.inner.conn_lifetime)
            .field("disconnect_timeout", &self.inner.disconnect_timeout)
            .field("minconn", &self.inner.minconn)
            .field("maxconn", &self.inner.maxconn)
            .field("max-streams", &self.inner.max_streams)
            .finish()
    }
}
