use std::{cell::Cell, cell::RefCell, fmt, mem, rc::Rc, task::Context, task::Poll};
use std::{collections::VecDeque, time::Duration, time::Instant};

use ntex_bytes::{ByteString, Bytes};
use ntex_http::{HeaderMap, Method};
use ntex_io::IoRef;
use ntex_rt::spawn;
use ntex_util::future::{poll_fn, Either};
use ntex_util::{task::LocalWaker, time::now, time::sleep, HashMap, HashSet};

use crate::error::{OperationError, ProtocolError, StreamErrorInner};
use crate::frame::{self, Headers, PseudoHeaders, Settings, StreamId, WindowSize, WindowUpdate};
use crate::stream::{Stream, StreamRef};
use crate::{codec::Codec, flow::FlowControl, message::Message};

#[derive(Debug)]
pub(crate) struct Config {
    /// Initial window size of locally initiated streams
    pub(crate) window_sz: WindowSize,
    pub(crate) window_sz_threshold: WindowSize,
    /// How long a locally reset stream should ignore frames
    pub(crate) reset_duration: Duration,
    /// Maximum number of locally reset streams to keep at a time
    pub(crate) reset_max: usize,
    pub(crate) settings: Settings,
    /// Initial window size for new connections.
    pub(crate) connection_window_sz: WindowSize,
    pub(crate) connection_window_sz_threshold: WindowSize,
    /// Maximum number of remote initiated streams
    pub(crate) remote_max_concurrent_streams: Option<u32>,
    // /// If extended connect protocol is enabled.
    // pub extended_connect_protocol_enabled: bool,
    /// Client flags
    pub(crate) client: bool,
}

const HTTP_SCHEME: ByteString = ByteString::from_static("http");
const _HTTPS_SCHEME: ByteString = ByteString::from_static("https");

#[derive(Clone, Debug)]
pub struct Connection(Rc<ConnectionState>);

pub(crate) struct ConnectionState {
    pub(crate) io: IoRef,
    pub(crate) codec: Rc<Codec>,
    pub(crate) send_flow: Cell<FlowControl>,
    pub(crate) recv_flow: Cell<FlowControl>,
    pub(crate) settings_processed: Cell<bool>,
    next_stream_id: Cell<StreamId>,
    streams: RefCell<HashMap<StreamId, StreamRef>>,
    active_remote_streams: Cell<u32>,
    active_local_streams: Cell<u32>,
    readiness: LocalWaker,

    // Local config
    pub(crate) local_config: Rc<Config>,
    // Maximum number of locally initiated streams
    pub(crate) local_max_concurrent_streams: Cell<Option<u32>>,
    // Initial window size of remote initiated streams
    pub(crate) remote_window_sz: Cell<WindowSize>,
    // Max frame size
    pub(crate) remote_frame_size: Cell<u32>,
    // Locally reset streams
    local_reset_queue: RefCell<VecDeque<(StreamId, Instant)>>,
    local_reset_ids: RefCell<HashSet<StreamId>>,
    local_reset_task: Cell<bool>,
    // protocol level error
    pub(crate) error: Cell<Option<OperationError>>,
}

impl ConnectionState {
    /// added new capacity, update recevice window size
    pub(crate) fn add_capacity(&self, size: u32) {
        let mut recv_flow = self.recv_flow.get().dec_window(size);

        // update connection window size
        if let Some(val) = recv_flow.update_window(
            0,
            self.local_config.connection_window_sz,
            self.local_config.connection_window_sz_threshold,
        ) {
            self.io
                .encode(WindowUpdate::new(StreamId::CON, val).into(), &self.codec)
                .unwrap();
        }
        self.recv_flow.set(recv_flow);
    }

    #[inline]
    pub(crate) fn remote_frame_size(&self) -> usize {
        self.remote_frame_size.get() as usize
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
                        self.readiness.wake()
                    }
                }
            }
        }

        let mut ids = self.local_reset_ids.borrow_mut();
        let mut queue = self.local_reset_queue.borrow_mut();

        // check queue size
        if queue.len() >= self.local_config.reset_max {
            if let Some((id, _)) = queue.pop_front() {
                ids.remove(&id);
            }
        }
        ids.insert(id);
        queue.push_back((id, now() + self.local_config.reset_duration));
        if !self.local_reset_task.get() {
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
}

impl Connection {
    pub(crate) fn new(io: IoRef, codec: Rc<Codec>, config: Rc<Config>) -> Self {
        // send setting to the peer
        io.encode(config.settings.clone().into(), &codec).unwrap();

        let mut recv_flow = FlowControl::new(frame::DEFAULT_INITIAL_WINDOW_SIZE as i32);
        let send_flow = FlowControl::new(frame::DEFAULT_INITIAL_WINDOW_SIZE as i32);

        // update connection window size
        if let Some(val) = recv_flow.update_window(
            0,
            config.connection_window_sz,
            config.connection_window_sz_threshold,
        ) {
            io.encode(WindowUpdate::new(StreamId::CON, val).into(), &codec)
                .unwrap();
        };
        let remote_frame_size = Cell::new(codec.send_frame_size());

        Connection(Rc::new(ConnectionState {
            io,
            codec,
            remote_frame_size,
            send_flow: Cell::new(send_flow),
            recv_flow: Cell::new(recv_flow),
            streams: RefCell::new(HashMap::default()),
            active_remote_streams: Cell::new(0),
            active_local_streams: Cell::new(0),
            readiness: LocalWaker::new(),
            next_stream_id: Cell::new(StreamId::new(1)),
            settings_processed: Cell::new(false),
            local_config: config,
            local_max_concurrent_streams: Cell::new(None),
            local_reset_task: Cell::new(false),
            local_reset_ids: RefCell::new(HashSet::default()),
            local_reset_queue: RefCell::new(VecDeque::new()),
            remote_window_sz: Cell::new(frame::DEFAULT_INITIAL_WINDOW_SIZE),
            error: Cell::new(None),
        }))
    }

    pub(crate) fn state(&self) -> &ConnectionState {
        self.0.as_ref()
    }

    pub(crate) fn get_state(&self) -> Rc<ConnectionState> {
        self.0.clone()
    }

    fn can_create_new_stream(&self) -> bool {
        if let Some(max) = self.0.local_max_concurrent_streams.get() {
            self.0.active_local_streams.get() < max
        } else {
            true
        }
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), OperationError>> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err.clone()));
            Poll::Ready(Err(err))
        } else if let Some(max) = self.0.local_max_concurrent_streams.get() {
            if self.0.active_local_streams.get() < max {
                Poll::Ready(Ok(()))
            } else {
                self.0.readiness.register(cx.waker());
                Poll::Pending
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }

    pub(crate) fn query(&self, id: StreamId) -> Option<StreamRef> {
        self.0.streams.borrow_mut().get(&id).cloned()
    }

    pub(crate) async fn send_request(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
    ) -> Result<Stream, OperationError> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err.clone()));
            return Err(err);
        }

        if !self.can_create_new_stream() {
            log::warn!("Cannot create new stream, waiting for available streams");
            poll_fn(|cx| self.poll_ready(cx)).await?;
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
            method: Some(method),
            path: Some(path),
            scheme: Some(HTTP_SCHEME),
            ..Default::default()
        };
        stream.send_headers(Headers::new(stream.id(), pseudo, headers));
        Ok(stream.into_stream())
    }

    pub(crate) fn recv_headers(
        &self,
        frm: Headers,
    ) -> Result<Option<(StreamRef, Message)>, Either<ProtocolError, StreamErrorInner>> {
        let id = frm.stream_id();

        if (self.0.local_config.client && id.is_client_initiated())
            || (!self.0.local_config.client && id.is_server_initiated())
        {
            return Err(Either::Left(ProtocolError::InvalidStreamId));
        }

        if let Some(stream) = self.query(id) {
            match stream.recv_headers(frm) {
                Ok(item) => Ok(item.map(move |msg| (stream, msg))),
                Err(kind) => Err(Either::Right(StreamErrorInner::new(stream, kind))),
            }
        } else if id < self.0.next_stream_id.get() {
            Err(Either::Left(ProtocolError::InvalidStreamId))
        } else if self.0.local_reset_ids.borrow().contains(&id) {
            Err(Either::Left(ProtocolError::StreamClosed(id)))
        } else {
            if let Some(max) = self.0.local_config.remote_max_concurrent_streams {
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
                Err(Either::Left(ProtocolError::MissingPseudo("path")))
            } else if pseudo.method.is_none() {
                Err(Either::Left(ProtocolError::MissingPseudo("method")))
            } else if pseudo
                .scheme
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("")
                .is_empty()
            {
                Err(Either::Left(ProtocolError::MissingPseudo("scheme")))
            } else if frm.pseudo().status.is_some() {
                Err(Either::Left(ProtocolError::UnexpectedPseudo("scheme")))
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
    ) -> Result<Option<(StreamRef, Message)>, Either<ProtocolError, StreamErrorInner>> {
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
            Err(Either::Left(ProtocolError::from(
                frame::FrameError::InvalidStreamId,
            )))
        }
    }

    pub(crate) fn recv_settings(
        &self,
        settings: frame::Settings,
    ) -> Result<(), Either<ProtocolError, Vec<StreamErrorInner>>> {
        log::trace!("processing incoming settings: {:#?}", settings);

        if settings.is_ack() {
            if !self.0.settings_processed.get() {
                self.0.settings_processed.set(true);
                if let Some(max) = self.0.local_config.settings.max_frame_size() {
                    self.0.codec.set_recv_frame_size(max as usize);
                }
                if let Some(max) = self.0.local_config.settings.max_header_list_size() {
                    self.0.codec.set_recv_header_list_size(max as usize);
                }

                let upd = (self.0.local_config.window_sz as i32)
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
                return Err(Either::Left(ProtocolError::UnexpectedSettingsAck));
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
            if let Some(max) = settings.max_header_list_size() {
                self.0.codec.set_send_header_list_size(max as usize);
            }
            if let Some(max) = settings.max_concurrent_streams() {
                self.0.local_max_concurrent_streams.set(Some(max));
                self.0.readiness.wake();
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
    ) -> Result<(), Either<ProtocolError, StreamErrorInner>> {
        log::trace!("processing incoming {:#?}", frm);

        if frm.stream_id().is_zero() {
            if frm.size_increment() == 0 {
                Err(Either::Left(ProtocolError::ZeroWindowUpdateValue))
            } else {
                let flow = self
                    .0
                    .send_flow
                    .get()
                    .inc_window(frm.size_increment())
                    .map_err(|_| {
                        Either::Left(ProtocolError::Reason(frame::Reason::FLOW_CONTROL_ERROR))
                    })?;
                self.0.send_flow.set(flow);
                Ok(())
            }
        } else if let Some(stream) = self.query(frm.stream_id()) {
            stream
                .recv_window_update(frm)
                .map_err(|kind| Either::Right(StreamErrorInner::new(stream, kind)))
        } else {
            Err(Either::Left(ProtocolError::UnknownStream(frm.into())))
        }
    }

    pub(crate) fn recv_rst_stream(
        &self,
        frm: frame::Reset,
    ) -> Result<(), Either<ProtocolError, StreamErrorInner>> {
        log::trace!("processing incoming {:#?}", frm);

        if frm.stream_id().is_zero() {
            Err(Either::Left(ProtocolError::UnknownStream(frm.into())))
        } else if let Some(stream) = self.query(frm.stream_id()) {
            stream.recv_rst_stream(frm);
            Ok(())
        } else {
            Err(Either::Left(ProtocolError::UnknownStream(frm.into())))
        }
    }

    pub(crate) fn recv_go_away(&self, reason: frame::Reason, data: &Bytes) {
        log::trace!(
            "processing go away with reason: {:?}, data: {:?}",
            reason,
            data.slice(..std::cmp::min(data.len(), 20))
        );

        self.0.error.set(Some(ProtocolError::Reason(reason).into()));
        self.0.readiness.wake();

        let streams = mem::take(&mut *self.0.streams.borrow_mut());
        for stream in streams.values() {
            stream.set_go_away(reason)
        }
    }

    pub(crate) fn proto_error(&self, err: &ProtocolError) {
        self.0.error.set(Some(err.clone().into()));
        self.0.readiness.wake();

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
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("ConnectionState");
        builder
            .field("io", &self.io)
            .field("codec", &self.codec)
            .field("recv_flow", &self.recv_flow.get())
            .field("send_flow", &self.send_flow.get())
            .field("settings_processed", &self.settings_processed.get())
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
    state.local_reset_task.set(true);

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
                state.local_reset_task.set(false);
                return;
            }
        }
    }
    state.local_reset_task.set(false);
}
