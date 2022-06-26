use std::{fmt, task::Context, task::Poll};

use ntex_bytes::ByteString;
use ntex_http::{HeaderMap, Method};
use ntex_io::{Dispatcher as IoDispatcher, IoBoxed, OnDisconnect};
use ntex_rt::spawn;
use ntex_service::{IntoService, Service};
use ntex_util::future::poll_fn;
use ntex_util::time::{sleep, Millis, Seconds};

use crate::default::DefaultControlService;
use crate::dispatcher::Dispatcher;
use crate::{
    codec::Codec, config::Config, connection::Connection, Message, OperationError, Stream,
};

/// Http2 client
pub struct Client(Connection);

/// Http2 client connection
pub struct ClientConnection(IoBoxed, Connection);

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::Client")
            // .field("connection", &self.0)
            .finish()
    }
}

impl Client {
    #[inline]
    /// Send request to the peer
    pub async fn send_request(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
    ) -> Result<Stream, OperationError> {
        self.0.send_request(method, path, headers).await
    }

    #[inline]
    /// Check client readiness
    ///
    /// Client is ready when it is possible to start new stream
    pub async fn ready(&self) -> Result<(), OperationError> {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    #[inline]
    /// Check client readiness
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), OperationError>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    /// Gracefully close connection
    pub fn close(&self) {
        self.0.state().io.close()
    }

    #[inline]
    /// Check if connection is closed
    pub fn is_closed(&self) -> bool {
        self.0.state().io.is_closed()
    }

    #[inline]
    /// Notify when connection get closed
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.state().io.on_disconnect()
    }
}

impl fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::ClientConnection")
            .field("config", &self.1.config())
            .finish()
    }
}

impl ClientConnection {
    /// Construct new `ClientConnection` instance.
    pub fn new<T>(io: T, config: Config) -> Self
    where
        IoBoxed: From<T>,
    {
        let io: IoBoxed = io.into();
        let codec = Codec::default();
        let con = Connection::new(io.get_ref(), codec, config);

        ClientConnection(io, con)
    }

    #[inline]
    /// Get client
    pub fn client(&self) -> Client {
        Client(self.1.clone())
    }

    /// Run client with provided control messages handler
    pub async fn start<F, S>(self, service: F) -> Result<(), ()>
    where
        F: IntoService<S, Message> + 'static,
        S: Service<Message, Response = ()> + 'static,
        S::Error: fmt::Debug,
    {
        if self.1.config().keepalive_timeout.get().non_zero() {
            spawn(keepalive(
                self.1.clone(),
                self.1.config().keepalive_timeout.get(),
            ));
        }

        let disp = Dispatcher::new(
            self.1.clone(),
            DefaultControlService,
            service.into_service(),
        );

        IoDispatcher::new(self.0, self.1.state().codec.clone(), disp)
            .keepalive_timeout(Seconds::ZERO)
            .disconnect_timeout(self.1.config().disconnect_timeout.get())
            .await
    }
}

async fn keepalive(con: Connection, timeout: Seconds) {
    log::debug!("start http client keep-alive task");

    let keepalive = Millis::from(timeout);
    loop {
        if con.state().is_disconnected() {
            break;
        }
        sleep(keepalive).await;

        //if !con.ping() {
        // connection is closed
        //log::debug!("http client connection is closed, stopping keep-alive task");
        //break;
        //}
    }
}
