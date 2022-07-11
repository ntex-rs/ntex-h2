use std::{cell::Cell, cell::RefCell, fmt, mem, rc::Rc};
use std::{collections::VecDeque, time::Instant};

use ntex_bytes::{ByteString, Bytes};
use ntex_http::{uri::Scheme, HeaderMap, Method};
use ntex_io::IoRef;
use ntex_rt::spawn;
use ntex_util::future::Either;
use ntex_util::{channel::pool, time, time::now, time::sleep, HashMap, HashSet};

use crate::config::{Config, ConfigInner};
use crate::error::{ConnectionError, OperationError, StreamError, StreamErrorInner};
use crate::frame::{self, Headers, PseudoHeaders, StreamId, WindowSize, WindowUpdate};
use crate::stream::{Stream, StreamRef};
use crate::{codec::Codec, consts, message::Message, window::Window};

#[derive(Clone, Debug)]
pub struct Connection(Rc<ConnectionState>);

bitflags::bitflags! {
    pub(crate) struct ConnectionFlags: u8 {
        const SETTINGS_PROCESSED      = 0b0000_0001;
        const DELAY_DROP_TASK_STARTED = 0b0000_0010;
        const SLOW_REQUEST_TIMEOUT    = 0b0000_0100;
        const PING_SENT               = 0b0000_1000;
    }
}

pub(crate) struct ConnectionState {
    pub(crate) io: IoRef,
    pub(crate) codec: Codec,
    pub(crate) send_window: Cell<Window>,
    pub(crate) recv_window: Cell<Window>,
    next_stream_id: Cell<StreamId>,
    streams: RefCell<HashMap<StreamId, StreamRef>>,
    active_remote_streams: Cell<u32>,
    active_local_streams: Cell<u32>,
    readiness: RefCell<VecDeque<pool::Sender<()>>>,

    // Local config
    pub(crate) local_config: Config,
    // Maximum number of locally initiated streams
    pub(crate) local_max_concurrent_streams: Cell<Option<u32>>,
    // Initial window size of remote initiated streams
    pub(crate) remote_window_sz: Cell<WindowSize>,
    // Max frame size
    pub(crate) remote_frame_size: Cell<u32>,
    // Locally reset streams
    local_reset_queue: RefCell<VecDeque<(StreamId, Instant)>>,
    local_reset_ids: RefCell<HashSet<StreamId>>,
    // protocol level error
    pub(crate) error: Cell<Option<OperationError>>,
    // connection state flags
    flags: Cell<ConnectionFlags>,
}

impl ConnectionState {
    /// added new capacity, update recevice window size
    pub(crate) fn add_capacity(&self, size: u32) {
        let mut recv_window = self.recv_window.get().dec(size);

        // update connection window size
        if let Some(val) = recv_window.update(
            0,
            self.local_config.0.connection_window_sz.get(),
            self.local_config.0.connection_window_sz_threshold.get(),
        ) {
            self.io
                .encode(WindowUpdate::new(StreamId::CON, val).into(), &self.codec)
                .unwrap();
        }
        self.recv_window.set(recv_window);
    }

    #[inline]
    pub(crate) fn flags(&self) -> ConnectionFlags {
        self.flags.get()
    }

    #[inline]
    pub(crate) fn set_flags(&self, f: ConnectionFlags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    #[inline]
    pub(crate) fn unset_flags(&self, f: ConnectionFlags) {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
    }

    #[inline]
    pub(crate) fn config(&self) -> &ConfigInner {
        &self.local_config.0
    }

    #[inline]
    pub(crate) fn remote_frame_size(&self) -> usize {
        self.remote_frame_size.get() as usize
    }

    pub(crate) fn settings_processed(&self) -> bool {
        self.flags().contains(ConnectionFlags::SETTINGS_PROCESSED)
    }

    #[inline]
    pub(crate) fn drop_stream(&self, id: StreamId, state: &Rc<ConnectionState>) {
        if let Some(stream) = self.streams.borrow_mut().remove(&id) {
            if stream.is_remote() {
                self.active_remote_streams
                    .set(self.active_remote_streams.get() - 1)
            } else {
                let local = self.active_local_streams.get();
                self.active_local_streams.set(local - 1);
                if let Some(max) = self.local_max_concurrent_streams.get() {
                    if local == max {
                        while let Some(tx) = self.readiness.borrow_mut().pop_front() {
                            if !tx.is_canceled() {
                                let _ = tx.send(());
                                break;
                            }
                        }
                    }
                }
            }
        }

        let mut ids = self.local_reset_ids.borrow_mut();
        let mut queue = self.local_reset_queue.borrow_mut();

        // check queue size
        if queue.len() >= self.local_config.0.reset_max.get() {
            if let Some((id, _)) = queue.pop_front() {
                ids.remove(&id);
            }
        }
        ids.insert(id);
        queue.push_back((id, now() + self.local_config.0.reset_duration.get()));
        if !self
            .flags()
            .contains(ConnectionFlags::DELAY_DROP_TASK_STARTED)
        {
            spawn(delay_drop_task(state.clone()));
        }
    }

    #[inline]
    pub(crate) fn rst_stream(&self, id: StreamId, reason: frame::Reason) {
        let stream = self.streams.borrow_mut().get(&id).cloned();
        if let Some(stream) = stream {
            stream.set_failed(Some(reason))
        }
    }

    #[inline]
    pub(crate) fn is_disconnected(&self) -> bool {
        if let Some(e) = self.error.take() {
            self.error.set(Some(e));
            true
        } else {
            false
        }
    }

    fn ping_timeout(&self) {
        if let Some(err) = self.error.take() {
            self.error.set(Some(err))
        } else {
            self.error.set(Some(OperationError::PingTimeout));

            let streams = mem::take(&mut *self.streams.borrow_mut());
            for stream in streams.values() {
                stream.set_failed_stream(OperationError::PingTimeout)
            }

            let _ = self.io.encode(
                frame::GoAway::new(frame::Reason::NO_ERROR).into(),
                &self.codec,
            );
            self.io.close();
        }
    }
}

impl Connection {
    pub(crate) fn new(io: IoRef, codec: Codec, config: Config) -> Self {
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
        let remote_frame_size = Cell::new(codec.send_frame_size());

        let state = Rc::new(ConnectionState {
            io,
            codec,
            remote_frame_size,
            send_window: Cell::new(send_window),
            recv_window: Cell::new(recv_window),
            streams: RefCell::new(HashMap::default()),
            active_remote_streams: Cell::new(0),
            active_local_streams: Cell::new(0),
            readiness: RefCell::new(VecDeque::new()),
            next_stream_id: Cell::new(StreamId::new(1)),
            local_config: config,
            local_max_concurrent_streams: Cell::new(None),
            local_reset_ids: RefCell::new(HashSet::default()),
            local_reset_queue: RefCell::new(VecDeque::new()),
            remote_window_sz: Cell::new(frame::DEFAULT_INITIAL_WINDOW_SIZE),
            error: Cell::new(None),
            flags: Cell::new(ConnectionFlags::empty()),
        });

        // start ping/pong
        if state.local_config.0.ping_timeout.get().non_zero() {
            spawn(ping(state.clone(), state.local_config.0.ping_timeout.get()));
        }

        Connection(state)
    }

    pub(crate) fn config(&self) -> &ConfigInner {
        &self.0.local_config.0
    }

    pub(crate) fn state(&self) -> &ConnectionState {
        self.0.as_ref()
    }

    pub(crate) fn get_state(&self) -> Rc<ConnectionState> {
        self.0.clone()
    }

    fn query(&self, id: StreamId) -> Option<StreamRef> {
        self.0.streams.borrow_mut().get(&id).cloned()
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
            if let Some(err) = self.0.error.take() {
                self.0.error.set(Some(err.clone()));
                return Err(err);
            } else if let Some(max) = self.0.local_max_concurrent_streams.get() {
                if self.0.active_local_streams.get() < max {
                    return Ok(());
                } else {
                    let (tx, rx) = self.config().pool.channel();
                    self.0.readiness.borrow_mut().push_back(tx);
                    match rx.await {
                        Ok(_) => continue,
                        Err(_) => return Err(OperationError::Disconnected),
                    }
                }
            } else {
                return Ok(());
            }
        }
    }

    pub(crate) async fn send_request(
        &self,
        scheme: Scheme,
        authority: ByteString,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<Stream, OperationError> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err.clone()));
            return Err(err);
        }

        if !self.can_create_new_stream() {
            log::warn!("Cannot create new stream, waiting for available streams");
            self.ready().await?
        }

        let stream = {
            let id = self.0.next_stream_id.get();
            let stream = StreamRef::new(id, false, self.0.clone());
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
            scheme: Some(if scheme == Scheme::HTTPS {
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

    pub(crate) fn recv_headers(
        &self,
        frm: Headers,
    ) -> Result<Option<(StreamRef, Message)>, Either<ConnectionError, StreamErrorInner>> {
        let id = frm.stream_id();

        if self.0.local_config.is_server() {
            self.0.unset_flags(ConnectionFlags::SLOW_REQUEST_TIMEOUT);

            if !id.is_client_initiated() {
                return Err(Either::Left(ConnectionError::InvalidStreamId));
            }
        }

        if let Some(stream) = self.query(id) {
            match stream.recv_headers(frm) {
                Ok(item) => Ok(item.map(move |msg| (stream, msg))),
                Err(kind) => Err(Either::Right(StreamErrorInner::new(stream, kind))),
            }
        } else if id < self.0.next_stream_id.get() {
            Err(Either::Left(ConnectionError::InvalidStreamId))
        } else if self.0.local_reset_ids.borrow().contains(&id) {
            Err(Either::Left(ConnectionError::StreamClosed(id)))
        } else {
            if let Some(max) = self.0.local_config.0.remote_max_concurrent_streams.get() {
                if self.0.active_remote_streams.get() >= max {
                    self.0
                        .io
                        .encode(
                            frame::Reset::new(id, frame::Reason::REFUSED_STREAM).into(),
                            &self.0.codec,
                        )
                        .unwrap();
                    return Ok(None);
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
                let stream = StreamRef::new(id, true, self.0.clone());
                self.0.next_stream_id.set(id);
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
            self.0
                .io
                .encode(
                    frame::Reset::new(frm.stream_id(), frame::Reason::STREAM_CLOSED).into(),
                    &self.0.codec,
                )
                .unwrap();
            Ok(None)
        } else {
            Err(Either::Left(ConnectionError::InvalidStreamId))
        }
    }

    pub(crate) fn recv_settings(
        &self,
        settings: frame::Settings,
    ) -> Result<(), Either<ConnectionError, Vec<StreamErrorInner>>> {
        log::trace!("processing incoming settings: {:#?}", settings);

        if settings.is_ack() {
            if !self.0.flags().contains(ConnectionFlags::SETTINGS_PROCESSED) {
                self.0.set_flags(ConnectionFlags::SETTINGS_PROCESSED);
                if let Some(max) = self.0.local_config.0.settings.get().max_frame_size() {
                    self.0.codec.set_recv_frame_size(max as usize);
                }
                if let Some(max) = self.0.local_config.0.settings.get().max_header_list_size() {
                    self.0.codec.set_recv_header_list_size(max as usize);
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
                        self.0
                            .io
                            .encode(WindowUpdate::new(stream.id(), val).into(), &self.0.codec)
                            .unwrap();
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
            self.0
                .io
                .encode(frame::Settings::ack().into(), &self.0.codec)
                .map_err(|e| Either::Left(e.into()))?;

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
        } else {
            Err(Either::Left(ConnectionError::UnknownStream(
                "WINDOW_UPDATE",
            )))
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
            Err(Either::Right(StreamErrorInner::new(
                stream,
                StreamError::Reset(frm.reason()),
            )))
        } else {
            Err(Either::Left(ConnectionError::UnknownStream("RST_STREAM")))
        }
    }

    pub(crate) fn recv_pong(&self, _: frame::Ping) {
        self.0.unset_flags(ConnectionFlags::PING_SENT);
    }

    pub(crate) fn recv_go_away(&self, reason: frame::Reason, data: &Bytes) {
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
    }

    pub(crate) fn proto_error(&self, err: &ConnectionError) {
        self.0.error.set(Some((*err).into()));
        self.0.readiness.borrow_mut().clear();

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_failed(None)
        }
    }

    pub(crate) fn disconnect(&self) {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err))
        } else {
            self.0.error.set(Some(OperationError::Disconnected));

            let streams = mem::take(&mut *self.0.streams.borrow_mut());
            for stream in streams.values() {
                stream.set_failed_stream(OperationError::Disconnected)
            }
        }
    }
}

impl fmt::Debug for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("ConnectionState");
        builder
            .field("io", &self.io)
            .field("codec", &self.codec)
            .field("recv_window", &self.recv_window.get())
            .field("send_window", &self.send_window.get())
            .field("settings_processed", &self.settings_processed())
            .field("next_stream_id", &self.next_stream_id.get())
            .field("local_config", &self.local_config)
            .field(
                "local_max_concurrent_streams",
                &self.local_max_concurrent_streams.get(),
            )
            .field("remote_window_sz", &self.remote_window_sz.get())
            .field("remote_frame_size", &self.remote_frame_size.get())
            .finish()
    }
}

async fn delay_drop_task(state: Rc<ConnectionState>) {
    state.set_flags(ConnectionFlags::DELAY_DROP_TASK_STARTED);

    #[allow(clippy::while_let_loop)]
    loop {
        let next = if let Some(item) = state.local_reset_queue.borrow().front() {
            item.1 - now()
        } else {
            break;
        };
        sleep(next).await;

        if state.is_disconnected() {
            return;
        }

        let now = now();
        let mut ids = state.local_reset_ids.borrow_mut();
        let mut queue = state.local_reset_queue.borrow_mut();
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

async fn ping(st: Rc<ConnectionState>, timeout: time::Seconds) {
    log::debug!("start http client ping/pong task");

    let mut counter: u64 = 0;
    let keepalive: time::Millis = timeout.into();
    loop {
        if st.is_disconnected() {
            log::debug!("http client connection is closed, stopping keep-alive task");
            break;
        }
        sleep(keepalive).await;

        if st.flags().contains(ConnectionFlags::PING_SENT) {
            // connection is closed
            log::warn!("did not receive pong response in time, closing connection");
            st.ping_timeout();
            break;
        }
        st.set_flags(ConnectionFlags::PING_SENT);

        counter += 1;
        let _ = st
            .io
            .encode(frame::Ping::new(counter.to_be_bytes()).into(), &st.codec);
    }
}
