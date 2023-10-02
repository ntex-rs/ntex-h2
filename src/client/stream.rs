use std::task::{Context, Poll};
use std::{cell::RefCell, pin::Pin, rc::Rc};

use ntex_bytes::Bytes;
use ntex_http::HeaderMap;
use ntex_service::{Service, ServiceCtx};
use ntex_util::future::{Either, Ready};
use ntex_util::{task::LocalWaker, HashMap, Stream as FutStream};

use crate::error::OperationError;
use crate::frame::{Reason, StreamId, WindowSize};
use crate::message::{Message, MessageKind};
use crate::{Stream, StreamRef};

#[derive(Clone, Debug)]
pub(super) struct InflightStorage {
    inflight: Rc<RefCell<HashMap<StreamId, Inflight>>>,
}

#[derive(Debug)]
pub(super) struct Inflight {
    stream: Stream,
    response: Option<Either<Message, Vec<Message>>>,
    waker: LocalWaker,
}

impl Inflight {
    fn pop(&mut self) -> Option<Message> {
        match self.response.take() {
            None => None,
            Some(Either::Left(msg)) => Some(msg),
            Some(Either::Right(mut msgs)) => {
                let msg = msgs.pop();

                if !msgs.is_empty() {
                    self.response = Some(Either::Right(msgs));
                }
                msg
            }
        }
    }

    fn push(&mut self, item: Message) {
        match self.response.take() {
            Some(Either::Left(msg)) => self.response = Some(Either::Right(vec![msg, item])),
            Some(Either::Right(mut messages)) => {
                messages.push(item);
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
        self.0.reset(Reason::CANCEL);
        self.1.inflight.borrow_mut().remove(&self.0.id());
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

impl Drop for RecvStream {
    fn drop(&mut self) {
        self.0.reset(Reason::CANCEL);
        self.1.inflight.borrow_mut().remove(&self.0.id());
    }
}

impl FutStream for RecvStream {
    type Item = Message;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut inner = self.1.inflight.borrow_mut();
        if let Some(inflight) = inner.get_mut(&self.0.id()) {
            if let Some(mut msg) = inflight.pop() {
                let to_remove = match msg.kind() {
                    MessageKind::Headers { eof, .. } => *eof,
                    MessageKind::Eof(..) | MessageKind::Empty | MessageKind::Disconnect(..) => true,
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

struct HandleService(InflightStorage);

impl Service<Message> for HandleService {
    type Response = ();
    type Error = ();
    type Future<'f> = Ready<(), ()>;

    fn call<'a>(&'a self, msg: Message, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        let id = msg.id();
        if let Some(inflight) = self.0.inflight.borrow_mut().get_mut(&id) {
            inflight.push(msg);
        }
        Ready::Ok(())
    }
}