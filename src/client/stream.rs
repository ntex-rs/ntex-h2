use std::task::{Context, Poll};
use std::{cell::RefCell, collections::VecDeque, fmt, future::poll_fn, pin::Pin, rc::Rc};

use ntex_bytes::Bytes;
use ntex_http::HeaderMap;
use ntex_service::{Service, ServiceCtx};
use ntex_util::{HashMap, Stream as FutStream, future::Either, task::LocalWaker};

use crate::error::OperationError;
use crate::frame::{Reason, StreamId, WindowSize};
use crate::message::{Message, MessageKind};
use crate::{Stream, StreamRef};

#[derive(Clone, Default)]
pub(super) struct InflightStorage(Rc<InflightStorageInner>);

#[derive(Default)]
struct InflightStorageInner {
    inflight: RefCell<HashMap<StreamId, Inflight>>,
    cb: Option<Box<dyn Fn(StreamId)>>,
}

#[derive(Debug)]
pub(super) struct Inflight {
    _stream: Stream,
    response: Option<Either<Message, VecDeque<Message>>>,
    waker: LocalWaker,
}

impl Inflight {
    fn pop(&mut self) -> Option<Message> {
        match self.response.take() {
            None => None,
            Some(Either::Left(msg)) => Some(msg),
            Some(Either::Right(mut msgs)) => {
                let msg = msgs.pop_front();
                if !msgs.is_empty() {
                    self.response = Some(Either::Right(msgs));
                }
                msg
            }
        }
    }

    fn push(&mut self, item: Message) {
        match self.response.take() {
            Some(Either::Left(msg)) => {
                let mut msgs = VecDeque::with_capacity(8);
                msgs.push_back(msg);
                msgs.push_back(item);
                self.response = Some(Either::Right(msgs));
            }
            Some(Either::Right(mut messages)) => {
                messages.push_back(item);
                self.response = Some(Either::Right(messages));
            }
            None => self.response = Some(Either::Left(item)),
        };
        self.waker.wake();
    }
}

#[derive(Debug)]
/// Send part of the client stream
pub struct SendStream(StreamRef, ());

impl Drop for SendStream {
    fn drop(&mut self) {
        if !self.0.send_state().is_closed() {
            self.0.reset(Reason::CANCEL);

            if self.0.is_disconnect_on_drop() {
                self.0.0.con.disconnect_when_ready();
            }
        }
    }
}

impl SendStream {
    #[inline]
    /// Get stream id
    pub fn id(&self) -> StreamId {
        self.0.id()
    }

    #[inline]
    /// Get io tag
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[inline]
    pub fn stream(&self) -> &StreamRef {
        &self.0
    }

    #[inline]
    /// Get available capacity
    pub fn available_send_capacity(&self) -> WindowSize {
        self.0.available_send_capacity()
    }

    #[inline]
    /// Wait for available capacity
    pub async fn send_capacity(&self) -> Result<WindowSize, OperationError> {
        self.0.send_capacity().await
    }

    #[inline]
    /// Send payload
    pub async fn send_payload(&self, res: Bytes, eof: bool) -> Result<(), OperationError> {
        self.0.send_payload(res, eof).await
    }

    #[inline]
    /// Send trailers
    pub fn send_trailers(&self, map: HeaderMap) {
        self.0.send_trailers(map)
    }

    /// Reset stream
    ///
    /// Returns `true` if the stream state is updated and a `Reset` frame
    /// has been sent to the peer.
    #[inline]
    pub fn reset(&self, reason: Reason) -> bool {
        self.0.reset(reason)
    }

    #[inline]
    /// Disconnect connection on stream drop
    pub fn disconnect_on_drop(&self) {
        self.0.disconnect_on_drop()
    }

    #[inline]
    /// Check for available send capacity
    pub fn poll_send_capacity(&self, cx: &Context<'_>) -> Poll<Result<WindowSize, OperationError>> {
        self.0.poll_send_capacity(cx)
    }

    #[inline]
    /// Check if send part of stream get reset
    pub fn poll_send_reset(&self, cx: &Context<'_>) -> Poll<Result<(), OperationError>> {
        self.0.poll_send_reset(cx)
    }
}

#[derive(Debug)]
/// Receiving part of the client stream
pub struct RecvStream(StreamRef, InflightStorage);

impl RecvStream {
    #[inline]
    /// Get stream id
    pub fn id(&self) -> StreamId {
        self.0.id()
    }

    #[inline]
    /// Get io tag
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[inline]
    pub fn stream(&self) -> &StreamRef {
        &self.0
    }

    #[inline]
    /// Disconnect connection on stream drop
    pub fn disconnect_on_drop(&self) {
        self.0.disconnect_on_drop()
    }

    /// Attempt to pull out the next value of http/2 stream
    pub async fn recv(&self) -> Option<Message> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Attempt to pull out the next value of this http/2 stream, registering
    /// the current task for wakeup if the value is not yet available,
    /// and returning None if the stream is exhausted.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<Message>> {
        if let Some(inflight) = self.1.0.inflight.borrow_mut().get_mut(&self.0.id()) {
            if let Some(msg) = inflight.pop() {
                Poll::Ready(Some(msg))
            } else if self.0.recv_state().is_closed() {
                Poll::Ready(None)
            } else {
                inflight.waker.register(cx.waker());
                Poll::Pending
            }
        } else {
            log::warn!(
                "{}: Stream does not exists, {:?}",
                self.0.tag(),
                self.0.id()
            );
            Poll::Ready(None)
        }
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        if !self.0.recv_state().is_closed() {
            self.0.reset(Reason::CANCEL);

            if self.0.is_disconnect_on_drop() {
                self.0.0.con.disconnect_when_ready();
            }
        }
        self.1.0.inflight.borrow_mut().remove(&self.0.id());
    }
}

impl FutStream for RecvStream {
    type Item = Message;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_recv(cx)
    }
}

pub(super) struct HandleService(InflightStorage);

impl HandleService {
    pub(super) fn new(storage: InflightStorage) -> Self {
        Self(storage)
    }
}

impl Service<Message> for HandleService {
    type Response = ();
    type Error = ();

    async fn call(&self, msg: Message, _: ServiceCtx<'_, Self>) -> Result<(), ()> {
        let id = msg.id();
        if let Some(inflight) = self.0.0.inflight.borrow_mut().get_mut(&id) {
            let eof = match msg.kind() {
                MessageKind::Headers { eof, .. } => *eof,
                MessageKind::Eof(..) | MessageKind::Disconnect(..) => true,
                _ => false,
            };
            inflight.push(msg);

            if eof {
                self.0.notify(id);
                log::debug!("Stream {id:?} is closed, notify");
            }
        } else if !matches!(msg.kind(), MessageKind::Disconnect(_)) {
            log::error!(
                "{}: Received message for unknown stream, {msg:?}",
                msg.stream().tag(),
            );
        }
        Ok(())
    }
}

impl InflightStorage {
    pub(super) fn new<F>(f: F) -> Self
    where
        F: Fn(StreamId) + 'static,
    {
        InflightStorage(Rc::new(InflightStorageInner {
            inflight: Default::default(),
            cb: Some(Box::new(f)),
        }))
    }

    pub(super) fn notify(&self, id: StreamId) {
        if let Some(ref cb) = self.0.cb {
            (*cb)(id)
        }
    }

    pub(super) fn inflight(&self, stream: Stream) -> (SendStream, RecvStream) {
        let id = stream.id();
        let snd = SendStream(stream.clone(), ());
        let rcv = RecvStream(stream.clone(), self.clone());
        let inflight = Inflight {
            _stream: stream,
            response: None,
            waker: LocalWaker::default(),
        };
        self.0.inflight.borrow_mut().insert(id, inflight);
        (snd, rcv)
    }
}

impl fmt::Debug for InflightStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InflightStorage")
            .field("inflight", &self.0.inflight)
            .finish()
    }
}
