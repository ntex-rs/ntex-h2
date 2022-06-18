use std::{cell::Cell, cmp::Ordering, fmt, rc::Rc};

use ntex_bytes::Bytes;
use ntex_http::{HeaderMap, StatusCode};

use crate::error::{ProtocolError, StreamError, StreamErrorKind};
use crate::frame::{Data, Headers, PseudoHeaders, Reason, StreamId, WindowSize, WindowUpdate};
use crate::{connection::ConnectionInner, flow::FlowControl, frame, message::Message};

pub struct Stream(Rc<StreamInner>);

#[derive(Clone, Debug)]
pub(crate) struct StreamRef(pub(crate) Rc<StreamInner>);

#[derive(Debug)]
pub(crate) struct StreamInner {
    /// The h2 stream identifier
    pub id: StreamId,
    /// Receive part
    recv: Cell<HalfState>,
    recv_flow: Cell<FlowControl>,
    /// Send part
    send: Cell<HalfState>,
    send_flow: Cell<FlowControl>,
    /// Connection config
    con: Rc<ConnectionInner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HalfState {
    Headers,
    Payload,
    Closed,
}

impl StreamRef {
    pub(crate) fn new(id: StreamId, con: Rc<ConnectionInner>) -> Self {
        // if peer has accepted settings, we can use local config window size
        // otherwise use default window size
        let recv_flow = if con.settings_processed.get() {
            FlowControl::new(con.local_config.window_sz as i32)
        } else {
            FlowControl::new(frame::DEFAULT_INITIAL_WINDOW_SIZE as i32)
        };
        let send_flow = FlowControl::new(con.remote_window_sz.get() as i32);

        StreamRef(Rc::new(StreamInner {
            id,
            con,
            recv: Cell::new(HalfState::Headers),
            recv_flow: Cell::new(recv_flow),
            send: Cell::new(HalfState::Headers),
            send_flow: Cell::new(send_flow),
        }))
    }

    pub(crate) fn id(&self) -> StreamId {
        self.0.id
    }

    pub(crate) fn send_headers(&self, mut hdrs: Headers) {
        hdrs.set_end_headers();
        if hdrs.is_end_stream() {
            self.0.send.set(HalfState::Closed)
        } else {
            self.0.send.set(HalfState::Payload)
        }
        log::trace!("send headers {:#?}", hdrs);

        self.0
            .con
            .io
            .encode(hdrs.into(), &self.0.con.codec)
            .unwrap();
    }

    pub(crate) fn recv_headers(&self, hdrs: Headers) -> Result<Message, StreamError> {
        log::trace!(
            "processing HEADERS for {:?}:\n{:#?}\nrecv_state:{:?}, send_state: {:?}",
            self.0.id,
            hdrs,
            self.0.recv.get(),
            self.0.send.get(),
        );

        match self.0.recv.get() {
            HalfState::Headers => {
                let eof = hdrs.is_end_stream();
                if eof {
                    self.0.recv.set(HalfState::Closed);
                } else {
                    self.0.recv.set(HalfState::Payload);
                }
                let (pseudo, headers) = hdrs.into_parts();
                Ok(Message::new(pseudo, headers, eof, self))
            }
            HalfState::Payload => {
                // trailers
                self.0.recv.set(HalfState::Closed);
                Ok(Message::trailers(hdrs.into_fields(), self))
            }
            HalfState::Closed => Err(StreamError::new(
                self.0.clone(),
                StreamErrorKind::UnexpectedHeadersFrame,
            )),
        }
    }

    pub(crate) fn recv_data(&self, data: Data) -> Result<Option<Message>, ProtocolError> {
        log::trace!(
            "processing DATA frame for {:?}: {:?}",
            self.0.id,
            data.payload().len()
        );

        match self.0.recv.get() {
            HalfState::Payload => {
                let eof = data.is_end_stream();
                if eof {
                    self.0.recv.set(HalfState::Closed);
                }
                Ok(Some(Message::data(data.into_payload(), eof, self)))
            }
            HalfState::Headers => Err(ProtocolError::StreamIdle("DATA framed received")),
            HalfState::Closed => Ok(None),
        }
    }

    pub(crate) fn recv_window_update(&self, frm: WindowUpdate) -> Result<(), StreamError> {
        if frm.size_increment() == 0 {
            Err(StreamError::new(
                self.0.clone(),
                StreamErrorKind::ZeroWindowUpdateValue,
            ))
        } else {
            let mut flow = self.0.send_flow.get();
            flow.inc_window(frm.size_increment())
                .map_err(|e| StreamError::new(self.0.clone(), StreamErrorKind::LocalReason(e)))?;
            self.0.send_flow.set(flow);
            Ok(())
        }
    }

    pub(crate) fn update_send_window(&self, upd: i32) -> Result<(), ProtocolError> {
        let mut flow = self.0.send_flow.get();
        match upd.cmp(&0) {
            Ordering::Less => flow.dec_window(upd.abs() as u32), // We must decrease the (remote) window
            Ordering::Greater => flow
                .inc_window(upd as u32)
                .map_err(|_| ProtocolError::Reason(Reason::FLOW_CONTROL_ERROR))?,
            Ordering::Equal => return Ok(()),
        }
        self.0.send_flow.set(flow);
        Ok(())
    }

    pub(crate) fn update_recv_window(&self, upd: i32) -> Result<Option<WindowSize>, ProtocolError> {
        let mut flow = self.0.recv_flow.get();
        match upd.cmp(&0) {
            Ordering::Less => flow.dec_window(upd.abs() as u32), // We must decrease the (local) window
            Ordering::Greater => flow
                .inc_window(upd as u32)
                .map_err(|_| ProtocolError::Reason(Reason::FLOW_CONTROL_ERROR))?,
            Ordering::Equal => return Ok(None),
        }
        if let Some(val) = flow.need_update_window(
            self.0.con.local_config.window_sz,
            self.0.con.local_config.window_sz_threshold,
        ) {
            let _ = flow.inc_window(val);
            Ok(Some(val))
        } else {
            self.0.recv_flow.set(flow);
            Ok(None)
        }
    }

    pub(crate) fn into_stream(self) -> Stream {
        Stream(self.0)
    }
}

impl Stream {
    pub fn id(&self) -> StreamId {
        self.0.id
    }

    pub fn send_response(&self, status: StatusCode, headers: HeaderMap, eof: bool) {
        match self.0.send.get() {
            HalfState::Headers => {
                let pseudo = PseudoHeaders::response(status);
                let mut hdrs = Headers::new(self.0.id, pseudo, headers);
                hdrs.set_end_headers();

                if eof {
                    hdrs.set_end_stream();
                    self.0.send.set(HalfState::Closed)
                } else {
                    self.0.send.set(HalfState::Payload)
                }

                self.0
                    .con
                    .io
                    .encode(hdrs.into(), &self.0.con.codec)
                    .unwrap();
            }
            _ => (),
        }
    }

    pub fn send_payload(&self, res: Bytes, eof: bool) {
        match self.0.send.get() {
            HalfState::Payload => {
                let mut data = Data::new(self.0.id, res);
                if eof {
                    data.set_end_stream();
                    self.0.send.set(HalfState::Closed);
                }

                self.0
                    .con
                    .io
                    .encode(data.into(), &self.0.con.codec)
                    .unwrap();
            }
            _ => (),
        }
    }

    pub fn send_trailers(&self, map: HeaderMap) {
        if self.0.send.get() == HalfState::Payload {
            self.0.send.set(HalfState::Closed);

            let mut hdrs = Headers::trailers(self.0.id, map);
            hdrs.set_end_headers();
            hdrs.set_end_stream();
            self.0
                .con
                .io
                .encode(hdrs.into(), &self.0.con.codec)
                .unwrap();
        }
    }
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("Stream");
        builder
            .field("stream_id", &self.0.id)
            .field("recv_state", &self.0.recv.get())
            .field("send_state", &self.0.send.get())
            .finish()
    }
}
