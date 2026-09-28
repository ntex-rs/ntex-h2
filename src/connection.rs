use std::task::{Context, Poll, Waker};
use std::{cell::Cell, cell::RefCell, fmt, io, mem, rc::Rc};
use std::{collections::VecDeque, future::poll_fn, time::Instant};

use ntex_bytes::{BytePages, ByteString, Bytes};
use ntex_error::Error;
use ntex_http::{HeaderMap, Method};
use ntex_io::IoRef;
use ntex_service::cfg::Cfg;
use ntex_util::time::{self, now, sleep};
use ntex_util::{HashMap, HashSet, channel::pool, future::Either, spawn};

use crate::error::{ConnectionError, OperationError, StreamError, StreamErrorInner};
use crate::frame::{self, Headers, PseudoHeaders, StreamId, WindowSize, WindowUpdate};
use crate::stream::{Stream, StreamRef};
use crate::{codec::Codec, config::ServiceConfig, consts, message::Message, window::Window};

pub(crate) type EitherError = Either<Error<ConnectionError>, StreamErrorInner>;

#[derive(Clone)]
pub struct Connection(Rc<ConnectionState>);

pub(crate) struct RecvHalfConnection(Rc<ConnectionState>);

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub(crate) struct ConnectionFlags: u16 {
        const SERVER                  = 0b0000_0001;
        const SETTINGS_PROCESSED      = 0b0000_0010;
        const UNKNOWN_STREAMS         = 0b0000_0100;
        const DISCONNECT_WHEN_READY   = 0b0000_1000;
        const SECURE                  = 0b0001_0000;
        const SETTINGS_TIMEOUT        = 0b0010_0000;
        const KA_TIMER                = 0b0100_0000;
        const RECV_PONG               = 0b1000_0000;
        const REMOTE_SETTINGS         = 0b0001_0000_0000;
    }
}

struct ConnectionState {
    io: IoRef,
    codec: Codec,
    send_window: Cell<Window>,
    recv_window: Cell<Window>,
    // received but not yet consumed data
    recv_size: Cell<u32>,
    next_stream_id: Cell<StreamId>,
    streams: RefCell<HashMap<StreamId, StreamRef>>,
    active_remote_streams: Cell<u32>,
    active_local_streams: Cell<u32>,
    // reserved local streams, not opened yet
    reserved_streams: Cell<u32>,
    readiness: RefCell<VecDeque<pool::Sender<()>>>,
    // capacity change notification
    on_capacity: Cell<Option<Rc<dyn Fn()>>>,

    rst_count: Cell<u32>,
    streams_count: Cell<u32>,
    pings_count: Cell<u16>,
    last_id: Cell<StreamId>,

    // Local config
    local_config: Cfg<ServiceConfig>,
    // Maximum number of locally initiated streams
    local_max_concurrent_streams: Cell<Option<u32>>,
    // Initial window size of remote initiated streams
    remote_window_sz: Cell<i32>,
    // Max frame size
    remote_frame_size: Cell<u32>,
    // Peer's max header list size
    remote_max_header_list_size: Cell<u32>,
    // Locally reset streams
    local_pending_reset: Pending,
    // protocol level error
    error: Cell<Option<Error<OperationError>>>,
    // connection state flags
    flags: Cell<ConnectionFlags>,

    pool: pool::Pool<()>,
}

impl Connection {
    pub(crate) fn new(
        server: bool,
        io: IoRef,
        codec: Codec,
        config: Cfg<ServiceConfig>,
        secure: bool,
        skip_streams: bool,
        pool: pool::Pool<()>,
    ) -> Self {
        // send preface
        if !server {
            let _ = io.encode_bytes(consts::PREFACE);
        }

        // send setting to the peer
        let settings = config.settings;
        log::debug!("{}: Sending local settings {settings:?}", io.tag());
        io.encode(settings.into(), &codec).unwrap();

        let mut recv_window = Window::new(frame::DEFAULT_INITIAL_WINDOW_SIZE);
        let send_window = Window::new(frame::DEFAULT_INITIAL_WINDOW_SIZE);

        // update connection window size
        if let Some(val) = recv_window.update(
            0,
            config.connection_window_sz,
            config.connection_window_sz_threshold,
        ) {
            log::debug!("{}: Sending connection window update to {val:?}", io.tag());
            io.encode(WindowUpdate::new(StreamId::CON, val).into(), &codec)
                .unwrap();
        }

        if let Some(max) = config.settings.max_header_list_size() {
            codec.set_recv_header_list_size(max as usize);
        }
        codec.set_max_header_continuations(config.max_header_continuations);

        let remote_frame_size = Cell::new(codec.send_frame_size());

        let mut flags = if secure {
            ConnectionFlags::SECURE
        } else {
            ConnectionFlags::empty()
        };
        if server {
            flags.insert(ConnectionFlags::SERVER);
        }
        if !skip_streams {
            flags.insert(ConnectionFlags::UNKNOWN_STREAMS);
        }

        let state = Rc::new(ConnectionState {
            codec,
            remote_frame_size,
            remote_max_header_list_size: Cell::new(u32::MAX),
            pool,
            io: io.clone(),
            send_window: Cell::new(send_window),
            recv_window: Cell::new(recv_window),
            recv_size: Cell::new(0),
            streams: RefCell::new(HashMap::default()),
            active_remote_streams: Cell::new(0),
            active_local_streams: Cell::new(0),
            reserved_streams: Cell::new(0),
            rst_count: Cell::new(0),
            streams_count: Cell::new(0),
            pings_count: Cell::new(0),
            last_id: Cell::new(StreamId::CON),
            readiness: RefCell::new(VecDeque::new()),
            on_capacity: Cell::new(None),
            next_stream_id: Cell::new(StreamId::CLIENT),
            local_config: config,
            local_max_concurrent_streams: Cell::new(Some(consts::DEFAULT_REMOTE_MAX_CONCURRENT_STREAMS)),
            local_pending_reset: Pending::default(),
            remote_window_sz: Cell::new(frame::DEFAULT_INITIAL_WINDOW_SIZE),
            error: Cell::new(None),
            flags: Cell::new(flags),
        });
        let con = Connection(state);

        // start ping/pong
        if con.0.local_config.ping_timeout.non_zero() {
            spawn(ping(con.clone(), con.0.local_config.ping_timeout, io));
        }

        // wait for the local settings acknowledgment
        if con.0.local_config.settings_timeout.non_zero() {
            spawn(settings_timer(con.clone(), con.0.local_config.settings_timeout));
        }

        con
    }

    pub(crate) fn io(&self) -> &IoRef {
        &self.0.io
    }

    pub(crate) fn tag(&self) -> &'static str {
        self.0.io.tag()
    }

    pub(crate) fn codec(&self) -> &Codec {
        &self.0.codec
    }

    pub(crate) fn config(&self) -> &ServiceConfig {
        &self.0.local_config
    }

    pub(crate) fn service(&self) -> &'static str {
        self.0.local_config.service()
    }

    pub(crate) fn flags(&self) -> ConnectionFlags {
        self.0.flags.get()
    }

    pub(crate) fn close(&self) {
        self.0.io.close();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.0.io.is_closed()
    }

    pub(crate) fn is_disconnecting(&self) -> bool {
        if self.is_closed() {
            false
        } else {
            self.0
                .flags
                .get()
                .contains(ConnectionFlags::DISCONNECT_WHEN_READY)
        }
    }

    pub(crate) fn set_secure(&self, secure: bool) {
        if secure {
            self.set_flags(ConnectionFlags::SECURE);
        } else {
            self.unset_flags(ConnectionFlags::SECURE);
        }
    }

    pub(crate) fn set_flags(&self, f: ConnectionFlags) {
        let mut flags = self.0.flags.get();
        flags.insert(f);
        self.0.flags.set(flags);
    }

    pub(crate) fn unset_flags(&self, f: ConnectionFlags) {
        let mut flags = self.0.flags.get();
        flags.remove(f);
        self.0.flags.set(flags);
    }

    pub(crate) fn encode<T>(&self, item: T)
    where
        frame::Frame: From<T>,
    {
        let _ = self.0.io.encode(item.into(), &self.0.codec);
    }

    pub(crate) fn encode_data_frame<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.0.io.with_write_src(f)
    }

    pub(crate) fn check_error(&self) -> Result<(), Error<OperationError>> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err.clone()));
            Err(err)
        } else {
            Ok(())
        }
    }

    pub(crate) fn check_error_with_disconnect(&self) -> Result<(), Error<OperationError>> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err.clone()));
            Err(err)
        } else if self
            .0
            .flags
            .get()
            .contains(ConnectionFlags::DISCONNECT_WHEN_READY)
        {
            Err(Error::new(OperationError::Disconnecting, self.service()))
        } else {
            Ok(())
        }
    }

    /// Consume connection level send capacity (window)
    pub(crate) fn consume_send_window(&self, cap: u32) {
        self.0.send_window.set(self.0.send_window.get().dec(cap));
    }

    /// data received, decrease receive window size
    pub(crate) fn data_received(&self, size: u32) {
        self.0.data_received(size);
    }

    pub(crate) fn data_consumed(&self, size: u32) {
        self.0.data_consumed(size);
    }

    pub(crate) fn send_window_size(&self) -> WindowSize {
        self.0.send_window.get().window_size()
    }

    pub(crate) fn remote_window_size(&self) -> i32 {
        self.0.remote_window_sz.get()
    }

    pub(crate) fn remote_frame_size(&self) -> usize {
        self.0.remote_frame_size.get() as usize
    }

    /// Checks the header list size against the peer's `SETTINGS_MAX_HEADER_LIST_SIZE`.
    pub(crate) fn check_header_list_size(
        &self,
        pseudo: &PseudoHeaders,
        headers: &HeaderMap,
    ) -> Result<(), Error<OperationError>> {
        let max = self.0.remote_max_header_list_size.get() as usize;
        let size = pseudo.header_list_size(headers);
        if size > max {
            Err(Error::new(
                OperationError::HeaderListTooLarge { size, max },
                self.service(),
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn settings_processed(&self) -> bool {
        self.flags().contains(ConnectionFlags::SETTINGS_PROCESSED)
    }

    pub(crate) fn max_streams(&self) -> Option<u32> {
        self.0.local_max_concurrent_streams.get()
    }

    pub(crate) fn active_streams(&self) -> u32 {
        self.0.active_local_streams.get()
    }

    /// Sets a callback that is called when the connection's capacity for local streams changes.
    pub(crate) fn set_on_capacity(&self, f: Option<Rc<dyn Fn()>>) {
        self.0.on_capacity.set(f);
    }

    pub(crate) fn can_create_new_stream(&self) -> bool {
        if let Some(max) = self.0.local_max_concurrent_streams.get() {
            self.0.active_local_streams.get() < max
        } else {
            true
        }
    }

    pub(crate) async fn ready(&self) -> Result<(), Error<OperationError>> {
        loop {
            self.check_error_with_disconnect()?;
            return if let Some(max) = self.0.local_max_concurrent_streams.get() {
                if self.0.active_local_streams.get() < max {
                    Ok(())
                } else {
                    let (tx, rx) = self.0.pool.channel();
                    self.0.readiness.borrow_mut().push_back(tx);
                    let waiter = ReadyWaiter { rx, state: &self.0 };
                    if poll_fn(|cx| waiter.rx.poll_recv(cx)).await.is_ok() {
                        continue;
                    }
                    // waiters are dropped on connection failure or disconnect
                    self.check_error_with_disconnect()?;
                    Err(Error::new(OperationError::Disconnected, self.service()))
                }
            } else {
                Ok(())
            };
        }
    }

    pub(crate) fn disconnect_when_ready(&self) {
        self.0.readiness.borrow_mut().clear();
        if self.0.streams.borrow().is_empty() && self.0.reserved_streams.get() == 0 {
            log::trace!("{}: All streams are closed, disconnecting", self.tag());
            self.0.io.close();
        } else {
            log::trace!("{}: Not all streams are closed, set disconnect flag", self.tag());
            self.set_flags(ConnectionFlags::DISCONNECT_WHEN_READY);
        }
    }

    pub(crate) async fn send_request(
        &self,
        authority: ByteString,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<Stream, Error<OperationError>> {
        self.check_error_with_disconnect()?;

        if !self.can_create_new_stream() {
            log::warn!(
                "{}: Cannot create new stream, waiting for available streams",
                self.tag()
            );
            self.ready().await?;
        }

        self.0
            .active_local_streams
            .set(self.0.active_local_streams.get() + 1);
        let result = self.open_stream(authority, method, path, headers, eof);
        if result.is_err() {
            self.0.release_local_stream();
        }
        result
    }

    /// Reserves a local stream, the stream is counted as active.
    pub(crate) fn reserve_stream(&self) -> bool {
        if self.check_error_with_disconnect().is_err() || !self.can_create_new_stream() {
            false
        } else {
            self.0
                .active_local_streams
                .set(self.0.active_local_streams.get() + 1);
            self.0.reserved_streams.set(self.0.reserved_streams.get() + 1);
            true
        }
    }

    /// Releases a reserved stream that has not been opened.
    pub(crate) fn release_reserved_stream(&self) {
        let reserved = self.0.reserved_streams.get().saturating_sub(1);
        self.0.reserved_streams.set(reserved);
        self.0.release_local_stream();
        self.0.notify_capacity();

        if reserved == 0
            && self.0.streams.borrow().is_empty()
            && self.flags().contains(ConnectionFlags::DISCONNECT_WHEN_READY)
        {
            log::trace!("{}: All streams are closed, disconnecting", self.tag());
            self.0.io.close();
        }
    }

    /// Opens a stream using a reservation.
    ///
    /// Reserved stream can be opened while graceful disconnect is in progress.
    pub(crate) fn send_reserved_request(
        &self,
        authority: ByteString,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<Stream, Error<OperationError>> {
        self.check_error()?;
        if self.is_closed() {
            return Err(Error::new(OperationError::Disconnected, self.service()));
        }
        let stream = self.open_stream(authority, method, path, headers, eof)?;
        self.0
            .reserved_streams
            .set(self.0.reserved_streams.get().saturating_sub(1));
        Ok(stream)
    }

    // stream must be counted in `active_local_streams`
    fn open_stream(
        &self,
        authority: ByteString,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<Stream, Error<OperationError>> {
        // CONNECT omits `:scheme` and `:path` (RFC 9113 §8.5)
        let connect = method == Method::CONNECT;
        let pseudo = PseudoHeaders {
            scheme: (!connect).then(|| {
                if self.0.flags.get().contains(ConnectionFlags::SECURE) {
                    consts::HTTPS_SCHEME
                } else {
                    consts::HTTP_SCHEME
                }
            }),
            method: Some(method),
            authority: Some(authority),
            path: (!connect).then_some(path),
            ..Default::default()
        };
        self.check_header_list_size(&pseudo, &headers)?;

        let stream = {
            let id = self.0.next_stream_id.get();
            let next_id = id
                .next_id()
                .map_err(|_| Error::new(OperationError::OverflowedStreamId, self.service()))?;
            self.0.next_stream_id.set(next_id);
            let stream = StreamRef::new(id, false, self.clone());
            self.0.streams.borrow_mut().insert(id, stream.clone());
            stream
        };
        stream.send_headers(Headers::new(stream.id(), pseudo, headers, eof));
        Ok(stream.into_stream())
    }

    pub(crate) fn drop_stream(&self, id: StreamId) {
        self.0.drop_stream(id);
    }

    pub(crate) fn recv_half(&self) -> RecvHalfConnection {
        RecvHalfConnection(self.0.clone())
    }

    pub(crate) fn pings_count(&self) -> u16 {
        self.0.pings_count.get()
    }

    /// Local capacity timeouts do not count toward the peer's reset limit.
    pub(crate) fn capacity_timeout(&self, id: StreamId) {
        self.0.drop_stream(id);
    }
}

impl ConnectionState {
    /// data received, decrease receive window size
    fn data_received(&self, size: u32) {
        self.recv_window.set(self.recv_window.get().dec(size));
        self.recv_size.set(self.recv_size.get() + size);
    }

    /// data consumed, update connection window size if needed
    fn data_consumed(&self, size: u32) {
        let recv_size = self.recv_size.get() - size;
        self.recv_size.set(recv_size);

        let mut recv_window = self.recv_window.get();
        if let Some(val) = recv_window.update(
            recv_size,
            self.local_config.connection_window_sz,
            self.local_config.connection_window_sz_threshold,
        ) {
            let _ = self
                .io
                .encode(WindowUpdate::new(StreamId::CON, val).into(), &self.codec);
        }
        self.recv_window.set(recv_window);
    }

    fn update_rst_count(&self) -> Result<(), Error<ConnectionError>> {
        let count = self.rst_count.get() + 1;
        let streams_count = self.streams_count.get();
        if streams_count >= 10 && count >= streams_count >> 1 {
            Err(Error::new(
                ConnectionError::StreamResetsLimit,
                self.local_config.service(),
            ))
        } else {
            self.rst_count.set(count);
            Ok(())
        }
    }

    fn err_unknown_streams(&self) -> bool {
        self.flags.get().contains(ConnectionFlags::UNKNOWN_STREAMS)
    }

    /// Checks if the stream id is used already, the stream is closed if it is not in the map
    fn is_used_id(&self, id: StreamId) -> bool {
        if id.is_client_initiated() == self.flags.get().contains(ConnectionFlags::SERVER) {
            id <= self.last_id.get()
        } else {
            id < self.next_stream_id.get()
        }
    }

    fn notify_capacity(&self) {
        if let Some(f) = self.on_capacity.take() {
            self.on_capacity.set(Some(f.clone()));
            f();
        }
    }

    fn release_local_stream(&self) {
        let local = self.active_local_streams.get().saturating_sub(1);
        self.active_local_streams.set(local);

        // wake a waiter for each free slot, woken waiters re-check the limit
        if let Some(max) = self.local_max_concurrent_streams.get() {
            self.wake_waiters(max.saturating_sub(local));
        }
    }

    fn wake_waiters(&self, mut count: u32) {
        let mut readiness = self.readiness.borrow_mut();
        while count > 0
            && let Some(tx) = readiness.pop_front()
        {
            if tx.send(()).is_ok() {
                count -= 1;
            }
        }
    }

    fn drop_stream(&self, id: StreamId) {
        let mut released = false;
        let empty = {
            let mut streams = self.streams.borrow_mut();
            if let Some(stream) = streams.remove(&id) {
                stream.stop_capacity_timer();
                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: Dropping stream {id:?} remote: {:?}",
                    self.io.tag(),
                    stream.is_remote()
                );
                if stream.is_remote() {
                    self.active_remote_streams
                        .set(self.active_remote_streams.get() - 1);
                } else {
                    self.release_local_stream();
                    released = true;
                }
            }
            streams.is_empty()
        };
        if released {
            self.notify_capacity();
        }
        let flags = self.flags.get();

        // Close connection
        if empty
            && self.reserved_streams.get() == 0
            && flags.contains(ConnectionFlags::DISCONNECT_WHEN_READY)
        {
            log::trace!("{}: All streams are closed, disconnecting", self.io.tag());
            self.io.close();
            return;
        }

        // Add ids to pending queue
        if flags.contains(ConnectionFlags::UNKNOWN_STREAMS) {
            self.local_pending_reset.add(id, &self.local_config);
        }
    }
}

impl RecvHalfConnection {
    pub(crate) fn tag(&self) -> &'static str {
        self.0.io.tag()
    }

    fn query(&self, id: StreamId) -> Option<StreamRef> {
        self.0.streams.borrow().get(&id).cloned()
    }

    fn flags(&self) -> ConnectionFlags {
        self.0.flags.get()
    }

    fn set_flags(&self, f: ConnectionFlags) {
        let mut flags = self.0.flags.get();
        flags.insert(f);
        self.0.flags.set(flags);
    }

    pub(crate) fn service(&self) -> &'static str {
        self.0.local_config.service()
    }

    pub(crate) fn encode<T>(&self, item: T)
    where
        frame::Frame: From<T>,
    {
        let _ = self.0.io.encode(item.into(), &self.0.codec);
    }

    pub(crate) fn recv_headers(&self, frm: Headers) -> Result<Option<(StreamRef, Message)>, EitherError> {
        let id = frm.stream_id();
        let is_server = self.0.flags.get().contains(ConnectionFlags::SERVER);

        // 1. Check if ID parity is correct (Client must send odd, Server even)
        if is_server && !id.is_client_initiated() {
            return Err(Either::Left(Error::new(
                ConnectionError::InvalidStreamId("Invalid id in received headers frame"),
                self.service(),
            )));
        }

        // 2. CORRECTION: Check logic priority
        // First we check if it's an ALREADY EXISTING stream.
        // If the stream exists (is OPEN or HALF_CLOSED), we accept the frame regardless of the last_id.
        // This allows intermediate trailers and headers.
        if let Some(stream) = self.query(id) {
            match stream.recv_headers(frm) {
                Ok(item) => return Ok(item.map(move |msg| (stream, msg))),
                Err(kind) => return Err(Either::Right(StreamErrorInner::new(stream, kind))),
            }
        }

        // a stream opened by the client itself, it is closed or idle,
        // `last_id` tracks streams opened by the peer only
        if !is_server && id.is_client_initiated() {
            if id >= self.0.next_stream_id.get() && self.0.err_unknown_streams() {
                return Err(Either::Left(Error::new(
                    ConnectionError::InvalidStreamId(
                        "Invalid id in received headers frame (idle stream)",
                    ),
                    self.service(),
                )));
            }
            // frames sent before the peer saw the reset are ignored (RFC 9113 §5.1)
            if !self.0.local_pending_reset.is_pending(id) {
                self.encode(frame::Reset::new(id, frame::Reason::STREAM_CLOSED));
            }
            return Ok(None);
        }

        // 3. Validation of RFC 5.1.1 for NEW streams
        // If we've arrived here, the stream does NOT exist on our map.
        // Therefore, it must be a newly created stream. The ID must be higher than the last one viewed.
        let last_id = self.0.last_id.get();
        if last_id >= id && self.0.local_pending_reset.is_pending(id) {
            // the stream is reset already, trailers sent before the peer
            // saw the reset must be ignored (RFC 9113 §5.1)
            return Ok(None);
        }
        if last_id >= id {
            // If the ID is old and the stream was not found above, it's an error (Stream Closed or ID Reuse).
            return Err(Either::Left(Error::new(
                ConnectionError::InvalidStreamId(
                    "Invalid id in received headers frame (stream closed or ID reused)",
                ),
                self.service(),
            )));
        }

        // 4. Update last_id (Valid new stream)
        self.0.last_id.set(id);

        // 5. Handle specific client closed/pending cases for new streams
        if !is_server && (!self.0.err_unknown_streams() || self.0.local_pending_reset.is_pending(id)) {
            // if client and no stream, then it was closed
            self.encode(frame::Reset::new(id, frame::Reason::STREAM_CLOSED));
            Ok(None)
        } else {
            // 6. New Stream Validation Logic (Disconnect, Max Concurrency, Pseudo Headers)

            // Refuse all new streams if connection is preparing for disconnect,
            // the peer might not know yet, so refusals are not counted
            if self
                .0
                .flags
                .get()
                .contains(ConnectionFlags::DISCONNECT_WHEN_READY)
            {
                self.encode(frame::Reset::new(id, frame::Reason::REFUSED_STREAM));
                if self.0.err_unknown_streams() {
                    self.0.local_pending_reset.add(id, &self.0.local_config);
                }
                return Ok(None);
            }

            // Max concurrent streams check, exceeding the limit is a stream
            // error (RFC 9113 §5.1.2), refusals count toward the reset limit
            if let Some(max) = self.0.local_config.remote_max_concurrent_streams
                && self.0.active_remote_streams.get() >= max
            {
                log::debug!("{}: refusing {id:?}, max concurrent streams {max}", self.tag());
                self.0.streams_count.set(self.0.streams_count.get() + 1);
                return self.reset_unknown_stream(id, frame::Reason::REFUSED_STREAM);
            }

            // Pseudo-headers validation, a malformed request is
            // a stream error (RFC 9113 §8.1.1)
            let pseudo = frm.pseudo();
            // CONNECT omits `:scheme` and `:path` (RFC 9113 §8.5)
            let connect = pseudo.method == Some(Method::CONNECT);
            let err = if pseudo.method.is_none() {
                Some(StreamError::MissingPseudo("method"))
            } else if pseudo.protocol.is_some() {
                // extended CONNECT is not supported, `SETTINGS_ENABLE_CONNECT_PROTOCOL`
                // is never sent (RFC 8441 §4)
                Some(StreamError::UnexpectedPseudo("protocol"))
            } else if connect && pseudo.authority.as_ref().is_none_or(|s| s.as_str().is_empty()) {
                Some(StreamError::MissingPseudo("authority"))
            } else if connect && pseudo.scheme.is_some() {
                Some(StreamError::UnexpectedPseudo("scheme"))
            } else if connect && pseudo.path.is_some() {
                Some(StreamError::UnexpectedPseudo("path"))
            } else if !connect && pseudo.path.as_ref().is_none_or(|s| s.as_str().is_empty()) {
                Some(StreamError::MissingPseudo("path"))
            } else if !connect && pseudo.scheme.as_ref().is_none_or(|s| s.as_str().is_empty()) {
                Some(StreamError::MissingPseudo("scheme"))
            } else if pseudo.status.is_some() {
                Some(StreamError::UnexpectedPseudo("status"))
            } else {
                None
            };

            if let Some(err) = err {
                log::debug!("{}: malformed request on {id:?}: {err}", self.tag());
                self.0.streams_count.set(self.0.streams_count.get() + 1);
                self.reset_unknown_stream(id, err.reason())
            } else {
                // Create the new stream
                let stream = StreamRef::new(id, true, Connection(self.0.clone()));
                self.0.streams_count.set(self.0.streams_count.get() + 1);
                self.0.streams.borrow_mut().insert(id, stream.clone());
                self.0
                    .active_remote_streams
                    .set(self.0.active_remote_streams.get() + 1);
                match stream.recv_headers(frm) {
                    Ok(item) => Ok(item.map(move |msg| (stream, msg))),
                    Err(kind) => Err(Either::Right(StreamErrorInner::new(stream, kind))),
                }
            }
        }
    }

    /// Handles a frame that is invalid for its stream only, the stream is
    /// reset and the connection continues.
    pub(crate) fn recv_invalid_frame(
        &self,
        frm: frame::InvalidFrame,
    ) -> Result<Option<(StreamRef, Message)>, EitherError> {
        let id = frm.stream_id();
        log::debug!("{}: received invalid frame: {frm:?}", self.tag());

        let err = Error::new(StreamError::InvalidFrame(frm.error()), self.service());
        if let Some(stream) = self.query(id) {
            self.update_rst_count().map_err(Either::Left)?;
            return Err(Either::Right(StreamErrorInner::new(stream, err)));
        }

        // an invalid HEADERS frame still opens and closes the stream
        if frm.kind() == frame::Kind::Headers {
            if self.0.flags.get().contains(ConnectionFlags::SERVER) && !id.is_client_initiated() {
                return Err(Either::Left(Error::new(
                    ConnectionError::InvalidStreamId("Invalid id in received headers frame"),
                    self.service(),
                )));
            }
            if self.0.last_id.get() >= id {
                return Err(Either::Left(Error::new(
                    ConnectionError::InvalidStreamId(
                        "Invalid id in received headers frame (stream closed or ID reused)",
                    ),
                    self.service(),
                )));
            }
            self.0.last_id.set(id);
            self.0.streams_count.set(self.0.streams_count.get() + 1);
        }
        self.reset_unknown_stream(id, err.reason())
    }

    /// Resets a stream that is not tracked by the connection.
    fn reset_unknown_stream(
        &self,
        id: StreamId,
        reason: frame::Reason,
    ) -> Result<Option<(StreamRef, Message)>, EitherError> {
        self.update_rst_count().map_err(Either::Left)?;
        self.encode(frame::Reset::new(id, reason));

        // the peer can still send frames for the stream before it sees the reset
        if self.0.err_unknown_streams() {
            self.0.local_pending_reset.add(id, &self.0.local_config);
        }
        Ok(None)
    }

    pub(crate) fn recv_data(
        &self,
        frm: frame::Data,
    ) -> Result<Option<(StreamRef, Message)>, EitherError> {
        if frm.flow_controlled_len().cast_signed() > self.0.recv_window.get().window_size {
            return Err(Either::Left(Error::new(
                ConnectionError::RecvWindowExceeded,
                self.service(),
            )));
        }

        if let Some(stream) = self.query(frm.stream_id()) {
            match stream.recv_data(frm) {
                Ok(item) => Ok(item.map(move |msg| (stream, msg))),
                Err(kind) => Err(Either::Right(StreamErrorInner::new(stream, kind))),
            }
        } else if self.0.local_pending_reset.is_pending(frm.stream_id()) {
            // the stream is reset already, frames sent before the peer saw
            // the reset must be ignored (RFC 9113 §5.1)
            self.0.data_received(frm.flow_controlled_len());
            self.0.data_consumed(frm.flow_controlled_len());
            Ok(None)
        } else if !self.0.err_unknown_streams() || self.0.is_used_id(frm.stream_id()) {
            // closed stream, the connection level recv window is released
            self.0.data_received(frm.flow_controlled_len());
            self.0.data_consumed(frm.flow_controlled_len());

            self.encode(frame::Reset::new(frm.stream_id(), frame::Reason::STREAM_CLOSED));
            Ok(None)
        } else {
            Err(Either::Left(Error::new(
                ConnectionError::UnknownStream("Received data"),
                self.service(),
            )))
        }
    }

    /// The first frame from the peer must be SETTINGS (RFC 9113 §3.4)
    pub(crate) fn check_first_frame(&self, frame: &frame::Frame) -> Result<(), Error<ConnectionError>> {
        if self.flags().contains(ConnectionFlags::REMOTE_SETTINGS)
            || matches!(frame, frame::Frame::Settings(s) if !s.is_ack())
        {
            Ok(())
        } else {
            proto_err!(conn: "first frame is not SETTINGS: {frame:?}");
            Err(Error::new(ConnectionError::MissingSettings, self.service()))
        }
    }

    pub(crate) fn recv_settings(
        &self,
        settings: frame::Settings,
    ) -> Result<(), Either<Error<ConnectionError>, Vec<StreamErrorInner>>> {
        log::trace!("{}: Processing incoming settings: {settings:#?}", self.tag());

        if settings.is_ack() {
            if self.flags().contains(ConnectionFlags::SETTINGS_PROCESSED) {
                proto_err!(conn: "received unexpected settings ack");
                return Err(Either::Left(Error::new(
                    ConnectionError::UnexpectedSettingsAck,
                    self.service(),
                )));
            }
            self.set_flags(ConnectionFlags::SETTINGS_PROCESSED);
            if let Some(max) = self.0.local_config.settings.max_frame_size() {
                self.0.codec.set_recv_frame_size(max as usize);
            }

            let upd = self.0.local_config.window_sz - frame::DEFAULT_INITIAL_WINDOW_SIZE;

            let mut stream_errors = Vec::new();
            for stream in self.0.streams.borrow().values() {
                let val = match stream.update_recv_window(upd) {
                    Ok(val) => val,
                    Err(e) => {
                        stream_errors.push(StreamErrorInner::new(stream.clone(), e));
                        continue;
                    }
                };
                if let Some(val) = val {
                    // send window size update to the peer
                    self.encode(WindowUpdate::new(stream.id(), val));
                }
            }
            if !stream_errors.is_empty() {
                return Err(Either::Right(stream_errors));
            }
        } else {
            if settings.is_push_enabled() == Some(true)
                && !self.0.flags.get().contains(ConnectionFlags::SERVER)
            {
                proto_err!(conn: "server sent SETTINGS_ENABLE_PUSH=1");
                return Err(Either::Left(Error::new(
                    ConnectionError::UnexpectedEnablePush,
                    self.service(),
                )));
            }
            if let Some(max) = settings.max_header_list_size() {
                self.0.remote_max_header_list_size.set(max);
            }
            if let Some(max) = settings.max_frame_size() {
                self.0.codec.set_send_frame_size(max as usize);
                self.0.remote_frame_size.set(max);
            }
            if let Some(max) = settings.header_table_size() {
                self.0.codec.set_send_header_table_size(max as usize);
            }
            // until the first SETTINGS the peer's limit is assumed, without
            // the setting the limit is removed
            let first = !self.flags().contains(ConnectionFlags::REMOTE_SETTINGS);
            self.set_flags(ConnectionFlags::REMOTE_SETTINGS);
            let max = settings.max_concurrent_streams();
            if max.is_some() || first {
                self.0.local_max_concurrent_streams.set(max);
                for tx in mem::take(&mut *self.0.readiness.borrow_mut()) {
                    let _ = tx.send(());
                }
                self.0.notify_capacity();
            }

            // RFC 7540 §6.9.2
            //
            // In addition to changing the flow-control window for streams that are
            // not yet active, a SETTINGS frame can alter the initial flow-control
            // window size for streams with active flow-control windows (that is,
            // streams in the "open" or "half-closed (remote)" state). When the
            // value of SETTINGS_INITIAL_WINDOW_SIZE changes, a receiver MUST adjust
            // the size of all stream flow-control windows that it maintains by the
            // difference between the new value and the old value.
            //
            // A change to `SETTINGS_INITIAL_WINDOW_SIZE` can cause the available
            // space in a flow-control window to become negative. A sender MUST
            // track the negative flow-control window and MUST NOT send new
            // flow-controlled frames until it receives WINDOW_UPDATE frames that
            // cause the flow-control window to become positive.
            if let Some(val) = settings.initial_window_size() {
                let old_val = self.0.remote_window_sz.get();
                self.0.remote_window_sz.set(val);
                log::trace!("Update remote initial window size to {val} from {old_val}");

                // RFC 9113 §6.9.2, a window overflow caused by the change is
                // a connection error of type FLOW_CONTROL_ERROR
                let upd = val - old_val;
                if upd != 0 {
                    for stream in self.0.streams.borrow().values() {
                        if stream.update_send_window(upd).is_err() {
                            proto_err!(conn: "initial window size overflows {:?}", stream.id());
                            return Err(Either::Left(Error::new(
                                ConnectionError::WindowValueOverflow,
                                self.service(),
                            )));
                        }
                    }
                }
            }

            // Ack settings to the peer once they are applied (RFC 9113 §6.5.3)
            self.encode(frame::Settings::ack());
        }
        Ok(())
    }

    pub(crate) fn recv_window_update(&self, frm: frame::WindowUpdate) -> Result<(), EitherError> {
        log::trace!("{}: processing incoming {:#?}", self.tag(), frm);

        if frm.stream_id().is_zero() {
            if frm.size_increment() == 0 {
                Err(Either::Left(Error::new(
                    ConnectionError::ZeroWindowUpdateValue,
                    self.service(),
                )))
            } else {
                let window = self.0.send_window.get().inc(frm.size_increment()).map_err(|()| {
                    Either::Left(Error::new(ConnectionError::WindowValueOverflow, self.service()))
                })?;
                self.0.send_window.set(window);

                // wake up streams if needed
                for stream in self.0.streams.borrow().values() {
                    stream.recv_window_update_connection();
                }
                Ok(())
            }
        } else if let Some(stream) = self.query(frm.stream_id()) {
            stream
                .recv_window_update(frm)
                .map_err(|kind| Either::Right(StreamErrorInner::new(stream, kind)))
        } else if self.0.local_pending_reset.is_pending(frm.stream_id())
            || self.0.is_used_id(frm.stream_id())
        {
            // late update for a closed stream (RFC 9113 §5.1)
            Ok(())
        } else if self.0.err_unknown_streams() {
            log::trace!("{}: Unknown WINDOW_UPDATE {frm:?}", self.tag());
            Err(Either::Left(Error::new(
                ConnectionError::UnknownStream("WINDOW_UPDATE"),
                self.service(),
            )))
        } else {
            self.encode(frame::Reset::new(frm.stream_id(), frame::Reason::STREAM_CLOSED));
            Ok(())
        }
    }

    pub(crate) fn update_rst_count(&self) -> Result<(), Error<ConnectionError>> {
        self.0.update_rst_count()
    }

    pub(crate) fn recv_rst_stream(&self, frm: frame::Reset) -> Result<(), EitherError> {
        log::trace!("{}: processing incoming {:#?}", self.tag(), frm);

        let id = frm.stream_id();
        if id.is_zero() {
            Err(Either::Left(Error::new(
                ConnectionError::UnknownStream("RST_STREAM-zero"),
                self.service(),
            )))
        } else if let Some(stream) = self.query(id) {
            // the response is complete, the server stops the request body,
            // the final message is already published
            let complete = !stream.is_remote()
                && stream.recv_state().is_closed()
                && frm.reason() == frame::Reason::NO_ERROR;
            stream.recv_rst_stream(frm);
            if complete {
                return Ok(());
            }
            self.update_rst_count().map_err(Either::Left)?;

            Err(Either::Right(StreamErrorInner::new(
                stream,
                Error::new(StreamError::Reset(frm.reason()), self.service()),
            )))
        } else if self.0.local_pending_reset.remove(id) {
            self.update_rst_count().map_err(Either::Left)
        } else {
            // late reset for a stream that is already forgotten
            Ok(())
        }
    }

    pub(crate) fn recv_pong(&self, _: frame::Ping) {
        self.set_flags(ConnectionFlags::RECV_PONG);
    }

    pub(crate) fn recv_go_away(
        &self,
        reason: frame::Reason,
        data: &Bytes,
    ) -> HashMap<StreamId, StreamRef> {
        log::trace!(
            "{}: processing go away with reason: {:?}, data: {:?}",
            self.tag(),
            reason,
            data.slice(..std::cmp::min(data.len(), 20))
        );

        self.0
            .error
            .set(Some(Error::new(ConnectionError::GoAway(reason), self.service())));
        self.0.readiness.borrow_mut().clear();

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_go_away(reason);
        }
        self.0.notify_capacity();
        streams
    }

    pub(crate) fn ping_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.timeout(ConnectionError::KeepaliveTimeout, frame::Reason::NO_ERROR)
    }

    pub(crate) fn read_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.timeout(ConnectionError::ReadTimeout, frame::Reason::NO_ERROR)
    }

    pub(crate) fn settings_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.timeout(ConnectionError::SettingsTimeout, frame::Reason::SETTINGS_TIMEOUT)
    }

    pub(crate) fn is_settings_timeout(&self) -> bool {
        self.flags().contains(ConnectionFlags::SETTINGS_TIMEOUT)
    }

    fn timeout(&self, err: ConnectionError, reason: frame::Reason) -> HashMap<StreamId, StreamRef> {
        let err: Error<OperationError> = Error::new(err, self.service());
        self.0.error.set(Some(err.clone()));
        self.0.readiness.borrow_mut().clear();

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_failed_stream(err.clone());
        }

        self.encode(frame::GoAway::new(reason));
        self.0.io.close();
        self.0.notify_capacity();
        streams
    }

    pub(crate) fn proto_error(&self, err: &Error<ConnectionError>) -> HashMap<StreamId, StreamRef> {
        let err = err.clone().map(OperationError::from);
        self.0.error.set(Some(err.clone()));
        self.0.readiness.borrow_mut().clear();

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in &mut streams.values() {
            stream.set_failed_stream(err.clone());
        }
        self.0.notify_capacity();
        streams
    }

    pub(crate) fn disconnect(&self) -> HashMap<StreamId, StreamRef> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err));
        } else {
            self.0
                .error
                .set(Some(Error::new(OperationError::Disconnected, self.service())));
        }
        self.0.readiness.borrow_mut().clear();

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_failed_stream(Error::new(OperationError::Disconnected, self.service()));
        }
        self.0.notify_capacity();
        streams
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("Connection");
        builder
            .field("io", &self.0.io)
            .field("codec", &self.0.codec)
            .field("recv_window", &self.0.recv_window.get())
            .field("send_window", &self.0.send_window.get())
            .field("settings_processed", &self.settings_processed())
            .field("next_stream_id", &self.0.next_stream_id.get())
            .field("local_config", &self.0.local_config)
            .field(
                "local_max_concurrent_streams",
                &self.0.local_max_concurrent_streams.get(),
            )
            .field("remote_window_sz", &self.0.remote_window_sz.get())
            .field("remote_frame_size", &self.0.remote_frame_size.get())
            .field("flags", &self.0.flags.get())
            .field("error", &self.check_error())
            .finish()
    }
}

async fn ping(st: Connection, timeout: time::Seconds, io: IoRef) {
    let mut counter: u64 = 0;
    let keepalive: time::Millis = time::Millis::from(timeout) + time::Millis(100);

    log::debug!(
        "{}: start http client ping/pong task, ka: {keepalive:?}",
        io.tag()
    );

    st.set_flags(ConnectionFlags::RECV_PONG);
    loop {
        if st.is_closed() {
            log::trace!(
                "{}: http client connection is closed, stopping keep-alive task",
                st.tag()
            );
            break;
        }
        sleep(keepalive).await;
        if st.is_closed() {
            break;
        }
        if !st.0.flags.get().contains(ConnectionFlags::RECV_PONG) {
            io.notify_timeout();
            break;
        }

        counter += 1;
        st.unset_flags(ConnectionFlags::RECV_PONG);
        st.encode(frame::Ping::new(counter.to_be_bytes()));
        st.0.pings_count.set(st.0.pings_count.get() + 1);
    }
}

/// Closes the connection if the peer does not acknowledge the local settings in time.
async fn settings_timer(st: Connection, timeout: time::Seconds) {
    sleep(timeout).await;
    if !st.is_closed() && !st.settings_processed() {
        st.set_flags(ConnectionFlags::SETTINGS_TIMEOUT);
        st.0.io.notify_timeout();
    }
}

/// Passes the wake up to the next waiter if the woken waiter is dropped.
struct ReadyWaiter<'a> {
    rx: pool::Receiver<()>,
    state: &'a ConnectionState,
}

impl Drop for ReadyWaiter<'_> {
    fn drop(&mut self) {
        let mut cx = Context::from_waker(Waker::noop());
        if let Poll::Ready(Ok(())) = self.rx.poll_recv(&mut cx) {
            self.state.wake_waiters(1);
        }
    }
}

struct Pending(Cell<Option<Box<PendingInner>>>);

struct PendingInner {
    ids: HashSet<StreamId>,
    queue: VecDeque<(StreamId, Instant)>,
}

impl Default for Pending {
    fn default() -> Self {
        Self(Cell::new(Some(Box::new(PendingInner {
            ids: HashSet::default(),
            queue: VecDeque::with_capacity(16),
        }))))
    }
}

impl Pending {
    fn add(&self, id: StreamId, config: &ServiceConfig) {
        let mut inner = self.0.take().unwrap();

        let current_time = now();

        // remove old ids
        if let Some(max_time) = current_time.checked_sub(config.reset_duration) {
            while let Some(item) = inner.queue.front() {
                if item.1 < max_time {
                    inner.ids.remove(&item.0);
                    inner.queue.pop_front();
                } else {
                    break;
                }
            }
        }

        // shrink size of ids
        while inner.queue.len() >= config.reset_max {
            if let Some((id, _)) = inner.queue.pop_front() {
                inner.ids.remove(&id);
            }
        }

        inner.ids.insert(id);
        inner.queue.push_back((id, current_time));
        self.0.set(Some(inner));
    }

    fn remove(&self, id: StreamId) -> bool {
        let mut inner = self.0.take().unwrap();
        let removed = inner.ids.remove(&id);
        if removed {
            for idx in 0..inner.queue.len() {
                if inner.queue[idx].0 == id {
                    inner.queue.remove(idx);
                    break;
                }
            }
        }
        self.0.set(Some(inner));
        removed
    }

    fn is_pending(&self, id: StreamId) -> bool {
        let inner = self.0.take().unwrap();
        let pending = inner.ids.contains(&id);
        self.0.set(Some(inner));
        pending
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ntex::http::{HeaderMap, Method, test, uri::Scheme};
    use ntex::service::{Service, fn_service};
    use ntex::time::{Millis, Seconds, sleep};
    use ntex::{Pipeline, SharedCfg, io::Io, util::Bytes};

    use crate::{self as h2, Codec, ServiceConfig, frame, frame::Reason};

    const PREFACE: [u8; 24] = *b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    #[allow(clippy::needless_pass_by_value)]
    fn get_reset(frm: frame::Frame) -> frame::Reset {
        match frm {
            frame::Frame::Reset(rst) => rst,
            _ => panic!("Expect Reset frame: {frm:?}"),
        }
    }

    fn goaway(frm: frame::Frame) -> frame::GoAway {
        match frm {
            frame::Frame::GoAway(f) => f,
            _ => panic!("Expect Reset frame: {frm:?}"),
        }
    }

    #[test]
    fn test_pending_reset_time_underflow() {
        let pending = super::Pending::default();
        let mut config = ServiceConfig::new();
        config.reset_duration = Duration::MAX;
        let id = frame::StreamId::CLIENT;

        pending.add(id, &config);

        assert!(pending.is_pending(id));
    }

    #[ntex::test]
    async fn test_remote_stream_refused() {
        let srv = test::server_with_config(
            async |()| {
                fn_service(async move |io: Io<_>| {
                    let _ = h2::server::handle_one(
                        io.into(),
                        Pipeline::new((), async move |msg: h2::Message| {
                            msg.stream().reset(Reason::REFUSED_STREAM);
                            Ok::<_, h2::StreamError>(())
                        }),
                        Pipeline::new(
                            (),
                            fn_service(async move |msg: h2::Control<h2::StreamError>| {
                                Ok::<_, ()>(msg.ack())
                            })
                            .map_err(|()| unreachable!()),
                        )
                        .bind(),
                    )
                    .await;

                    Ok::<_, ()>(())
                })
            },
            SharedCfg::new("SRV").add(ServiceConfig::new().set_ping_timeout(Seconds::ZERO)),
        );

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let client = h2::client::SimpleClient::new(io, Scheme::HTTP, "localhost".into());
        sleep(Millis(150)).await;

        let (stream, recv_stream) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let msg = recv_stream.recv().await.unwrap();
        assert!(matches!(msg.kind(), h2::MessageKind::Eof(_)));

        let res = stream.send_payload(Bytes::from_static(b"hello"), false).await;
        assert!(res.is_err());

        let con = &recv_stream.stream().0.con.0;
        assert!(con.streams.borrow().is_empty());
    }

    /// A sender waiting for capacity in its own task continues when SETTINGS
    /// increases the initial window size.
    #[ntex::test]
    async fn test_settings_initial_window_wakes_sender() {
        let srv = test::server(async |()| {
            fn_service(async move |io: Io<_>| {
                let codec = Codec::default();
                let mut preface = [0; 24];
                io.read_exact(&mut preface).await.unwrap();
                assert_eq!(preface, PREFACE);

                let mut settings = frame::Settings::default();
                settings.set_initial_window_size(Some(1));
                io.send(settings.into(), &codec).await.unwrap();

                while let Ok(Some(frm)) = io.recv(&codec).await {
                    if let frame::Frame::Data(data) = frm {
                        let id = data.stream_id();
                        if data.payload().as_ref() == b"t" {
                            // grow the window with SETTINGS only
                            let mut settings = frame::Settings::default();
                            settings.set_initial_window_size(Some(65_535));
                            io.send(settings.into(), &codec).await.unwrap();
                        } else if data.is_end_stream() {
                            let hdrs = frame::Headers::new(
                                id,
                                frame::PseudoHeaders::response(ntex::http::StatusCode::OK),
                                HeaderMap::new(),
                                true,
                            );
                            io.send(hdrs.into(), &codec).await.unwrap();
                        }
                    }
                }
                Ok::<_, ()>(())
            })
        });

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let client = h2::client::SimpleClient::new(io, Scheme::HTTP, "localhost".into());

        // wait for the server settings
        sleep(Millis(150)).await;

        let (snd, rcv) = client
            .send(Method::POST, "/".into(), HeaderMap::new(), false)
            .await
            .unwrap();
        let start = std::time::Instant::now();
        snd.send_payload(Bytes::from_static(b"test body"), true)
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "sender is not woken"
        );

        let msg = rcv.recv().await.unwrap();
        assert!(matches!(msg.kind(), h2::MessageKind::Headers { .. }), "{msg:?}");
    }

    /// Until the peer's SETTINGS arrive, the client assumes a limit of
    /// 100 concurrent streams.
    #[ntex::test]
    async fn test_assumed_max_concurrent_streams() {
        let srv = test::server(async |()| {
            fn_service(async move |io: Io<_>| {
                let codec = Codec::default();
                let mut preface = [0; 24];
                io.read_exact(&mut preface).await.unwrap();

                let mut streams = 0;
                while let Ok(Some(frm)) = io.recv(&codec).await {
                    if let frame::Frame::Headers(_) = frm {
                        streams += 1;
                        if streams == 100 {
                            // settings without a concurrency limit
                            sleep(Millis(250)).await;
                            io.send(frame::Settings::default().into(), &codec).await.unwrap();
                        }
                    }
                }
                Ok::<_, ()>(())
            })
        });

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let client = h2::client::SimpleClient::new(io, Scheme::HTTP, "localhost".into());
        assert_eq!(client.max_streams(), Some(100));

        let mut streams = Vec::new();
        for _ in 0..100 {
            streams.push(
                client
                    .send(Method::GET, "/".into(), HeaderMap::new(), true)
                    .await
                    .unwrap(),
            );
        }
        assert!(!client.is_ready());

        // waits for the peer's settings
        let start = std::time::Instant::now();
        let stream = ntex::time::timeout(
            Millis(2_000),
            client.send(Method::GET, "/".into(), HeaderMap::new(), true),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(100));
        streams.push(stream);

        assert_eq!(client.max_streams(), None);
        assert!(client.is_ready());
    }

    /// A client must treat `SETTINGS_ENABLE_PUSH=1` as a connection error.
    #[ntex::test]
    async fn test_client_rejects_enable_push() {
        use std::sync::{Arc, Mutex};

        let goaway = Arc::new(Mutex::new(None));
        let goaway2 = goaway.clone();
        let srv = test::server(async move |()| {
            let goaway = goaway2.clone();
            fn_service(move |io: Io<_>| {
                let goaway = goaway.clone();
                async move {
                    let codec = Codec::default();
                    let mut preface = [0; 24];
                    io.read_exact(&mut preface).await.unwrap();
                    let mut settings = frame::Settings::default();
                    settings.set_enable_push(true);
                    io.send(settings.into(), &codec).await.unwrap();

                    while let Ok(Some(frm)) = io.recv(&codec).await {
                        if let frame::Frame::GoAway(frm) = frm {
                            *goaway.lock().unwrap() = Some((frm.reason(), frm.data().clone()));
                        }
                    }
                    Ok::<_, ()>(())
                }
            })
        });

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let client = h2::client::SimpleClient::new(io, Scheme::HTTP, "localhost".into());
        sleep(Millis(150)).await;

        assert_eq!(
            goaway.lock().unwrap().take(),
            Some((Reason::PROTOCOL_ERROR, Bytes::from_static(b"Server enabled push")))
        );
        assert!(
            client
                .send(Method::GET, "/".into(), HeaderMap::new(), true)
                .await
                .is_err()
        );
    }

    /// Requests over the peer's `SETTINGS_MAX_HEADER_LIST_SIZE` fail locally.
    #[ntex::test]
    async fn test_client_max_header_list_size() {
        let srv = test::server(async |()| {
            fn_service(async move |io: Io<_>| {
                let codec = Codec::default();
                let mut preface = [0; 24];
                io.read_exact(&mut preface).await.unwrap();
                let mut settings = frame::Settings::default();
                settings.set_max_header_list_size(Some(200));
                io.send(settings.into(), &codec).await.unwrap();

                while let Ok(Some(frm)) = io.recv(&codec).await {
                    if let frame::Frame::Headers(hdrs) = frm {
                        assert_eq!(hdrs.stream_id(), frame::StreamId::CLIENT);
                        let pseudo = frame::PseudoHeaders::response(ntex::http::StatusCode::OK);
                        let hdrs = frame::Headers::new(hdrs.stream_id(), pseudo, HeaderMap::new(), true);
                        io.send(hdrs.into(), &codec).await.unwrap();
                    }
                }
                Ok::<_, ()>(())
            })
        });

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let client = h2::client::SimpleClient::new(io, Scheme::HTTP, "localhost".into());
        sleep(Millis(150)).await;

        let mut large = HeaderMap::new();
        large.insert(
            ntex::http::header::USER_AGENT,
            ntex::http::header::HeaderValue::from_static(concat!(
                "0123456789012345678901234567890123456789012345678901234567890123456789",
                "0123456789012345678901234567890123456789012345678901234567890123456789",
            )),
        );
        let err = client
            .send(Method::GET, "/".into(), large, true)
            .await
            .err()
            .unwrap();
        assert!(
            matches!(*err, h2::OperationError::HeaderListTooLarge { max: 200, .. }),
            "{err:?}"
        );

        // the stream id is not used, the next request gets the first id
        let (_snd, rcv) = client
            .send(Method::GET, "/".into(), HeaderMap::new(), true)
            .await
            .unwrap();
        let msg = rcv.recv().await.unwrap();
        assert!(matches!(msg.kind(), h2::MessageKind::Headers { .. }), "{msg:?}");
    }

    /// Responses and trailers over the peer's `SETTINGS_MAX_HEADER_LIST_SIZE` fail locally.
    #[ntex::test]
    async fn test_server_max_header_list_size() {
        use std::sync::{Arc, Mutex};

        let results = Arc::new(Mutex::new(Vec::new()));
        let results2 = results.clone();
        let srv = test::server(async move |()| {
            let results = results2.clone();
            fn_service(move |io: Io<_>| {
                let results = results.clone();
                async move {
                    let _ = h2::server::handle_one(
                        io.into(),
                        Pipeline::new((), async move |msg: h2::Message| {
                            if let h2::MessageKind::Headers { .. } = msg.kind {
                                let mut large = HeaderMap::new();
                                large.insert(
                                    ntex::http::header::SERVER,
                                    ntex::http::header::HeaderValue::from_static(concat!(
                                        "01234567890123456789012345678901234567890123456789",
                                        "01234567890123456789012345678901234567890123456789",
                                    )),
                                );
                                let st = ntex::http::StatusCode::OK;
                                let mut res = results.lock().unwrap();
                                res.push(msg.stream.send_response(st, large.clone(), false).is_ok());
                                res.push(msg.stream.send_response(st, HeaderMap::new(), false).is_ok());
                                res.push(msg.stream.send_trailers(large).is_ok());
                                res.push(msg.stream.send_trailers(HeaderMap::new()).is_ok());
                            }
                            Ok::<_, h2::StreamError>(())
                        }),
                        Pipeline::new(
                            (),
                            fn_service(async move |msg: h2::Control<h2::StreamError>| {
                                Ok::<_, ()>(msg.ack())
                            })
                            .map_err(|()| unreachable!()),
                        )
                        .bind(),
                    )
                    .await;
                    Ok::<_, ()>(())
                }
            })
        });

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
        let mut settings = frame::Settings::default();
        settings.set_max_header_list_size(Some(120));
        io.encode(settings.into(), &codec).unwrap();

        let pseudo = frame::PseudoHeaders {
            method: Some(Method::GET),
            scheme: Some("https".into()),
            authority: Some("localhost".into()),
            path: Some("/".into()),
            ..Default::default()
        };
        let hdrs = frame::Headers::new(frame::StreamId::CLIENT, pseudo, HeaderMap::new(), true);
        io.send(hdrs.into(), &codec).await.unwrap();

        let mut headers = 0;
        loop {
            match io.recv(&codec).await.unwrap().unwrap() {
                frame::Frame::Headers(hdrs) => {
                    headers += 1;
                    if hdrs.is_end_stream() {
                        break;
                    }
                }
                frame::Frame::Settings(s) if !s.is_ack() => {
                    io.encode(frame::Settings::ack().into(), &codec).unwrap();
                }
                frame::Frame::Reset(rst) => panic!("unexpected reset: {rst:?}"),
                _ => {}
            }
        }
        assert_eq!(headers, 2);
        assert_eq!(*results.lock().unwrap(), [false, true, false, true]);
    }

    /// A malformed response is a stream error, the connection stays usable.
    #[ntex::test]
    async fn test_malformed_response_pseudo() {
        let srv = test::server(async |()| {
            fn_service(async move |io: Io<_>| {
                let codec = Codec::default();
                let mut preface = [0; 24];
                io.read_exact(&mut preface).await.unwrap();
                io.send(frame::Settings::default().into(), &codec).await.unwrap();

                while let Ok(Some(frm)) = io.recv(&codec).await {
                    match frm {
                        frame::Frame::Headers(hdrs) => {
                            let id = hdrs.stream_id();
                            let pseudo = match hdrs.pseudo().path.as_deref() {
                                Some("/no-status") => frame::PseudoHeaders::default(),
                                Some("/path") => frame::PseudoHeaders {
                                    status: Some(ntex::http::StatusCode::OK),
                                    path: Some("/".into()),
                                    ..Default::default()
                                },
                                _ => frame::PseudoHeaders::response(ntex::http::StatusCode::OK),
                            };
                            let hdrs = frame::Headers::new(id, pseudo, HeaderMap::new(), true);
                            io.send(hdrs.into(), &codec).await.unwrap();
                        }
                        frame::Frame::Reset(rst) => {
                            assert_eq!(rst.reason(), Reason::PROTOCOL_ERROR);
                        }
                        _ => {}
                    }
                }
                Ok::<_, ()>(())
            })
        });

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let client = h2::client::SimpleClient::new(io, Scheme::HTTP, "localhost".into());
        sleep(Millis(150)).await;

        for (path, expected) in [
            ("/no-status", Some(h2::StreamError::MissingPseudo("status"))),
            ("/path", Some(h2::StreamError::UnexpectedPseudo("path"))),
            ("/", None),
        ] {
            let (_snd, rcv) = client
                .send(Method::GET, path.into(), HeaderMap::new(), true)
                .await
                .unwrap();
            let msg = rcv.recv().await.unwrap();
            match (msg.kind, expected) {
                (h2::MessageKind::Eof(h2::StreamEof::Error(err)), Some(expected)) => {
                    assert_eq!(*err, expected);
                }
                (h2::MessageKind::Headers { pseudo, .. }, None) => {
                    assert_eq!(pseudo.status, Some(ntex::http::StatusCode::OK));
                }
                (kind, _) => panic!("unexpected message for {path}: {kind:?}"),
            }
        }
    }

    /// Streams reset by the local side get the final message, if the
    /// receive side is still open.
    #[ntex::test]
    async fn test_local_reset_publishes_eof() {
        use std::sync::{Arc, Mutex};

        let events = Arc::new(Mutex::new(Vec::new()));
        let events2 = events.clone();
        let srv = test::server_with_config(
            async move |()| {
                let events = events2.clone();
                fn_service(move |io: Io<_>| {
                    let events = events.clone();
                    async move {
                        let _ = h2::server::handle_one(
                            io.into(),
                            Pipeline::new((), async move |msg: h2::Message| {
                                match msg.kind {
                                    h2::MessageKind::Headers { pseudo, .. } => {
                                        if pseudo.path.as_deref() == Some("/reset") {
                                            msg.stream.reset(Reason::CANCEL);
                                        } else {
                                            msg.stream
                                                .send_response(
                                                    ntex::http::StatusCode::OK,
                                                    HeaderMap::default(),
                                                    false,
                                                )
                                                .unwrap();
                                            let _ = msg.stream.send_payload("data", true).await;
                                        }
                                    }
                                    h2::MessageKind::Eof(h2::StreamEof::Error(err)) => {
                                        events.lock().unwrap().push((msg.stream.id(), *err));
                                    }
                                    _ => {}
                                }
                                Ok::<_, h2::StreamError>(())
                            }),
                            Pipeline::new(
                                (),
                                fn_service(async move |msg: h2::Control<h2::StreamError>| {
                                    Ok::<_, ()>(msg.ack())
                                })
                                .map_err(|()| unreachable!()),
                            )
                            .bind(),
                        )
                        .await;

                        Ok::<_, ()>(())
                    }
                })
            },
            SharedCfg::new("SRV").add(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_capacity_timeout(Seconds(1)),
            ),
        );

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
        // no send window for the server
        let mut settings = frame::Settings::default();
        settings.set_initial_window_size(Some(0));
        io.encode(settings.into(), &codec).unwrap();

        let open = async |id, path: &'static str| {
            let pseudo = frame::PseudoHeaders {
                method: Some(Method::POST),
                scheme: Some("HTTPS".into()),
                authority: Some("localhost".into()),
                path: Some(path.into()),
                ..Default::default()
            };
            let hdrs = frame::Headers::new(id, pseudo, HeaderMap::new(), false);
            io.send(hdrs.into(), &codec).await.unwrap();
            loop {
                match io.recv(&codec).await.unwrap().unwrap() {
                    frame::Frame::Reset(rst) => return rst,
                    frame::Frame::Settings(s) if !s.is_ack() => {
                        io.encode(frame::Settings::ack().into(), &codec).unwrap();
                    }
                    _ => {}
                }
            }
        };

        let id = frame::StreamId::CLIENT;
        let rst = open(id, "/reset").await;
        assert_eq!(rst.reason(), Reason::CANCEL);

        let id2 = id.next_id().unwrap();
        let rst = open(id2, "/timeout").await;
        assert_eq!(rst.reason(), Reason::CANCEL);

        sleep(Millis(100)).await;
        let events = events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                (id, h2::StreamError::LocalReset(Reason::CANCEL)),
                (id2, h2::StreamError::CapacityTimeout)
            ]
        );
    }

    /// During shutdown all new streams are refused, without hitting the
    /// reset limit.
    #[ntex::test]
    async fn test_shutdown_refuses_new_streams() {
        let srv = test::server_with_config(
            async |()| {
                fn_service(async move |io: Io<_>| {
                    let _ = h2::server::handle_one(
                        io.into(),
                        Pipeline::new((), async move |_: h2::Message| {
                            ServiceConfig::shutdown();
                            sleep(Millis(10_000)).await;
                            Ok::<_, h2::StreamError>(())
                        }),
                        Pipeline::new(
                            (),
                            fn_service(async move |msg: h2::Control<h2::StreamError>| {
                                Ok::<_, ()>(msg.ack())
                            })
                            .map_err(|()| unreachable!()),
                        )
                        .bind(),
                    )
                    .await;

                    Ok::<_, ()>(())
                })
            },
            SharedCfg::new("SRV").add(ServiceConfig::new().set_ping_timeout(Seconds::ZERO)),
        );

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
        io.encode(frame::Settings::default().into(), &codec).unwrap();

        // settings & window
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;

        let pseudo = frame::PseudoHeaders {
            method: Some(Method::POST),
            scheme: Some("HTTPS".into()),
            authority: Some("localhost".into()),
            path: Some("/".into()),
            ..Default::default()
        };
        let mut id = frame::StreamId::CLIENT;
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
        io.send(hdrs.into(), &codec).await.unwrap();
        sleep(Millis(100)).await;

        for _ in 0..32 {
            id = id.next_id().unwrap();
            let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
            io.send(hdrs.into(), &codec).await.unwrap();
            assert_eq!(
                io.recv(&codec).await.unwrap().unwrap(),
                frame::Frame::Reset(frame::Reset::new(id, Reason::REFUSED_STREAM))
            );
            // data sent before the reset is seen is ignored
            io.send(frame::Data::new(id, Bytes::from_static(b"data")).into(), &codec)
                .await
                .unwrap();
        }

        io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
        match io.recv(&codec).await.unwrap().unwrap() {
            frame::Frame::Ping(ping) => assert!(ping.is_ack()),
            frm => panic!("unexpected frame: {frm:?}"),
        }
    }

    #[ntex::test]
    async fn test_delay_reset_queue() {
        let srv = test::server_with_config(
            async |()| {
                fn_service(async move |io: Io<_>| {
                    let _ = h2::server::handle_one(
                        io.into(),
                        Pipeline::new((), async move |msg: h2::Message| {
                            msg.stream().reset(Reason::NO_ERROR);
                            Ok::<_, h2::StreamError>(())
                        }),
                        Pipeline::new(
                            (),
                            fn_service(async move |msg: h2::Control<h2::StreamError>| {
                                Ok::<_, ()>(msg.ack())
                            })
                            .map_err(|()| unreachable!()),
                        )
                        .bind(),
                    )
                    .await;

                    Ok::<_, ()>(())
                })
            },
            SharedCfg::new("SRV").add(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_reset_stream_duration(Seconds(1)),
            ),
        );

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr.clone()).await.unwrap();
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));

        let settings = frame::Settings::default();
        io.encode(settings.into(), &codec).unwrap();

        // settings & window
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;

        let id = frame::StreamId::CLIENT;
        let pseudo = frame::PseudoHeaders {
            method: Some(Method::GET),
            scheme: Some("HTTPS".into()),
            authority: Some("localhost".into()),
            path: Some("/".into()),
            ..Default::default()
        };
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
        io.send(hdrs.into(), &codec).await.unwrap();

        // server resets stream
        let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::NO_ERROR);

        // server should keep reseted streams for some time,
        // data for such streams is ignored
        let pl = frame::Data::new(id, Bytes::from_static(b"data"));
        io.send(pl.clone().into(), &codec).await.unwrap();
        io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
        let res = io.recv(&codec).await.unwrap().unwrap();
        assert!(
            matches!(res, frame::Frame::Ping(ping) if ping.is_ack()),
            "{res:?}"
        );

        // reset queue cleared in 1 sec (for test)
        sleep(Millis(1100)).await;

        let id2 = id.next_id().unwrap();
        let hdrs = frame::Headers::new(id2, pseudo.clone(), HeaderMap::new(), false);
        io.send(hdrs.into(), &codec).await.unwrap();
        let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::NO_ERROR);

        // prev closed stream, stream error only
        io.send(pl.into(), &codec).await.unwrap();
        let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.stream_id(), id);
        assert_eq!(res.reason(), Reason::STREAM_CLOSED);

        // late window update for the closed stream is ignored
        io.send(frame::WindowUpdate::new(id, 10).into(), &codec)
            .await
            .unwrap();
        io.send(frame::Ping::new([2; 8]).into(), &codec).await.unwrap();
        let res = io.recv(&codec).await.unwrap().unwrap();
        assert!(
            matches!(res, frame::Frame::Ping(ping) if ping.is_ack()),
            "{res:?}"
        );

        // idle stream
        let pl = frame::Data::new(101.into(), Bytes::from_static(b"data"));
        io.send(pl.into(), &codec).await.unwrap();
        let res = goaway(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::PROTOCOL_ERROR);

        // SECOND connection
        let io = ntex::connect::connect(addr).await.unwrap();
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));

        let settings = frame::Settings::default();
        io.encode(settings.into(), &codec).unwrap();

        // settings & window
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;

        let id = frame::StreamId::CLIENT;
        let pseudo = frame::PseudoHeaders {
            method: Some(Method::GET),
            scheme: Some("HTTPS".into()),
            authority: Some("localhost".into()),
            path: Some("/".into()),
            ..Default::default()
        };
        let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
        io.send(hdrs.into(), &codec).await.unwrap();

        // server resets stream
        let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::NO_ERROR);

        // after server receives remote reset, next frame is a stream error
        io.send(frame::Reset::new(id, Reason::NO_ERROR).into(), &codec)
            .await
            .unwrap();

        let pl = frame::Data::new(id, Bytes::from_static(b"data"));
        io.send(pl.clone().into(), &codec).await.unwrap();
        let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::STREAM_CLOSED);
    }

    /// Late frames for closed streams that are purged from the reset queue
    /// are not connection errors (F5).
    #[ntex::test]
    async fn test_late_frames_for_forgotten_streams() {
        let srv = test::server_with_config(
            async |()| {
                fn_service(async move |io: Io<_>| {
                    let _ = h2::server::handle_one(
                        io.into(),
                        Pipeline::new((), async move |msg: h2::Message| {
                            if matches!(msg.kind(), h2::MessageKind::Headers { .. }) {
                                msg.stream().reset(Reason::NO_ERROR);
                            }
                            Ok::<_, h2::StreamError>(())
                        }),
                        Pipeline::new(
                            (),
                            fn_service(async move |msg: h2::Control<h2::StreamError>| {
                                Ok::<_, ()>(msg.ack())
                            })
                            .map_err(|()| unreachable!()),
                        )
                        .bind(),
                    )
                    .await;
                    Ok::<_, ()>(())
                })
            },
            SharedCfg::new("SRV").add(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_max_concurrent_reset_streams(1),
            ),
        );

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let codec = Codec::default();
        let _ = io.with_write_src(|buf| buf.extend_from_slice(&PREFACE));
        io.encode(frame::Settings::default().into(), &codec).unwrap();
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;
        let _ = io.recv(&codec).await;

        let pseudo = frame::PseudoHeaders {
            method: Some(Method::GET),
            scheme: Some("HTTPS".into()),
            authority: Some("localhost".into()),
            path: Some("/".into()),
            ..Default::default()
        };

        // stream 1 and 3 are reset, stream 1 is purged from the reset queue
        let id1 = frame::StreamId::CLIENT;
        let id3 = id1.next_id().unwrap();
        for id in [id1, id3] {
            let hdrs = frame::Headers::new(id, pseudo.clone(), HeaderMap::new(), false);
            io.send(hdrs.into(), &codec).await.unwrap();
            let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
            assert_eq!(res.reason(), Reason::NO_ERROR);
        }

        // trailers for the reset stream are ignored
        let hdrs = frame::Headers::trailers(id3, HeaderMap::new());
        io.send(hdrs.into(), &codec).await.unwrap();

        // late window update is ignored
        io.send(frame::WindowUpdate::new(id1, 10).into(), &codec)
            .await
            .unwrap();
        io.send(frame::Ping::new([1; 8]).into(), &codec).await.unwrap();
        let res = io.recv(&codec).await.unwrap().unwrap();
        assert!(
            matches!(res, frame::Frame::Ping(ping) if ping.is_ack()),
            "{res:?}"
        );

        // late data is a stream error
        let pl = frame::Data::new(id1, Bytes::from_static(b"data"));
        io.send(pl.into(), &codec).await.unwrap();
        let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.stream_id(), id1);
        assert_eq!(res.reason(), Reason::STREAM_CLOSED);
        io.send(frame::Ping::new([2; 8]).into(), &codec).await.unwrap();
        let res = io.recv(&codec).await.unwrap().unwrap();
        assert!(
            matches!(res, frame::Frame::Ping(ping) if ping.is_ack()),
            "{res:?}"
        );
    }

    async fn zero_window_client() -> (h2::client::SimpleClient, ntex::io::testing::IoTest) {
        let (io, srv) = ntex::io::testing::IoTest::create();
        srv.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("CLI")
            .add(ServiceConfig::new().set_capacity_timeout(Seconds(1)))
            .build();
        let client = h2::client::SimpleClient::new(Io::new(io, cfg), Scheme::HTTP, "localhost".into());

        // peer sets zero stream window and never updates it
        srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0]);
        sleep(Millis(50)).await;
        (client, srv)
    }

    #[ntex::test]
    async fn test_capacity_timer_stopped_on_close() {
        let (client, _srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();

        // pending capacity wait starts the timer
        let waiter = stream.stream().clone();
        let fut = ntex::rt::spawn(async move { waiter.send_capacity().await });
        sleep(Millis(50)).await;
        assert!(crate::timer::is_registered(stream.stream()));

        // closing the stream releases the timer reference
        assert!(stream.reset(Reason::CANCEL));
        assert!(!crate::timer::is_registered(stream.stream()));
        assert!(fut.await.unwrap().is_err());
    }

    #[ntex::test]
    async fn test_capacity_timer_stopped_on_failure() {
        let (client, _srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();

        let waiter = stream.stream().clone();
        let fut = ntex::rt::spawn(async move { waiter.send_capacity().await });
        sleep(Millis(50)).await;
        assert!(crate::timer::is_registered(stream.stream()));

        // connection failure drains the streams map
        stream.stream().0.con.recv_half().disconnect();
        assert!(!crate::timer::is_registered(stream.stream()));
        assert!(fut.await.unwrap().is_err());
    }

    #[ntex::test]
    async fn test_dropped_capacity_wait_stops_timer() {
        let (client, srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();

        // the waiter gives up before the capacity timeout
        assert!(
            ntex::time::timeout(Millis(100), stream.send_capacity())
                .await
                .is_err()
        );
        assert!(!crate::timer::is_registered(stream.stream()));

        // the stream is not reset after the capacity timeout
        sleep(Millis(2500)).await;
        srv.write([0, 0, 4, 8, 0, 0, 0, 0, 1, 0, 0, 0, 4]);
        sleep(Millis(50)).await;
        stream.send_payload("test", true).await.unwrap();
    }

    #[ntex::test]
    async fn test_rst_stream_for_unknown_stream_ignored() {
        let (client, srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let id = stream.id();
        let con = stream.stream().0.con.clone();

        // local reset keeps the id in the pending list
        assert!(stream.reset(Reason::CANCEL));
        assert!(con.0.local_pending_reset.is_pending(id));

        // peer reset removes the id from the pending list
        srv.write([0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8]);
        sleep(Millis(50)).await;
        assert!(!con.0.local_pending_reset.is_pending(id));

        // late resets for forgotten and unknown streams are ignored
        srv.write([0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8]);
        srv.write([0, 0, 4, 3, 0, 0, 0, 0, 9, 0, 0, 0, 8]);
        sleep(Millis(50)).await;
        assert!(!client.is_closed());
        client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
    }

    /// Sum of connection level window updates sent by the client
    fn conn_window_updates(srv: &ntex::io::testing::IoTest, codec: &Codec) -> i32 {
        use ntex_codec::Decoder;

        let mut buf = ntex::util::BytesMut::from(&srv.read_any()[..]);
        let mut size = 0;
        while let Some(frm) = codec.decode(&mut buf).unwrap() {
            if let frame::Frame::WindowUpdate(upd) = frm
                && upd.stream_id().is_zero()
            {
                size += upd.size_increment();
            }
        }
        size
    }

    #[ntex::test]
    async fn test_connection_window_released_on_consume() {
        let (io, srv) = ntex::io::testing::IoTest::create();
        srv.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("CLI")
            .add(ServiceConfig::new().set_initial_connection_window_size(100_000))
            .build();
        let client = h2::client::SimpleClient::new(Io::new(io, cfg), Scheme::HTTP, "localhost".into());
        srv.write([0, 0, 0, 4, 0, 0, 0, 0, 0]);
        sleep(Millis(50)).await;

        let (_stream, recv) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        sleep(Millis(50)).await;

        // skip preface, settings and the initial connection window update
        let codec = Codec::default();
        let _ = srv.read_any();

        // response headers and 30000 bytes of data
        srv.write([0, 0, 1, 1, 4, 0, 0, 0, 1, 0x88]);
        for _ in 0..2 {
            srv.write([0, 0x3a, 0x98, 0, 0, 0, 0, 0, 1]);
            srv.write(vec![0; 15_000]);
        }

        let _hdrs = recv.recv().await.unwrap();
        let msg1 = recv.recv().await.unwrap();
        let msg2 = recv.recv().await.unwrap();
        assert!(matches!(msg2.kind(), h2::MessageKind::Data(..)));
        sleep(Millis(50)).await;

        // unconsumed data does not replenish the connection window
        assert_eq!(conn_window_updates(&srv, &codec), 0);

        // consumed data does
        drop(msg1);
        drop(msg2);
        sleep(Millis(50)).await;
        assert_eq!(conn_window_updates(&srv, &codec), 30_000);
    }

    /// Returns stream 1 window updates, consumes the data received by the client.
    async fn stream_window_updates(reset: bool) -> i32 {
        use ntex_codec::Decoder;

        let (io, srv) = ntex::io::testing::IoTest::create();
        srv.remote_buffer_cap(1024 * 1024);
        let client = h2::client::SimpleClient::new(
            Io::new(io, SharedCfg::default()),
            Scheme::HTTP,
            "localhost".into(),
        );
        srv.write([0, 0, 0, 4, 0, 0, 0, 0, 0]);
        sleep(Millis(50)).await;

        let (_stream, recv) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        sleep(Millis(50)).await;
        let _ = srv.read_any();

        // response headers and 30000 bytes of data
        srv.write([0, 0, 1, 1, 4, 0, 0, 0, 1, 0x88]);
        for _ in 0..2 {
            srv.write([0, 0x3a, 0x98, 0, 0, 0, 0, 0, 1]);
            srv.write(vec![0; 15_000]);
        }
        let _hdrs = recv.recv().await.unwrap();
        let msg1 = recv.recv().await.unwrap();
        let msg2 = recv.recv().await.unwrap();
        assert!(matches!(msg2.kind(), h2::MessageKind::Data(..)));

        if reset {
            // RST_STREAM(CANCEL)
            srv.write([0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8]);
        }
        sleep(Millis(50)).await;
        let _ = srv.read_any();

        drop(msg1);
        drop(msg2);
        sleep(Millis(50)).await;

        let codec = Codec::default();
        let mut buf = ntex::util::BytesMut::from(&srv.read_any()[..]);
        let mut size = 0;
        while let Some(frm) = codec.decode(&mut buf).unwrap() {
            if let frame::Frame::WindowUpdate(upd) = frm
                && upd.stream_id() == frame::StreamId::CLIENT
            {
                size += upd.size_increment();
            }
        }
        size
    }

    #[ntex::test]
    async fn test_no_window_update_for_closed_stream() {
        assert!(stream_window_updates(false).await > 0);
        assert_eq!(stream_window_updates(true).await, 0);
    }

    #[ntex::test]
    async fn test_capacity_timeout_not_counted_as_reset() {
        use ntex_codec::Decoder;

        let (client, srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let con = stream.stream().0.con.clone();

        assert!(matches!(
            &*stream.send_capacity().await.unwrap_err(),
            h2::OperationError::Stream(h2::StreamError::CapacityTimeout)
        ));
        assert_eq!(con.0.rst_count.get(), 0);

        // the stream is cancelled
        sleep(Millis(50)).await;
        let codec = Codec::default();
        let mut buf = ntex::util::BytesMut::from(&srv.read_any()[PREFACE.len()..]);
        let rst = loop {
            if let frame::Frame::Reset(rst) = codec.decode(&mut buf).unwrap().unwrap() {
                break rst;
            }
        };
        assert_eq!(rst.stream_id(), stream.id());
        assert_eq!(rst.reason(), Reason::CANCEL);
    }

    /// SETTINGS frame with `SETTINGS_INITIAL_WINDOW_SIZE`
    fn initial_window_frame(window: u32) -> Vec<u8> {
        let mut frm = vec![0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4];
        frm.extend_from_slice(&window.to_be_bytes());
        frm
    }

    fn window_update_frame(id: u32, inc: u32) -> Vec<u8> {
        let mut frm = vec![0, 0, 4, 8, 0];
        frm.extend_from_slice(&id.to_be_bytes());
        frm.extend_from_slice(&inc.to_be_bytes());
        frm
    }

    /// Client without capacity timeout, the peer sets the initial stream window
    async fn window_client(window: u32) -> (h2::client::SimpleClient, ntex::io::testing::IoTest) {
        window_client_with(window, Seconds::ZERO).await
    }

    async fn window_client_with(
        window: u32,
        capacity_timeout: Seconds,
    ) -> (h2::client::SimpleClient, ntex::io::testing::IoTest) {
        let (io, srv) = ntex::io::testing::IoTest::create();
        srv.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("CLI")
            .add(ServiceConfig::new().set_capacity_timeout(capacity_timeout))
            .build();
        let client = h2::client::SimpleClient::new(Io::new(io, cfg), Scheme::HTTP, "localhost".into());
        srv.write(initial_window_frame(window));
        sleep(Millis(50)).await;
        (client, srv)
    }

    /// Decodes the frames written by the client since the last call
    fn client_frames(srv: &ntex::io::testing::IoTest, codec: &Codec) -> Vec<frame::Frame> {
        use ntex_codec::Decoder;

        let data = srv.read_any();
        let data = data.strip_prefix(&PREFACE[..]).unwrap_or(&data);
        let mut buf = ntex::util::BytesMut::from(data);
        let mut frames = Vec::new();
        while let Some(frm) = codec.decode(&mut buf).unwrap() {
            frames.push(frm);
        }
        assert!(buf.is_empty());
        frames
    }

    /// Sizes and `END_STREAM` flags of the DATA frames written by the client
    fn client_data(srv: &ntex::io::testing::IoTest, codec: &Codec) -> Vec<(usize, bool)> {
        client_frames(srv, codec)
            .into_iter()
            .filter_map(|frm| match frm {
                frame::Frame::Data(data) => Some((data.payload().len(), data.is_end_stream())),
                _ => None,
            })
            .collect()
    }

    /// Sum of the window updates written by the client for the stream `id`
    fn client_window_updates(srv: &ntex::io::testing::IoTest, codec: &Codec, id: frame::StreamId) -> i32 {
        client_frames(srv, codec)
            .into_iter()
            .filter_map(|frm| match frm {
                frame::Frame::WindowUpdate(upd) if upd.stream_id() == id => Some(upd.size_increment()),
                _ => None,
            })
            .sum()
    }

    /// Payload is split by the stream send window, the sender waits for
    /// window updates.
    #[ntex::test]
    async fn test_send_limited_by_stream_window() {
        let (client, srv) = window_client(10).await;
        let codec = Codec::default();
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        assert_eq!(stream.stream().available_send_capacity(), 10);
        let _ = client_frames(&srv, &codec);

        let s = stream.stream().clone();
        let fut = ntex::rt::spawn(async move { s.send_payload(Bytes::from(vec![b'x'; 25]), true).await });
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(10, false)]);
        assert_eq!(stream.stream().available_send_capacity(), 0);

        srv.write(window_update_frame(1, 10));
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(10, false)]);
        assert_eq!(stream.stream().available_send_capacity(), 0);

        srv.write(window_update_frame(1, 100));
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(5, true)]);
        assert!(fut.await.unwrap().is_ok());
        assert_eq!(stream.stream().available_send_capacity(), 95);
    }

    /// Payload is limited by the connection send window, stream window
    /// updates do not release data while the connection window is exhausted.
    #[ntex::test]
    async fn test_send_limited_by_connection_window() {
        let (client, srv) = window_client(1_000_000).await;
        let codec = Codec::default();
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let _ = client_frames(&srv, &codec);

        // the default connection window is 65535 bytes
        let conn_window = frame::DEFAULT_INITIAL_WINDOW_SIZE as usize;
        assert_eq!(stream.stream().available_send_capacity() as usize, conn_window);

        let s = stream.stream().clone();
        let fut =
            ntex::rt::spawn(async move { s.send_payload(Bytes::from(vec![b'x'; 70_000]), true).await });
        sleep(Millis(50)).await;
        let frames = client_data(&srv, &codec);
        assert!(
            frames
                .iter()
                .all(|(size, eof)| *size <= frame::DEFAULT_MAX_FRAME_SIZE as usize && !eof)
        );
        assert_eq!(frames.iter().map(|(size, _)| size).sum::<usize>(), conn_window);
        assert_eq!(stream.stream().available_send_capacity(), 0);

        // stream window update does not help
        srv.write(window_update_frame(1, 1000));
        sleep(Millis(50)).await;
        assert!(client_data(&srv, &codec).is_empty());
        assert_eq!(stream.stream().available_send_capacity(), 0);

        // connection window update releases the rest
        srv.write(window_update_frame(0, 10_000));
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(70_000 - conn_window, true)]);
        assert!(fut.await.unwrap().is_ok());
        assert_eq!(
            stream.stream().available_send_capacity() as usize,
            10_000 - (70_000 - conn_window)
        );
    }

    /// `SETTINGS_INITIAL_WINDOW_SIZE` changes adjust open stream windows, the
    /// window can become negative (RFC 9113 §6.9.2).
    #[ntex::test]
    async fn test_send_window_adjusted_by_settings() {
        let (client, srv) = window_client(10).await;
        let codec = Codec::default();
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        stream
            .send_payload(Bytes::from(vec![b'x'; 10]), false)
            .await
            .unwrap();
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(10, false)]);

        // window is -5
        srv.write(initial_window_frame(5));
        sleep(Millis(50)).await;
        assert_eq!(stream.stream().available_send_capacity(), 0);

        let s = stream.stream().clone();
        let fut = ntex::rt::spawn(async move { s.send_payload(Bytes::from(vec![b'x'; 3]), true).await });

        // window is 0
        srv.write(window_update_frame(1, 5));
        sleep(Millis(50)).await;
        assert!(client_data(&srv, &codec).is_empty());
        assert_eq!(stream.stream().available_send_capacity(), 0);

        // window is 2
        srv.write(window_update_frame(1, 2));
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(2, false)]);

        // settings grow the window by 15 and wake the sender
        srv.write(initial_window_frame(20));
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(1, true)]);
        assert!(fut.await.unwrap().is_ok());
        assert_eq!(stream.stream().available_send_capacity(), 14);

        // new streams use the current initial window
        let (stream2, _recv2) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        assert_eq!(stream2.stream().available_send_capacity(), 20);
    }

    /// Empty payload without eof sends nothing and does not wait for capacity.
    #[ntex::test]
    async fn test_send_empty_payload_zero_window() {
        let (client, srv) = window_client(0).await;
        let codec = Codec::default();
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let _ = client_frames(&srv, &codec);

        let res = ntex::time::timeout(Millis(500), stream.send_payload(Bytes::new(), false)).await;
        assert!(res.unwrap().is_ok());

        srv.write(window_update_frame(1, 10));
        sleep(Millis(50)).await;
        stream.send_payload(Bytes::new(), false).await.unwrap();
        sleep(Millis(50)).await;
        assert!(client_data(&srv, &codec).is_empty());
        assert_eq!(stream.stream().available_send_capacity(), 10);

        // empty payload with eof ends the stream regardless of the window
        stream.send_payload(Bytes::new(), true).await.unwrap();
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(0, true)]);
    }

    /// Stream window updates do not extend the capacity timeout while the
    /// connection window is exhausted.
    #[ntex::test]
    async fn test_stream_updates_do_not_extend_capacity_timeout() {
        let (client, srv) = window_client_with(1_000_000, Seconds(1)).await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();

        // exhaust the default connection window
        let conn_window = frame::DEFAULT_INITIAL_WINDOW_SIZE as usize;
        stream
            .send_payload(Bytes::from(vec![b'x'; conn_window]), false)
            .await
            .unwrap();
        assert_eq!(stream.stream().available_send_capacity(), 0);

        let s = stream.stream().clone();
        let waiter = ntex::rt::spawn(async move { s.send_payload("x", true).await });

        // the peer trickles stream window updates only
        for _ in 0..10 {
            sleep(Millis(300)).await;
            srv.write(window_update_frame(1, 1));
        }
        let res = ntex::time::timeout(Millis(100), waiter).await;
        let res = res.expect("capacity timeout is extended").unwrap();
        assert!(matches!(
            &*res.unwrap_err(),
            h2::OperationError::Stream(h2::StreamError::CapacityTimeout)
        ));
    }

    /// Capacity waiters fail once the send side is closed.
    #[ntex::test]
    async fn test_send_capacity_after_send_close() {
        let (client, srv) = window_client(3).await;
        let codec = Codec::default();

        // closed with zero window, a waiting sender is woken
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        stream.send_payload("abc", false).await.unwrap();
        let s = stream.stream().clone();
        let waiter = ntex::rt::spawn(async move { s.send_capacity().await });
        sleep(Millis(50)).await;
        stream.send_trailers(HeaderMap::default()).unwrap();
        let res = ntex::time::timeout(Millis(500), waiter).await.unwrap().unwrap();
        assert!(matches!(&*res.unwrap_err(), h2::OperationError::Closed(None)));

        // closed with available window
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        stream.send_payload("a", true).await.unwrap();
        assert_eq!(stream.stream().available_send_capacity(), 2);
        assert!(matches!(
            &*stream.send_capacity().await.unwrap_err(),
            h2::OperationError::Closed(None)
        ));

        // no data after END_STREAM
        sleep(Millis(50)).await;
        let data = client_data(&srv, &codec);
        assert_eq!(data.last(), Some(&(1, true)));
    }

    /// Received data capacity is released on consume, stream window updates
    /// are sent once the released size reaches the threshold.
    #[ntex::test]
    async fn test_recv_capacity_consume() {
        let (client, srv) = window_client(65_535).await;
        let codec = Codec::default();
        let (_stream, recv) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        sleep(Millis(50)).await;
        let _ = client_frames(&srv, &codec);

        // response headers and 30000 bytes of data
        srv.write([0, 0, 1, 1, 4, 0, 0, 0, 1, 0x88]);
        for _ in 0..2 {
            srv.write([0, 0x3a, 0x98, 0, 0, 0, 0, 0, 1]);
            srv.write(vec![0; 15_000]);
        }
        let _hdrs = recv.recv().await.unwrap();
        let mut caps = Vec::new();
        for _ in 0..2 {
            match recv.recv().await.unwrap().kind {
                h2::MessageKind::Data(data, cap) => {
                    assert_eq!(data.len(), 15_000);
                    assert_eq!(cap.size(), 15_000);
                    caps.push(cap);
                }
                kind => panic!("unexpected message: {kind:?}"),
            }
        }

        // capacities of the same stream can be combined
        let mut cap = recv.stream().empty_capacity();
        assert_eq!(cap.size(), 0);
        cap += caps.pop().unwrap();
        let cap = cap + caps.pop().unwrap();
        assert_eq!(cap.size(), 30_000);

        // over-consume panics without releasing capacity
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cap.consume(30_001)));
        assert!(res.is_err());
        assert_eq!(cap.size(), 30_000);

        // 10000 bytes are below the update threshold
        let id = frame::StreamId::CLIENT;
        cap.consume(10_000);
        assert_eq!(cap.size(), 20_000);
        sleep(Millis(50)).await;
        assert_eq!(client_window_updates(&srv, &codec, id), 0);

        // 25000 bytes reach the threshold
        cap.consume(15_000);
        sleep(Millis(50)).await;
        assert_eq!(client_window_updates(&srv, &codec, id), 25_000);

        // remaining 5000 bytes are released on drop, below the threshold
        drop(cap);
        sleep(Millis(50)).await;
        assert_eq!(client_window_updates(&srv, &codec, id), 0);

        // the window is replenished, the peer can send the full window
        let mut frm = vec![0, 0x3f, 0xff, 0, 0, 0, 0, 0, 1];
        frm.resize(9 + 16_383, 0);
        for _ in 0..3 {
            srv.write(frm.clone());
        }
        for _ in 0..3 {
            assert!(matches!(
                recv.recv().await.unwrap().kind,
                h2::MessageKind::Data(..)
            ));
        }
        assert!(!client.is_closed());
    }
}
