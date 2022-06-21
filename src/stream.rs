use std::{cell::Cell, cmp, cmp::Ordering, fmt, mem, ops, rc::Rc, task::Context, task::Poll};

use ntex_bytes::Bytes;
use ntex_http::{header::CONTENT_LENGTH, HeaderMap, StatusCode};
use ntex_util::{future::poll_fn, task::LocalWaker};

use crate::error::{OperationError, StreamError};
use crate::frame::{
    Data, Headers, PseudoHeaders, Reason, Reset, StreamId, WindowSize, WindowUpdate,
};
use crate::{connection::ConnectionState, flow::FlowControl, frame, message::Message};

pub struct Stream(StreamRef);

#[derive(Debug)]
pub struct Capacity {
    size: Cell<u32>,
    stream: Rc<StreamState>,
}

impl Capacity {
    fn new(size: u32, stream: &Rc<StreamState>) -> Self {
        stream.add_capacity(size);

        Self {
            size: Cell::new(size),
            stream: stream.clone(),
        }
    }

    /// Consume specified amount of capacity.
    ///
    /// Panics if provided size larger than capacity.
    pub fn consume(&self, sz: u32) {
        if let Some(sz) = self.size.get().checked_sub(sz) {
            log::trace!(
                "{:?} capacity consumed from {} to {}",
                self.stream.id,
                self.size.get(),
                sz
            );
            self.size.set(sz);
            self.stream.consume_capacity(sz);
        } else {
            panic!("Capacity overflow");
        }
    }
}

/// Panics if capacity belongs to different streams
impl ops::Add for Capacity {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        if Rc::ptr_eq(&self.stream, &other.stream) {
            let size = Cell::new(self.size.get() + other.size.get());
            self.size.set(0);
            other.size.set(0);
            Self {
                size,
                stream: self.stream.clone(),
            }
        } else {
            panic!("Cannot add capacity from different streams");
        }
    }
}

/// Panics if capacity belongs to different streams
impl ops::AddAssign for Capacity {
    fn add_assign(&mut self, other: Self) {
        if Rc::ptr_eq(&self.stream, &other.stream) {
            let size = self.size.get() + other.size.get();
            self.size.set(size);
            other.size.set(0);
        } else {
            panic!("Cannot add capacity from different streams");
        }
    }
}

impl Drop for Capacity {
    fn drop(&mut self) {
        let size = self.size.get();
        if size > 0 {
            self.stream.consume_capacity(size);
        }
    }
}

/// State related to validating a stream's content-length
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ContentLength {
    Omitted,
    Head,
    Remaining(u64),
}

#[derive(Clone, Debug)]
pub struct StreamRef(pub(crate) Rc<StreamState>);

pub(crate) struct StreamState {
    /// The h2 stream identifier
    id: StreamId,
    remote: bool,
    content_length: Cell<ContentLength>,
    /// Receive part
    recv: Cell<HalfState>,
    recv_flow: Cell<FlowControl>,
    recv_size: Cell<u32>,
    /// Send part
    send: Cell<HalfState>,
    send_flow: Cell<FlowControl>,
    send_waker: LocalWaker,
    /// Connection config
    con: Rc<ConnectionState>,
    /// error state
    error: Cell<Option<OperationError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HalfState {
    Idle,
    Payload,
    Closed(Option<Reason>),
}

impl HalfState {
    fn is_closed(&self) -> bool {
        matches!(self, HalfState::Closed(_))
    }
}

impl StreamState {
    fn state_send_payload(&self) {
        self.send.set(HalfState::Payload);
    }

    fn state_send_close(&self, reason: Option<Reason>) {
        log::trace!("{:?} send half is closed with reason {:?}", self.id, reason);
        self.send.set(HalfState::Closed(reason));
        self.review_state();
    }

    fn state_recv_payload(&self) {
        self.recv.set(HalfState::Payload);
    }

    fn state_recv_close(&self, reason: Option<Reason>) {
        log::trace!("{:?} receive half is closed", self.id);
        self.recv.set(HalfState::Closed(reason));
        self.send_waker.wake();
        self.review_state();
    }

    fn reset_stream(&self, reason: Option<Reason>) {
        self.recv.set(HalfState::Closed(None));
        self.send.set(HalfState::Closed(reason));
        if let Some(reason) = reason {
            self.error.set(Some(OperationError::RemoteReset(reason)));
        }
        self.send_waker.wake();
        self.review_state();
    }

    fn remote_reset_stream(&self, reason: Reason) {
        self.recv.set(HalfState::Closed(Some(reason)));
        self.send.set(HalfState::Closed(None));
        self.error.set(Some(OperationError::RemoteReset(reason)));
        self.send_waker.wake();
        self.review_state();
    }

    fn failed(&self, err: OperationError) {
        self.recv.set(HalfState::Closed(None));
        self.send.set(HalfState::Closed(None));
        self.error.set(Some(err));
        self.send_waker.wake();
        self.review_state();
    }

    fn review_state(&self) {
        if self.recv.get().is_closed() {
            if let HalfState::Closed(reason) = self.send.get() {
                // stream is closed
                if reason.is_some() {
                    log::trace!("{:?} is closed with local reset, dropping stream", self.id);
                    self.con.drop_stream(self.id, &self.con);
                } else {
                    log::trace!("{:?} both halfs are closed, dropping stream", self.id);
                    self.con.drop_stream(self.id, &self.con);
                }
            }
        }
    }

    /// added new capacity, update recevice window size
    fn add_capacity(&self, size: u32) {
        let cap = self.recv_size.get();
        self.recv_size.set(cap + size);
        self.recv_flow.set(self.recv_flow.get().dec_window(size));
        log::trace!("{:?} capacity incresed from {} to {}", self.id, cap, size);

        // connection level recv flow
        self.con.add_capacity(size);
    }

    /// check and update recevice window size
    fn consume_capacity(&self, size: u32) {
        let cap = self.recv_size.get();
        let size = cap - size;
        log::trace!("{:?} capacity decresed from {} to {}", self.id, cap, size);
        self.recv_size.set(size);

        let mut flow = self.recv_flow.get();
        if let Some(val) = flow.update_window(
            size,
            self.con.local_config.window_sz,
            self.con.local_config.window_sz_threshold,
        ) {
            log::trace!(
                "{:?} capacity decresed below threshold {} increase by {} ({})",
                self.id,
                self.con.local_config.window_sz_threshold,
                val,
                self.con.local_config.window_sz,
            );
            self.recv_flow.set(flow);
            self.con
                .io
                .encode(WindowUpdate::new(self.id, val).into(), &self.con.codec)
                .unwrap();
        }
    }
}

impl StreamRef {
    pub(crate) fn new(id: StreamId, remote: bool, con: Rc<ConnectionState>) -> Self {
        // if peer has accepted settings, we can use local config window size
        // otherwise use default window size
        let recv_flow = if con.settings_processed.get() {
            FlowControl::new(con.local_config.window_sz as i32)
        } else {
            FlowControl::new(frame::DEFAULT_INITIAL_WINDOW_SIZE as i32)
        };
        let send_flow = FlowControl::new(con.remote_window_sz.get() as i32);

        StreamRef(Rc::new(StreamState {
            id,
            con,
            remote,
            recv: Cell::new(HalfState::Idle),
            recv_flow: Cell::new(recv_flow),
            recv_size: Cell::new(0),
            send: Cell::new(HalfState::Idle),
            send_flow: Cell::new(send_flow),
            send_waker: LocalWaker::new(),
            error: Cell::new(None),
            content_length: Cell::new(ContentLength::Omitted),
        }))
    }

    #[inline]
    pub fn id(&self) -> StreamId {
        self.0.id
    }

    /// Check if stream has been opened from remote side
    #[inline]
    pub fn is_remote(&self) -> bool {
        self.0.remote
    }

    /// Check if stream has failed
    #[inline]
    pub fn is_failed(&self) -> bool {
        if let Some(e) = self.0.error.take() {
            self.0.error.set(Some(e));
            true
        } else {
            false
        }
    }

    #[inline]
    pub(crate) fn into_stream(self) -> Stream {
        Stream(self)
    }

    pub(crate) fn send_headers(&self, mut hdrs: Headers) {
        hdrs.set_end_headers();
        if hdrs.is_end_stream() {
            self.0.state_send_close(None);
        } else {
            self.0.state_send_payload();
        }
        log::trace!("send headers {:#?}", hdrs);

        if hdrs
            .pseudo()
            .status
            .map_or(false, |status| status.is_informational())
        {
            self.0.content_length.set(ContentLength::Head)
        }
        self.0
            .con
            .io
            .encode(hdrs.into(), &self.0.con.codec)
            .unwrap();
    }

    pub(crate) fn set_failed(&self, reason: Option<Reason>) {
        self.0.reset_stream(reason);
    }

    pub(crate) fn set_go_away(&self, reason: Reason) {
        self.0.remote_reset_stream(reason)
    }

    pub(crate) fn set_failed_stream(&self, err: OperationError) {
        self.0.failed(err);
    }

    pub(crate) fn recv_headers(&self, hdrs: Headers) -> Result<Option<Message>, StreamError> {
        log::trace!(
            "processing HEADERS for {:?}:\n{:#?}\nrecv_state:{:?}, send_state: {:?}",
            self.0.id,
            hdrs,
            self.0.recv.get(),
            self.0.send.get(),
        );

        match self.0.recv.get() {
            HalfState::Idle => {
                let eof = hdrs.is_end_stream();
                if eof {
                    self.0.state_recv_close(None);
                } else {
                    self.0.state_recv_payload();
                }
                let (pseudo, headers) = hdrs.into_parts();

                if self.0.content_length.get() != ContentLength::Head {
                    if let Some(content_length) = headers.get(CONTENT_LENGTH) {
                        if let Some(v) = parse_u64(content_length.as_bytes()) {
                            self.0.content_length.set(ContentLength::Remaining(v));
                        } else {
                            proto_err!(stream: "could not parse content-length; stream={:?}", self.0.id);
                            return Err(StreamError::InvalidContentLength);
                        }
                    }
                }
                Ok(Some(Message::new(pseudo, headers, eof, self)))
            }
            HalfState::Payload => {
                // trailers
                if !hdrs.is_end_stream() {
                    Err(StreamError::TrailersWithoutEos)
                } else {
                    self.0.state_recv_close(None);
                    Ok(Some(Message::trailers(hdrs.into_fields(), self)))
                }
            }
            HalfState::Closed(_) => Err(StreamError::Closed),
        }
    }

    pub(crate) fn recv_data(&self, data: Data) -> Result<Option<Message>, StreamError> {
        let cap = Capacity::new(data.payload().len() as u32, &self.0);
        log::trace!(
            "processing DATA frame for {:?}, len: {:?}",
            self.0.id,
            data.payload().len()
        );

        match self.0.recv.get() {
            HalfState::Payload => {
                let eof = data.is_end_stream();

                // Returns `Err` when the decrement cannot be completed due to overflow
                match self.0.content_length.get() {
                    ContentLength::Remaining(rem) => {
                        match rem.checked_sub(data.payload().len() as u64) {
                            Some(val) => {
                                self.0.content_length.set(ContentLength::Remaining(val));
                                if eof && val != 0 {
                                    return Err(StreamError::WrongPayloadLength);
                                }
                            }
                            None => return Err(StreamError::WrongPayloadLength),
                        }
                    }
                    ContentLength::Head => {
                        if !data.payload().is_empty() {
                            return Err(StreamError::NonEmptyPayload);
                        }
                    }
                    _ => (),
                }

                if eof {
                    self.0.state_recv_close(None);
                    Ok(Some(Message::eof_data(data.into_payload(), self)))
                } else {
                    Ok(Some(Message::data(data.into_payload(), cap, self)))
                }
            }
            HalfState::Idle => Err(StreamError::Idle("DATA framed received")),
            HalfState::Closed(_) => Err(StreamError::Closed),
        }
    }

    pub(crate) fn recv_rst_stream(&self, frm: Reset) {
        self.0.remote_reset_stream(frm.reason())
    }

    pub(crate) fn recv_window_update(&self, frm: WindowUpdate) -> Result<(), StreamError> {
        if frm.size_increment() == 0 {
            Err(StreamError::WindowZeroUpdateValue)
        } else {
            let flow = self
                .0
                .send_flow
                .get()
                .inc_window(frm.size_increment())
                .map_err(|_| StreamError::WindowOverflowed)?;
            self.0.send_flow.set(flow);

            if flow.window_size() > 0 {
                self.0.send_waker.wake();
            }
            Ok(())
        }
    }

    pub(crate) fn update_send_window(&self, upd: i32) -> Result<(), StreamError> {
        let orig = self.0.send_flow.get();
        let flow = match upd.cmp(&0) {
            Ordering::Less => orig.dec_window(upd.abs() as u32), // We must decrease the (remote) window
            Ordering::Greater => orig
                .inc_window(upd as u32)
                .map_err(|_| StreamError::WindowOverflowed)?,
            Ordering::Equal => return Ok(()),
        };
        log::trace!(
            "Updating send window size from {} to {}",
            orig.window_size,
            flow.window_size
        );
        self.0.send_flow.set(flow);
        Ok(())
    }

    pub(crate) fn update_recv_window(&self, upd: i32) -> Result<Option<WindowSize>, StreamError> {
        let mut flow = match upd.cmp(&0) {
            Ordering::Less => self.0.recv_flow.get().dec_window(upd.abs() as u32), // We must decrease the (local) window
            Ordering::Greater => self
                .0
                .recv_flow
                .get()
                .inc_window(upd as u32)
                .map_err(|_| StreamError::WindowOverflowed)?,
            Ordering::Equal => return Ok(None),
        };
        if let Some(val) = flow.update_window(
            self.0.recv_size.get(),
            self.0.con.local_config.window_sz,
            self.0.con.local_config.window_sz_threshold,
        ) {
            self.0.recv_flow.set(flow);
            Ok(Some(val))
        } else {
            self.0.recv_flow.set(flow);
            Ok(None)
        }
    }

    pub fn send_response(
        &self,
        status: StatusCode,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<(), OperationError> {
        match self.0.send.get() {
            HalfState::Idle => {
                let pseudo = PseudoHeaders::response(status);
                let mut hdrs = Headers::new(self.0.id, pseudo, headers);
                hdrs.set_end_headers();

                if eof {
                    hdrs.set_end_stream();
                    self.0.state_send_close(None);
                } else {
                    self.0.state_send_payload();
                }
                self.0
                    .con
                    .io
                    .encode(hdrs.into(), &self.0.con.codec)
                    .unwrap();
                Ok(())
            }
            HalfState::Payload => Err(OperationError::Payload),
            HalfState::Closed(r) => Err(OperationError::Closed(r)),
        }
    }

    pub async fn send_payload(&self, mut res: Bytes, eof: bool) -> Result<(), OperationError> {
        match self.0.send.get() {
            HalfState::Payload => {
                // check is stream is disconnected
                if let Some(e) = self.0.error.take() {
                    let res = e.clone();
                    self.0.error.set(Some(e));
                    return Err(res);
                }
                log::trace!(
                    "{:?} sending {} bytes, eof: {}, send: {:?}",
                    self.0.id,
                    res.len(),
                    eof,
                    self.0.send.get()
                );

                loop {
                    // calaculate available send window size
                    let win = self.available_send_capacity() as usize;
                    if win > 0 {
                        let size =
                            cmp::min(win, cmp::min(res.len(), self.0.con.remote_frame_size()));
                        let mut data = if size >= res.len() {
                            Data::new(self.0.id, mem::replace(&mut res, Bytes::new()))
                        } else {
                            log::trace!(
                                "{:?} sending {} out of {} bytes",
                                self.0.id,
                                size,
                                res.len()
                            );
                            Data::new(self.0.id, res.split_to(size))
                        };
                        if eof && res.is_empty() {
                            data.set_end_stream();
                            self.0.state_send_close(None);
                        }

                        // update send window
                        self.0
                            .send_flow
                            .set(self.0.send_flow.get().dec_window(size as u32));
                        // write to io buffer
                        self.0
                            .con
                            .io
                            .encode(data.into(), &self.0.con.codec)
                            .unwrap();
                        if res.is_empty() {
                            return Ok(());
                        }
                    }
                    // wait for available send window
                    self.send_capacity().await?;
                }
            }
            HalfState::Idle => Err(OperationError::Idle),
            HalfState::Closed(reason) => Err(OperationError::Closed(reason)),
        }
    }

    pub fn send_trailers(&self, map: HeaderMap) {
        if self.0.send.get() == HalfState::Payload {
            let mut hdrs = Headers::trailers(self.0.id, map);
            hdrs.set_end_headers();
            hdrs.set_end_stream();
            self.0
                .con
                .io
                .encode(hdrs.into(), &self.0.con.codec)
                .unwrap();
            self.0.state_send_close(None);
        }
    }

    pub fn available_send_capacity(&self) -> WindowSize {
        self.0.send_flow.get().window_size()
    }

    pub async fn send_capacity(&self) -> Result<WindowSize, OperationError> {
        poll_fn(|cx| self.poll_send_capacity(cx)).await
    }

    pub fn poll_send_capacity(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<WindowSize, OperationError>> {
        if let Some(err) = self.0.error.take() {
            self.0.error.set(Some(err.clone()));
            Poll::Ready(Err(err))
        } else if let Some(err) = self.0.con.error.take() {
            self.0.con.error.set(Some(err.clone()));
            Poll::Ready(Err(err))
        } else {
            let win = self.0.send_flow.get().window_size();
            if win > 0 {
                Poll::Ready(Ok(win))
            } else {
                self.0.send_waker.register(cx.waker());
                Poll::Pending
            }
        }
    }
}

impl ops::Deref for Stream {
    type Target = StreamRef;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("Stream");
        builder
            .field("stream_id", &self.0 .0.id)
            .field("recv_state", &self.0 .0.recv.get())
            .field("send_state", &self.0 .0.send.get())
            .finish()
    }
}

impl fmt::Debug for StreamState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("StreamState");
        builder
            .field("id", &self.id)
            .field("recv", &self.recv.get())
            .field("recv_flow", &self.recv_flow.get())
            .field("recv_size", &self.recv_size.get())
            .field("send", &self.send.get())
            .field("send_flow", &self.send_flow.get())
            .finish()
    }
}

pub fn parse_u64(src: &[u8]) -> Option<u64> {
    if src.len() > 19 {
        // At danger for overflow...
        return None;
    }

    let mut ret = 0;

    for &d in src {
        if !(b'0'..=b'9').contains(&d) {
            return None;
        }

        ret *= 10;
        ret += (d - b'0') as u64;
    }

    Some(ret)
}
