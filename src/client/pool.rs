use std::{cell::Cell, cell::RefCell, collections::VecDeque, fmt, rc::Rc, time::Duration};

use nanorand::{Rng, WyRand};
use ntex_bytes::ByteString;
use ntex_http::{uri::Scheme, HeaderMap, Method};
use ntex_io::IoBoxed;
use ntex_net::connect::{self as connect, Address, Connect, Connector as DefaultConnector};
use ntex_service::{IntoService, Pipeline, Service};
use ntex_util::time::{timeout_checked, Millis, Seconds};
use ntex_util::{channel::oneshot, future::BoxFuture};

use super::stream::{InflightStorage, RecvStream, SendStream};
use super::{simple::SimpleClient, ClientError};

type Fut = BoxFuture<'static, Result<IoBoxed, connect::ConnectError>>;
type Connector = Box<dyn Fn() -> BoxFuture<'static, Result<IoBoxed, connect::ConnectError>>>;

#[derive(Clone)]
/// Manages http client network connectivity.
pub struct Client {
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

impl Client {
    #[inline]
    /// Configure and build client
    pub fn build<A, U, T, F>(addr: U, connector: F) -> ClientBuilder
    where
        A: Address + Clone,
        F: IntoService<T, Connect<A>>,
        T: Service<Connect<A>, Error = connect::ConnectError> + 'static,
        IoBoxed: From<T::Response>,
        Connect<A>: From<U>,
    {
        ClientBuilder::new(addr, connector)
    }

    #[inline]
    /// Configure and build client
    pub fn with_default<A, U>(addr: U) -> ClientBuilder
    where
        A: Address + Clone,
        Connect<A>: From<U>,
    {
        ClientBuilder::with_default(addr)
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
        // can create new connection
        if !self.inner.connecting.get()
            && (num < self.inner.maxconn || (self.inner.minconn > 0 && num < self.inner.minconn))
        {
            // create new connection
            self.inner.connecting.set(true);

            self.create_connection().await?;
        } else {
            log::debug!(
                "New connection is being established {:?} or number of existing cons {} greater than allowed {}",
                self.inner.connecting.get(), num, self.inner.maxconn);

            // wait for available connection
            let (tx, rx) = oneshot::channel();
            self.waiters.borrow_mut().push_back(tx);
            let _ = rx.await?;
        }
        Ok(())
    }

    fn get_client(&self) -> (Option<SimpleClient>, usize) {
        let mut connections = self.inner.connections.borrow_mut();

        // cleanup connections
        let mut idx = 0;
        while idx < connections.len() {
            if connections[idx].is_closed() {
                connections.remove(idx);
            } else if connections[idx].is_disconnecting() {
                let con = connections.remove(idx);
                let timeout = self.inner.disconnect_timeout;
                let f = ntex_util::spawn(async move {
                    let _ = con.disconnect().disconnect_timeout(timeout).await;
                });
                drop(f);
            } else {
                idx += 1;
            }
        }
        let num = connections.len();
        if self.inner.minconn > 0 && num < self.inner.minconn {
            // create new connection
            (None, num)
        } else {
            // first search for connections with less than 50% capacity usage
            let client = connections.iter().find(|item| {
                let cap = item.max_streams().unwrap_or(self.inner.max_streams) >> 1;
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
            let res = match timeout_checked(inner.conn_timeout, (*inner.connector)()).await {
                Ok(Ok(io)) => {
                    // callbacks for end of stream
                    let waiters2 = waiters.clone();
                    let storage = InflightStorage::new(move |_| {
                        notify(&mut waiters2.borrow_mut());
                    });
                    // construct client
                    let client = SimpleClient::with_params(
                        io,
                        inner.config.clone(),
                        inner.scheme.clone(),
                        inner.authority.clone(),
                        storage,
                    );
                    inner.connections.borrow_mut().push(client);
                    inner
                        .total_connections
                        .set(inner.total_connections.get() + 1);
                    Ok(())
                }
                Ok(Err(err)) => Err(ClientError::from(err)),
                Err(_) => Err(ClientError::HandshakeTimeout),
            };
            inner.connecting.set(false);
            for waiter in waiters.borrow_mut().drain(..) {
                let _ = waiter.send(());
            }

            if res.is_err() {
                inner.connect_errors.set(inner.connect_errors.get() + 1);
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

#[doc(hidden)]
impl Client {
    pub fn stat_active_connections(&self) -> usize {
        self.inner.connections.borrow().len()
    }

    pub fn stat_total_connections(&self) -> usize {
        self.inner.total_connections.get()
    }

    pub fn stat_connect_errors(&self) -> usize {
        self.inner.connect_errors.get()
    }

    pub fn stat_connections<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&[SimpleClient]) -> R,
    {
        f(&self.inner.connections.borrow())
    }
}

/// Manages http client network connectivity.
///
/// The `ClientBuilder` type uses a builder-like combinator pattern for service
/// construction that finishes by calling the `.finish()` method.
pub struct ClientBuilder(Inner);

struct Inner {
    minconn: usize,
    maxconn: usize,
    conn_timeout: Millis,
    conn_lifetime: Duration,
    disconnect_timeout: Millis,
    max_streams: u32,
    scheme: Scheme,
    config: crate::Config,
    authority: ByteString,
    connector: Connector,
    connecting: Cell<bool>,
    connections: RefCell<Vec<SimpleClient>>,
    total_connections: Cell<usize>,
    connect_errors: Cell<usize>,
}

impl ClientBuilder {
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
            log::trace!("Opening http/2 connection to {}", connect.host());
            let connect = connect.clone();
            let svc = connector.clone();
            let f: Fut = Box::pin(async move { svc.call(connect).await.map(IoBoxed::from) });
            f
        });

        ClientBuilder(Inner {
            authority,
            connector,
            conn_timeout: Millis(1_000),
            conn_lifetime: Duration::from_secs(0),
            disconnect_timeout: Millis(15_000),
            max_streams: 100,
            minconn: 1,
            maxconn: 16,
            scheme: Scheme::HTTP,
            config: crate::Config::client(),
            connecting: Cell::new(false),
            connections: Default::default(),
            total_connections: Cell::new(0),
            connect_errors: Cell::new(0),
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

impl ClientBuilder {
    #[inline]
    /// Set client's connection scheme
    pub fn scheme(mut self, scheme: Scheme) -> Self {
        self.0.scheme = scheme;
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
    pub fn max_streams(mut self, limit: u32) -> Self {
        self.0.max_streams = limit;
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

    /// Sets the minimum concurrent connections.
    ///
    /// By default min connections is set to a 1.
    pub fn minconn(mut self, num: usize) -> Self {
        self.0.minconn = num;
        self
    }

    /// Sets the maximum concurrent connections.
    ///
    /// By default max connections is set to a 16.
    pub fn maxconn(mut self, num: usize) -> Self {
        self.0.maxconn = num;
        self
    }

    /// Set client connection disconnect timeout.
    ///
    /// Defines a timeout for disconnect connection. Disconnecting connection
    /// involes closing all active streams. If a disconnect procedure does not complete
    /// within this time, the socket get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 15 seconds.
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
    pub fn finish(self) -> Client {
        Client {
            inner: Rc::new(self.0),
            waiters: Default::default(),
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("scheme", &self.inner.scheme)
            .field("authority", &self.inner.authority)
            .field("conn_timeout", &self.inner.conn_timeout)
            .field("conn_lifetime", &self.inner.conn_lifetime)
            .field("disconnect_timeout", &self.inner.disconnect_timeout)
            .field("minconn", &self.inner.minconn)
            .field("maxconn", &self.inner.maxconn)
            .field("max-streams", &self.inner.max_streams)
            .field("config", &self.inner.config)
            .finish()
    }
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("scheme", &self.0.scheme)
            .field("authority", &self.0.authority)
            .field("conn_timeout", &self.0.conn_timeout)
            .field("conn_lifetime", &self.0.conn_lifetime)
            .field("disconnect_timeout", &self.0.disconnect_timeout)
            .field("minconn", &self.0.minconn)
            .field("maxconn", &self.0.maxconn)
            .field("max-streams", &self.0.max_streams)
            .field("config", &self.0.config)
            .finish()
    }
}
