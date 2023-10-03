use std::{fmt, rc::Rc};

use ntex_bytes::ByteString;
use ntex_http::{uri::Scheme, HeaderMap, Method};
use ntex_io::{Dispatcher as IoDispatcher, IoBoxed, OnDisconnect};
use ntex_util::time::Seconds;

use crate::connection::Connection;
use crate::default::DefaultControlService;
use crate::dispatcher::Dispatcher;
use crate::{codec::Codec, config::Config, OperationError};

use super::stream::{HandleService, InflightStorage, RecvStream, SendStream};

/// Http2 client
#[derive(Clone)]
pub struct Client(Rc<ClientRef>);

/// Http2 client
struct ClientRef {
    con: Connection,
    authority: ByteString,
    storage: InflightStorage,
}

impl Client {
    /// Construct new `Client` instance.
    pub fn new<T>(io: T, config: Config, scheme: Scheme, authority: ByteString) -> Self
    where
        IoBoxed: From<T>,
    {
        Client::with_params(
            io.into(),
            config,
            scheme,
            authority,
            InflightStorage::default(),
        )
    }

    pub(super) fn with_params(
        io: IoBoxed,
        config: Config,
        scheme: Scheme,
        authority: ByteString,
        storage: InflightStorage,
    ) -> Self {
        let codec = Codec::default();
        let con = Connection::new(io.get_ref(), codec, config, false);
        con.set_secure(scheme == Scheme::HTTPS);

        let disp = Dispatcher::new(
            con.clone(),
            DefaultControlService,
            HandleService::new(storage.clone()),
        );
        let fut = IoDispatcher::new(io, con.state().codec.clone(), disp)
            .keepalive_timeout(Seconds::ZERO)
            .disconnect_timeout(con.config().disconnect_timeout.get());

        ntex_rt::spawn(async move {
            let _ = fut.await;
        });

        Client(Rc::new(ClientRef {
            con,
            authority,
            storage,
        }))
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
    /// Check if connection is closed
    pub fn is_closed(&self) -> bool {
        self.0.con.is_closed()
    }

    #[inline]
    /// Notify when connection get closed
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.con.state().io.on_disconnect()
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
