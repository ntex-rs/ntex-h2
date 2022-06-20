use std::{cell::Cell, cmp::Ordering, fmt, ops, mem, rc::Rc, task::Context, task::Poll};

use ntex_bytes::Bytes;
use ntex_http::{HeaderMap, StatusCode};
use ntex_util::{future::poll_fn, task::LocalWaker};

use crate::error::{ProtocolError, StreamError, StreamErrorKind};
use crate::frame::{
    Data, Headers, PseudoHeaders, Reason, Reset, StreamId, WindowSize, WindowUpdate,
};
use crate::{connection::ConnectionInner, flow::FlowControl, frame, message::Message};

pub struct Stream(StreamRef);

#[derive(Debug)]
pub struct Capacity {
    size: Cell<u32>,
    stream: Rc<StreamInner>,
}

impl Capacity {
    fn new(size: u32, stream: &Rc<StreamInner>) -> Self {
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

#[derive(Clone, Debug)]
pub struct StreamRef(pub(crate) Rc<StreamInner>);

#[derive(Debug)]
pub(crate) struct StreamInner {
    /// The h2 stream identifier
    id: StreamId,
    /// Receive part
    recv: Cell<HalfState>,
    recv_flow: Cell<FlowControl>,
    recv_size: Cell<u32>,
    /// Send part
    send: Cell<HalfState>,
    send_flow: Cell<FlowControl>,
    send_waker: LocalWaker,
    /// Connection config
    con: Rc<ConnectionInner>,
    /// error state
    error: Cell<Option<StreamErrorKind>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HalfState {
    Headers,
    Payload,
    Closed,
}

impl StreamInner {
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
            recv_size: Cell::new(0),
            send: Cell::new(HalfState::Headers),
            send_flow: Cell::new(send_flow),
            send_waker: LocalWaker::new(),
            error: Cell::new(None),
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

        self.0
            .con
            .io
            .encode(hdrs.into(), &self.0.con.codec)
            .unwrap();
    }

    pub(crate) fn recv_headers(&self, hdrs: Headers) -> Option<Message> {
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
                Some(Message::new(pseudo, headers, eof, self))
            }
            HalfState::Payload => {
                // trailers
                self.0.recv.set(HalfState::Closed);
                Some(Message::trailers(hdrs.into_fields(), self))
            }
            HalfState::Closed => None,
        }
    }

    pub(crate) fn recv_data(&self, data: Data) -> Result<Option<Message>, ProtocolError> {
        let cap = Capacity::new(data.payload().len() as u32, &self.0);
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
                    Ok(Some(Message::eof_data(data.into_payload(), self)))
                } else {
                    Ok(Some(Message::data(data.into_payload(), cap, self)))
                }
            }
            HalfState::Headers => Err(ProtocolError::StreamIdle("DATA framed received")),
            HalfState::Closed => Ok(None),
        }
    }

    pub(crate) fn recv_rst_stream(&self, frm: Reset) {
        self.0.recv.set(HalfState::Closed);
        self.0.send.set(HalfState::Closed);
        self.0.error.set(Some(StreamErrorKind::Reset(frm.reason())));
        self.0.send_waker.wake();
    }

    pub(crate) fn recv_window_update(&self, frm: WindowUpdate) -> Result<(), StreamError> {
        if frm.size_increment() == 0 {
            Err(StreamError::new(
                self.clone(),
                StreamErrorKind::ZeroWindowUpdateValue,
            ))
        } else {
            let flow = self
                .0
                .send_flow
                .get()
                .inc_window(frm.size_increment())
                .map_err(|e| StreamError::new(self.clone(), StreamErrorKind::LocalReason(e)))?;
            self.0.send_flow.set(flow);

            if flow.window_size() > 0 {
                self.0.send_waker.wake();
            }
            Ok(())
        }
    }

    pub(crate) fn update_send_window(&self, upd: i32) -> Result<(), ProtocolError> {
        let flow = match upd.cmp(&0) {
            Ordering::Less => self.0.send_flow.get().dec_window(upd.abs() as u32), // We must decrease the (remote) window
            Ordering::Greater => self
                .0
                .send_flow
                .get()
                .inc_window(upd as u32)
                .map_err(|_| ProtocolError::Reason(Reason::FLOW_CONTROL_ERROR))?,
            Ordering::Equal => return Ok(()),
        };
        self.0.send_flow.set(flow);
        Ok(())
    }

    pub(crate) fn update_recv_window(&self, upd: i32) -> Result<Option<WindowSize>, ProtocolError> {
        let mut flow = match upd.cmp(&0) {
            Ordering::Less => self.0.recv_flow.get().dec_window(upd.abs() as u32), // We must decrease the (local) window
            Ordering::Greater => self
                .0
                .recv_flow
                .get()
                .inc_window(upd as u32)
                .map_err(|_| ProtocolError::Reason(Reason::FLOW_CONTROL_ERROR))?,
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

    pub(crate) fn into_stream(self) -> Stream {
        Stream(self)
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

    pub async fn send_payload(&self, mut res: Bytes, eof: bool) -> Result<(), StreamError> {
        match self.0.send.get() {
            HalfState::Payload => loop {
                let win = self.available_send_capacity();
                if win > 0 {
                    let mut data = if (win as usize) >= res.len() {
                        Data::new(self.0.id, mem::replace(&mut res, Bytes::new()))
                    } else {
                        Data::new(self.0.id, res.split_to(win as usize))
                    };
                    if eof && res.is_empty() {
                        data.set_end_stream();
                        self.0.send.set(HalfState::Closed);
                    }

                    self.0
                        .con
                        .io
                        .encode(data.into(), &self.0.con.codec)
                        .unwrap();
                    if res.is_empty() {
                        return Ok(());
                    }
                }
                self.send_capacity().await?;
            },
            _ => Ok(()),
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

    pub fn available_send_capacity(&self) -> WindowSize {
        self.0.send_flow.get().window_size()
    }

    pub async fn send_capacity(&self) -> Result<WindowSize, StreamError> {
        poll_fn(|cx| self.poll_send_capacity(cx)).await
    }

    pub fn poll_send_capacity(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<WindowSize, StreamError>> {
        if let Some(kind) = self.0.error.get() {
            Poll::Ready(Err(StreamError::new(self.clone(), kind)))
        } else {
            let win = self.0.send_flow.get().window_size();
            if win > 0 {
                Poll::Ready(Ok(win))
            } else {
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
            .field("stream_id", &self.0.0.id)
            .field("recv_state", &self.0.0.recv.get())
            .field("send_state", &self.0.0.send.get())
            .finish()
    }
}
