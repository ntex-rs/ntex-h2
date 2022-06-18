use std::{cell::Cell, cell::RefCell, fmt, rc::Rc};

use ntex_bytes::Bytes;
use ntex_http::{HeaderMap, StatusCode};

use crate::connection::ConnectionInner;
use crate::error::StreamError;
use crate::frame::{Data, Headers, PseudoHeaders, StreamId, WindowUpdate};
use crate::{flow::FlowControl, message::Message};

#[derive(Clone)]
pub struct Stream(Rc<StreamInner>);

#[derive(Debug)]
struct StreamInner {
    /// The h2 stream identifier
    pub id: StreamId,
    /// Receive part
    recv: Cell<HalfState>,
    recv_flow: Cell<FlowControl>,
    /// Send part
    send: Cell<HalfState>,
    send_flow: Cell<FlowControl>,
    /// config
    connection: Rc<RefCell<ConnectionInner>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HalfState {
    Headers,
    Payload,
    Closed,
}

impl Stream {
    pub(crate) fn new(id: StreamId, connection: Rc<RefCell<ConnectionInner>>) -> Self {
        Stream(Rc::new(StreamInner {
            id,
            connection,
            recv: Cell::new(HalfState::Headers),
            recv_flow: Cell::new(FlowControl::new()),
            send: Cell::new(HalfState::Headers),
            send_flow: Cell::new(FlowControl::new()),
        }))
    }

    pub fn id(&self) -> StreamId {
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

        let con = self.0.connection.borrow();
        con.io.encode(hdrs.into(), &con.codec).unwrap();
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

                let con = self.0.connection.borrow();
                con.io.encode(hdrs.into(), &con.codec).unwrap();
            }
            _ => (),
        }
    }

    pub fn send_data(&self, chunk: Bytes, eof: bool) {
        let mut data = Data::new(self.0.id, chunk);
        if eof {
            data.set_end_stream();
            self.0.send.set(HalfState::Closed);
        }

        let con = self.0.connection.borrow();
        con.io.encode(data.into(), &con.codec).unwrap();
    }

    pub fn send_payload(&self, res: Bytes, eof: bool) {
        match self.0.send.get() {
            HalfState::Payload => {
                let mut data = Data::new(self.0.id, res);
                if eof {
                    data.set_end_stream();
                    self.0.send.set(HalfState::Closed);
                }

                let con = self.0.connection.borrow();
                con.io.encode(data.into(), &con.codec).unwrap();
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
            let con = self.0.connection.borrow();
            con.io.encode(hdrs.into(), &con.codec).unwrap();
        }
    }

    pub(crate) fn recv_headers(&self, hdrs: Headers) -> Result<Message, StreamError> {
        log::trace!(
            "processing HEADERS for {:?}:\n{:#?}\n{:#?}",
            self.0.id,
            hdrs,
            self
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
            HalfState::Closed => Err(StreamError::UnexpectedHeadersFrame),
        }
    }

    pub(crate) fn recv_data(&mut self, data: Data) -> Result<Message, StreamError> {
        log::trace!(
            "processing DATA for {:?}: {:?}",
            self.0.id,
            data.payload().len()
        );

        match self.0.recv.get() {
            HalfState::Payload => {
                let eof = data.is_end_stream();
                if eof {
                    self.0.recv.set(HalfState::Closed);
                }
                Ok(Message::data(data.into_payload(), eof, self))
            }
            _ => Err(StreamError::UnexpectedDataFrame),
        }
    }

    pub(crate) fn recv_window_update(&mut self, wu: WindowUpdate) {}
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
