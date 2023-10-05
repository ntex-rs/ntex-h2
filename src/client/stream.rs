use std::task::{Context, Poll};
use std::{cell::RefCell, collections::VecDeque, fmt, pin::Pin, rc::Rc};

use ntex_bytes::Bytes;
use ntex_http::HeaderMap;
use ntex_service::{Service, ServiceCtx};
use ntex_util::future::{poll_fn, Either, Ready};
use ntex_util::{task::LocalWaker, HashMap, Stream as FutStream};

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
                let mut msgs = VecDeque::new();
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
pub struct SendStream(StreamRef, InflightStorage);

impl Drop for SendStream {
    fn drop(&mut self) {
        if !self.0.send_state().is_closed() {
            self.0.reset(Reason::CANCEL);
        }
    }
}

impl SendStream {
    #[inline]
    pub fn available_send_capacity(&self) -> WindowSize {
        self.0.available_send_capacity()
    }

    #[inline]
    pub async fn send_capacity(&self) -> Result<WindowSize, OperationError> {
        self.0.send_capacity().await
    }

    #[inline]
    /// Send payload
    pub async fn send_payload(&self, res: Bytes, eof: bool) -> Result<(), OperationError> {
        self.0.send_payload(res, eof).await
    }

    #[inline]
    pub fn send_trailers(&self, map: HeaderMap) {
        self.0.send_trailers(map)
    }

    #[inline]
    /// Reset stream
    pub fn reset(&self, reason: Reason) {
        self.0.reset(reason)
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
    /// Attempt to pull out the next value of http/2 stream
    pub async fn recv(&self) -> Option<Message> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Attempt to pull out the next value of this http/2 stream, registering
    /// the current task for wakeup if the value is not yet available,
    /// and returning None if the stream is exhausted.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<Message>> {
        let mut inner = self.1 .0.inflight.borrow_mut();
        if let Some(inflight) = inner.get_mut(&self.0.id()) {
            if let Some(msg) = inflight.pop() {
                let to_remove = match msg.kind() {
                    MessageKind::Headers { eof, .. } => *eof,
                    MessageKind::Eof(..) | MessageKind::Disconnect(..) => true,
                    _ => false,
                };
                if to_remove {
                    inner.remove(&self.0.id());
                }
                Poll::Ready(Some(msg))
            } else {
                inflight.waker.register(cx.waker());
                Poll::Pending
            }
        } else {
            Poll::Ready(None)
        }
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        if !self.0.recv_state().is_closed() {
            self.0.reset(Reason::CANCEL);
        }
        self.1 .0.inflight.borrow_mut().remove(&self.0.id());
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
    type Future<'f> = Ready<(), ()>;

    fn call<'a>(&'a self, msg: Message, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        let id = msg.id();
        if let Some(inflight) = self.0 .0.inflight.borrow_mut().get_mut(&id) {
            let eof = match msg.kind() {
                MessageKind::Headers { eof, .. } => *eof,
                MessageKind::Eof(..) | MessageKind::Disconnect(..) => true,
                _ => false,
            };
            if eof {
                self.0.notify(id);
                log::debug!("Stream {:?} is closed, notify", id);
            }
            inflight.push(msg);
        }
        Ready::Ok(())
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
        let snd = SendStream(stream.clone(), self.clone());
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
