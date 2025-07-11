use std::{fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use nanorand::Rng;
use ntex_bytes::ByteString;
use ntex_http::{uri::Scheme, HeaderMap, Method};
use ntex_io::{Dispatcher as IoDispatcher, IoBoxed, IoRef, OnDisconnect};
use ntex_util::time::{Millis, Sleep};

use crate::connection::Connection;
use crate::default::DefaultControlService;
use crate::dispatcher::Dispatcher;
use crate::{codec::Codec, config::Config, OperationError};

use super::stream::{HandleService, InflightStorage, RecvStream, SendStream};

/// Http2 client
#[derive(Clone)]
pub struct SimpleClient(Rc<ClientRef>);

/// Http2 client
struct ClientRef {
    id: ByteString,
    con: Connection,
    authority: ByteString,
    storage: InflightStorage,
}

impl SimpleClient {
    /// Construct new `Client` instance.
    pub fn new<T>(io: T, config: Config, scheme: Scheme, authority: ByteString) -> Self
    where
        IoBoxed: From<T>,
    {
        SimpleClient::with_params(
            io.into(),
            config,
            scheme,
            authority,
            false,
            InflightStorage::default(),
        )
    }

    pub(super) fn with_params(
        io: IoBoxed,
        config: Config,
        scheme: Scheme,
        authority: ByteString,
        skip_unknown_streams: bool,
        storage: InflightStorage,
    ) -> Self {
        let codec = Codec::default();
        let con = Connection::new(io.get_ref(), codec, config, false, skip_unknown_streams);
        con.set_secure(scheme == Scheme::HTTPS);

        let disp = Dispatcher::new(
            con.clone(),
            DefaultControlService,
            HandleService::new(storage.clone()),
        );

        let fut = IoDispatcher::new(
            io,
            con.codec().clone(),
            disp,
            &con.config().dispatcher_config,
        );
        let _ = ntex_util::spawn(async move {
            let _ = fut.await;
        });

        SimpleClient(Rc::new(ClientRef {
            con,
            authority,
            storage,
            id: gen_id(),
        }))
    }

    #[inline]
    /// Get client id
    pub fn id(&self) -> &ByteString {
        &self.0.id
    }

    #[inline]
    /// Get io tag
    pub fn tag(&self) -> &'static str {
        self.0.con.tag()
    }

    #[inline]
    /// Send request to the peer
    pub async fn send(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<(SendStream, RecvStream), OperationError> {
        let stream = self
            .0
            .con
            .send_request(self.0.authority.clone(), method, path, headers, eof)
            .await?;

        Ok(self.0.storage.inflight(stream))
    }

    #[inline]
    /// Check if client is allowed to send new request
    ///
    /// Readiness depends on number of opened streams and max concurrency setting
    pub fn is_ready(&self) -> bool {
        self.0.con.can_create_new_stream()
    }

    #[inline]
    /// Check client readiness
    ///
    /// Client is ready when it is possible to start new stream
    pub async fn ready(&self) -> Result<(), OperationError> {
        self.0.con.ready().await
    }

    #[inline]
    /// Gracefully close connection
    pub fn close(&self) {
        log::debug!("Closing client");
        self.0.con.disconnect_when_ready()
    }

    #[inline]
    /// Close connection
    pub fn force_close(&self) {
        self.0.con.close()
    }

    #[inline]
    /// Gracefully disconnect connection
    ///
    /// Connection force closes if `ClientDisconnect` get dropped
    pub fn disconnect(&self) -> ClientDisconnect {
        ClientDisconnect::new(self.clone())
    }

    #[inline]
    /// Check if connection is closed
    pub fn is_closed(&self) -> bool {
        self.0.con.is_closed()
    }

    #[inline]
    /// Check if connection is disconnecting
    pub fn is_disconnecting(&self) -> bool {
        self.0.con.is_disconnecting()
    }

    #[inline]
    /// Notify when connection get closed
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.con.io().on_disconnect()
    }

    #[inline]
    /// Client's authority
    pub fn authority(&self) -> &ByteString {
        &self.0.authority
    }

    /// Get max number of active streams
    pub fn max_streams(&self) -> Option<u32> {
        self.0.con.max_streams()
    }

    /// Get number of active streams
    pub fn active_streams(&self) -> u32 {
        self.0.con.active_streams()
    }

    #[doc(hidden)]
    /// Get number of active streams
    pub fn pings_count(&self) -> u16 {
        self.0.con.pings_count()
    }

    #[doc(hidden)]
    /// Get access to underlining io object
    pub fn io_ref(&self) -> &IoRef {
        self.0.con.io()
    }

    #[doc(hidden)]
    /// Get access to underlining http/2 connection object
    pub fn connection(&self) -> &Connection {
        &self.0.con
    }
}

impl Drop for SimpleClient {
    fn drop(&mut self) {
        if Rc::strong_count(&self.0) == 1 {
            log::debug!("Last h2 client has been dropped, disconnecting");
            self.0.con.disconnect_when_ready()
        }
    }
}

impl fmt::Debug for SimpleClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::SimpleClient")
            .field("authority", &self.0.authority)
            .field("connection", &self.0.con)
            .finish()
    }
}

#[derive(Debug)]
pub struct ClientDisconnect {
    client: SimpleClient,
    disconnect: OnDisconnect,
    timeout: Option<Sleep>,
}

impl ClientDisconnect {
    fn new(client: SimpleClient) -> Self {
        log::debug!("Disconnecting client");

        client.0.con.disconnect_when_ready();
        ClientDisconnect {
            disconnect: client.on_disconnect(),
            timeout: None,
            client,
        }
    }

    pub fn disconnect_timeout<T>(mut self, timeout: T) -> Self
    where
        Millis: From<T>,
    {
        self.timeout = Some(Sleep::new(timeout.into()));
        self
    }
}

impl Drop for ClientDisconnect {
    fn drop(&mut self) {
        self.client.0.con.close();
    }
}

impl Future for ClientDisconnect {
    type Output = Result<(), OperationError>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut();

        if Pin::new(&mut this.disconnect).poll(cx).is_ready() {
            return Poll::Ready(this.client.0.con.check_error_with_disconnect());
        } else if let Some(ref mut sleep) = this.timeout {
            if sleep.poll_elapsed(cx).is_ready() {
                this.client.0.con.close();
                return Poll::Ready(Err(OperationError::Disconnected));
            }
        }
        Poll::Pending
    }
}

fn gen_id() -> ByteString {
    const BASE: &str = "abcdefghijklmnopqrstuvwxyz234567";

    let mut rng = nanorand::tls_rng();
    let mut id = String::with_capacity(16);
    for _ in 0..16 {
        let idx = rng.generate_range::<usize, _>(..BASE.len());
        id.push_str(&BASE[idx..idx + 1]);
    }
    ByteString::from(id)
}
