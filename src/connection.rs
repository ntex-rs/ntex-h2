use std::{cell::Cell, cell::RefCell, fmt, mem, rc::Rc};

use ntex_bytes::Bytes;
use ntex_http::{HeaderMap, StatusCode};
use ntex_io::IoRef;
use ntex_util::{time::Seconds, HashMap};
use slab::Slab;

use crate::error::{ProtocolError, StreamError};
use crate::frame::{self, Data, GoAway, Headers, PseudoHeaders, Reason, StreamId, WindowSize};
use crate::{codec::Codec, message::Message};

#[derive(Debug)]
pub struct Config {
    /// Initial window size of locally initiated streams
    pub local_init_window_sz: WindowSize,

    /// Initial maximum number of locally initiated streams.
    /// After receiving a Settings frame from the remote peer,
    /// the connection will overwrite this value with the
    /// MAX_CONCURRENT_STREAMS specified in the frame.
    pub initial_max_send_streams: usize,

    /// The stream ID to start the next local stream with
    pub local_next_stream_id: StreamId,

    /// If extended connect protocol is enabled.
    pub extended_connect_protocol_enabled: bool,

    /// How long a locally reset stream should ignore frames
    pub local_reset_duration: Seconds,

    /// Maximum number of locally reset streams to keep at a time
    pub local_reset_max: usize,

    /// Initial window size of remote initiated streams
    pub remote_init_window_sz: WindowSize,

    /// Maximum number of remote initiated streams
    pub remote_max_initiated: Option<usize>,
}

bitflags::bitflags! {
    struct Flags: u8 {
        const WAITING_ACK = 0b0000_0001;
    }
}

#[derive(Clone)]
pub struct Stream(Rc<StreamInner>);

#[derive(Clone)]
pub struct Connection(Rc<RefCell<ConnectionInner>>);

struct ConnectionInner {
    io: IoRef,
    codec: Rc<Codec>,
    cfg: Config,
    flags: Flags,
    streams: HashMap<StreamId, Stream>,
}

struct StreamInner {
    /// The h2 stream identifier
    pub id: StreamId,
    recv: Cell<HalfState>,
    send: Cell<HalfState>,
    ///// Send data flow control
    //pub send_flow: FlowControl,
    ///// Receive data flow control
    //pub recv_flow: FlowControl,
    connection: Rc<RefCell<ConnectionInner>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HalfState {
    Headers,
    Payload,
    Closed,
}

impl Stream {
    fn new(id: StreamId, connection: Rc<RefCell<ConnectionInner>>) -> Self {
        Stream(Rc::new(StreamInner {
            id,
            connection,
            recv: Cell::new(HalfState::Headers),
            send: Cell::new(HalfState::Headers),
        }))
    }

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

                let con = self.0.connection.borrow();
                con.io.encode(hdrs.into(), &con.codec).unwrap();
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
            HalfState::Payload => Err(StreamError::UnexpectedHeadersFrame),
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
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("Stream");
        builder
            .field("stream_id", &self.0.id)
            .field("recv_state", &self.0.recv)
            .finish()
    }
}

impl Connection {
    pub(crate) fn new(cfg: Config, io: IoRef, codec: Rc<Codec>) -> Self {
        Connection(Rc::new(RefCell::new(ConnectionInner {
            cfg,
            io,
            codec,
            flags: Flags::WAITING_ACK,
            streams: HashMap::default(),
        })))
    }

    pub(crate) fn get(&self, id: StreamId) -> Stream {
        self.0
            .borrow_mut()
            .streams
            .entry(id)
            .or_insert_with(|| Stream::new(id, self.0.clone()))
            .clone()
    }

    pub(crate) fn query(&self, id: StreamId) -> Option<Stream> {
        self.0.borrow_mut().streams.get(&id).map(|s| s.clone())
    }

    pub(crate) fn recv_settings(&self, settings: frame::Settings) -> Result<(), ProtocolError> {
        log::trace!("processing SETTINGS: {:#?}", settings);

        let mut inner = self.0.borrow_mut();
        if settings.is_ack() {
            if inner.flags.contains(Flags::WAITING_ACK) {
                inner.flags.remove(Flags::WAITING_ACK);
            } else {
                // We haven't sent any SETTINGS frames to be ACKed, so
                // this is very bizarre! Remote is either buggy or malicious.
                proto_err!(conn: "received unexpected settings ack");
                return Err(ProtocolError::UnexpectedSettingsAck);
            }
        } else {
            inner.io.encode(frame::Settings::ack().into(), &inner.codec);
        }
        Ok(())
    }
}
