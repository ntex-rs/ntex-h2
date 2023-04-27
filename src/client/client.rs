use std::{fmt, rc::Rc};

use ntex_bytes::ByteString;
use ntex_http::{uri::Scheme, HeaderMap, Method};
use ntex_io::{Dispatcher as IoDispatcher, IoBoxed, OnDisconnect};
use ntex_service::{IntoService, Service};
use ntex_util::time::Seconds;

use crate::connection::Connection;
use crate::default::DefaultControlService;
use crate::dispatcher::Dispatcher;
use crate::{codec::Codec, config::Config, Message, OperationError, Stream};

/// Http2 client
#[derive(Clone)]
pub struct Client(Rc<ClientRef>);

/// Http2 client
struct ClientRef {
    con: Connection,
    authority: ByteString,
}

/// Http2 client connection
pub struct ClientConnection {
    io: IoBoxed,
    client: Rc<ClientRef>,
}

impl Client {
    #[inline]
    /// Send request to the peer
    pub async fn send_request(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<Stream, OperationError> {
        self.0
            .con
            .send_request(self.0.authority.clone(), method, path, headers, eof)
            .await
    }

    #[inline]
    /// Check if client is allowed to send new request
    ///
    /// Readiness depends on number of opened streams and max concurrency setting
    pub fn is_ready(&self) -> bool {
        self.0.con.can_create_new_stream()
    }

    #[doc(hidden)]
    #[inline]
    /// Set client's secure state
    pub fn set_scheme(&self, scheme: Scheme) {
        if scheme == Scheme::HTTPS {
            self.0.con.set_secure(true)
        } else {
            self.0.con.set_secure(false)
        }
    }

    #[doc(hidden)]
    /// Set client's authority
    pub fn set_authority(&self, _: ByteString) {}

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
    /// Check if connection is closed
    pub fn is_closed(&self) -> bool {
        self.0.con.is_closed()
    }

    #[inline]
    /// Notify when connection get closed
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.con.state().io.on_disconnect()
    }

    /// Get max number of active streams
    pub fn max_streams(&self) -> Option<u32> {
        self.0.con.max_streams()
    }

    /// Get number of active streams
    pub fn active_streams(&self) -> u32 {
        self.0.con.active_streams()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if Rc::strong_count(&self.0) == 1 {
            log::debug!("Last h2 client has been dropped, disconnecting");
            self.0.con.disconnect_when_ready()
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::Client")
            .field("authority", &self.0.authority)
            .field("connection", &self.0.con)
            .finish()
    }
}

impl fmt::Debug for ClientRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::Client")
            .field("authority", &self.authority)
            .field("connection", &self.con)
            .finish()
    }
}

impl ClientConnection {
    /// Construct new `ClientConnection` instance.
    pub fn new<T>(io: T, config: Config) -> Self
    where
        IoBoxed: From<T>,
    {
        Self::with_params(io, config, false, ByteString::new())
    }

    /// Construct new `ClientConnection` instance.
    pub fn with_params<T>(io: T, config: Config, secure: bool, authority: ByteString) -> Self
    where
        IoBoxed: From<T>,
    {
        let io: IoBoxed = io.into();
        let codec = Codec::default();
        let con = Connection::new(io.get_ref(), codec, config, false);
        con.set_secure(secure);

        ClientConnection {
            io,
            client: Rc::new(ClientRef { con, authority }),
        }
    }

    #[inline]
    /// Get client
    pub fn client(&self) -> Client {
        Client(self.client.clone())
    }

    /// Run client with provided control messages handler
    pub async fn start<F, S>(self, service: F) -> Result<(), ()>
    where
        F: IntoService<S, Message> + 'static,
        S: Service<Message, Response = ()> + 'static,
        S::Error: fmt::Debug,
    {
        let disp = Dispatcher::new(
            self.client.con.clone(),
            DefaultControlService,
            service.into_service(),
        );

        IoDispatcher::new(self.io, self.client.con.state().codec.clone(), disp)
            .keepalive_timeout(Seconds::ZERO)
            .disconnect_timeout(self.client.con.config().disconnect_timeout.get())
            .await
    }
}

impl fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::ClientConnection")
            .field("authority", &self.client.authority)
            .field("config", &self.client.con.config())
            .finish()
    }
}
