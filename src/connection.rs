use std::{cell::Cell, cell::RefCell, fmt, mem, rc::Rc};
use std::{collections::VecDeque, time::Instant};

use ntex_bytes::{ByteString, Bytes};
use ntex_http::{HeaderMap, Method};
use ntex_io::IoRef;
use ntex_util::time::{self, now, sleep};
use ntex_util::{channel::pool, future::Either, spawn, HashMap, HashSet};

use crate::config::{Config, ConfigInner};
use crate::error::{ConnectionError, OperationError, StreamError, StreamErrorInner};
use crate::frame::{self, Headers, PseudoHeaders, StreamId, WindowSize, WindowUpdate};
use crate::stream::{Stream, StreamRef};
use crate::{codec::Codec, consts, message::Message, window::Window};

#[derive(Clone)]
pub struct Connection(Rc<ConnectionState>);

pub(crate) struct RecvHalfConnection(Rc<ConnectionState>);

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub(crate) struct ConnectionFlags: u8 {
        const SETTINGS_PROCESSED      = 0b0000_0001;
        const UNKNOWN_STREAMS         = 0b0000_0010;
        const DISCONNECT_WHEN_READY   = 0b0000_1000;
        const SECURE                  = 0b0001_0000;
        const STREAM_REFUSED          = 0b0010_0000;
        const KA_TIMER                = 0b0100_0000;
        const RECV_PONG               = 0b1000_0000;
    }
}

struct ConnectionState {
    io: IoRef,
    codec: Codec,
    send_window: Cell<Window>,
    recv_window: Cell<Window>,
    next_stream_id: Cell<StreamId>,
    streams: RefCell<HashMap<StreamId, StreamRef>>,
    active_remote_streams: Cell<u32>,
    active_local_streams: Cell<u32>,
    readiness: RefCell<VecDeque<pool::Sender<()>>>,

    rst_count: Cell<u32>,
    streams_count: Cell<u32>,
    pings_count: Cell<u16>,

    // Local config
    local_config: Config,
    // Maximum number of locally initiated streams
    local_max_concurrent_streams: Cell<Option<u32>>,
    // Initial window size of remote initiated streams
    remote_window_sz: Cell<WindowSize>,
    // Max frame size
    remote_frame_size: Cell<u32>,
    // Locally reset streams
    local_pending_reset: Pending,
    // protocol level error
    error: Cell<Option<OperationError>>,
    // connection state flags
    flags: Cell<ConnectionFlags>,
}

impl Connection {
    pub(crate) fn new(
        io: IoRef,
        codec: Codec,
        config: Config,
        secure: bool,
        skip_streams: bool,
    ) -> Self {
        // send preface
        if !config.is_server() {
            let _ = io.with_write_buf(|buf| buf.extend_from_slice(&consts::PREFACE));
        }

        // send setting to the peer
        let settings = config.0.settings.get();
        log::debug!("Sending local settings {:?}", settings);
        io.encode(settings.into(), &codec).unwrap();

        let mut recv_window = Window::new(frame::DEFAULT_INITIAL_WINDOW_SIZE as i32);
        let send_window = Window::new(frame::DEFAULT_INITIAL_WINDOW_SIZE as i32);

        // update connection window size
        if let Some(val) = recv_window.update(
            0,
            config.0.connection_window_sz.get(),
            config.0.connection_window_sz_threshold.get(),
        ) {
            log::debug!("Sending connection window update to {:?}", val);
            io.encode(WindowUpdate::new(StreamId::CON, val).into(), &codec)
                .unwrap();
        };

        if let Some(max) = config.0.settings.get().max_header_list_size() {
            codec.set_recv_header_list_size(max as usize);
        }
        codec.set_max_header_continuations(config.0.max_header_continuations.get());

        let remote_frame_size = Cell::new(codec.send_frame_size());

        let mut flags = if secure {
            ConnectionFlags::SECURE
        } else {
            ConnectionFlags::empty()
        };
        if !skip_streams {
            flags.insert(ConnectionFlags::UNKNOWN_STREAMS);
        }

        let state = Rc::new(ConnectionState {
            codec,
            remote_frame_size,
            io: io.clone(),
            send_window: Cell::new(send_window),
            recv_window: Cell::new(recv_window),
            streams: RefCell::new(HashMap::default()),
            active_remote_streams: Cell::new(0),
            active_local_streams: Cell::new(0),
            rst_count: Cell::new(0),
            streams_count: Cell::new(0),
            pings_count: Cell::new(0),
            readiness: RefCell::new(VecDeque::new()),
            next_stream_id: Cell::new(StreamId::CLIENT),
            local_config: config,
            local_max_concurrent_streams: Cell::new(None),
            local_pending_reset: Default::default(),
            remote_window_sz: Cell::new(frame::DEFAULT_INITIAL_WINDOW_SIZE),
            error: Cell::new(None),
            flags: Cell::new(flags),
        });
        let con = Connection(state);

        // start ping/pong
        if con.0.local_config.0.ping_timeout.get().non_zero() {
            let _ = spawn(ping(
                con.clone(),
                con.0.local_config.0.ping_timeout.get(),
                io,
            ));
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

    pub(crate) fn config(&self) -> &ConfigInner {
        &self.0.local_config.0
    }

    pub(crate) fn flags(&self) -> ConnectionFlags {
        self.0.flags.get()
    }

    pub(crate) fn close(&self) {
        self.0.io.close()
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.0.io.is_closed()
    }

    pub(crate) fn is_disconnecting(&self) -> bool {
        if !self.is_closed() {
            self.0
                .flags
                .get()
                .contains(ConnectionFlags::DISCONNECT_WHEN_READY)
        } else {
            false
        }
    }

    pub(crate) fn set_secure(&self, secure: bool) {
        if secure {
            self.set_flags(ConnectionFlags::SECURE)
        } else {
            self.unset_flags(ConnectionFlags::SECURE)
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

    pub(crate) fn check_error(&self) -> Result<(), OperationError> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err.clone()));
            Err(err)
        } else {
            Ok(())
        }
    }

    pub(crate) fn check_error_with_disconnect(&self) -> Result<(), OperationError> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err.clone()));
            Err(err)
        } else if self
            .0
            .flags
            .get()
            .contains(ConnectionFlags::DISCONNECT_WHEN_READY)
        {
            Err(OperationError::Disconnecting)
        } else {
            Ok(())
        }
    }

    /// Consume connection level send capacity (window)
    pub(crate) fn consume_send_window(&self, cap: u32) {
        self.0.send_window.set(self.0.send_window.get().dec(cap));
    }

    /// added new capacity, update recevice window size
    pub(crate) fn add_recv_capacity(&self, size: u32) {
        let mut recv_window = self.0.recv_window.get().dec(size);

        // update connection window size
        if let Some(val) = recv_window.update(
            0,
            self.0.local_config.0.connection_window_sz.get(),
            self.0.local_config.0.connection_window_sz_threshold.get(),
        ) {
            self.encode(WindowUpdate::new(StreamId::CON, val));
        }
        self.0.recv_window.set(recv_window);
    }

    pub(crate) fn send_window_size(&self) -> WindowSize {
        self.0.send_window.get().window_size()
    }

    pub(crate) fn remote_window_size(&self) -> WindowSize {
        self.0.remote_window_sz.get()
    }

    pub(crate) fn remote_frame_size(&self) -> usize {
        self.0.remote_frame_size.get() as usize
    }

    pub(crate) fn settings_processed(&self) -> bool {
        self.flags().contains(ConnectionFlags::SETTINGS_PROCESSED)
    }

    pub(crate) fn max_streams(&self) -> Option<u32> {
        self.0.local_max_concurrent_streams.get()
    }

    pub(crate) fn active_streams(&self) -> u32 {
        if self.0.local_max_concurrent_streams.get().is_some() {
            self.0.active_local_streams.get()
        } else {
            0
        }
    }

    pub(crate) fn can_create_new_stream(&self) -> bool {
        if let Some(max) = self.0.local_max_concurrent_streams.get() {
            self.0.active_local_streams.get() < max
        } else {
            true
        }
    }

    pub(crate) async fn ready(&self) -> Result<(), OperationError> {
        loop {
            self.check_error_with_disconnect()?;
            return if let Some(max) = self.0.local_max_concurrent_streams.get() {
                if self.0.active_local_streams.get() < max {
                    Ok(())
                } else {
                    let (tx, rx) = self.config().pool.channel();
                    self.0.readiness.borrow_mut().push_back(tx);
                    match rx.await {
                        Ok(_) => continue,
                        Err(_) => Err(OperationError::Disconnected),
                    }
                }
            } else {
                Ok(())
            };
        }
    }

    pub(crate) fn disconnect_when_ready(&self) {
        if self.0.streams.borrow().is_empty() {
            log::trace!("{}: All streams are closed, disconnecting", self.tag());
            self.0.io.close();
        } else {
            log::trace!(
                "{}: Not all streams are closed, set disconnect flag",
                self.tag()
            );
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
    ) -> Result<Stream, OperationError> {
        self.check_error_with_disconnect()?;

        if !self.can_create_new_stream() {
            log::warn!(
                "{}: Cannot create new stream, waiting for available streams",
                self.tag()
            );
            self.ready().await?
        }

        let stream = {
            let id = self.0.next_stream_id.get();
            let stream = StreamRef::new(id, false, self.clone());
            self.0.streams.borrow_mut().insert(id, stream.clone());
            self.0
                .active_local_streams
                .set(self.0.active_local_streams.get() + 1);
            self.0.next_stream_id.set(
                id.next_id()
                    .map_err(|_| OperationError::OverflowedStreamId)?,
            );
            stream
        };

        let pseudo = PseudoHeaders {
            scheme: Some(if self.0.flags.get().contains(ConnectionFlags::SECURE) {
                consts::HTTPS_SCHEME
            } else {
                consts::HTTP_SCHEME
            }),
            method: Some(method),
            authority: Some(authority),
            path: Some(path),
            ..Default::default()
        };
        stream.send_headers(Headers::new(stream.id(), pseudo, headers, eof));
        Ok(stream.into_stream())
    }

    pub(crate) fn rst_stream(&self, id: StreamId, reason: frame::Reason) {
        let stream = self.0.streams.borrow_mut().get(&id).cloned();
        if let Some(stream) = stream {
            stream.set_failed(Some(reason))
        }
    }

    pub(crate) fn drop_stream(&self, id: StreamId) {
        let empty = {
            let mut streams = self.0.streams.borrow_mut();
            if let Some(stream) = streams.remove(&id) {
                log::trace!(
                    "{}: Dropping stream {:?} remote: {:?}",
                    self.tag(),
                    id,
                    stream.is_remote()
                );
                if stream.is_remote() {
                    self.0
                        .active_remote_streams
                        .set(self.0.active_remote_streams.get() - 1)
                } else {
                    let local = self.0.active_local_streams.get();
                    self.0.active_local_streams.set(local - 1);
                    if let Some(max) = self.0.local_max_concurrent_streams.get() {
                        if local == max {
                            while let Some(tx) = self.0.readiness.borrow_mut().pop_front() {
                                if !tx.is_canceled() {
                                    let _ = tx.send(());
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            streams.is_empty()
        };
        let flags = self.flags();

        // Close connection
        if empty && flags.contains(ConnectionFlags::DISCONNECT_WHEN_READY) {
            log::trace!("{}: All streams are closed, disconnecting", self.tag());
            self.0.io.close();
            return;
        }

        // Add ids to pending queue
        if flags.contains(ConnectionFlags::UNKNOWN_STREAMS) {
            self.0.local_pending_reset.add(id, &self.0.local_config);
        }
    }

    pub(crate) fn recv_half(&self) -> RecvHalfConnection {
        RecvHalfConnection(self.0.clone())
    }

    pub(crate) fn pings_count(&self) -> u16 {
        self.0.pings_count.get()
    }
}

impl ConnectionState {
    fn err_unknown_streams(&self) -> bool {
        self.flags.get().contains(ConnectionFlags::UNKNOWN_STREAMS)
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

    pub(crate) fn connection(&self) -> Connection {
        Connection(self.0.clone())
    }

    pub(crate) fn encode<T>(&self, item: T)
    where
        frame::Frame: From<T>,
    {
        let _ = self.0.io.encode(item.into(), &self.0.codec);
    }

    pub(crate) fn recv_headers(
        &self,
        frm: Headers,
    ) -> Result<Option<(StreamRef, Message)>, Either<ConnectionError, StreamErrorInner>> {
        let id = frm.stream_id();
        let is_server = self.0.local_config.is_server();

        if is_server && !id.is_client_initiated() {
            return Err(Either::Left(ConnectionError::InvalidStreamId(
                "Invalid id in received headers frame",
            )));
        }

        if let Some(stream) = self.query(id) {
            match stream.recv_headers(frm) {
                Ok(item) => Ok(item.map(move |msg| (stream, msg))),
                Err(kind) => Err(Either::Right(StreamErrorInner::new(stream, kind))),
            }
        } else if !is_server
            && (self.0.local_pending_reset.is_pending(id) || !self.0.err_unknown_streams())
        {
            // if client and no stream, then it was closed
            self.encode(frame::Reset::new(id, frame::Reason::STREAM_CLOSED));
            Ok(None)
        } else {
            // refuse stream if connection is preparing for disconnect
            if self
                .0
                .flags
                .get()
                .contains(ConnectionFlags::DISCONNECT_WHEN_READY)
            {
                self.encode(frame::Reset::new(id, frame::Reason::REFUSED_STREAM));
                self.set_flags(ConnectionFlags::STREAM_REFUSED);
                return Ok(None);
            }

            if let Some(max) = self.0.local_config.0.remote_max_concurrent_streams.get() {
                if self.0.active_remote_streams.get() >= max {
                    // check if client opened more streams than allowed
                    // in that case close connection
                    return if self.flags().contains(ConnectionFlags::STREAM_REFUSED) {
                        Err(Either::Left(ConnectionError::ConcurrencyOverflow))
                    } else {
                        self.encode(frame::Reset::new(id, frame::Reason::REFUSED_STREAM));
                        self.set_flags(ConnectionFlags::STREAM_REFUSED);
                        Ok(None)
                    };
                }
            }

            let pseudo = frm.pseudo();
            if pseudo
                .path
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("")
                .is_empty()
            {
                Err(Either::Left(ConnectionError::MissingPseudo("path")))
            } else if pseudo.method.is_none() {
                Err(Either::Left(ConnectionError::MissingPseudo("method")))
            } else if pseudo
                .scheme
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("")
                .is_empty()
            {
                Err(Either::Left(ConnectionError::MissingPseudo("scheme")))
            } else if frm.pseudo().status.is_some() {
                Err(Either::Left(ConnectionError::UnexpectedPseudo("scheme")))
            } else {
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

    pub(crate) fn recv_data(
        &self,
        frm: frame::Data,
    ) -> Result<Option<(StreamRef, Message)>, Either<ConnectionError, StreamErrorInner>> {
        if let Some(stream) = self.query(frm.stream_id()) {
            match stream.recv_data(frm) {
                Ok(item) => Ok(item.map(move |msg| (stream, msg))),
                Err(kind) => Err(Either::Right(StreamErrorInner::new(stream, kind))),
            }
        } else if self.0.local_pending_reset.is_pending(frm.stream_id())
            || !self.0.err_unknown_streams()
        {
            self.encode(frame::Reset::new(
                frm.stream_id(),
                frame::Reason::STREAM_CLOSED,
            ));
            Ok(None)
        } else {
            Err(Either::Left(ConnectionError::UnknownStream(
                "Received data",
            )))
        }
    }

    pub(crate) fn recv_settings(
        &self,
        settings: frame::Settings,
    ) -> Result<(), Either<ConnectionError, Vec<StreamErrorInner>>> {
        log::trace!("processing incoming settings: {:#?}", settings);

        if settings.is_ack() {
            if !self.flags().contains(ConnectionFlags::SETTINGS_PROCESSED) {
                self.set_flags(ConnectionFlags::SETTINGS_PROCESSED);
                if let Some(max) = self.0.local_config.0.settings.get().max_frame_size() {
                    self.0.codec.set_recv_frame_size(max as usize);
                }

                let upd = (self.0.local_config.0.window_sz.get() as i32)
                    - (frame::DEFAULT_INITIAL_WINDOW_SIZE as i32);

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
                proto_err!(conn: "received unexpected settings ack");
                return Err(Either::Left(ConnectionError::UnexpectedSettingsAck));
            }
        } else {
            // Ack settings to the peer
            self.encode(frame::Settings::ack());

            if let Some(max) = settings.max_frame_size() {
                self.0.codec.set_send_frame_size(max as usize);
                self.0.remote_frame_size.set(max);
            }
            if let Some(max) = settings.header_table_size() {
                self.0.codec.set_send_header_table_size(max as usize);
            }
            if let Some(max) = settings.max_concurrent_streams() {
                self.0.local_max_concurrent_streams.set(Some(max));
                for tx in mem::take(&mut *self.0.readiness.borrow_mut()) {
                    let _ = tx.send(());
                }
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
                log::trace!(
                    "Update remote initial window size to {} from {}",
                    val,
                    old_val
                );

                let mut stream_errors = Vec::new();

                let upd = (val as i32) - (old_val as i32);
                if upd != 0 {
                    for stream in self.0.streams.borrow().values() {
                        if let Err(kind) = stream.update_send_window(upd) {
                            stream_errors.push(StreamErrorInner::new(stream.clone(), kind))
                        }
                    }
                }

                if !stream_errors.is_empty() {
                    return Err(Either::Right(stream_errors));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn recv_window_update(
        &self,
        frm: frame::WindowUpdate,
    ) -> Result<(), Either<ConnectionError, StreamErrorInner>> {
        log::trace!("{}: processing incoming {:#?}", self.tag(), frm);

        if frm.stream_id().is_zero() {
            if frm.size_increment() == 0 {
                Err(Either::Left(ConnectionError::ZeroWindowUpdateValue))
            } else {
                let window = self
                    .0
                    .send_window
                    .get()
                    .inc(frm.size_increment())
                    .map_err(|_| Either::Left(ConnectionError::WindowValueOverflow))?;
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
        } else if self.0.local_pending_reset.is_pending(frm.stream_id()) {
            Ok(())
        } else if self.0.err_unknown_streams() {
            log::trace!("Unknown WINDOW_UPDATE {:?}", frm);
            Err(Either::Left(ConnectionError::UnknownStream(
                "WINDOW_UPDATE",
            )))
        } else {
            self.encode(frame::Reset::new(
                frm.stream_id(),
                frame::Reason::STREAM_CLOSED,
            ));
            Ok(())
        }
    }

    fn update_rst_count(&self) -> Result<(), Either<ConnectionError, StreamErrorInner>> {
        let count = self.0.rst_count.get() + 1;
        let streams_count = self.0.streams_count.get();
        if streams_count >= 10 && count >= streams_count >> 1 {
            Err(Either::Left(ConnectionError::StreamResetsLimit))
        } else {
            self.0.rst_count.set(count);
            Ok(())
        }
    }

    pub(crate) fn recv_rst_stream(
        &self,
        frm: frame::Reset,
    ) -> Result<(), Either<ConnectionError, StreamErrorInner>> {
        log::trace!("{}: processing incoming {:#?}", self.tag(), frm);

        let id = frm.stream_id();
        if id.is_zero() {
            Err(Either::Left(ConnectionError::UnknownStream(
                "RST_STREAM-zero",
            )))
        } else if let Some(stream) = self.query(id) {
            stream.recv_rst_stream(&frm);
            self.update_rst_count()?;

            Err(Either::Right(StreamErrorInner::new(
                stream,
                StreamError::Reset(frm.reason()),
            )))
        } else if self.0.local_pending_reset.remove(id) {
            self.update_rst_count()
        } else if self.0.err_unknown_streams() {
            self.update_rst_count()?;
            Err(Either::Left(ConnectionError::UnknownStream("RST_STREAM")))
        } else {
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
            .set(Some(ConnectionError::GoAway(reason).into()));
        self.0.readiness.borrow_mut().clear();

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_go_away(reason)
        }
        streams
    }

    pub(crate) fn ping_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.0
            .error
            .set(Some(ConnectionError::KeepaliveTimeout.into()));

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_failed_stream(ConnectionError::KeepaliveTimeout.into())
        }

        self.encode(frame::GoAway::new(frame::Reason::NO_ERROR));
        self.0.io.close();
        streams
    }

    pub(crate) fn read_timeout(&self) -> HashMap<StreamId, StreamRef> {
        self.0.error.set(Some(ConnectionError::ReadTimeout.into()));

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_failed_stream(ConnectionError::ReadTimeout.into())
        }

        self.encode(frame::GoAway::new(frame::Reason::NO_ERROR));
        self.0.io.close();
        streams
    }

    pub(crate) fn proto_error(&self, err: &ConnectionError) -> HashMap<StreamId, StreamRef> {
        self.0.error.set(Some((*err).into()));
        self.0.readiness.borrow_mut().clear();

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in &mut streams.values() {
            stream.set_failed_stream((*err).into())
        }
        streams
    }

    pub(crate) fn disconnect(&self) -> HashMap<StreamId, StreamRef> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err))
        } else {
            self.0.error.set(Some(OperationError::Disconnected));
        }

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_failed_stream(OperationError::Disconnected)
        }
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
    log::debug!("start http client ping/pong task");

    let mut counter: u64 = 0;
    let keepalive: time::Millis = time::Millis::from(timeout) + time::Millis(100);

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
    fn add(&self, id: StreamId, config: &Config) {
        let mut inner = self.0.take().unwrap();

        let current_time = now();

        // remove old ids
        let max_time = current_time - config.0.reset_duration.get();
        while let Some(item) = inner.queue.front() {
            if item.1 < max_time {
                inner.ids.remove(&item.0);
                inner.queue.pop_front();
            } else {
                break;
            }
        }

        // shrink size of ids
        while inner.queue.len() >= config.0.reset_max.get() {
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
    use ntex::http::{test::server as test_server, uri::Scheme, HeaderMap, Method};
    use ntex::time::{sleep, Millis, Seconds};
    use ntex::{io::Io, service::fn_service, util::Bytes};

    use crate::{self as h2, frame, frame::Reason, Codec};

    const PREFACE: [u8; 24] = *b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    fn get_reset(frm: frame::Frame) -> frame::Reset {
        match frm {
            frame::Frame::Reset(rst) => rst,
            _ => panic!("Expect Reset frame: {:?}", frm),
        }
    }

    fn goaway(frm: frame::Frame) -> frame::GoAway {
        match frm {
            frame::Frame::GoAway(f) => f,
            _ => panic!("Expect Reset frame: {:?}", frm),
        }
    }

    #[ntex::test]
    async fn test_remote_stream_refused() {
        let srv = test_server(|| {
            fn_service(|io: Io<_>| async move {
                let _ = h2::server::handle_one(
                    io.into(),
                    h2::Config::server().ping_timeout(Seconds::ZERO).clone(),
                    fn_service(|msg: h2::ControlMessage<h2::StreamError>| async move {
                        Ok::<_, ()>(msg.ack())
                    }),
                    fn_service(|msg: h2::Message| async move {
                        msg.stream().reset(Reason::REFUSED_STREAM);
                        Ok::<_, h2::StreamError>(())
                    }),
                )
                .await;

                Ok::<_, ()>(())
            })
        });

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr).await.unwrap();
        let client = h2::client::SimpleClient::new(
            io,
            h2::Config::client(),
            Scheme::HTTP,
            "localhost".into(),
        );
        sleep(Millis(150)).await;

        let (stream, recv_stream) = client
            .send(Method::GET, "/".into(), HeaderMap::default(), false)
            .await
            .unwrap();
        sleep(Millis(150)).await;

        let res = stream
            .send_payload(Bytes::from_static(b"hello"), false)
            .await;
        assert!(res.is_err());

        let msg = recv_stream.recv().await.unwrap();
        assert!(matches!(msg.kind(), h2::MessageKind::Eof(_)));

        let con = &recv_stream.stream().0.con.0;
        assert!(con.streams.borrow().is_empty());
    }

    #[ntex::test]
    async fn test_delay_reset_queue() {
        let _ = env_logger::try_init();

        let srv = test_server(|| {
            fn_service(|io: Io<_>| async move {
                let _ = h2::server::handle_one(
                    io.into(),
                    h2::Config::server()
                        .ping_timeout(Seconds::ZERO)
                        .reset_stream_duration(Seconds(1))
                        .clone(),
                    fn_service(|msg: h2::ControlMessage<h2::StreamError>| async move {
                        Ok::<_, ()>(msg.ack())
                    }),
                    fn_service(|msg: h2::Message| async move {
                        msg.stream().reset(Reason::NO_ERROR);
                        Ok::<_, h2::StreamError>(())
                    }),
                )
                .await;

                Ok::<_, ()>(())
            })
        });

        let addr = ntex::connect::Connect::new("localhost").set_addr(Some(srv.addr()));
        let io = ntex::connect::connect(addr.clone()).await.unwrap();
        let codec = Codec::default();
        let _ = io.with_write_buf(|buf| buf.extend_from_slice(&PREFACE));

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

        // server should keep reseted streams for some time
        let pl = frame::Data::new(id, Bytes::from_static(b"data"));
        io.send(pl.clone().into(), &codec).await.unwrap();

        let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::STREAM_CLOSED);

        // reset queue cleared in 1 sec (for test)
        sleep(Millis(1100)).await;

        let id2 = id.next_id().unwrap();
        let hdrs = frame::Headers::new(id2, pseudo.clone(), HeaderMap::new(), false);
        io.send(hdrs.into(), &codec).await.unwrap();
        let res = get_reset(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::NO_ERROR);

        // prev closed stream
        io.send(pl.into(), &codec).await.unwrap();
        let res = goaway(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::PROTOCOL_ERROR);

        // SECOND connection
        let io = ntex::connect::connect(addr).await.unwrap();
        let codec = Codec::default();
        let _ = io.with_write_buf(|buf| buf.extend_from_slice(&PREFACE));

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

        // after server receives remote reset, any next frame cause protocol error
        io.send(frame::Reset::new(id, Reason::NO_ERROR).into(), &codec)
            .await
            .unwrap();

        let pl = frame::Data::new(id, Bytes::from_static(b"data"));
        io.send(pl.clone().into(), &codec).await.unwrap();
        let res = goaway(io.recv(&codec).await.unwrap().unwrap());
        assert_eq!(res.reason(), Reason::PROTOCOL_ERROR);
    }
}
