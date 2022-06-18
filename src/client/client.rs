use std::{fmt, rc::Rc};

use ntex_bytes::ByteString;
use ntex_http::{HeaderMap, Method};
use ntex_io::{Dispatcher as IoDispatcher, IoBoxed};
use ntex_service::{IntoService, Service};
use ntex_util::time::{sleep, Millis, Seconds};

use crate::default::DefaultControlService;
use crate::dispatcher::Dispatcher;
use crate::{codec::Codec, connection::Connection, Message, Stream};

/// Http2 client
#[derive(Clone)]
pub struct Client(Connection);

/// Http2 client connection
pub struct ClientConnection {
    io: IoBoxed,
    con: Connection,
    codec: Rc<Codec>,
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

    pub fn send_request(&self, method: Method, path: ByteString, headers: HeaderMap) -> Stream {
        self.0.send_request(method, path, headers)
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
    pub(super) fn new(
        io: IoBoxed,
        con: Connection,
        codec: Rc<Codec>,
        keepalive: Seconds,
        disconnect_timeout: Seconds,
    ) -> Self {
        ClientConnection {
            io,
            con,
            codec,
            keepalive,
            disconnect_timeout,
        }
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
            ntex::rt::spawn(keepalive(self.con.clone(), self.keepalive));
        }

        let disp = Dispatcher::new(
            self.con.clone(),
            DefaultControlService,
            service.into_service(),
        );

        IoDispatcher::new(self.io, self.codec, disp)
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
