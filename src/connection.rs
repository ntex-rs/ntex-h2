use std::{cell::RefCell, rc::Rc};

use ntex_bytes::ByteString;
use ntex_http::{HeaderMap, Method};
use ntex_io::IoRef;
use ntex_util::{time::Seconds, HashMap};

use crate::frame::{self, Headers, PseudoHeaders, StreamId, WindowSize};
use crate::{codec::Codec, error::ProtocolError, stream::Stream};

#[derive(Debug)]
pub(crate) struct Config {
    /// Initial window size of locally initiated streams
    pub local_init_window_sz: WindowSize,

    /// Initial window size of remote initiated streams
    pub remote_init_window_sz: WindowSize,

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

    /// Maximum number of remote initiated streams
    pub remote_max_initiated: Option<usize>,
}

const HTTP_SCHEME: ByteString = ByteString::from_static("http");
const HTTPS_SCHEME: ByteString = ByteString::from_static("https");

bitflags::bitflags! {
    struct Flags: u8 {
        const WAITING_ACK = 0b0000_0001;
    }
}

#[derive(Clone, Debug)]
pub struct Connection(Rc<RefCell<ConnectionInner>>);

#[derive(Debug)]
pub(crate) struct ConnectionInner {
    pub(crate) io: IoRef,
    pub(crate) codec: Rc<Codec>,
    cfg: Config,
    flags: Flags,
    streams: HashMap<StreamId, Stream>,
    last_stream_id: StreamId,
}

impl Connection {
    pub(crate) fn new(cfg: Config, io: IoRef, codec: Rc<Codec>) -> Self {
        Connection(Rc::new(RefCell::new(ConnectionInner {
            io,
            codec,
            cfg,
            flags: Flags::WAITING_ACK,
            streams: HashMap::default(),
            last_stream_id: 1.into(),
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

    pub(crate) fn send_request(
        &self,
        method: Method,
        path: ByteString,
        headers: HeaderMap,
    ) -> Stream {
        let stream = {
            let mut con = self.0.borrow_mut();
            let id = con.last_stream_id.next_id().unwrap();
            con.last_stream_id = id;
            let stream = Stream::new(id, self.0.clone());
            con.streams.insert(id, stream.clone());
            stream
        };

        let pseudo = PseudoHeaders {
            method: Some(method),
            path: Some(path),
            scheme: Some(HTTP_SCHEME),
            ..Default::default()
        };
        stream.send_headers(Headers::new(stream.id(), pseudo, headers));
        stream
    }

    pub(crate) fn recv_settings(&self, settings: frame::Settings) -> Result<(), ProtocolError> {
        log::trace!("processing incoming SETTINGS: {:#?}", settings);

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
            inner
                .io
                .encode(frame::Settings::ack().into(), &inner.codec)?;
        }
        Ok(())
    }

    pub(crate) fn recv_window_update(&self, frm: frame::WindowUpdate) -> Result<(), ProtocolError> {
        log::trace!("processing incoming WINDOW_UPDATE: {:#?}", frm);

        Ok(())
    }
}
