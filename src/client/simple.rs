use std::{fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll, time::SystemTime};

use nanorand::Rng;
use ntex_bytes::{BufMut, ByteString, BytesMut};
use ntex_dispatcher::Dispatcher as IoDispatcher;
use ntex_error::Error;
use ntex_http::{HeaderMap, Method, uri::Scheme};
use ntex_io::{IoBoxed, IoRef, OnDisconnect};
use ntex_service::{Pipeline, cfg::Cfg};
use ntex_util::{channel::pool, time::Millis, time::Sleep, time::system_time};

use crate::{OperationError, ServiceConfig, codec::Codec};
use crate::{connection::Connection, default::DefaultControlService, dispatcher::Dispatcher};

use super::stream::{HandleService, InflightStorage, RecvStream, SendStream};

/// Client for HTTP/2 connection.
#[derive(Clone)]
pub struct SimpleClient(Rc<ClientRef>);

/// Shared state for an HTTP/2 client connection.
struct ClientRef {
    id: ByteString,
    con: Connection,
    authority: ByteString,
    storage: InflightStorage,
    created: SystemTime,
}

impl SimpleClient {
    /// Creates a client over an established HTTP/2 transport.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new<T>(io: T, scheme: Scheme, authority: ByteString) -> Self
    where
        IoBoxed: From<T>,
    {
        let io: IoBoxed = io.into();
        let cfg = io.shared().get();
        SimpleClient::with_params(
            io,
            cfg,
            &scheme,
            authority,
            false,
            InflightStorage::default(),
            pool::new(),
        )
    }

    pub(super) fn with_params(
        io: IoBoxed,
        cfg: Cfg<ServiceConfig>,
        scheme: &Scheme,
        authority: ByteString,
        skip_unknown_streams: bool,
        storage: InflightStorage,
        pool: pool::Pool<()>,
    ) -> Self {
        let codec = Codec::default();
        codec.set_max_headers(cfg.max_headers);

        let con = Connection::new(false, io.get_ref(), codec, cfg, false, skip_unknown_streams, pool);
        con.set_secure(*scheme == Scheme::HTTPS);

        let disp = Pipeline::new(
            (),
            Dispatcher::new(
                con.clone(),
                Pipeline::new((), HandleService::new(storage.clone())),
                Pipeline::new((), DefaultControlService).bind(),
            ),
        );

        let fut = IoDispatcher::new(io, con.codec().clone(), disp);
        ntex_util::spawn(async move {
            let _ = fut.await;
        });

        SimpleClient(Rc::new(ClientRef {
            con,
            authority,
            storage,
            id: gen_id(),
            created: system_time(),
        }))
    }

    #[inline]
    /// Returns the generated client identifier.
    pub fn id(&self) -> &ByteString {
        &self.0.id
    }

    #[inline]
    /// Returns the connection's shared configuration tag.
    pub fn tag(&self) -> &'static str {
        self.0.con.tag()
    }

    #[inline]
    /// Returns the connection's service name.
    pub fn service(&self) -> &'static str {
        self.0.con.service()
    }

    #[inline]
    /// Returns when this client was created.
    pub fn created(&self) -> SystemTime {
        self.0.created
    }

    #[inline]
    /// Opens a stream and sends request headers to the peer.
    pub async fn send(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<(SendStream, RecvStream), Error<OperationError>> {
        let stream = self
            .0
            .con
            .send_request(self.0.authority.clone(), method, path, headers, eof)
            .await?;

        Ok(self.0.storage.inflight(stream))
    }

    /// Reserves a stream for a later request.
    ///
    /// The reserved stream is counted by [`active_streams`](Self::active_streams)
    /// until the reservation is dropped or the request's stream is closed.
    /// Returns `None` if the peer's concurrent stream limit is reached, or the
    /// connection is failed or disconnecting.
    ///
    /// Graceful disconnect waits for outstanding reservations, a reserved
    /// stream can be opened after [`close`](Self::close) is called.
    pub fn reserve(&self) -> Option<StreamReservation> {
        if self.0.con.reserve_stream() {
            Some(StreamReservation(Some(self.clone())))
        } else {
            None
        }
    }

    #[inline]
    /// Returns whether the connection can open another stream.
    ///
    /// Readiness depends on the active stream count and the peer's concurrency
    /// setting.
    pub fn is_ready(&self) -> bool {
        self.0.con.can_create_new_stream()
    }

    #[inline]
    /// Waits until the connection can open another stream.
    ///
    /// Client is ready when it is possible to start new stream
    pub async fn ready(&self) -> Result<(), Error<OperationError>> {
        self.0.con.ready().await
    }

    #[inline]
    /// Starts graceful connection shutdown.
    pub fn close(&self) {
        log::debug!("Closing client");
        self.0.con.disconnect_when_ready();
    }

    #[inline]
    /// Closes the connection immediately.
    pub fn force_close(&self) {
        self.0.con.close();
    }

    #[inline]
    /// Starts graceful shutdown and returns a completion future.
    ///
    /// Dropping the returned [`ClientDisconnect`] before completion force-closes
    /// the connection.
    pub fn disconnect(&self) -> ClientDisconnect {
        ClientDisconnect::new(self.clone())
    }

    #[inline]
    /// Returns whether the connection is closed.
    pub fn is_closed(&self) -> bool {
        self.0.con.is_closed()
    }

    #[inline]
    /// Returns whether graceful shutdown is in progress.
    pub fn is_disconnecting(&self) -> bool {
        self.0.con.is_disconnecting()
    }

    #[inline]
    /// Returns a notification future for connection closure.
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.con.io().on_disconnect()
    }

    #[inline]
    /// Returns the authority used for requests.
    pub fn authority(&self) -> &ByteString {
        &self.0.authority
    }

    /// Returns the peer's maximum concurrent stream count, if known.
    pub fn max_streams(&self) -> Option<u32> {
        self.0.con.max_streams()
    }

    /// Returns the number of active client-initiated streams.
    ///
    /// A stream is active from [`send`](Self::send) until both of its sides
    /// are closed or it is reset.
    pub fn active_streams(&self) -> u32 {
        self.0.con.active_streams()
    }

    /// Sets a callback that is called when the connection's stream capacity changes.
    ///
    /// The callback is called when a client-initiated stream is released,
    /// when the peer changes its maximum concurrent stream count, and when
    /// the connection fails or is closed. It runs inside the connection's
    /// dispatcher, so it should only schedule work, for example wake a task.
    /// Setting a new callback replaces the previous one. The callback must
    /// not hold the client, it would keep the connection alive.
    pub fn on_capacity<F>(&self, f: F)
    where
        F: Fn() + 'static,
    {
        self.0.con.set_on_capacity(Some(Rc::new(f)));
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
            self.0.con.disconnect_when_ready();
        }
    }
}

/// Reserved stream of an HTTP/2 client connection.
///
/// Created by [`SimpleClient::reserve`]. Dropping the reservation without
/// sending a request releases the stream.
pub struct StreamReservation(Option<SimpleClient>);

impl StreamReservation {
    /// Opens the reserved stream and sends request headers to the peer.
    pub fn send(
        mut self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<(SendStream, RecvStream), Error<OperationError>> {
        let Some(client) = self.0.take() else { unreachable!() };
        match client
            .0
            .con
            .send_reserved_request(client.0.authority.clone(), method, path, headers, eof)
        {
            Ok(stream) => Ok(client.0.storage.inflight(stream)),
            Err(err) => {
                client.0.con.release_reserved_stream();
                Err(err)
            }
        }
    }
}

impl Drop for StreamReservation {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            client.0.con.release_reserved_stream();
        }
    }
}

impl fmt::Debug for StreamReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ntex_h2::StreamReservation").finish()
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

/// Future that completes when a client connection disconnects.
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

    /// Sets the maximum time to wait for graceful disconnection.
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
    type Output = Result<(), Error<OperationError>>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut();

        if Pin::new(&mut this.disconnect).poll(cx).is_ready() {
            return Poll::Ready(this.client.0.con.check_error_with_disconnect());
        } else if let Some(ref mut sleep) = this.timeout
            && sleep.poll_elapsed(cx).is_ready()
        {
            this.client.0.con.close();
            return Poll::Ready(Err(Error::new(
                OperationError::Disconnected,
                self.client.0.con.service(),
            )));
        }
        Poll::Pending
    }
}

fn gen_id() -> ByteString {
    const BASE: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

    let mut rng = nanorand::tls_rng();
    let mut id = BytesMut::with_capacity(16);
    for _ in 0..16 {
        let idx = rng.generate_range::<usize, _>(..BASE.len());
        id.put_u8(BASE[idx]);
    }
    ByteString::try_from(id).unwrap()
}
