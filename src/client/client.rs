use std::{fmt, task::Context, task::Poll};

use ntex_bytes::ByteString;
use ntex_http::{HeaderMap, Method};
use ntex_io::{Dispatcher as IoDispatcher, IoBoxed};
use ntex_rt::spawn;
use ntex_service::{IntoService, Service};
use ntex_util::future::poll_fn;
use ntex_util::time::{sleep, Millis, Seconds};

use crate::default::DefaultControlService;
use crate::dispatcher::Dispatcher;
use crate::{connection::Connection, Message, OperationError, Stream};

/// Http2 client
pub struct Client(Connection);

/// Http2 client connection
pub struct ClientConnection {
    io: IoBoxed,
    con: Connection,
    keepalive: Seconds,
    disconnect_timeout: Seconds,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::Client")
            // .field("connection", &self.0)
            .finish()
    }
}

impl Client {
    fn new(con: Connection) -> Self {
        Self(con)
    }

    /// Check client readiness.
    ///
    /// Client is ready when it is possible to start new stream.
    pub async fn ready(&self) -> Result<(), OperationError> {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    pub async fn send_request(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
    ) -> Result<Stream, OperationError> {
        self.0.send_request(method, path, headers).await
    }

    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), OperationError>> {
        self.0.poll_ready(cx)
    }

    pub fn close(&self) {}
}

impl fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::ClientConnection")
            .field("keepalive", &self.keepalive)
            .field("disconnect_timeout", &self.disconnect_timeout)
            .finish()
    }
}

impl ClientConnection {
    /// Construct new `ClientConnection` instance.
    pub(super) fn new(io: IoBoxed, con: Connection) -> Self {
        ClientConnection {
            io,
            con,
            keepalive: Seconds(120),
            disconnect_timeout: Seconds(3),
        }
    }

    /// Set server connection disconnect timeout.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout(mut self, val: Seconds) -> Self {
        self.disconnect_timeout = val;
        self
    }

    /// Set keep-alive timeout.
    ///
    /// By default keep-alive time-out is set to 120 seconds.
    pub fn idle_timeout(mut self, timeout: Seconds) -> Self {
        self.keepalive = timeout;
        self
    }

    #[inline]
    /// Get client
    pub fn client(&self) -> Client {
        Client::new(self.con.clone())
    }

    /// Run client with provided control messages handler
    pub async fn start<F, S>(self, service: F) -> Result<(), ()>
    where
        F: IntoService<S, Message> + 'static,
        S: Service<Message, Response = ()> + 'static,
        S::Error: fmt::Debug,
    {
        if self.keepalive.non_zero() {
            spawn(keepalive(self.con.clone(), self.keepalive));
        }

        let disp = Dispatcher::new(
            self.con.clone(),
            DefaultControlService,
            service.into_service(),
        );

        IoDispatcher::new(self.io, self.con.state().codec.clone(), disp)
            .keepalive_timeout(Seconds::ZERO)
            .disconnect_timeout(self.disconnect_timeout)
            .await
    }
}

async fn keepalive(con: Connection, timeout: Seconds) {
    log::debug!("start http client keep-alive task");

    let keepalive = Millis::from(timeout);
    loop {
        sleep(keepalive).await;

        //if !con.ping() {
        // connection is closed
        //log::debug!("http client connection is closed, stopping keep-alive task");
        //break;
        //}
    }
}
