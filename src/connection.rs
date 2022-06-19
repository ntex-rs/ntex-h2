use std::{cell::Cell, cell::RefCell, rc::Rc, task::Context, task::Poll};

use ntex_bytes::ByteString;
use ntex_http::{HeaderMap, Method};
use ntex_io::IoRef;
use ntex_util::{future::Either, task::LocalWaker, time::Seconds, HashMap};

use crate::error::{ProtocolError, StreamError};
use crate::frame::{self, Headers, PseudoHeaders, Settings, StreamId, WindowSize, WindowUpdate};
use crate::stream::{Stream, StreamRef};
use crate::{codec::Codec, flow::FlowControl, message::Message};

#[derive(Debug)]
pub(crate) struct Config {
    /// Initial window size of locally initiated streams
    pub(crate) window_sz: WindowSize,
    pub(crate) window_sz_threshold: WindowSize,
    /// How long a locally reset stream should ignore frames
    pub(crate) reset_duration: Seconds,
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
}

const HTTP_SCHEME: ByteString = ByteString::from_static("http");
const HTTPS_SCHEME: ByteString = ByteString::from_static("https");

#[derive(Clone, Debug)]
pub struct Connection(Rc<ConnectionInner>);

#[derive(Debug)]
pub(crate) struct ConnectionInner {
    pub(crate) io: IoRef,
    pub(crate) codec: Rc<Codec>,
    pub(crate) send_flow: Cell<FlowControl>,
    pub(crate) recv_flow: Cell<FlowControl>,
    pub(crate) settings_processed: Cell<bool>,
    next_stream_id: Cell<StreamId>,
    streams: RefCell<HashMap<StreamId, StreamRef>>,
    active_streams: Cell<u32>,
    readiness: LocalWaker,

    // Local config
    pub(crate) local_config: Rc<Config>,
    // Maximum number of locally initiated streams
    pub(crate) local_max_concurrent_streams: Cell<Option<u32>>,
    // Initial window size of remote initiated streams
    pub(crate) remote_window_sz: Cell<WindowSize>,
}

impl ConnectionInner {
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

        Connection(Rc::new(ConnectionInner {
            io,
            codec,
            send_flow: Cell::new(send_flow),
            recv_flow: Cell::new(recv_flow),
            streams: RefCell::new(HashMap::default()),
            active_streams: Cell::new(0),
            readiness: LocalWaker::new(),
            next_stream_id: Cell::new(StreamId::new(1)),
            settings_processed: Cell::new(false),
            local_config: config,
            local_max_concurrent_streams: Cell::new(None),
            remote_window_sz: Cell::new(frame::DEFAULT_INITIAL_WINDOW_SIZE),
        }))
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(max) = self.0.local_max_concurrent_streams.get() {
            if self.0.active_streams.get() < max {
                Poll::Ready(())
            } else {
                self.0.readiness.register(cx.waker());
                Poll::Pending
            }
        } else {
            Poll::Ready(())
        }
    }

    pub(crate) fn get(&self, id: StreamId) -> StreamRef {
        if let Some(s) = self.0.streams.borrow().get(&id) {
            return s.clone();
        }

        let s = StreamRef::new(id, self.0.clone());
        self.0.streams.borrow_mut().insert(id, s.clone());
        s
    }

    pub(crate) fn query(&self, id: StreamId) -> Option<StreamRef> {
        self.0.streams.borrow_mut().get(&id).cloned()
    }

    pub(crate) fn send_request(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
    ) -> Stream {
        let stream = {
            let id = self.0.next_stream_id.get();
            let stream = StreamRef::new(id, self.0.clone());
            self.0.streams.borrow_mut().insert(id, stream.clone());
            self.0.next_stream_id.set(id.next_id().unwrap());
            stream
        };

        let pseudo = PseudoHeaders {
            method: Some(method),
            path: Some(path),
            scheme: Some(HTTP_SCHEME),
            ..Default::default()
        };
        stream.send_headers(Headers::new(stream.id(), pseudo, headers));
        stream.into_stream()
    }

    pub(crate) fn recv_headers(
        &self,
        frm: Headers,
    ) -> Result<Option<(StreamRef, Message)>, ProtocolError> {
        let stream = self.get(frm.stream_id());
        Ok(stream.recv_headers(frm).map(move |item| (stream, item)))
    }

    pub(crate) fn recv_data(
        &self,
        frm: frame::Data,
    ) -> Result<Option<(StreamRef, Message)>, ProtocolError> {
        if let Some(stream) = self.query(frm.stream_id()) {
            stream
                .recv_data(frm)
                .map(|item| item.map(move |item| (stream, item)))
        } else {
            Err(ProtocolError::from(frame::FrameError::InvalidStreamId))
        }
    }

    pub(crate) fn recv_settings(&self, settings: frame::Settings) -> Result<(), ProtocolError> {
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

                for stream in self.0.streams.borrow().values() {
                    if let Some(val) = stream.update_recv_window(upd)? {
                        // send window size update to the peer
                        self.0
                            .io
                            .encode(WindowUpdate::new(stream.id(), val).into(), &self.0.codec)
                            .unwrap();
                    }
                }
            } else {
                proto_err!(conn: "received unexpected settings ack");
                return Err(ProtocolError::UnexpectedSettingsAck);
            }
        } else {
            // Ack settings to the peer
            self.0
                .io
                .encode(frame::Settings::ack().into(), &self.0.codec)?;

            if let Some(max) = settings.max_frame_size() {
                self.0.codec.set_send_frame_size(max as usize);
            }
            if let Some(max) = settings.max_header_list_size() {
                self.0.codec.set_send_header_list_size(max as usize);
            }
            if let Some(max) = settings.max_concurrent_streams() {
                self.0.local_max_concurrent_streams.set(Some(max));
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

                let upd = (val as i32) - (old_val as i32);
                for stream in self.0.streams.borrow().values() {
                    stream.update_send_window(upd)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn recv_window_update(
        &self,
        frm: frame::WindowUpdate,
    ) -> Result<(), Either<ProtocolError, StreamError>> {
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
                    .map_err(|e| Either::Left(e.into()))?;
                self.0.send_flow.set(flow);
                Ok(())
            }
        } else if let Some(stream) = self.query(frm.stream_id()) {
            stream.recv_window_update(frm).map_err(Either::Right)
        } else {
            Err(Either::Left(ProtocolError::UnknownStream))
        }
    }
}
