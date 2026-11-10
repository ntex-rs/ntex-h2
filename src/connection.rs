use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};
use std::{cell::Cell, cell::RefCell, fmt, io, mem};
use std::{collections::VecDeque, future::poll_fn, time::Instant};

use ntex_bytes::{BytePages, ByteString, Bytes};
use ntex_error::Error;
use ntex_http::{HeaderMap, Method};
use ntex_io::IoRef;
use ntex_service::cfg::Cfg;
use ntex_util::time::now;
use ntex_util::{HashMap, HashSet, channel::pool, future::Either};

use crate::error::{ConnectionError, OperationError, StreamError, StreamErrorInner};
use crate::frame::{self, Headers, PseudoHeaders, StreamId, WindowSize, WindowUpdate};
use crate::stream::{Stream, StreamRef};
use crate::{codec::Codec, config::ServiceConfig, consts, message::Message, timer, window::Window};

pub(crate) type EitherError = Either<Error<ConnectionError>, StreamErrorInner>;

/// Smallest blocked stream queue that is checked for closed streams.
const MIN_BLOCKED_COMPACTION: usize = 32;

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

pub(crate) struct ConnectionState {
    io: IoRef,
    codec: Codec,
    send_window: Cell<Window>,
    recv_window: Cell<Window>,
    // received but not yet consumed data
    recv_size: Cell<u32>,
    next_stream_id: Cell<StreamId>,
    streams: RefCell<HashMap<StreamId, StreamRef>>,
    // most streams open since the connection was last idle
    streams_peak: Cell<usize>,
    // streams waiting for connection send capacity, in arrival order
    blocked: RefCell<VecDeque<StreamId>>,
    // connection send capacity promised to woken streams that have not run yet
    claimed: Cell<WindowSize>,
    // number of received connection WINDOW_UPDATE frames
    send_window_updates: Cell<u32>,
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
    // payload of the last keep-alive ping
    ping_payload: Cell<[u8; 8]>,
    // shared timer registration of settings and keep-alive deadlines
    timer_id: Cell<u64>,
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
    // reason of the received GOAWAY frame
    go_away: Cell<Option<frame::Reason>>,
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
            io,
            send_window: Cell::new(send_window),
            recv_window: Cell::new(recv_window),
            recv_size: Cell::new(0),
            streams: RefCell::new(HashMap::default()),
            streams_peak: Cell::new(0),
            blocked: RefCell::new(VecDeque::new()),
            claimed: Cell::new(0),
            send_window_updates: Cell::new(0),
            active_remote_streams: Cell::new(0),
            active_local_streams: Cell::new(0),
            reserved_streams: Cell::new(0),
            rst_count: Cell::new(0),
            streams_count: Cell::new(0),
            pings_count: Cell::new(0),
            ping_payload: Cell::new([0; 8]),
            timer_id: Cell::new(0),
            last_id: Cell::new(StreamId::CON),
            readiness: RefCell::new(VecDeque::new()),
            on_capacity: Cell::new(None),
            next_stream_id: Cell::new(StreamId::CLIENT),
            local_config: config,
            local_max_concurrent_streams: Cell::new(Some(consts::DEFAULT_REMOTE_MAX_CONCURRENT_STREAMS)),
            local_pending_reset: Pending::default(),
            remote_window_sz: Cell::new(frame::DEFAULT_INITIAL_WINDOW_SIZE),
            error: Cell::new(None),
            go_away: Cell::new(None),
            flags: Cell::new(flags),
        });
        let con = Connection(state);

        // keep-alive pings and the local settings acknowledgment deadline
        // run on the shared per-thread timer
        con.set_flags(ConnectionFlags::RECV_PONG);
        con.0.timer_id.set(timer::register_connection(
            Rc::downgrade(&con.0),
            con.0.local_config.settings_timeout,
            con.0.local_config.ping_timeout,
        ));

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

    /// Releases codec state and reset history after the protocol dispatcher
    /// has stopped, handles that outlive the connection keep only small state.
    pub(crate) fn release(&self) {
        timer::unregister_connection(self.0.timer_id.replace(0));
        self.0.codec.release();
        self.0.local_pending_reset.release();
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

    /// Encodes a HEADERS frame without taking ownership of the header fields.
    pub(crate) fn encode_headers_ref(
        &self,
        id: frame::StreamId,
        pseudo: frame::PseudoHeaders,
        fields: &HeaderMap,
        eof: bool,
    ) {
        let _ = self
            .0
            .io
            .with_write_src(|buf| self.0.codec.encode_headers_ref(id, pseudo, fields, eof, buf));
    }

    pub(crate) fn encode_data_frame<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.0.io.with_write_src(f)
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &pool::Pool<()> {
        &self.0.pool
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
        } else if let Some(reason) = self.0.go_away.get() {
            Err(Error::new(ConnectionError::GoAway(reason), self.service()))
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

    /// Returns the connection send capacity not promised to woken streams.
    pub(crate) fn unclaimed_send_window(&self) -> WindowSize {
        self.0.unclaimed_send_window()
    }

    /// Returns the number of received connection `WINDOW_UPDATE` frames.
    pub(crate) fn send_window_updates(&self) -> u32 {
        self.0.send_window_updates.get()
    }

    /// Queues a stream that waits for connection send capacity.
    pub(crate) fn enqueue_blocked(&self, id: StreamId) {
        self.0.enqueue_blocked(id);
    }

    /// Wakes queued streams while unclaimed connection send capacity is left.
    pub(crate) fn wake_blocked(&self) {
        self.0.wake_blocked();
    }

    /// Returns connection send capacity promised to a stream.
    pub(crate) fn release_claim(&self, claim: WindowSize) {
        self.0.release_claim(claim);
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

    /// Returns the last stream initiated by the peer, for `GOAWAY` frames.
    pub(crate) fn last_stream_id(&self) -> StreamId {
        self.0.last_id.get()
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

    /// Starts graceful shutdown, new streams are refused and the connection
    /// closes after all streams are closed.
    pub(crate) fn disconnect_when_ready(&self) {
        self.0.readiness.borrow_mut().clear();

        // the client learns which of its streams are processed (RFC 9113 §6.8),
        // a failed connection has sent GOAWAY already. client does not send
        // GOAWAY, push is not supported and the server would close the
        // connection before reserved streams are opened
        let flags = self.flags();
        if flags.contains(ConnectionFlags::SERVER)
            && !flags.contains(ConnectionFlags::DISCONNECT_WHEN_READY)
            && !self.0.io.is_closed()
            && self.check_error().is_ok()
        {
            self.encode(
                frame::GoAway::new(frame::Reason::NO_ERROR).set_last_stream_id(self.0.last_id.get()),
            );
        }
        self.set_flags(ConnectionFlags::DISCONNECT_WHEN_READY);

        if self.0.streams.borrow().is_empty() && self.0.reserved_streams.get() == 0 {
            log::trace!("{}: All streams are closed, disconnecting", self.tag());
            self.0.io.close();
        } else {
            log::trace!("{}: Not all streams are closed, set disconnect flag", self.tag());
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
        // the peer does not process new streams after GOAWAY
        if let Some(reason) = self.0.go_away.get() {
            return Err(Error::new(ConnectionError::GoAway(reason), self.service()));
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
        // old history decays, a long-lived connection cannot bank
        // allowance for a burst of resets
        let mut count = self.rst_count.get();
        let mut streams_count = self.streams_count.get();
        while streams_count >= consts::RESET_RATIO_WINDOW {
            streams_count >>= 1;
            count >>= 1;
        }
        self.streams_count.set(streams_count);
        self.rst_count.set(count);

        let count = count + 1;
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

    fn unclaimed_send_window(&self) -> WindowSize {
        self.send_window
            .get()
            .window_size()
            .saturating_sub(self.claimed.get())
    }

    fn enqueue_blocked(&self, id: StreamId) {
        let mut blocked = self.blocked.borrow_mut();

        // ids of closed streams are skipped when dequeued, drop them before
        // they outnumber the open streams
        if blocked.len() >= MIN_BLOCKED_COMPACTION
            && let Ok(streams) = self.streams.try_borrow()
            && blocked.len() >= streams.len() * 2
        {
            blocked.retain(|id| streams.contains_key(id));
        }
        blocked.push_back(id);
    }

    /// Wakes queued streams in arrival order, each woken stream claims
    /// up to its stream window of the unclaimed connection send capacity.
    ///
    /// Waking only as many senders as the capacity can serve avoids waking
    /// every blocked sender when a small window update arrives.
    fn wake_blocked(&self) {
        let mut budget = self.unclaimed_send_window();
        while budget > 0 {
            let Some(id) = self.blocked.borrow_mut().pop_front() else {
                break;
            };
            let stream = self.streams.borrow().get(&id).cloned();
            if let Some(stream) = stream {
                let claim = stream.wake_from_queue(budget);
                self.claimed.set(self.claimed.get() + claim);
                budget -= claim;
            }
        }
    }

    fn release_claim(&self, claim: WindowSize) {
        // claims are dropped with the streams on connection failure
        self.claimed.set(self.claimed.get().saturating_sub(claim));
    }

    fn drop_stream(&self, id: StreamId) {
        let mut released = false;
        let mut removed = false;
        let mut claim = 0;
        let empty = {
            let mut streams = self.streams.borrow_mut();
            self.streams_peak.set(self.streams_peak.get().max(streams.len()));
            if let Some(stream) = streams.remove(&id) {
                removed = true;
                claim = stream.take_claim();
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
            let empty = streams.is_empty();
            // free the memory of a burst once the connection is idle
            if empty && self.streams_peak.replace(0) > consts::IDLE_STREAMS_MAX {
                *streams = HashMap::default();
            }
            empty
        };
        if released {
            self.notify_capacity();
        }
        // pass the capacity promised to the dropped stream to queued streams
        if claim > 0 {
            self.release_claim(claim);
            self.wake_blocked();
        }
        let flags = self.flags.get();

        // Close connection, streams failed by a connection error are already
        // removed, the connection error handler closes the connection
        if removed
            && empty
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

    /// Returns the last stream initiated by the peer, for `GOAWAY` frames.
    pub(crate) fn last_stream_id(&self) -> StreamId {
        self.0.last_id.get()
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

        // 1. Check if ID parity is correct, a server can open streams only
        // with PUSH_PROMISE, push is never enabled (RFC 9113 §5.1.1, §8.4)
        if !id.is_client_initiated() {
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

        // 5. New Stream Validation Logic (Disconnect, Max Concurrency, Pseudo Headers)

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

        // PRIORITY can be sent for any stream, resetting an idle
        // stream is a connection error for the peer (RFC 9113 §5.1)
        if frm.kind() == frame::Kind::Priority {
            return Ok(None);
        }

        // an invalid HEADERS frame still opens and closes the stream
        if frm.kind() == frame::Kind::Headers {
            if !id.is_client_initiated() {
                return Err(Either::Left(Error::new(
                    ConnectionError::InvalidStreamId("Invalid id in received headers frame"),
                    self.service(),
                )));
            }
            // `last_id` tracks streams opened by the peer only
            if !self.0.flags.get().contains(ConnectionFlags::SERVER) {
                if self.0.local_pending_reset.is_pending(id) {
                    return Ok(None);
                }
                return self.reset_unknown_stream(id, err.reason());
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
            timer::settings_acked(self.0.timer_id.get());
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
                let original = self.0.send_window.get();
                let window = original.inc(frm.size_increment()).map_err(|()| {
                    Either::Left(Error::new(ConnectionError::WindowValueOverflow, self.service()))
                })?;
                self.0.send_window.set(window);
                self.0
                    .send_window_updates
                    .set(self.0.send_window_updates.get().wrapping_add(1));

                // wake queued senders in arrival order, as many as the new
                // capacity can serve
                if original.window_size() < window.window_size() {
                    self.0.wake_blocked();
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

    pub(crate) fn recv_pong(&self, ping: &frame::Ping) {
        // only the ack for the last keep-alive ping proves the peer is alive
        if *ping.payload() == self.0.ping_payload.get() {
            self.set_flags(ConnectionFlags::RECV_PONG);
        } else {
            log::trace!("{}: unexpected ping ack {:?}", self.tag(), ping.payload());
        }
    }

    /// Handles a received GOAWAY frame (RFC 9113 §6.8).
    ///
    /// Locally initiated streams above `last_stream_id` were not processed
    /// by the peer and are failed, and new streams are refused. Remaining
    /// streams complete normally, the connection closes after that.
    pub(crate) fn recv_go_away(
        &self,
        reason: frame::Reason,
        last_stream_id: StreamId,
        data: &Bytes,
    ) -> HashMap<StreamId, StreamRef> {
        log::trace!(
            "{}: processing go away with reason: {:?}, last stream: {:?}, data: {:?}",
            self.tag(),
            reason,
            last_stream_id,
            data.slice(..std::cmp::min(data.len(), 20))
        );

        self.0.go_away.set(Some(reason));

        let streams: HashMap<_, _> = self
            .0
            .streams
            .borrow()
            .iter()
            .filter(|(id, stream)| !stream.is_remote() && **id > last_stream_id)
            .map(|(id, stream)| (*id, stream.clone()))
            .collect();

        // failing a stream removes it from the connection
        for stream in streams.values() {
            stream.set_go_away(reason);
        }
        Connection(self.0.clone()).disconnect_when_ready();
        self.0.notify_capacity();
        streams
    }

    pub(crate) fn ping_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.timeout(ConnectionError::KeepaliveTimeout, frame::Reason::NO_ERROR)
    }

    pub(crate) fn read_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.timeout(ConnectionError::ReadTimeout, frame::Reason::NO_ERROR)
    }

    pub(crate) fn write_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.timeout(ConnectionError::WriteTimeout, frame::Reason::NO_ERROR)
    }

    pub(crate) fn settings_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.timeout(ConnectionError::SettingsTimeout, frame::Reason::SETTINGS_TIMEOUT)
    }

    pub(crate) fn is_settings_timeout(&self) -> bool {
        self.flags().contains(ConnectionFlags::SETTINGS_TIMEOUT)
    }

    /// Removes all streams, taken streams are not released by `drop_stream`
    /// so the active stream counters are reset here. Reservations stay
    /// counted until they are released.
    fn take_streams(&self) -> HashMap<StreamId, StreamRef> {
        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        self.0.streams_peak.set(0);
        self.0.blocked.take();
        self.0.claimed.set(0);
        self.0.active_remote_streams.set(0);
        self.0.active_local_streams.set(self.0.reserved_streams.get());
        streams
    }

    fn timeout(&self, err: ConnectionError, reason: frame::Reason) -> HashMap<StreamId, StreamRef> {
        let err: Error<OperationError> = Error::new(err, self.service());
        self.0.error.set(Some(err.clone()));
        self.0.readiness.borrow_mut().clear();

        let streams = self.take_streams();
        for stream in streams.values() {
            stream.set_failed_stream(err.clone());
        }

        self.encode(frame::GoAway::new(reason).set_last_stream_id(self.0.last_id.get()));
        self.0.io.close();
        self.0.notify_capacity();
        streams
    }

    pub(crate) fn proto_error(&self, err: &Error<ConnectionError>) -> HashMap<StreamId, StreamRef> {
        let err = err.clone().map(OperationError::from);
        self.0.error.set(Some(err.clone()));
        self.0.readiness.borrow_mut().clear();

        let streams = self.take_streams();
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

        let streams = self.take_streams();
        for stream in streams.values() {
            stream.set_failed_stream(Error::new(OperationError::Disconnected, self.service()));
        }
        // graceful disconnect is in progress, no streams are left
        if !streams.is_empty()
            && self.0.reserved_streams.get() == 0
            && self
                .0
                .flags
                .get()
                .contains(ConnectionFlags::DISCONNECT_WHEN_READY)
        {
            self.0.io.close();
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

impl Connection {
    /// Handles expired shared timer deadlines.
    ///
    /// Returns `true` if the next keep-alive ping must be scheduled.
    pub(crate) fn timer_expired(state: &Weak<ConnectionState>, settings: bool, ping: bool) -> bool {
        let Some(con) = state.upgrade().map(Connection) else {
            return false;
        };
        if settings && !con.is_closed() && !con.settings_processed() {
            con.set_flags(ConnectionFlags::SETTINGS_TIMEOUT);
            con.0.io.notify_timeout();
        }
        ping && con.keepalive()
    }

    /// Sends the next keep-alive ping, or notifies an I/O timeout if the
    /// previous ping is not acknowledged.
    fn keepalive(&self) -> bool {
        if self.is_closed() || self.flags().contains(ConnectionFlags::SETTINGS_TIMEOUT) {
            log::trace!("{}: connection is closed, stopping keep-alive timer", self.tag());
            return false;
        }
        if !self.flags().contains(ConnectionFlags::RECV_PONG) {
            self.0.io.notify_timeout();
            return false;
        }

        let payload = u64::from_be_bytes(self.0.ping_payload.get())
            .wrapping_add(1)
            .to_be_bytes();
        self.unset_flags(ConnectionFlags::RECV_PONG);
        self.0.ping_payload.set(payload);
        self.encode(frame::Ping::new(payload));
        self.0.pings_count.set(self.0.pings_count.get().saturating_add(1));
        true
    }
}

impl Drop for ConnectionState {
    fn drop(&mut self) {
        timer::unregister_connection(self.timer_id.get());
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

/// Recently closed stream ids, allocated on the first retained id.
#[derive(Default)]
struct Pending(Cell<Option<Box<PendingInner>>>);

struct PendingInner {
    ids: HashSet<StreamId>,
    queue: VecDeque<(StreamId, Instant)>,
}

impl Pending {
    fn release(&self) {
        self.0.set(None);
    }

    fn add(&self, id: StreamId, config: &ServiceConfig) {
        if config.reset_max == 0 {
            self.0.set(None);
            return;
        }
        let mut inner = self.0.take().unwrap_or_else(|| {
            Box::new(PendingInner {
                ids: HashSet::default(),
                queue: VecDeque::with_capacity(config.reset_max.min(16)),
            })
        });

        // a repeated reset refreshes the entry, a stale duplicate would
        // forget the id early
        if inner.ids.remove(&id)
            && let Some(idx) = inner.queue.iter().position(|item| item.0 == id)
        {
            inner.queue.remove(idx);
        }

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
        let Some(mut inner) = self.0.take() else {
            return false;
        };
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
        let Some(inner) = self.0.take() else {
            return false;
        };
        let pending = inner.ids.contains(&id);
        self.0.set(Some(inner));
        pending
    }

    #[cfg(test)]
    fn is_allocated(&self) -> bool {
        let inner = self.0.take();
        let allocated = inner.is_some();
        self.0.set(inner);
        allocated
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ntex::http::{HeaderMap, Method, test};
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

    async fn timer_releases_connection(config: ServiceConfig, ack: bool) {
        let cfg = SharedCfg::new("CLI")
            .add(config)
            .add(ntex::io::IoConfig::new().set_metrics(true))
            .build();
        for started in [false, true] {
            let (wire, _peer) = ntex::io::testing::IoTest::create();
            let io = Io::new(wire, cfg.clone());
            let metrics = io.cfg().metrics();
            let closed = metrics.snapshot().closed.total;
            let con = super::Connection::new(
                false,
                io.get_ref(),
                Codec::default(),
                cfg.get(),
                false,
                false,
                ntex::channel::pool::new(),
            );
            if ack {
                con.recv_half().recv_settings(frame::Settings::ack()).unwrap();
            }
            if started {
                sleep(Millis(20)).await;
            }

            let id = con.0.timer_id.get();
            let weak = super::Rc::downgrade(&con.0);
            drop(con);
            drop(io);
            assert!(
                weak.upgrade().is_none(),
                "timer retained the connection (started: {started}, ack: {ack})"
            );
            assert!(
                crate::timer::connection_deadlines(id).is_none(),
                "timer kept a stale entry (started: {started}, ack: {ack})"
            );
            assert_eq!(
                weak.weak_count(),
                0,
                "timer retained the connection allocation (started: {started}, ack: {ack})"
            );

            // IoState records closure on drop, after the transport tasks release it.
            sleep(Millis(20)).await;
            assert_eq!(
                metrics.snapshot().closed.total,
                closed + 1,
                "timer retained an IoRef (started: {started}, ack: {ack})"
            );
        }
    }

    /// STATEMENT: The keep-alive timer does not retain the connection or its I/O after their
    /// owners drop.
    ///
    /// HOW:
    /// - Enable the ping timer and disable the settings timer; test both before and after timer
    ///   startup.
    /// - Drop the connection and I/O, then verify the weak connection reference expires, the
    ///   shared timer entry and its `Weak` are removed, and the I/O closure metric increases.
    #[ntex::test]
    async fn test_ping_timer_does_not_retain_connection() {
        timer_releases_connection(
            ServiceConfig::new()
                .set_ping_timeout(Seconds(30))
                .set_settings_timeout(Seconds::ZERO),
            false,
        )
        .await;
    }

    /// STATEMENT: The settings timer does not retain the connection or its I/O, with or without
    /// an acknowledgment.
    ///
    /// HOW:
    /// - Enable only the settings timer and repeat with settings acknowledged and
    ///   unacknowledged, before and after timer startup.
    /// - Drop the connection and I/O; verify the weak reference expires, the shared timer entry
    ///   and its `Weak` are removed, and the I/O closure metric increases.
    #[ntex::test]
    async fn test_settings_timer_does_not_retain_connection() {
        for ack in [false, true] {
            timer_releases_connection(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_settings_timeout(Seconds(30)),
                ack,
            )
            .await;
        }
    }

    /// STATEMENT: The ping counter saturates at `u16::MAX` without disabling missing-pong
    /// timeouts.
    ///
    /// HOW:
    /// - Start the counter near its maximum and expire the keep-alive deadline through
    ///   `Connection::timer_expired` three times, acknowledging only the first two pings.
    /// - Check each ping payload, the saturated counter, and that the next ping is scheduled.
    /// - Expire the deadline again with the final ping unacknowledged and verify no ping is
    ///   scheduled, the I/O reports a timeout, and the counter remains saturated.
    #[ntex::test]
    async fn test_ping_counter_saturates() {
        use std::task::{Context, Poll, Waker};

        let (wire, peer) = ntex::io::testing::IoTest::create();
        peer.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("CLI")
            .add(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_settings_timeout(Seconds::ZERO),
            )
            .build();
        let io = Io::new(wire, cfg.clone());
        let codec = Codec::default();
        let con = super::Connection::new(
            false,
            io.get_ref(),
            codec.clone(),
            cfg.get(),
            false,
            false,
            ntex::channel::pool::new(),
        );
        io.flush(true).await.unwrap();
        let _ = client_frames(&peer, &codec);
        con.0.pings_count.set(u16::MAX - 2);

        let weak = super::Rc::downgrade(&con.0);
        for (counter, count) in [(1_u64, u16::MAX - 1), (2, u16::MAX), (3, u16::MAX)] {
            let payload = counter.to_be_bytes();
            assert!(super::Connection::timer_expired(&weak, false, true));
            assert_eq!(con.0.ping_payload.get(), payload);
            assert_eq!(con.pings_count(), count);
            io.flush(true).await.unwrap();
            let frames = client_frames(&peer, &codec);
            let [frame::Frame::Ping(frm)] = frames.as_slice() else {
                panic!("expected a single ping: {frames:?}");
            };
            assert!(!frm.is_ack());
            assert_eq!(*frm.payload(), payload);
            if counter < 3 {
                con.recv_half().recv_pong(&frame::Ping::pong(payload));
            }
        }

        // The unacknowledged ping must still time out after the statistic saturates.
        assert!(!super::Connection::timer_expired(&weak, false, true));
        assert!(matches!(
            io.poll_status_update(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(ntex::io::IoStatusUpdate::Timeout)
        ));
        assert_eq!(con.pings_count(), u16::MAX);
    }

    /// STATEMENT: Connection deadlines leave the shared timer once they are no longer needed.
    ///
    /// HOW:
    /// - Create connections with both timeouts disabled and verify none is registered.
    /// - With both timeouts enabled, acknowledge the local settings and verify only the
    ///   keep-alive deadline remains.
    /// - With only the settings timeout enabled, verify the acknowledgment removes the entry.
    /// - Call `Connection::release` on a connection with both deadlines and verify its entry is
    ///   removed.
    #[ntex::test]
    async fn test_connection_timer_entries() {
        let new = |settings, ping| {
            let cfg = SharedCfg::new("CLI")
                .add(
                    ServiceConfig::new()
                        .set_ping_timeout(ping)
                        .set_settings_timeout(settings),
                )
                .build();
            let (wire, _peer) = ntex::io::testing::IoTest::create();
            let io = Io::new(wire, cfg.clone());
            let con = super::Connection::new(
                false,
                io.get_ref(),
                Codec::default(),
                cfg.get(),
                false,
                false,
                ntex::channel::pool::new(),
            );
            (io, con)
        };
        let deadlines =
            |con: &super::Connection| crate::timer::connection_deadlines(con.0.timer_id.get());

        let (_io, con) = new(Seconds::ZERO, Seconds::ZERO);
        assert_eq!(con.0.timer_id.get(), 0);
        assert_eq!(deadlines(&con), None);

        let (_io, con) = new(Seconds(30), Seconds(30));
        assert_eq!(deadlines(&con), Some((true, true)));
        con.recv_half().recv_settings(frame::Settings::ack()).unwrap();
        assert_eq!(deadlines(&con), Some((false, true)));

        let (_io, con) = new(Seconds(30), Seconds::ZERO);
        let id = con.0.timer_id.get();
        assert_eq!(deadlines(&con), Some((true, false)));
        con.recv_half().recv_settings(frame::Settings::ack()).unwrap();
        assert_eq!(crate::timer::connection_deadlines(id), None);

        let (_io, con) = new(Seconds(30), Seconds(30));
        let id = con.0.timer_id.get();
        con.release();
        assert_eq!(crate::timer::connection_deadlines(id), None);
    }

    /// STATEMENT: The shared timer fires a missed settings acknowledgment deadline and stops
    /// once no deadlines remain.
    ///
    /// HOW:
    /// - Create a connection with a one-second settings timeout and keep-alive disabled.
    /// - Wait past the deadline and verify the `SETTINGS_TIMEOUT` flag and an I/O timeout.
    /// - Verify the connection entry is removed and the shared timer driver stopped.
    #[ntex::test]
    async fn test_settings_deadline_fires() {
        use std::task::{Context, Poll, Waker};

        let cfg = SharedCfg::new("CLI")
            .add(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_settings_timeout(Seconds(1)),
            )
            .build();
        let (wire, _peer) = ntex::io::testing::IoTest::create();
        let io = Io::new(wire, cfg.clone());
        let con = super::Connection::new(
            false,
            io.get_ref(),
            Codec::default(),
            cfg.get(),
            false,
            false,
            ntex::channel::pool::new(),
        );
        let id = con.0.timer_id.get();

        sleep(Millis(2_500)).await;
        assert!(con.flags().contains(super::ConnectionFlags::SETTINGS_TIMEOUT));
        assert!(matches!(
            io.poll_status_update(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(ntex::io::IoStatusUpdate::Timeout)
        ));
        assert_eq!(crate::timer::connection_deadlines(id), None);
        assert!(!crate::timer::is_running());
    }

    /// STATEMENT: An excessive reset retention duration does not prevent an ID from entering
    /// reset history.
    ///
    /// HOW:
    /// - Set `reset_duration` to `Duration::MAX` and add a stream ID to `Pending`.
    /// - Verify `is_pending` still reports the ID without a time-underflow panic.
    #[test]
    fn test_pending_reset_time_underflow() {
        let pending = super::Pending::default();
        let mut config = ServiceConfig::new();
        config.reset_duration = Duration::MAX;
        let id = frame::StreamId::CLIENT;

        pending.add(id, &config);

        assert!(pending.is_pending(id));
    }

    /// STATEMENT: Duplicate reset IDs do not consume history slots or evict a newly reinserted
    /// ID.
    ///
    /// HOW:
    /// - Set the reset history limit to two, add one ID twice and another once, and verify both
    ///   remain pending.
    /// - Remove and reinsert the first ID, then add a third ID and verify the reinserted ID is
    ///   retained.
    #[test]
    fn test_pending_reset_duplicates() {
        let pending = super::Pending::default();
        let mut config = ServiceConfig::new();
        config.reset_max = 2;
        let id1 = frame::StreamId::CLIENT;
        let id2 = id1.next_id().unwrap();

        pending.add(id1, &config);
        pending.add(id1, &config);
        pending.add(id2, &config);
        assert!(pending.is_pending(id1));
        assert!(pending.is_pending(id2));

        assert!(pending.remove(id1));
        pending.add(id1, &config);
        pending.add(id1.next_id().unwrap().next_id().unwrap(), &config);
        assert!(pending.is_pending(id1));
    }

    /// STATEMENT: A zero reset history limit retains no IDs and clears previously retained
    /// entries.
    ///
    /// HOW:
    /// - Add repeated and distinct IDs with a zero limit; verify none can be found or removed
    ///   and no history storage is allocated.
    /// - Populate history with the default configuration, add with the zero limit, and verify
    ///   the history storage is freed.
    #[test]
    fn test_pending_reset_zero_limit() {
        let pending = super::Pending::default();
        let config = ServiceConfig::new().set_max_concurrent_reset_streams(0);
        let id1 = frame::StreamId::CLIENT;
        let id2 = id1.next_id().unwrap();

        for id in [id1, id1, id2] {
            pending.add(id, &config);
            assert!(!pending.is_pending(id));
            assert!(!pending.remove(id));
            assert!(!pending.is_allocated());
        }

        pending.add(id1, &ServiceConfig::new());
        assert!(pending.is_allocated());
        pending.add(id2, &config);
        assert!(!pending.is_pending(id1));
        assert!(!pending.is_allocated());
    }

    /// STATEMENT: Reset history allocates storage on the first retained ID and frees it on
    /// release.
    ///
    /// HOW:
    /// - Create `Pending` and verify lookups and removals report nothing without allocating.
    /// - Add an ID and verify storage is allocated and the ID is pending.
    /// - Release the history and verify the storage is freed and the ID is forgotten.
    #[test]
    fn test_pending_reset_lazy_allocation() {
        let pending = super::Pending::default();
        let id = frame::StreamId::CLIENT;
        assert!(!pending.is_allocated());
        assert!(!pending.is_pending(id));
        assert!(!pending.remove(id));
        assert!(!pending.is_allocated());

        pending.add(id, &ServiceConfig::new());
        assert!(pending.is_allocated());
        assert!(pending.is_pending(id));

        pending.release();
        assert!(!pending.is_allocated());
        assert!(!pending.is_pending(id));
    }

    /// STATEMENT: An idle connection frees a stream map grown by a large burst and keeps a
    /// map sized for the default concurrency limit.
    ///
    /// HOW:
    /// - Open 600 peer streams on a server connection, complete all but one, and verify the
    ///   map still tracks the open stream.
    /// - Complete the last stream and verify the map capacity drops to zero and the active
    ///   stream counter is zero.
    /// - Open and complete 512 streams, twice the default concurrency limit, and verify the
    ///   idle map keeps its allocation.
    #[ntex::test]
    async fn test_idle_stream_map_capacity() {
        let (wire, peer) = ntex::io::testing::IoTest::create();
        peer.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("SRV")
            .add(
                ServiceConfig::new()
                    .set_max_concurrent_streams(1024)
                    .set_ping_timeout(Seconds::ZERO)
                    .set_settings_timeout(Seconds::ZERO),
            )
            .build();
        let io = Io::new(wire, cfg.clone());
        let con = super::Connection::new(
            true,
            io.get_ref(),
            Codec::default(),
            cfg.get(),
            false,
            false,
            ntex::channel::pool::new(),
        );
        let recv = con.recv_half();
        let mut next = 1u32;
        let mut open = |count: usize| -> Vec<crate::StreamRef> {
            (0..count)
                .map(|_| {
                    let id = frame::StreamId::from(next);
                    next += 2;
                    let headers = frame::Headers::new(
                        id,
                        frame::PseudoHeaders {
                            method: Some(Method::GET),
                            scheme: Some("http".into()),
                            path: Some("/".into()),
                            ..Default::default()
                        },
                        HeaderMap::new(),
                        true,
                    );
                    recv.recv_headers(headers).unwrap().unwrap().0
                })
                .collect()
        };
        let complete = |stream: &crate::StreamRef| {
            stream
                .send_response(ntex::http::StatusCode::OK, HeaderMap::new(), true)
                .unwrap();
        };

        let mut streams = open(600);
        let last = streams.pop().unwrap();
        streams.iter().for_each(complete);
        assert_eq!(con.0.streams.borrow().len(), 1);
        assert!(con.0.streams.borrow().contains_key(&last.id()));

        complete(&last);
        assert_eq!(con.0.streams.borrow().capacity(), 0);
        assert_eq!(con.0.active_remote_streams.get(), 0);

        // removals leave tombstones that lower `capacity`, a kept map is not empty
        let streams = open(512);
        assert!(con.0.streams.borrow().capacity() >= 512);
        streams.iter().for_each(complete);
        assert!(con.0.streams.borrow().is_empty());
        assert!(con.0.streams.borrow().capacity() > 0);
    }

    /// STATEMENT: A one-entry reset history ignores duplicate additions and evicts the older
    /// distinct ID.
    ///
    /// HOW:
    /// - Add the first ID twice with a limit of one and verify it remains pending.
    /// - Add a second ID, verify the first is evicted, then remove the second and confirm it is
    ///   no longer pending.
    #[test]
    fn test_pending_reset_one_limit() {
        let pending = super::Pending::default();
        let config = ServiceConfig::new().set_max_concurrent_reset_streams(1);
        let id1 = frame::StreamId::CLIENT;
        let id2 = id1.next_id().unwrap();

        pending.add(id1, &config);
        pending.add(id1, &config);
        assert!(pending.is_pending(id1));
        pending.add(id2, &config);
        assert!(!pending.is_pending(id1));
        assert!(pending.is_pending(id2));
        assert!(pending.remove(id2));
        assert!(!pending.is_pending(id2));
    }

    /// STATEMENT: Disabling reset history preserves stream cleanup and confines late data
    /// errors to the stream.
    ///
    /// HOW:
    /// - Create a server connection with zero reset history and finish one stream normally and
    ///   another with a local reset.
    /// - Verify each stream leaves the active map without entering or allocating reset
    ///   history; send late window updates, resets, and data.
    /// - Check that only a `Reset` with `Reason::STREAM_CLOSED` is emitted, the connection
    ///   remains healthy, and `recv_size` stays zero.
    #[ntex::test]
    async fn test_zero_reset_history_stream_lifecycle() {
        let (wire, peer) = ntex::io::testing::IoTest::create();
        peer.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("SRV")
            .add(
                ServiceConfig::new()
                    .set_max_concurrent_reset_streams(0)
                    .set_ping_timeout(Seconds::ZERO)
                    .set_settings_timeout(Seconds::ZERO),
            )
            .build();
        let io = Io::new(wire, cfg.clone());
        let con = super::Connection::new(
            true,
            io.get_ref(),
            Codec::default(),
            cfg.get(),
            false,
            false,
            ntex::channel::pool::new(),
        );
        let recv = con.recv_half();
        let codec = Codec::default();

        for (id, reset) in [(frame::StreamId::CLIENT, false), (3.into(), true)] {
            let headers = frame::Headers::new(
                id,
                frame::PseudoHeaders {
                    method: Some(Method::GET),
                    scheme: Some("http".into()),
                    path: Some("/".into()),
                    ..Default::default()
                },
                HeaderMap::new(),
                true,
            );
            let (stream, _msg) = recv.recv_headers(headers).unwrap().unwrap();
            if reset {
                assert!(stream.reset(Reason::CANCEL));
            } else {
                stream
                    .send_response(ntex::http::StatusCode::OK, HeaderMap::new(), true)
                    .unwrap();
            }
            assert!(!con.0.streams.borrow().contains_key(&id));
            assert!(!con.0.local_pending_reset.is_pending(id));
            assert!(!con.0.local_pending_reset.is_allocated());
            io.flush(true).await.unwrap();
            let _ = client_frames(&peer, &codec);

            // Without history, late DATA is a stream error; updates and resets are ignored.
            recv.recv_window_update(frame::WindowUpdate::new(id, 10)).unwrap();
            recv.recv_rst_stream(frame::Reset::new(id, Reason::CANCEL))
                .unwrap();
            assert!(
                recv.recv_data(frame::Data::new(id, Bytes::from_static(b"data")))
                    .unwrap()
                    .is_none()
            );
            io.flush(true).await.unwrap();
            let frames = client_frames(&peer, &codec);
            assert_eq!(frames.len(), 1);
            let reset = get_reset(frames.into_iter().next().unwrap());
            assert_eq!(reset.stream_id(), id);
            assert_eq!(reset.reason(), Reason::STREAM_CLOSED);
            assert!(con.check_error().is_ok());
            assert_eq!(con.0.recv_size.get(), 0);
        }
        recv.disconnect();
    }

    /// STATEMENT: A remotely refused stream delivers EOF, rejects further payload, and leaves
    /// no active stream entry.
    ///
    /// HOW:
    /// - Run a server that resets incoming streams with `Reason::REFUSED_STREAM`, then open a
    ///   client request.
    /// - Verify the receiver obtains `MessageKind::Eof`, sending payload fails, and the
    ///   connection stream map is empty.
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
                        ),
                    )
                    .await;

                    Ok::<_, ()>(())
                })
            },
            SharedCfg::new("SRV").add(ServiceConfig::new().set_ping_timeout(Seconds::ZERO)),
        );

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let client = h2::client::SimpleClient::new(io, false, "localhost".into());
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

    /// STATEMENT: A sender waiting in its own task resumes when peer settings increase the
    /// initial stream window.
    ///
    /// HOW:
    /// - Have the peer advertise a one-byte window, then raise it to 65,535 using only
    ///   `Settings` after the first payload byte.
    /// - Send the full request body, verify completion in under 500 milliseconds, and receive
    ///   response headers.
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
        let client = h2::client::SimpleClient::new(io, false, "localhost".into());

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

    /// STATEMENT: The client assumes 100 concurrent streams until peer settings remove the
    /// provisional limit.
    ///
    /// HOW:
    /// - Withhold peer settings, verify `max_streams` is 100, open 100 streams, and check the
    ///   client is not ready.
    /// - Delay settings without a concurrency limit and verify the next request waits at least
    ///   100 milliseconds before succeeding.
    /// - Check that `max_streams` becomes `None` and the client is ready again.
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
        let client = h2::client::SimpleClient::new(io, false, "localhost".into());
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

    /// STATEMENT: A client treats `SETTINGS_ENABLE_PUSH=1` from the server as a connection
    /// protocol error.
    ///
    /// HOW:
    /// - Have the server enable push in its initial `Settings` and capture the resulting
    ///   `GoAway`.
    /// - Verify `Reason::PROTOCOL_ERROR` with the diagnostic "Server enabled push", then
    ///   confirm a new request fails.
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
        let client = h2::client::SimpleClient::new(io, false, "localhost".into());
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

    /// STATEMENT: Oversized request headers fail locally without consuming a stream ID.
    ///
    /// HOW:
    /// - Advertise `SETTINGS_MAX_HEADER_LIST_SIZE` of 200 and send a request with an oversized
    ///   user-agent header.
    /// - Verify `OperationError::HeaderListTooLarge` reports the limit, then send a small
    ///   request successfully.
    /// - Have the peer verify that the successful request still uses `StreamId::CLIENT` and
    ///   return response headers.
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
        let client = h2::client::SimpleClient::new(io, false, "localhost".into());
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

    /// STATEMENT: Responses and trailers above the peer header-list limit fail locally without
    /// closing the stream.
    ///
    /// HOW:
    /// - Advertise `SETTINGS_MAX_HEADER_LIST_SIZE` of 120 and request a response from the
    ///   server.
    /// - Attempt oversized and empty response headers, then oversized and empty trailers;
    ///   verify the success flags are false, true, false, true.
    /// - Receive exactly two `Headers` frames through end-of-stream without a reset.
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
                        ),
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

    /// STATEMENT: Malformed response pseudo-headers fail only their streams and leave the
    /// connection usable.
    ///
    /// HOW:
    /// - Return one response without a status and another with an unexpected path pseudo-
    ///   header.
    /// - Verify EOF errors report `MissingPseudo("status")` and `UnexpectedPseudo("path")`, and
    ///   observed resets use `Reason::PROTOCOL_ERROR`.
    /// - Send a valid request on the same connection and verify response headers contain status
    ///   200.
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
        let client = h2::client::SimpleClient::new(io, false, "localhost".into());
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

    /// STATEMENT: Local resets publish a final error message while the stream receive side
    /// remains open.
    ///
    /// HOW:
    /// - Advertise a zero send window to a server configured with a one-second capacity
    ///   timeout.
    /// - Open one request that the server resets explicitly and another whose response payload
    ///   times out; verify both wire resets use `Reason::CANCEL`.
    /// - Check that server EOF events identify the streams with `StreamError::LocalReset` and
    ///   `StreamError::CapacityTimeout`, respectively.
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
                            ),
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

    /// STATEMENT: Shutdown refuses new streams without exhausting the reset limit or disrupting
    /// the connection.
    ///
    /// HOW:
    /// - Trigger `ServiceConfig::shutdown` from an open request and verify a `GoAway` with
    ///   `Reason::NO_ERROR` names that stream.
    /// - Open 32 later streams, verify each receives `Reason::REFUSED_STREAM`, and send late
    ///   data for each refused stream.
    /// - Send a `Ping` and verify its acknowledgment still arrives.
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
                        ),
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

        // shutdown is announced, the open stream is processed
        let frm = goaway(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(frm.reason(), Reason::NO_ERROR);
        assert_eq!(frm.last_stream_id(), id);

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

    /// STATEMENT: Reset history temporarily ignores late data, then treats forgotten-stream
    /// data as a stream error.
    ///
    /// HOW:
    /// - Reset a stream with one-second retention and verify late data is ignored by observing
    ///   a ping acknowledgment.
    /// - Wait for expiry and reset another stream; verify old-stream data receives
    ///   `Reason::STREAM_CLOSED`, late window updates are ignored, and idle-stream data
    ///   triggers `Reason::PROTOCOL_ERROR`.
    /// - On a second connection, acknowledge a local reset with a peer reset and verify
    ///   subsequent data immediately receives `Reason::STREAM_CLOSED`.
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
                        ),
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

    /// STATEMENT: Late frames for streams purged from reset history do not cause connection
    /// errors (F5).
    ///
    /// HOW:
    /// - Limit reset history to one entry and reset streams 1 and 3, evicting stream 1.
    /// - Send trailers for retained stream 3 and a late window update for stream 1; verify a
    ///   ping is acknowledged.
    /// - Send late data for stream 1, verify `Reason::STREAM_CLOSED` on that stream, and
    ///   confirm another ping is acknowledged.
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
                        ),
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
        let client = h2::client::SimpleClient::new(Io::new(io, cfg), false, "localhost".into());

        // peer sets zero stream window and never updates it
        srv.write([0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0]);
        sleep(Millis(50)).await;
        (client, srv)
    }

    /// STATEMENT: A reset waiter completes when the peer resets its stream.
    ///
    /// HOW:
    /// - Open a zero-window stream, verify it is not reset, and spawn a task awaiting
    ///   `on_reset`.
    /// - Check the task remains pending, inject a peer reset, and verify both the task and
    ///   stream report the reset within 500 milliseconds.
    #[ntex::test]
    async fn test_on_reset_remote_reset() {
        let (client, srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        assert!(!stream.is_reset());

        let st = stream.stream().clone();
        let fut = ntex::rt::spawn(async move {
            st.on_reset().await;
            st.is_reset()
        });
        sleep(Millis(50)).await;
        assert!(!fut.is_finished());

        srv.write([0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8]);
        let res = ntex::time::timeout(Millis(500), fut).await;
        assert!(res.unwrap().unwrap());
        assert!(stream.is_reset());
    }

    /// STATEMENT: A reset waiter completes and reports a reset when the connection closes.
    ///
    /// HOW:
    /// - Open a zero-window stream and spawn a task awaiting `on_reset`.
    /// - Close the peer transport and verify the task finishes within 500 milliseconds with
    ///   `is_reset` true.
    #[ntex::test]
    async fn test_on_reset_connection_closed() {
        let (client, srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();

        let st = stream.stream().clone();
        let fut = ntex::rt::spawn(async move {
            st.on_reset().await;
            st.is_reset()
        });
        sleep(Millis(50)).await;

        srv.close().await;
        let res = ntex::time::timeout(Millis(500), fut).await;
        assert!(res.unwrap().unwrap());
    }

    /// STATEMENT: In debug builds, distinct tasks cannot wait for send capacity on the same
    /// stream.
    ///
    /// HOW:
    /// - Open a zero-window stream and spawn a task waiting in `send_capacity`.
    /// - Poll capacity from the test task and expect a panic containing "concurrent send
    ///   capacity waiters".
    #[cfg(debug_assertions)]
    #[ntex::test]
    #[should_panic(expected = "concurrent send capacity waiters")]
    async fn test_concurrent_capacity_waiters_panic() {
        let (client, _srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();

        let waiter = stream.stream().clone();
        let _fut = ntex::rt::spawn(async move { waiter.send_capacity().await });
        sleep(Millis(50)).await;

        let _ = ntex::util::lazy(|cx| stream.stream().poll_send_capacity(cx)).await;
    }

    /// STATEMENT: The same task may repeatedly poll send capacity and observe readiness after a
    /// reset.
    ///
    /// HOW:
    /// - Open a zero-window stream and poll `poll_send_capacity` twice from the same task,
    ///   verifying both results are pending.
    /// - Reset the stream with `Reason::CANCEL` and verify the next capacity poll is ready.
    #[ntex::test]
    async fn test_capacity_waiter_repoll() {
        let (client, _srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();

        let st = stream.stream();
        assert!(
            ntex::util::lazy(|cx| st.poll_send_capacity(cx))
                .await
                .is_pending()
        );
        assert!(
            ntex::util::lazy(|cx| st.poll_send_capacity(cx))
                .await
                .is_pending()
        );
        assert!(stream.reset(Reason::CANCEL));
        assert!(ntex::util::lazy(|cx| st.poll_send_capacity(cx)).await.is_ready());
    }

    /// STATEMENT: Resetting a stream unregisters its capacity timer and fails its pending
    /// capacity waiter.
    ///
    /// HOW:
    /// - Spawn a `send_capacity` waiter on a zero-window stream and verify its timer is
    ///   registered.
    /// - Reset the stream with `Reason::CANCEL`, verify the timer is unregistered, and check
    ///   the waiter returns an error.
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

    /// STATEMENT: Connection failure unregisters stream capacity timers and fails pending
    /// capacity waiters.
    ///
    /// HOW:
    /// - Spawn a `send_capacity` waiter on a zero-window stream and verify its timer is
    ///   registered.
    /// - Disconnect the receive half of the connection, verify the timer is unregistered, and
    ///   check the waiter returns an error.
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

    /// STATEMENT: Cancelling the timer task releases its registered streams, as during runtime
    /// shutdown.
    ///
    /// HOW:
    /// - Spawn a capacity waiter on a zero-window stream and verify the stream has a registered
    ///   timer.
    /// - Call `timer::cancel` to simulate timer-task cancellation and verify the stream is no
    ///   longer registered.
    #[ntex::test]
    async fn test_capacity_timer_released_on_timer_cancel() {
        let (client, _srv) = zero_window_client().await;
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();

        let waiter = stream.stream().clone();
        let _fut = ntex::rt::spawn(async move { waiter.send_capacity().await });
        sleep(Millis(50)).await;
        assert!(crate::timer::is_registered(stream.stream()));

        // timer task is dropped, e.g. on runtime shutdown
        crate::timer::cancel();
        assert!(!crate::timer::is_registered(stream.stream()));
    }

    /// STATEMENT: Dropping a capacity wait removes its timer and prevents a later spurious
    /// stream reset.
    ///
    /// HOW:
    /// - Time out a zero-window `send_capacity` wait after 100 milliseconds and verify its
    ///   timer is unregistered.
    /// - Wait beyond the configured capacity timeout, inject a stream window update, and verify
    ///   payload can still be sent.
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

    /// STATEMENT: Peer resets clear retained local reset history while later unknown-stream
    /// resets are ignored.
    ///
    /// HOW:
    /// - Locally reset a stream, verify its ID is retained, then inject a peer reset and verify
    ///   the ID is removed.
    /// - Inject resets for the forgotten stream and an unknown stream; verify the client
    ///   remains open and accepts a new request.
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

    /// STATEMENT: Connection receive capacity is replenished only when received data is
    /// consumed.
    ///
    /// HOW:
    /// - Configure a 100,000-byte connection window, receive two 15,000-byte data messages, and
    ///   retain them.
    /// - Verify no connection window update is sent while the messages are retained, then drop
    ///   them and verify updates total 30,000 bytes.
    #[ntex::test]
    async fn test_connection_window_released_on_consume() {
        let (io, srv) = ntex::io::testing::IoTest::create();
        srv.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("CLI")
            .add(ServiceConfig::new().set_initial_connection_window_size(100_000))
            .build();
        let client = h2::client::SimpleClient::new(Io::new(io, cfg), false, "localhost".into());
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

    /// STATEMENT: Final data retains connection receive capacity until its EOF messages are
    /// consumed.
    ///
    /// HOW:
    /// - Configure a 100,000-byte connection window and receive 15,000 bytes with end-of-stream
    ///   on each of two streams.
    /// - Retain both `StreamEof::Data` messages and verify no connection window update is sent.
    /// - Drop the messages and verify connection window updates total 30,000 bytes.
    #[ntex::test]
    async fn test_connection_window_released_on_eof_data_consume() {
        let (io, srv) = ntex::io::testing::IoTest::create();
        srv.remote_buffer_cap(1024 * 1024);
        let cfg = SharedCfg::new("CLI")
            .add(ServiceConfig::new().set_initial_connection_window_size(100_000))
            .build();
        let client = h2::client::SimpleClient::new(Io::new(io, cfg), false, "localhost".into());
        srv.write([0, 0, 0, 4, 0, 0, 0, 0, 0]);
        sleep(Millis(50)).await;

        let (_stream1, recv1) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        let (_stream3, recv3) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        sleep(Millis(50)).await;

        // skip preface, settings and the initial connection window update
        let codec = Codec::default();
        let _ = srv.read_any();

        // response headers and 15000 bytes of data with END_STREAM for both streams,
        // 30000 bytes are above the connection window update threshold
        let mut msgs = Vec::new();
        for (id, recv) in [(1, &recv1), (3, &recv3)] {
            srv.write([0, 0, 1, 1, 4, 0, 0, 0, id, 0x88]);
            srv.write([0, 0x3a, 0x98, 0, 1, 0, 0, 0, id]);
            srv.write(vec![0; 15_000]);

            let _hdrs = recv.recv().await.unwrap();
            let msg = recv.recv().await.unwrap();
            assert!(matches!(
                msg.kind(),
                h2::MessageKind::Eof(h2::StreamEof::Data(..))
            ));
            msgs.push(msg);
        }
        sleep(Millis(50)).await;

        // unconsumed final data does not replenish the connection window
        assert_eq!(conn_window_updates(&srv, &codec), 0);

        drop(msgs);
        sleep(Millis(50)).await;
        assert_eq!(conn_window_updates(&srv, &codec), 30_000);
    }

    /// Returns stream 1 window updates, consumes the data received by the client.
    async fn stream_window_updates(reset: bool) -> i32 {
        use ntex_codec::Decoder;

        let (io, srv) = ntex::io::testing::IoTest::create();
        srv.remote_buffer_cap(1024 * 1024);
        let client =
            h2::client::SimpleClient::new(Io::new(io, SharedCfg::default()), false, "localhost".into());
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

    /// STATEMENT: Consuming received data sends stream window updates only while the stream
    /// remains open.
    ///
    /// HOW:
    /// - Receive and retain 30,000 bytes, then consume them with the stream still open and
    ///   verify a positive stream window update.
    /// - Repeat with a peer reset before consumption and verify no stream window update is
    ///   emitted.
    #[ntex::test]
    async fn test_no_window_update_for_closed_stream() {
        assert!(stream_window_updates(false).await > 0);
        assert_eq!(stream_window_updates(true).await, 0);
    }

    /// STATEMENT: A capacity timeout cancels its stream without incrementing the connection
    /// reset counter.
    ///
    /// HOW:
    /// - Await `send_capacity` on a zero-window stream and verify
    ///   `StreamError::CapacityTimeout` and a zero `rst_count`.
    /// - Decode outgoing frames and verify a `Reset` targets the timed-out stream with
    ///   `Reason::CANCEL`.
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
        let client = h2::client::SimpleClient::new(Io::new(io, cfg), false, "localhost".into());
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

    /// STATEMENT: Payload is split by the stream send window and resumes only as window updates
    /// arrive.
    ///
    /// HOW:
    /// - Start with a ten-byte stream window and send 25 bytes with end-of-stream from a
    ///   spawned task.
    /// - Verify the first ten bytes exhaust capacity, then a ten-byte update releases another
    ///   ten without end-of-stream.
    /// - Add 100 bytes of window, verify the final five bytes carry end-of-stream, and check
    ///   the task succeeds with 95 bytes of capacity left.
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

    /// STATEMENT: An exhausted connection window blocks payload even when the stream window has
    /// capacity.
    ///
    /// HOW:
    /// - Give the stream a large window and send 70,000 bytes; verify only the default
    ///   connection window is sent in bounded, nonfinal frames.
    /// - Increase only the stream window and verify no data or usable capacity appears.
    /// - Increase the connection window by 10,000, verify the remaining payload ends the
    ///   stream, and check the sender succeeds with the expected capacity.
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

    /// STATEMENT: A capacity waiter wakes exactly once when both stream and connection windows
    /// become positive.
    ///
    /// HOW:
    /// - Exhaust both windows, make the stream window negative with `Settings`, and register a
    ///   counting waker and capacity timer.
    /// - Test both update orders and both `Settings` and `WindowUpdate` stream increases;
    ///   verify partial updates, including negative-to-zero, do not wake or cancel the timer.
    /// - Once both windows open, verify one wake, timer removal, ten bytes of ready capacity,
    ///   and a successful payload send.
    #[ntex::test]
    async fn test_window_updates_wake_after_both_windows_open() {
        use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
        use std::task::{Context, Poll, Wake, Waker};

        #[derive(Default)]
        struct WakeCount(AtomicUsize);

        impl Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        for connection_first in [false, true] {
            for settings_update in [false, true] {
                let initial = u32::try_from(frame::DEFAULT_INITIAL_WINDOW_SIZE).unwrap();
                let (client, _srv) = window_client_with(initial, Seconds(5)).await;
                let (stream, _recv) = client
                    .send(Method::POST, "/".into(), HeaderMap::new(), false)
                    .await
                    .unwrap();
                stream
                    .send_payload(Bytes::from(vec![b'x'; initial as usize]), false)
                    .await
                    .unwrap();
                let st = stream.stream();
                let recv = st.0.con.recv_half();

                // Both windows are exhausted; SETTINGS makes the stream window negative.
                let mut settings = frame::Settings::default();
                settings.set_initial_window_size(Some(initial - 5));
                recv.recv_settings(settings).unwrap();
                let count = Arc::new(WakeCount::default());
                let waker = Waker::from(count.clone());
                let cx = Context::from_waker(&waker);
                assert!(st.poll_send_capacity(&cx).is_pending());
                assert!(crate::timer::is_registered(st));

                if connection_first {
                    // The second update takes the already-positive fast path.
                    for _ in 0..2 {
                        recv.recv_window_update(frame::WindowUpdate::new(frame::StreamId::CON, 10))
                            .unwrap();
                        assert_eq!(count.0.load(Ordering::Relaxed), 0);
                        assert!(crate::timer::is_registered(st));
                    }
                }

                // Bringing the stream window from negative to zero still cannot wake it.
                recv.recv_window_update(frame::WindowUpdate::new(st.id(), 5))
                    .unwrap();
                assert_eq!(count.0.load(Ordering::Relaxed), 0);
                assert!(crate::timer::is_registered(st));

                if settings_update {
                    let mut settings = frame::Settings::default();
                    settings.set_initial_window_size(Some(initial + 5));
                    recv.recv_settings(settings).unwrap();
                } else {
                    recv.recv_window_update(frame::WindowUpdate::new(st.id(), 10))
                        .unwrap();
                }

                if !connection_first {
                    assert_eq!(count.0.load(Ordering::Relaxed), 0);
                    assert!(crate::timer::is_registered(st));
                    recv.recv_window_update(frame::WindowUpdate::new(frame::StreamId::CON, 10))
                        .unwrap();
                }

                assert_eq!(count.0.load(Ordering::Relaxed), 1);
                assert!(!crate::timer::is_registered(st));
                assert!(matches!(st.poll_send_capacity(&cx), Poll::Ready(Ok(10))));
                stream.send_payload("done", true).await.unwrap();
            }
        }
    }

    /// Waker that counts wake-ups
    #[derive(Default)]
    struct WakeCount(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for WakeCount {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl WakeCount {
        fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// Senders blocked on an exhausted connection window, polled with counting wakers
    struct BlockedSenders {
        client: h2::client::SimpleClient,
        srv: ntex::io::testing::IoTest,
        streams: Vec<h2::client::SendStream>,
        _recv: Vec<h2::client::RecvStream>,
        wakers: Vec<(std::sync::Arc<WakeCount>, std::task::Waker)>,
    }

    impl BlockedSenders {
        /// Opens `n` streams with `window` bytes of stream window and blocks each of them,
        /// in order, on the connection window
        async fn new(n: usize, window: u32, capacity_timeout: Seconds) -> Self {
            let (client, srv) = window_client_with(window, capacity_timeout).await;
            let mut streams = Vec::new();
            let mut recv = Vec::new();
            for _ in 0..n {
                let (stream, rx) = client
                    .send(Method::POST, "/".into(), HeaderMap::default(), false)
                    .await
                    .unwrap();
                streams.push(stream);
                recv.push(rx);
            }
            let con = &streams[0].stream().0.con;
            con.consume_send_window(con.send_window_size());

            let mut senders = BlockedSenders {
                client,
                srv,
                streams,
                _recv: recv,
                wakers: Vec::new(),
            };
            for i in 0..n {
                let count = std::sync::Arc::new(WakeCount::default());
                senders
                    .wakers
                    .push((count.clone(), std::task::Waker::from(count)));
                assert!(senders.poll(i).is_pending());
            }
            senders
        }

        fn stream(&self, i: usize) -> &crate::StreamRef {
            self.streams[i].stream()
        }

        fn con(&self) -> &super::Connection {
            &self.stream(0).0.con
        }

        fn poll(
            &self,
            i: usize,
        ) -> std::task::Poll<Result<frame::WindowSize, ntex_error::Error<h2::OperationError>>> {
            let cx = std::task::Context::from_waker(&self.wakers[i].1);
            self.stream(i).poll_send_capacity(&cx)
        }

        fn wakes(&self) -> Vec<usize> {
            self.wakers.iter().map(|(count, _)| count.count()).collect()
        }

        fn connection_update(&self, inc: u32) {
            self.con()
                .recv_half()
                .recv_window_update(frame::WindowUpdate::new(frame::StreamId::CON, inc))
                .unwrap();
        }
    }

    /// STATEMENT: A connection window update wakes blocked senders in arrival order, only as
    /// many as the new capacity can serve.
    ///
    /// HOW:
    /// - Block four senders with ten-byte stream windows on an exhausted connection window,
    ///   and re-poll the last one to check it is queued once.
    /// - Open the connection window by 25 bytes and verify only the first three senders are
    ///   woken, the third with the five bytes left after the first two stream windows.
    /// - Send from the woken senders until the window is exhausted, then open it by ten bytes
    ///   and verify the fourth sender is woken.
    #[ntex::test]
    async fn test_connection_window_update_wakes_blocked_senders_in_order() {
        let s = BlockedSenders::new(4, 10, Seconds::ZERO).await;
        assert!(s.poll(3).is_pending());
        assert_eq!(s.con().0.blocked.borrow().len(), 4);

        s.connection_update(25);
        assert_eq!(s.wakes(), [1, 1, 1, 0]);
        assert_eq!(s.con().0.claimed.get(), 25);

        // the woken senders send without polling for capacity again
        for (i, size) in [(0, 10), (1, 10), (2, 5)] {
            s.streams[i]
                .send_payload(Bytes::from(vec![b'x'; size]), false)
                .await
                .unwrap();
        }
        assert_eq!(s.con().send_window_size(), 0);
        assert_eq!(s.con().0.claimed.get(), 0);
        assert_eq!(s.wakes(), [1, 1, 1, 0]);

        s.connection_update(10);
        assert_eq!(s.wakes(), [1, 1, 1, 1]);
        assert!(matches!(s.poll(3), std::task::Poll::Ready(Ok(10))));
    }

    /// STATEMENT: Connection capacity claimed by a woken sender but not used is passed to the
    /// next blocked sender.
    ///
    /// HOW:
    /// - Block three senders, the second one through a `send_capacity` future, and open the
    ///   connection window by ten bytes, which wakes only the first sender.
    /// - Send three bytes from the first sender and verify the second sender is woken for the
    ///   seven bytes left.
    /// - Drop the second sender's wait before it runs and verify the third sender is woken.
    /// - Repeat with a woken sender that is reset before it runs.
    #[ntex::test]
    async fn test_unused_connection_capacity_passed_to_blocked_senders() {
        let s = BlockedSenders::new(3, 10, Seconds::ZERO).await;
        let st = s.stream(1).clone();
        let mut wait = Box::pin(st.send_capacity());
        let mut cx = std::task::Context::from_waker(&s.wakers[1].1);
        assert!(wait.as_mut().poll(&mut cx).is_pending());

        s.connection_update(10);
        assert_eq!(s.wakes(), [1, 0, 0]);

        // sending less than claimed
        s.streams[0].send_payload("xxx", true).await.unwrap();
        assert_eq!(s.wakes(), [1, 1, 0]);
        assert_eq!(s.con().0.claimed.get(), 7);

        // abandoned wait
        drop(wait);
        assert_eq!(s.wakes(), [1, 1, 1]);
        assert_eq!(s.con().0.claimed.get(), 7);

        // reset before running
        let s = BlockedSenders::new(2, 10, Seconds::ZERO).await;
        s.connection_update(10);
        assert_eq!(s.wakes(), [1, 0]);
        s.stream(0).reset(Reason::CANCEL);
        assert_eq!(s.wakes(), [1, 1]);
        assert_eq!(s.con().0.claimed.get(), 10);
    }

    /// STATEMENT: The blocked sender queue drops ids of closed streams and is cleared when
    /// the connection closes.
    ///
    /// HOW:
    /// - Block 40 senders and reset 39 of them, which leaves their ids in the queue.
    /// - Block a new sender and verify the queue keeps only the two open streams.
    /// - Open the connection window and verify the first open sender is woken.
    /// - Close the connection and verify the queue and claims are cleared.
    #[ntex::test]
    async fn test_blocked_senders_queue_bounded() {
        let s = BlockedSenders::new(41, 10, Seconds::ZERO).await;
        let (stream, _recv) = s
            .client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        for i in 1..40 {
            s.stream(i).reset(Reason::CANCEL);
        }
        assert_eq!(s.con().0.blocked.borrow().len(), 41);

        let cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(stream.stream().poll_send_capacity(&cx).is_pending());
        assert_eq!(
            *s.con().0.blocked.borrow(),
            [s.stream(0).id(), s.stream(40).id(), stream.stream().id()]
        );

        s.connection_update(10);
        assert_eq!(s.wakes()[0], 1);
        assert_eq!(s.con().0.blocked.borrow().len(), 2);

        s.srv.close().await;
        sleep(Millis(50)).await;
        assert!(s.con().is_closed());
        assert!(s.con().0.blocked.borrow().is_empty());
        assert_eq!(s.con().0.claimed.get(), 0);
    }

    /// STATEMENT: The capacity timeout of a sender waiting behind other senders is extended
    /// while the peer opens the connection window, and fires when it stops.
    ///
    /// HOW:
    /// - Block two senders with a capacity timeout.
    /// - Open the connection window for the first sender and send its data, which keeps the
    ///   second sender queued.
    /// - Expire the second sender's timer and verify it is re-armed instead of resetting the
    ///   stream.
    /// - Expire it again without connection progress and verify the stream is reset with
    ///   `CapacityTimeout`.
    #[ntex::test]
    async fn test_queued_sender_capacity_timeout_extended_by_connection_progress() {
        let s = BlockedSenders::new(2, 10, Seconds(30)).await;
        s.connection_update(10);
        assert_eq!(s.wakes(), [1, 0]);
        s.streams[0]
            .send_payload(Bytes::from(vec![b'x'; 10]), true)
            .await
            .unwrap();

        s.stream(1).capacity_timeout();
        assert!(!s.stream(1).is_reset());
        assert!(crate::timer::is_registered(s.stream(1)));
        assert!(s.poll(1).is_pending());

        s.stream(1).capacity_timeout();
        assert!(s.stream(1).is_reset());
        assert!(matches!(
            s.poll(1),
            std::task::Poll::Ready(Err(err))
                if matches!(&*err, h2::OperationError::Stream(h2::StreamError::CapacityTimeout))
        ));
    }

    /// STATEMENT: The capacity timeout wakes a queued sender instead of resetting it while
    /// connection capacity is available.
    ///
    /// HOW:
    /// - Block two senders with a capacity timeout and open the connection window for the
    ///   first one, which never runs and keeps its claim.
    /// - Expire the second sender's timer and verify it is woken, not reset, and gets
    ///   capacity.
    #[ntex::test]
    async fn test_capacity_timeout_wakes_queued_sender_with_capacity() {
        let s = BlockedSenders::new(2, 10, Seconds(30)).await;
        s.connection_update(10);
        assert_eq!(s.wakes(), [1, 0]);

        s.stream(1).capacity_timeout();
        assert_eq!(s.wakes(), [1, 1]);
        assert!(!s.stream(1).is_reset());
        assert!(matches!(s.poll(1), std::task::Poll::Ready(Ok(10))));
    }

    /// STATEMENT: I/O write backpressure pauses payload sending despite large peer windows,
    /// bounding buffered data.
    ///
    /// HOW:
    /// - Raise both windows, stop the peer from reading, and spawn a one-megabyte payload send.
    /// - Verify the send remains unfinished with ample flow-control capacity still available.
    /// - Restore peer reads, collect exactly one megabyte through end-of-stream, and verify the
    ///   send completes successfully.
    #[ntex::test]
    async fn test_send_waits_for_write_backpressure() {
        const SIZE: usize = 1024 * 1024;

        let (client, srv) = window_client(i32::MAX as u32).await;
        let codec = Codec::default();
        srv.write(window_update_frame(
            0,
            i32::MAX as u32 - frame::DEFAULT_INITIAL_WINDOW_SIZE as u32,
        ));
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        sleep(Millis(50)).await;
        let _ = client_frames(&srv, &codec);
        assert!(stream.stream().available_send_capacity() as usize > SIZE);

        // the peer does not read
        srv.remote_buffer_cap(0);
        let done = std::rc::Rc::new(std::cell::Cell::new(false));
        let done2 = done.clone();
        let s = stream.stream().clone();
        let fut = ntex::rt::spawn(async move {
            let res = s.send_payload(Bytes::from(vec![b'x'; SIZE]), true).await;
            done2.set(true);
            res
        });
        sleep(Millis(100)).await;
        assert!(!done.get());
        assert!(stream.stream().available_send_capacity() as usize > i32::MAX as usize - SIZE);

        // the peer reads, the rest of the payload is sent
        srv.remote_buffer_cap(SIZE * 2);
        let mut received = 0;
        let mut eof = false;
        for _ in 0..100 {
            for (size, end) in client_data(&srv, &codec) {
                received += size;
                eof |= end;
            }
            if eof {
                break;
            }
            sleep(Millis(20)).await;
        }
        assert!(eof);
        assert_eq!(received, SIZE);
        assert!(fut.await.unwrap().is_ok());
        assert!(done.get());
    }

    /// STATEMENT: A sender blocked by I/O write backpressure fails when its stream is reset.
    ///
    /// HOW:
    /// - Give both windows ample capacity, stop peer reads, and spawn a one-megabyte payload
    ///   send.
    /// - Verify the task remains pending, reset the stream with `Reason::CANCEL`, and check the
    ///   task returns an error within 500 milliseconds.
    #[ntex::test]
    async fn test_write_backpressure_wait_fails_on_reset() {
        let (client, srv) = window_client(i32::MAX as u32).await;
        let codec = Codec::default();
        srv.write(window_update_frame(
            0,
            i32::MAX as u32 - frame::DEFAULT_INITIAL_WINDOW_SIZE as u32,
        ));
        let (stream, _recv) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        sleep(Millis(50)).await;
        let _ = client_frames(&srv, &codec);

        srv.remote_buffer_cap(0);
        let s = stream.stream().clone();
        let fut =
            ntex::rt::spawn(
                async move { s.send_payload(Bytes::from(vec![b'x'; 1024 * 1024]), true).await },
            );
        sleep(Millis(100)).await;
        assert!(!fut.is_finished());

        stream.stream().reset(Reason::CANCEL);
        let res = ntex::time::timeout(Millis(500), fut).await;
        assert!(res.unwrap().unwrap().is_err());
    }

    /// STATEMENT: A received `GoAway` rejects only later local streams and permits earlier
    /// streams to finish, per RFC 9113 §6.8.
    ///
    /// HOW:
    /// - Open streams 1 and 3, then inject `GoAway` with last stream ID 1 and
    ///   `Reason::INTERNAL_ERROR`.
    /// - Verify stream 3 rejects payload and new requests report the `GoAway` error while the
    ///   client is disconnecting but not closed.
    /// - Complete stream 1 in both directions, verify its final data and headers, and check the
    ///   connection then closes.
    #[ntex::test]
    async fn test_go_away_last_stream_id() {
        use crate::error::{ConnectionError, OperationError};

        let (client, srv) = window_client(1024).await;
        let codec = Codec::default();
        let (s1, r1) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let (s3, _r3) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let _ = client_frames(&srv, &codec);

        // GOAWAY, last stream id 1, INTERNAL_ERROR
        srv.write([0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2]);
        sleep(Millis(50)).await;

        // stream 3 was not processed by the peer
        assert!(s3.send_payload(Bytes::from_static(b"data"), true).await.is_err());

        // new streams are refused
        let err = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            *err,
            OperationError::Connection(ConnectionError::GoAway(Reason::INTERNAL_ERROR))
        ));
        assert!(client.is_disconnecting());
        assert!(!client.is_closed());

        // stream 1 completes
        s1.send_payload(Bytes::from_static(b"data"), true).await.unwrap();
        sleep(Millis(50)).await;
        assert_eq!(client_data(&srv, &codec), [(4, true)]);
        // HEADERS, END_STREAM | END_HEADERS, `:status: 200`
        srv.write([0, 0, 1, 1, 5, 0, 0, 0, 1, 0x88]);
        let msg = r1.recv().await.unwrap();
        assert!(matches!(msg.kind(), h2::MessageKind::Headers { eof: true, .. }));

        // connection closes after the last stream
        sleep(Millis(50)).await;
        assert!(client.is_closed());
    }

    /// STATEMENT: A protocol error during graceful `GoAway` handling closes the connection
    /// immediately.
    ///
    /// HOW:
    /// - Open a stream, inject a graceful `GoAway`, and verify the client is disconnecting.
    /// - Inject a zero-increment connection window update, then verify closure and an outgoing
    ///   `GoAway` with `Reason::PROTOCOL_ERROR`.
    #[ntex::test]
    async fn test_go_away_then_protocol_error() {
        let (client, srv) = window_client(1024).await;
        let codec = Codec::default();
        let (_s1, _r1) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let _ = client_frames(&srv, &codec);

        srv.write([0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        sleep(Millis(50)).await;
        assert!(client.is_disconnecting());

        // zero connection window update is a connection error
        srv.write(window_update_frame(0, 0));
        sleep(Millis(50)).await;
        assert!(client.is_closed());
        let frames = client_frames(&srv, &codec);
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, frame::Frame::GoAway(g) if g.reason() == Reason::PROTOCOL_ERROR))
        );
    }

    /// STATEMENT: `SETTINGS_INITIAL_WINDOW_SIZE` adjusts existing stream windows, including
    /// negative values, per RFC 9113 §6.9.2.
    ///
    /// HOW:
    /// - Exhaust a ten-byte window, reduce the initial window to five, and verify usable
    ///   capacity remains zero.
    /// - Queue three bytes, bring the window from negative to zero without releasing data, then
    ///   add two and verify exactly two bytes are sent.
    /// - Raise the initial window to 20, verify the final byte is sent with 14 bytes remaining,
    ///   and check a new stream starts with capacity 20.
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

    /// STATEMENT: Empty nonfinal payloads send nothing and neither wait for nor consume flow-
    /// control capacity.
    ///
    /// HOW:
    /// - With a zero stream window, verify an empty nonfinal payload completes within 500
    ///   milliseconds.
    /// - Add ten bytes of window, send another empty nonfinal payload, and verify no data
    ///   frames or capacity consumption.
    /// - Send an empty final payload and verify a zero-length `Data` frame carries end-of-
    ///   stream.
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

    /// STATEMENT: Stream window updates do not extend a capacity timeout while the connection
    /// window is exhausted.
    ///
    /// HOW:
    /// - Configure a one-second capacity timeout and exhaust the default connection window on a
    ///   stream with a large window.
    /// - Queue another payload and inject only stream window updates every 300 milliseconds for
    ///   three seconds.
    /// - Verify the sender has finished with `StreamError::CapacityTimeout` rather than
    ///   continuing to wait.
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

    /// STATEMENT: Closing the send side fails capacity waiters regardless of remaining flow-
    /// control capacity.
    ///
    /// HOW:
    /// - Exhaust a three-byte stream window, start a capacity waiter, and close with trailers;
    ///   verify the waiter returns `OperationError::Closed(None)`.
    /// - End another stream with one byte and verify a capacity wait fails with the same error
    ///   despite two bytes remaining.
    /// - Check the last data frame is the one-byte final frame, with no later data.
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

    /// STATEMENT: Receive capacity is released on consumption, with stream window updates gated
    /// by a threshold.
    ///
    /// HOW:
    /// - Receive two 15,000-byte chunks, combine their capacities, and verify overconsumption
    ///   panics without changing the 30,000-byte capacity.
    /// - Consume 10,000 bytes without an update, then 15,000 more and verify a 25,000-byte
    ///   update; dropping the final 5,000 stays below the threshold.
    /// - Send three more 16,383-byte chunks, verify all arrive, and check the connection
    ///   remains open.
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

    /// Encodes a frame sent by the peer
    fn peer_frame(codec: &Codec, frm: impl Into<frame::Frame>) -> Bytes {
        use ntex_codec::Encoder;

        let mut buf = ntex_bytes::BytePages::default();
        codec.encode(frm.into(), &mut buf).unwrap();
        buf.freeze()
    }

    fn response(id: frame::StreamId, status: u16, len: Option<&str>, eof: bool) -> frame::Headers {
        let mut hdrs = HeaderMap::new();
        if let Some(len) = len {
            hdrs.insert(ntex::http::header::CONTENT_LENGTH, len.try_into().unwrap());
        }
        let status = ntex::http::StatusCode::from_u16(status).unwrap();
        frame::Headers::new(id, frame::PseudoHeaders::response(status), hdrs, eof)
    }

    /// STATEMENT: A server cannot initiate a stream with `Headers` when push is disabled, per
    /// RFC 9113 §8.4.
    ///
    /// HOW:
    /// - Inject response headers for server-initiated stream 2 into an otherwise idle client
    ///   connection.
    /// - Verify the client closes and sends `GoAway` with `Reason::PROTOCOL_ERROR`.
    #[ntex::test]
    async fn test_client_rejects_server_initiated_stream() {
        let (client, srv) = window_client(1024).await;
        let codec = Codec::default();
        let _ = client_frames(&srv, &codec);

        srv.write(peer_frame(&codec, response(2.into(), 200, None, false)));
        sleep(Millis(50)).await;
        assert!(client.is_closed());
        assert!(
            client_frames(&srv, &codec)
                .iter()
                .any(|f| matches!(f, frame::Frame::GoAway(g) if g.reason() == Reason::PROTOCOL_ERROR))
        );
    }

    /// STATEMENT: End-of-stream headers or trailers enforce content length, except for omitted
    /// 304 bodies, per RFC 9113 §8.1.1.
    ///
    /// HOW:
    /// - End a 200 response in headers despite content length ten; verify `WrongPayloadLength`
    ///   and a stream reset with `Reason::PROTOCOL_ERROR`.
    /// - Send five of ten declared bytes before trailers and verify headers and data precede a
    ///   `WrongPayloadLength` EOF error.
    /// - End a 304 response with content length ten and no body; verify final headers are
    ///   accepted and the client remains open.
    #[ntex::test]
    async fn test_content_length_without_data() {
        let (client, srv) = window_client(1024).await;
        let enc = Codec::default();
        let dec = Codec::default();

        // headers with END_STREAM
        let (_s1, r1) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        srv.write(peer_frame(&enc, response(r1.id(), 200, Some("10"), true)));
        let msg = r1.recv().await.unwrap();
        let h2::MessageKind::Eof(h2::StreamEof::Error(err)) = msg.kind else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(*err, h2::StreamError::WrongPayloadLength);
        sleep(Millis(50)).await;
        assert!(client_frames(&srv, &dec).iter().any(
            |f| matches!(f, frame::Frame::Reset(r) if r.stream_id() == r1.id() && r.reason() == Reason::PROTOCOL_ERROR)
        ));

        // trailers before the end of content
        let (_s2, r2) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        srv.write(peer_frame(&enc, response(r2.id(), 200, Some("10"), false)));
        srv.write(peer_frame(
            &enc,
            frame::Data::new(r2.id(), Bytes::from_static(b"12345")),
        ));
        let trailers = frame::Headers::trailers(r2.id(), HeaderMap::new());
        srv.write(peer_frame(&enc, trailers));
        assert!(matches!(
            r2.recv().await.unwrap().kind,
            h2::MessageKind::Headers { .. }
        ));
        assert!(matches!(r2.recv().await.unwrap().kind, h2::MessageKind::Data(..)));
        let msg = r2.recv().await.unwrap();
        let h2::MessageKind::Eof(h2::StreamEof::Error(err)) = msg.kind else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(*err, h2::StreamError::WrongPayloadLength);

        // `304` describes the omitted body
        let (_s3, r3) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        srv.write(peer_frame(&enc, response(r3.id(), 304, Some("10"), true)));
        let msg = r3.recv().await.unwrap();
        assert!(
            matches!(msg.kind, h2::MessageKind::Headers { eof: true, .. }),
            "{msg:?}"
        );
        assert!(!client.is_closed());
    }

    /// STATEMENT: Old streams cannot justify an unbounded burst of resets after the reset-ratio
    /// window decays.
    ///
    /// HOW:
    /// - Seed a connection with 100,000 streams and 40,000 resets, then update its reset count.
    /// - Verify the stream count drops below `RESET_RATIO_WINDOW` and fewer than half a window
    ///   of further resets succeeds before an error.
    #[ntex::test]
    async fn test_rst_count_decays() {
        let (client, _srv) = window_client(1024).await;
        let (stream, _recv) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        let con = stream.stream().0.con.0.clone();

        // long-lived connection with a healthy reset ratio
        con.streams_count.set(100_000);
        con.rst_count.set(40_000);
        con.update_rst_count().unwrap();
        assert!(con.streams_count.get() < crate::consts::RESET_RATIO_WINDOW);

        let resets = (0..crate::consts::RESET_RATIO_WINDOW)
            .take_while(|_| con.update_rst_count().is_ok())
            .count();
        assert!(
            resets < crate::consts::RESET_RATIO_WINDOW as usize / 2,
            "{resets}"
        );
    }

    /// STATEMENT: Trailers containing pseudo-headers fail the stream with a protocol error, per
    /// RFC 9113 §8.1.
    ///
    /// HOW:
    /// - Send valid initial response headers followed by final headers containing another
    ///   status pseudo-header.
    /// - Verify the initial headers arrive, then EOF reports `UnexpectedPseudo("status")` and a
    ///   matching stream reset uses `Reason::PROTOCOL_ERROR`.
    #[ntex::test]
    async fn test_pseudo_header_in_trailers() {
        let (client, srv) = window_client(1024).await;
        let enc = Codec::default();
        let dec = Codec::default();

        let (_s, r) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), true)
            .await
            .unwrap();
        srv.write(peer_frame(&enc, response(r.id(), 200, None, false)));
        srv.write(peer_frame(&enc, response(r.id(), 200, None, true)));
        assert!(matches!(
            r.recv().await.unwrap().kind,
            h2::MessageKind::Headers { .. }
        ));
        let msg = r.recv().await.unwrap();
        let h2::MessageKind::Eof(h2::StreamEof::Error(err)) = msg.kind else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(*err, h2::StreamError::UnexpectedPseudo("status"));
        sleep(Millis(50)).await;
        assert!(client_frames(&srv, &dec).iter().any(
            |f| matches!(f, frame::Frame::Reset(rst) if rst.stream_id() == r.id() && rst.reason() == Reason::PROTOCOL_ERROR)
        ));
    }

    /// STATEMENT: Local resets and capacity timeouts wake a pending client receiver to return
    /// `None`.
    ///
    /// HOW:
    /// - Open zero-window streams and spawn receivers, checking each remains pending before
    ///   failure.
    /// - Trigger a timeout through `StreamRef::send_capacity`, then through a single capacity
    ///   poll without an active waiter; verify each receiver finishes and the latter receive
    ///   state is closed.
    /// - Reset another stream through `StreamRef` with `Reason::CANCEL` and verify its receiver
    ///   also finishes.
    #[ntex::test]
    async fn test_recv_woken_on_local_reset() {
        use std::{cell::Cell, rc::Rc};

        async fn waiting_recv(r: h2::client::RecvStream) -> Rc<Cell<bool>> {
            let done = Rc::new(Cell::new(false));
            let done2 = done.clone();
            ntex::rt::spawn(async move {
                assert!(r.recv().await.is_none());
                done2.set(true);
            });
            sleep(Millis(50)).await;
            assert!(!done.get());
            done
        }

        let (client, _srv) = zero_window_client().await;

        // capacity timeout through StreamRef
        let (s, r) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let done = waiting_recv(r).await;
        assert!(s.stream().send_capacity().await.is_err());
        sleep(Millis(50)).await;
        assert!(done.get());

        // capacity timeout without an active waiter
        let (s, r) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let done = waiting_recv(r).await;
        std::future::poll_fn(|cx| {
            assert!(s.poll_send_capacity(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        sleep(Millis(2500)).await;
        assert!(s.stream().recv_state().is_closed());
        assert!(done.get());

        // reset through StreamRef
        let (s, r) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let done = waiting_recv(r).await;
        s.stream().reset(Reason::CANCEL);
        sleep(Millis(50)).await;
        assert!(done.get());
    }

    /// STATEMENT: A failed stream delivers its final disconnect message instead of reporting
    /// closure prematurely.
    ///
    /// HOW:
    /// - Open a stream and spawn a task awaiting its next received message.
    /// - Close the peer transport and verify the task returns
    ///   `Some(MessageKind::Disconnect(_))` within 500 milliseconds.
    #[ntex::test]
    async fn test_recv_waits_for_disconnect_message() {
        let (client, srv) = window_client(1024).await;
        let (_s, r) = client
            .send(Method::POST, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        let fut = ntex::rt::spawn(async move { r.recv().await });
        sleep(Millis(50)).await;

        srv.close().await;
        let msg = ntex::time::timeout(Millis(500), fut).await.unwrap().unwrap();
        assert!(matches!(
            msg.map(|m| m.kind),
            Some(h2::MessageKind::Disconnect(_))
        ));
    }
}
