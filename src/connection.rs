use std::{cell::Cell, cell::RefCell, fmt, mem, rc::Rc};
use std::{collections::VecDeque, time::Instant};

use ntex_bytes::{ByteString, Bytes};
use ntex_http::{HeaderMap, Method};
use ntex_io::IoRef;
use ntex_rt::spawn;
use ntex_util::{channel::pool, future::Either, time, time::now, time::sleep, HashMap, HashSet};

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
        const DELAY_DROP_TASK_STARTED = 0b0000_0010;
        const SLOW_REQUEST_TIMEOUT    = 0b0000_0100;
        const DISCONNECT_WHEN_READY   = 0b0000_1000;
        const SECURE                  = 0b0001_0000;
        const STREAM_REFUSED          = 0b0010_0000;
        const KA_TIMER                = 0b0100_0000;
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
    total_count: Cell<u32>,

    // Local config
    local_config: Config,
    // Maximum number of locally initiated streams
    local_max_concurrent_streams: Cell<Option<u32>>,
    // Initial window size of remote initiated streams
    remote_window_sz: Cell<WindowSize>,
    // Max frame size
    remote_frame_size: Cell<u32>,
    // Locally reset streams
    local_reset_queue: RefCell<VecDeque<(StreamId, Instant)>>,
    local_reset_ids: RefCell<HashSet<StreamId>>,
    // protocol level error
    error: Cell<Option<OperationError>>,
    // connection state flags
    flags: Cell<ConnectionFlags>,
}

impl Connection {
    pub(crate) fn new(io: IoRef, codec: Codec, config: Config, secure: bool) -> Self {
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

        let remote_frame_size = Cell::new(codec.send_frame_size());

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
            total_count: Cell::new(0),
            readiness: RefCell::new(VecDeque::new()),
            next_stream_id: Cell::new(StreamId::new(1)),
            local_config: config,
            local_max_concurrent_streams: Cell::new(None),
            local_reset_ids: RefCell::new(HashSet::default()),
            local_reset_queue: RefCell::new(VecDeque::new()),
            remote_window_sz: Cell::new(frame::DEFAULT_INITIAL_WINDOW_SIZE),
            error: Cell::new(None),
            flags: Cell::new(if secure {
                ConnectionFlags::SECURE
            } else {
                ConnectionFlags::empty()
            }),
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

    /// added new capacity, update recevice window size
    pub(crate) fn add_capacity(&self, size: u32) {
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
            self.check_error()?;
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
            log::debug!("All streams are closed, disconnecting");
            self.0.io.close();
        } else {
            log::debug!("Not all streams are closed, set disconnect flag");
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
        self.check_error()?;

        if !self.can_create_new_stream() {
            log::warn!("Cannot create new stream, waiting for available streams");
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
                log::trace!("Dropping stream {:?} remote: {:?}", id, stream.is_remote());
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
            log::debug!("All streams are closed, disconnecting");
            self.0.io.close();
            return;
        }

        let mut ids = self.0.local_reset_ids.borrow_mut();
        let mut queue = self.0.local_reset_queue.borrow_mut();

        // check queue size
        if queue.len() >= self.0.local_config.0.reset_max.get() {
            if let Some((id, _)) = queue.pop_front() {
                ids.remove(&id);
            }
        }
        ids.insert(id);
        queue.push_back((id, now() + self.0.local_config.0.reset_duration.get()));
        if !flags.contains(ConnectionFlags::DELAY_DROP_TASK_STARTED) {
            let _ = spawn(delay_drop_task(self.clone()));
        }
    }

    pub(crate) fn recv_half(&self) -> RecvHalfConnection {
        RecvHalfConnection(self.0.clone())
    }
}

impl RecvHalfConnection {
    fn query(&self, id: StreamId) -> Option<StreamRef> {
        self.0.streams.borrow_mut().get(&id).cloned()
    }

    fn flags(&self) -> ConnectionFlags {
        self.0.flags.get()
    }

    fn set_flags(&self, f: ConnectionFlags) {
        let mut flags = self.0.flags.get();
        flags.insert(f);
        self.0.flags.set(flags);
    }

    fn unset_flags(&self, f: ConnectionFlags) {
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

    pub(crate) fn recv_headers(
        &self,
        frm: Headers,
    ) -> Result<Option<(StreamRef, Message)>, Either<ConnectionError, StreamErrorInner>> {
        let id = frm.stream_id();

        if self.0.local_config.is_server() {
            self.unset_flags(ConnectionFlags::SLOW_REQUEST_TIMEOUT);

            if !id.is_client_initiated() {
                return Err(Either::Left(ConnectionError::InvalidStreamId(
                    "Invalid id in received headers frame",
                )));
            }
        }

        if let Some(stream) = self.query(id) {
            match stream.recv_headers(frm) {
                Ok(item) => Ok(item.map(move |msg| (stream, msg))),
                Err(kind) => Err(Either::Right(StreamErrorInner::new(stream, kind))),
            }
        } else if id < self.0.next_stream_id.get() {
            Err(Either::Left(ConnectionError::InvalidStreamId(
                "Received headers",
            )))
        } else if self.0.local_reset_ids.borrow().contains(&id) {
            Err(Either::Left(ConnectionError::StreamClosed(
                id,
                "Received headers",
            )))
        } else {
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
                self.0.next_stream_id.set(id);
                self.0.total_count.set(self.0.total_count.get() + 1);
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
        } else if self.0.local_reset_ids.borrow().contains(&frm.stream_id()) {
            self.encode(frame::Reset::new(
                frm.stream_id(),
                frame::Reason::STREAM_CLOSED,
            ));
            Ok(None)
        } else {
            Err(Either::Left(ConnectionError::InvalidStreamId(
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
        log::trace!("processing incoming {:#?}", frm);

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
                Ok(())
            }
        } else if let Some(stream) = self.query(frm.stream_id()) {
            stream
                .recv_window_update(frm)
                .map_err(|kind| Either::Right(StreamErrorInner::new(stream, kind)))
        } else if self.0.local_reset_ids.borrow().contains(&frm.stream_id()) {
            Ok(())
        } else {
            log::trace!("Unknown WINDOW_UPDATE {:?}", frm);
            Err(Either::Left(ConnectionError::UnknownStream(
                "WINDOW_UPDATE",
            )))
        }
    }

    fn update_rst_count(&self) -> Result<(), Either<ConnectionError, StreamErrorInner>> {
        let count = self.0.rst_count.get() + 1;
        let total_count = self.0.total_count.get();
        if total_count >= 10 && count >= total_count >> 1 {
            Err(Either::Left(ConnectionError::ConcurrencyOverflow))
        } else {
            self.0.rst_count.set(count);
            Ok(())
        }
    }

    pub(crate) fn recv_rst_stream(
        &self,
        frm: frame::Reset,
    ) -> Result<(), Either<ConnectionError, StreamErrorInner>> {
        log::trace!("processing incoming {:#?}", frm);

        if frm.stream_id().is_zero() {
            Err(Either::Left(ConnectionError::UnknownStream("RST_STREAM")))
        } else if let Some(stream) = self.query(frm.stream_id()) {
            stream.recv_rst_stream(&frm);
            self.update_rst_count()?;

            Err(Either::Right(StreamErrorInner::new(
                stream,
                StreamError::Reset(frm.reason()),
            )))
        } else if self.0.local_reset_ids.borrow().contains(&frm.stream_id()) {
            self.update_rst_count()
        } else {
            self.update_rst_count()?;
            Err(Either::Left(ConnectionError::UnknownStream("RST_STREAM")))
        }
    }

    pub(crate) fn recv_pong(&self, _: frame::Ping) {
        self.0.io.stop_timer();
    }

    pub(crate) fn recv_go_away(
        &self,
        reason: frame::Reason,
        data: &Bytes,
    ) -> HashMap<StreamId, StreamRef> {
        log::trace!(
            "processing go away with reason: {:?}, data: {:?}",
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
            .finish()
    }
}

async fn delay_drop_task(state: Connection) {
    state.set_flags(ConnectionFlags::DELAY_DROP_TASK_STARTED);

    loop {
        let next = if let Some(item) = state.0.local_reset_queue.borrow().front() {
            item.1 - now()
        } else {
            break;
        };
        sleep(next).await;

        if state.is_closed() {
            return;
        }

        let now = now();
        let mut ids = state.0.local_reset_ids.borrow_mut();
        let mut queue = state.0.local_reset_queue.borrow_mut();
        loop {
            if let Some(item) = queue.front() {
                if item.1 <= now {
                    log::trace!("dropping {:?} after delay", item.0);
                    ids.remove(&item.0);
                    queue.pop_front();
                } else {
                    break;
                }
            } else {
                state.unset_flags(ConnectionFlags::DELAY_DROP_TASK_STARTED);
                return;
            }
        }
    }
    state.unset_flags(ConnectionFlags::DELAY_DROP_TASK_STARTED);
}

async fn ping(st: Connection, timeout: time::Seconds, io: IoRef) {
    log::debug!("start http client ping/pong task");

    let mut counter: u64 = 0;
    let keepalive: time::Millis = time::Millis::from(timeout) + time::Millis(100);
    loop {
        if st.is_closed() {
            log::debug!("http client connection is closed, stopping keep-alive task");
            break;
        }
        sleep(keepalive).await;
        if st.is_closed() {
            break;
        }

        counter += 1;
        io.start_timer(timeout);
        st.encode(frame::Ping::new(counter.to_be_bytes()));
    }
}
