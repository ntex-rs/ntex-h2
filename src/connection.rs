use std::{cell::RefCell, rc::Rc};

use ntex_io::IoRef;
use ntex_util::{time::Seconds, HashMap};
use slab::Slab;

use crate::codec::Codec;
use crate::error::ProtocolError;
use crate::frame::{self, GoAway, Reason, StreamId, WindowSize};
use crate::stream::Stream;

#[derive(Debug)]
pub struct Config {
    /// Initial window size of locally initiated streams
    pub local_init_window_sz: WindowSize,

    /// Initial maximum number of locally initiated streams.
    /// After receiving a Settings frame from the remote peer,
    /// the connection will overwrite this value with the
    /// MAX_CONCURRENT_STREAMS specified in the frame.
    pub initial_max_send_streams: usize,

    /// Max amount of DATA bytes to buffer per stream.
    pub local_max_buffer_size: usize,

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
pub struct Connection {
    inner: Rc<RefCell<ConnectionInner>>,
}

struct ConnectionInner {
    cfg: Config,
    io: IoRef,
    codec: Rc<Codec>,
    flags: Flags,
    streams: HashMap<StreamId, Stream>,
}

impl Connection {
    pub(crate) fn new(cfg: Config, io: IoRef, codec: Rc<Codec>) -> Self {
        Connection {
            inner: Rc::new(RefCell::new(ConnectionInner {
                cfg,
                io,
                codec,
                flags: Flags::WAITING_ACK,
                streams: HashMap::default(),
            })),
        }
    }

    pub(crate) fn recv_settings(&self, settings: frame::Settings) -> Result<(), ProtocolError> {
        log::trace!("processing SETTINGS: {:#?}", settings);

        let mut inner = self.inner.borrow_mut();
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

    pub(crate) fn recv_headers(&self, hdrs: frame::Headers) -> Result<(), ProtocolError> {
        log::trace!("processing HEADERS: {:?}", hdrs);
        let (p, hdrs) = hdrs.into_parts();
        log::trace!("HEADERS: {:#?}\n{:#?}", p, hdrs);
        Ok(())
    }

    pub(crate) fn recv_data(&self, data: frame::Data) -> Result<(), ProtocolError> {
        log::trace!("processing DATA: {:?}", data.payload().len());
        Ok(())
    }
}
