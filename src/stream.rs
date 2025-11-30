use std::{cell::Cell, cmp, fmt, future::poll_fn, mem, ops, rc::Rc, task::Context, task::Poll};

use ntex_bytes::Bytes;
use ntex_http::{HeaderMap, StatusCode, header::CONTENT_LENGTH};
use ntex_util::task::LocalWaker;

use crate::error::{OperationError, StreamError};
use crate::frame::{
    Data, Headers, PseudoHeaders, Reason, Reset, StreamId, WindowSize, WindowUpdate,
};
use crate::{connection::Connection, frame, message::Message, window::Window};

/// HTTP/2 Stream
pub struct Stream(StreamRef);

/// Stream capacity information
#[derive(Debug)]
pub struct Capacity {
    size: Cell<u32>,
    stream: Rc<StreamState>,
}

impl Capacity {
    fn new(size: u32, stream: &Rc<StreamState>) -> Self {
        stream.add_recv_capacity(size);

        Self {
            size: Cell::new(size),
            stream: stream.clone(),
        }
    }

    #[inline]
    /// Size of capacity
    pub fn size(&self) -> usize {
        self.size.get() as usize
    }

    /// Consume specified amount of capacity.
    ///
    /// Panics if provided size larger than capacity.
    pub fn consume(&self, sz: u32) {
        let size = self.size.get();
        if let Some(sz) = size.checked_sub(sz) {
            log::trace!(
                "{}: {:?} capacity consumed from {} to {}",
                self.stream.tag(),
                self.stream.id,
                size,
                sz
            );
            self.size.set(sz);
            self.stream.consume_capacity(size - sz);
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

/// State related to a stream's content-length validation
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ContentLength {
    Omitted,
    Head,
    Remaining(u64),
}

#[derive(Clone, Debug)]
pub struct StreamRef(pub(crate) Rc<StreamState>);

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    struct StreamFlags: u8 {
        const REMOTE = 0b0000_0001;
        const FAILED = 0b0000_0010;
        const DISCONNECT_ON_DROP = 0b0000_0100;
        const WAIT_FOR_CAPACITY  = 0b0000_1000;
    }
}

pub(crate) struct StreamState {
    /// The h2 stream identifier
    id: StreamId,
    flags: Cell<StreamFlags>,
    content_length: Cell<ContentLength>,
    /// Receive part
    recv: Cell<HalfState>,
    recv_window: Cell<Window>,
    recv_size: Cell<u32>,
    /// Send part
    send: Cell<HalfState>,
    send_window: Cell<Window>,
    send_cap: LocalWaker,
    send_reset: LocalWaker,
    /// Connection config
    pub(crate) con: Connection,
    /// error state
    error: Cell<Option<OperationError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HalfState {
    Idle,
    Payload,
    Closed(Option<Reason>),
}

impl HalfState {
    pub(crate) fn is_closed(&self) -> bool {
        matches!(self, HalfState::Closed(_))
    }
}

impl StreamState {
    fn tag(&self) -> &'static str {
        self.con.tag()
    }

    fn state_send_payload(&self) {
        self.send.set(HalfState::Payload);
    }

    fn state_send_close(&self, reason: Option<Reason>) {
        log::trace!(
            "{}: {:?} send side is closed with reason {:?}",
            self.tag(),
            self.id,
            reason
        );
        self.send.set(HalfState::Closed(reason));
        self.send_cap.wake();
        self.review_state();
    }

    fn state_recv_payload(&self) {
        self.recv.set(HalfState::Payload);
    }

    fn state_recv_close(&self, reason: Option<Reason>) {
        log::trace!("{}: {:?} receive side is closed", self.tag(), self.id);
        self.recv.set(HalfState::Closed(reason));
        self.review_state();
    }

    fn reset_stream(&self, reason: Option<Reason>) {
        if !self.recv.get().is_closed() {
            self.recv.set(HalfState::Closed(reason));
            if let Some(reason) = reason {
                self.error.set(Some(OperationError::LocalReset(reason)));
            }
        }
        self.send.set(HalfState::Closed(None));
        self.review_state();
    }

    fn remote_reset_stream(&self, reason: Reason) {
        self.recv.set(HalfState::Closed(None));
        self.send.set(HalfState::Closed(Some(reason)));
        self.error.set(Some(OperationError::RemoteReset(reason)));
        self.review_state();
    }

    fn failed(&self, err: OperationError) {
        if !self.recv.get().is_closed() {
            self.recv.set(HalfState::Closed(None));
        }
        if !self.send.get().is_closed() {
            self.send.set(HalfState::Closed(None));
        }
        self.error.set(Some(err));
        self.insert_flag(StreamFlags::FAILED);
        self.review_state();
    }

    fn insert_flag(&self, f: StreamFlags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn remove_flag(&self, f: StreamFlags) {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
    }

    fn check_error(&self) -> Result<(), OperationError> {
        if let Some(err) = self.error.take() {
            self.error.set(Some(err.clone()));
            Err(err)
        } else {
            Ok(())
        }
    }

    fn review_state(&self) {
        if self.recv.get().is_closed() {
            self.send_reset.wake();

            if let HalfState::Closed(reason) = self.send.get() {
                // stream is closed
                if let Some(reason) = reason {
                    log::trace!(
                        "{}: {:?} is closed with remote reset {:?}, dropping stream",
                        self.tag(),
                        self.id,
                        reason
                    );
                } else {
                    log::trace!(
                        "{}: {:?} both sides are closed, dropping stream",
                        self.tag(),
                        self.id
                    );
                }
                self.send_cap.wake();
                self.con.drop_stream(self.id);
            }
        }
    }

    /// added new capacity, update recevice window size
    fn add_recv_capacity(&self, size: u32) {
        let cap = self.recv_size.get();
        self.recv_size.set(cap + size);
        self.recv_window.set(self.recv_window.get().dec(size));
        log::trace!(
            "{}: {:?} capacity incresed from {} to {}",
            self.tag(),
            self.id,
            cap,
            cap + size
        );

        // connection level recv window
        self.con.add_recv_capacity(size);
    }

    /// check and update recevice window size
    fn consume_capacity(&self, size: u32) {
        let cap = self.recv_size.get();
        let size = cap - size;
        log::trace!(
            "{}: {:?} capacity decresed from {} to {}",
            self.tag(),
            self.id,
            cap,
            size
        );

        self.recv_size.set(size);
        let mut window = self.recv_window.get();
        if let Some(val) = window.update(
            size,
            self.con.config().window_sz,
            self.con.config().window_sz_threshold,
        ) {
            log::trace!(
                "{}: {:?} capacity decresed below threshold {} increase by {} ({})",
                self.tag(),
                self.id,
                self.con.config().window_sz_threshold,
                val,
                self.con.config().window_sz,
            );
            self.recv_window.set(window);
            self.con.encode(WindowUpdate::new(self.id, val));
        }
    }
}

impl StreamRef {
    pub(crate) fn new(id: StreamId, remote: bool, con: Connection) -> Self {
        // if peer has accepted settings, we can use local config window size
        // otherwise use default window size
        let recv_window = if con.settings_processed() {
            Window::new(con.config().window_sz as i32)
        } else {
            Window::new(frame::DEFAULT_INITIAL_WINDOW_SIZE as i32)
        };
        let send_window = Window::new(con.remote_window_size() as i32);

        StreamRef(Rc::new(StreamState {
            id,
            con,
            recv: Cell::new(HalfState::Idle),
            recv_window: Cell::new(recv_window),
            recv_size: Cell::new(0),
            send: Cell::new(HalfState::Idle),
            send_window: Cell::new(send_window),
            send_cap: LocalWaker::new(),
            send_reset: LocalWaker::new(),
            error: Cell::new(None),
            content_length: Cell::new(ContentLength::Omitted),
            flags: Cell::new(if remote {
                StreamFlags::REMOTE
            } else {
                StreamFlags::empty()
            }),
        }))
    }

    #[inline]
    pub fn id(&self) -> StreamId {
        self.0.id
    }

    #[inline]
    pub fn tag(&self) -> &'static str {
        self.0.con.tag()
    }

    /// Check if stream has been opened from remote side
    #[inline]
    pub fn is_remote(&self) -> bool {
        self.0.flags.get().contains(StreamFlags::REMOTE)
    }

    /// Check if stream has failed
    #[inline]
    pub fn is_failed(&self) -> bool {
        self.0.flags.get().contains(StreamFlags::FAILED)
    }

    pub(crate) fn send_state(&self) -> HalfState {
        self.0.send.get()
    }

    pub(crate) fn recv_state(&self) -> HalfState {
        self.0.recv.get()
    }

    pub(crate) fn disconnect_on_drop(&self) {
        self.0.insert_flag(StreamFlags::DISCONNECT_ON_DROP);
    }

    pub(crate) fn is_disconnect_on_drop(&self) -> bool {
        self.0.flags.get().contains(StreamFlags::DISCONNECT_ON_DROP)
    }

    /// Reset stream
    #[inline]
    pub fn reset(&self, reason: Reason) {
        if !self.0.recv.get().is_closed() || !self.0.send.get().is_closed() {
            self.0.con.encode(Reset::new(self.0.id, reason));
            self.0.reset_stream(Some(reason));
        }
    }

    /// Get capacity instance for current stream
    #[inline]
    pub fn empty_capacity(&self) -> Capacity {
        Capacity {
            size: Cell::new(0),
            stream: self.0.clone(),
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
        log::trace!(
            "{}: send headers {:#?} eos: {:?}",
            self.tag(),
            hdrs,
            hdrs.is_end_stream()
        );

        if hdrs
            .pseudo()
            .status
            .is_some_and(|status| status.is_informational())
        {
            self.0.content_length.set(ContentLength::Head)
        }
        self.0.con.encode(hdrs);
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
            "{}: processing HEADERS for {:?}:\n{:#?}\nrecv_state:{:?}, send_state: {:?}",
            self.tag(),
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
            "{}: processing DATA frame for {:?}, len: {:?}",
            self.tag(),
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

    pub(crate) fn recv_rst_stream(&self, frm: &Reset) {
        self.0.remote_reset_stream(frm.reason())
    }

    pub(crate) fn recv_window_update_connection(&self) {
        if self.0.flags.get().contains(StreamFlags::WAIT_FOR_CAPACITY)
            && self.0.send_window.get().window_size() > 0
        {
            self.0.send_cap.wake();
        }
    }

    pub(crate) fn recv_window_update(&self, frm: WindowUpdate) -> Result<(), StreamError> {
        if frm.size_increment() == 0 {
            Err(StreamError::WindowZeroUpdateValue)
        } else {
            let window = self
                .0
                .send_window
                .get()
                .inc(frm.size_increment())
                .map_err(|_| StreamError::WindowOverflowed)?;
            self.0.send_window.set(window);

            if window.window_size() > 0 {
                self.0.send_cap.wake();
            }
            Ok(())
        }
    }

    pub(crate) fn update_send_window(&self, upd: i32) -> Result<(), StreamError> {
        let orig = self.0.send_window.get();
        let window = match upd.cmp(&0) {
            cmp::Ordering::Less => orig.dec(upd.unsigned_abs()), // We must decrease the (remote) window
            cmp::Ordering::Greater => orig
                .inc(upd as u32)
                .map_err(|_| StreamError::WindowOverflowed)?,
            cmp::Ordering::Equal => return Ok(()),
        };
        log::trace!(
            "{}: Updating send window size from {} to {}",
            self.tag(),
            orig.window_size,
            window.window_size
        );
        self.0.send_window.set(window);
        Ok(())
    }

    pub(crate) fn update_recv_window(&self, upd: i32) -> Result<Option<WindowSize>, StreamError> {
        let mut window = match upd.cmp(&0) {
            cmp::Ordering::Less => self.0.recv_window.get().dec(upd.unsigned_abs()), // We must decrease the (local) window
            cmp::Ordering::Greater => self
                .0
                .recv_window
                .get()
                .inc(upd as u32)
                .map_err(|_| StreamError::WindowOverflowed)?,
            cmp::Ordering::Equal => return Ok(None),
        };
        if let Some(val) = window.update(
            self.0.recv_size.get(),
            self.0.con.config().window_sz,
            self.0.con.config().window_sz_threshold,
        ) {
            self.0.recv_window.set(window);
            Ok(Some(val))
        } else {
            self.0.recv_window.set(window);
            Ok(None)
        }
    }

    /// Send stream response
    pub fn send_response(
        &self,
        status: StatusCode,
        headers: HeaderMap,
        eof: bool,
    ) -> Result<(), OperationError> {
        self.0.check_error()?;

        match self.0.send.get() {
            HalfState::Idle => {
                let pseudo = PseudoHeaders::response(status);
                let mut hdrs = Headers::new(self.0.id, pseudo, headers, eof);

                if eof {
                    hdrs.set_end_stream();
                    self.0.state_send_close(None);
                } else {
                    self.0.state_send_payload();
                }
                self.0.con.encode(hdrs);
                Ok(())
            }
            HalfState::Payload => Err(OperationError::Payload),
            HalfState::Closed(r) => Err(OperationError::Closed(r)),
        }
    }

    /// Send payload
    pub async fn send_payload(&self, mut res: Bytes, eof: bool) -> Result<(), OperationError> {
        match self.0.send.get() {
            HalfState::Payload => {
                // check if stream is disconnected
                self.0.check_error()?;

                log::trace!(
                    "{}: {:?} sending {} bytes, eof: {}, send: {:?}",
                    self.0.tag(),
                    self.0.id,
                    res.len(),
                    eof,
                    self.0.send.get()
                );

                // eof and empty data
                if eof && res.is_empty() {
                    let mut data = Data::new(self.0.id, Bytes::new());
                    data.set_end_stream();
                    self.0.state_send_close(None);

                    // write to io buffer
                    self.0.con.encode(data);
                    return Ok(());
                }

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
                                "{}: {:?} sending {} out of {} bytes",
                                self.0.tag(),
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
                            .send_window
                            .set(self.0.send_window.get().dec(size as u32));

                        // update connection send window
                        self.0.con.consume_send_window(size as u32);

                        // write to io buffer
                        self.0.con.encode(data);
                        if res.is_empty() {
                            return Ok(());
                        }
                    } else {
                        log::trace!(
                            "{}: Not enough sending capacity for {:?} remaining {:?}",
                            self.0.tag(),
                            self.0.id,
                            res.len()
                        );
                        // wait for available send window
                        self.send_capacity().await?;
                    }
                }
            }
            HalfState::Idle => Err(OperationError::Idle),
            HalfState::Closed(reason) => Err(OperationError::Closed(reason)),
        }
    }

    /// Send client trailers and close stream
    pub fn send_trailers(&self, map: HeaderMap) {
        if self.0.send.get() == HalfState::Payload {
            let mut hdrs = Headers::trailers(self.0.id, map);
            hdrs.set_end_headers();
            hdrs.set_end_stream();
            self.0.con.encode(hdrs);
            self.0.state_send_close(None);
        }
    }

    pub fn available_send_capacity(&self) -> WindowSize {
        cmp::min(
            self.0.send_window.get().window_size(),
            self.0.con.send_window_size(),
        )
    }

    pub async fn send_capacity(&self) -> Result<WindowSize, OperationError> {
        poll_fn(|cx| self.poll_send_capacity(cx)).await
    }

    /// Check for available send capacity
    pub fn poll_send_capacity(&self, cx: &Context<'_>) -> Poll<Result<WindowSize, OperationError>> {
        self.0.check_error()?;
        self.0.con.check_error()?;

        let win = self.available_send_capacity();
        if win > 0 {
            self.0.remove_flag(StreamFlags::WAIT_FOR_CAPACITY);
            Poll::Ready(Ok(win))
        } else {
            self.0.insert_flag(StreamFlags::WAIT_FOR_CAPACITY);
            self.0.send_cap.register(cx.waker());
            Poll::Pending
        }
    }

    /// Check if send part of stream get reset
    pub fn poll_send_reset(&self, cx: &Context<'_>) -> Poll<Result<(), OperationError>> {
        if self.0.send.get().is_closed() {
            Poll::Ready(Ok(()))
        } else {
            self.0.check_error()?;
            self.0.con.check_error()?;
            self.0.send_reset.register(cx.waker());
            Poll::Pending
        }
    }
}

impl PartialEq for StreamRef {
    fn eq(&self, other: &StreamRef) -> bool {
        Rc::as_ptr(&self.0) == Rc::as_ptr(&other.0)
    }
}

impl ops::Deref for Stream {
    type Target = StreamRef;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.0.reset(Reason::CANCEL);
    }
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("Stream");
        builder
            .field("stream_id", &self.0.0.id)
            .field("recv_state", &self.0.0.recv.get())
            .field("send_state", &self.0.0.send.get())
            .finish()
    }
}

impl fmt::Debug for StreamState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("StreamState");
        builder
            .field("id", &self.id)
            .field("recv", &self.recv.get())
            .field("recv_window", &self.recv_window.get())
            .field("recv_size", &self.recv_size.get())
            .field("send", &self.send.get())
            .field("send_window", &self.send_window.get())
            .field("flags", &self.flags.get())
            .finish()
    }
}

pub fn parse_u64(src: &[u8]) -> Option<u64> {
    if src.len() > 19 {
        // At danger for overflow...
        None
    } else {
        let mut ret = 0;
        for &d in src {
            if !d.is_ascii_digit() {
                return None;
            }

            ret *= 10;
            ret += (d - b'0') as u64;
        }

        Some(ret)
    }
}
