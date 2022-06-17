use std::{cell, fmt, future::Future, marker, pin::Pin, rc::Rc, task::Context, task::Poll};

use ntex_io::DispatchItem;
use ntex_service::Service;
use ntex_util::{future::Either, future::Ready, ready, HashMap};

use crate::control::{ControlMessage, ControlResult};
use crate::error::{ProtocolError, StreamError};
use crate::frame::{self, Data, Frame, GoAway, Headers, Reason, StreamId};
use crate::{codec::Codec, connection::Connection, message::Message, stream::Stream};

/// Amqp server dispatcher service.
pub(crate) struct Dispatcher<Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>>,
    Pub: Service<Message>,
{
    inner: Rc<Inner<Ctl>>,
    publish: Pub,
    connection: Connection,
    shutdown: cell::RefCell<Shutdown<Ctl::Future>>,
}

enum Shutdown<F> {
    NotSet,
    Done,
    InProcess(Pin<Box<F>>),
}

struct Inner<Ctl> {
    control: Ctl,
    last_stream_id: StreamId,
}

impl<Ctl, Pub> Dispatcher<Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult>,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()>,
    Pub::Error: fmt::Debug,
{
    pub(crate) fn new(connection: Connection, control: Ctl, publish: Pub) -> Self {
        Dispatcher {
            publish,
            connection,
            inner: Rc::new(Inner {
                control,
                last_stream_id: 0.into(),
            }),
            shutdown: cell::RefCell::new(Shutdown::NotSet),
        }
    }

    fn handle_proto_error(
        &self,
        result: Result<(), ProtocolError>,
    ) -> Either<
        PublishResponse<Pub, Ctl>,
        Either<Ready<Option<Frame>, ()>, ControlResponse<Pub::Error, Ctl>>,
    > {
        match result {
            Ok(()) => Either::Right(Either::Left(Ready::Ok(None))),
            Err(err) => Either::Right(Either::Right(ControlResponse::new(
                ControlMessage::proto_error(err),
                &self.inner,
            ))),
        }
    }

    fn recv_headers(
        &self,
        hdrs: Headers,
    ) -> Either<
        PublishResponse<Pub, Ctl>,
        Either<Ready<Option<Frame>, ()>, ControlResponse<Pub::Error, Ctl>>,
    > {
        let id = hdrs.stream_id();
        let eos = hdrs.is_end_stream();
        let stream = self.connection.get(id);
        match stream.recv_headers(hdrs) {
            Ok(msg) => Either::Left(PublishResponse::new(
                self.publish.call(msg),
                stream,
                &self.inner,
            )),
            Err(err) => Either::Right(Either::Right(ControlResponse::new(
                ControlMessage::stream_error(err, stream.clone()),
                &self.inner,
            ))),
        }
    }

    pub(crate) fn recv_data(
        &self,
        data: Data,
    ) -> Either<
        PublishResponse<Pub, Ctl>,
        Either<Ready<Option<Frame>, ()>, ControlResponse<Pub::Error, Ctl>>,
    > {
        let id = data.stream_id();
        let eos = data.is_end_stream();
        let stream = self.connection.query(id);

        if let Some(mut stream) = stream {
            match stream.recv_data(data) {
                Ok(msg) => Either::Left(PublishResponse::new(
                    self.publish.call(msg),
                    stream,
                    &self.inner,
                )),
                Err(err) => Either::Right(Either::Right(ControlResponse::new(
                    ControlMessage::stream_error(err, stream.clone()),
                    &self.inner,
                ))),
            }
        } else {
            Either::Right(Either::Right(ControlResponse::new(
                ControlMessage::proto_error(frame::Error::InvalidStreamId.into()),
                &self.inner,
            )))
        }
    }
}

impl<Ctl, Pub> Service<DispatchItem<Rc<Codec>>> for Dispatcher<Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()>,
    Pub::Error: fmt::Debug,
{
    type Response = Option<Frame>;
    type Error = ();
    type Future = Either<
        PublishResponse<Pub, Ctl>,
        Either<Ready<Self::Response, Self::Error>, ControlResponse<Pub::Error, Ctl>>,
    >;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // check publish service readiness
        let res1 = self.publish.poll_ready(cx);

        // check control service readiness
        let res2 = self.inner.control.poll_ready(cx);

        if res1.is_pending() || res2.is_pending() {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let mut shutdown = self.shutdown.borrow_mut();
        if matches!(&*shutdown, &Shutdown::NotSet) {
            // self.inner.sink.drop_sink();
            *shutdown = Shutdown::InProcess(Box::pin(
                self.inner
                    .control
                    .call(ControlMessage::terminated(is_error)),
            ));
        }

        let shutdown_ready = match &mut *shutdown {
            Shutdown::NotSet => panic!("guard above"),
            Shutdown::Done => true,
            Shutdown::InProcess(ref mut fut) => {
                let res = fut.as_mut().poll(cx);
                if res.is_ready() {
                    *shutdown = Shutdown::Done;
                    true
                } else {
                    false
                }
            }
        };

        if shutdown_ready {
            let res1 = self.publish.poll_shutdown(cx, is_error);
            let res2 = self.inner.control.poll_shutdown(cx, is_error);
            if res1.is_pending() || res2.is_pending() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        } else {
            Poll::Pending
        }
    }

    fn call(&self, request: DispatchItem<Rc<Codec>>) -> Self::Future {
        match request {
            DispatchItem::Item(frame) => match frame {
                Frame::Headers(hdrs) => self.recv_headers(hdrs),
                Frame::Data(data) => self.recv_data(data),
                Frame::Settings(settings) => {
                    self.handle_proto_error(self.connection.recv_settings(settings))
                }
                Frame::WindowUpdate(update) => {
                    log::trace!("processing WINDOW_UPDATE: {:#?}", update);
                    Either::Right(Either::Left(Ready::Ok(None)))
                }
                Frame::Reset(reset) => {
                    log::trace!("processing RESET: {:#?}", reset);
                    Either::Right(Either::Left(Ready::Ok(None)))
                }
                Frame::Ping(ping) => {
                    log::trace!("processing PING: {:#?}", ping);
                    Either::Right(Either::Left(Ready::Ok(None)))
                }
                Frame::GoAway(frm) => {
                    log::trace!("processing GoAway: {:#?}", frm);
                    Either::Right(Either::Right(ControlResponse::new(
                        ControlMessage::go_away(frm),
                        &self.inner,
                    )))
                }
                Frame::Priority(prio) => {
                    log::trace!("PRIORITY frame is not supported: {:#?}", prio);
                    Either::Right(Either::Left(Ready::Ok(None)))
                }
            },
            DispatchItem::EncoderError(_err) => {
                // let frame = ControlMessage::new_kind(ControlFrameKind::ProtocolError(err.into()));
                // *self.ctl_fut.borrow_mut() =
                // Some((Some(frame.clone()), Box::pin(self.inner.control.call(frame))));
                Either::Right(Either::Left(Ready::Ok(None)))
            }
            DispatchItem::DecoderError(_err) => {
                // let frame = ControlMessage::new_kind(ControlFrameKind::ProtocolError(err.into()));
                // *self.ctl_fut.borrow_mut() =
                // Some((Some(frame.clone()), Box::pin(self.inner.control.call(frame))));
                Either::Right(Either::Left(Ready::Ok(None)))
            }
            DispatchItem::KeepAliveTimeout => {
                // let frame = ControlMessage::new_kind(ControlFrameKind::ProtocolError(
                //     AmqpProtocolError::KeepAliveTimeout,
                // ));
                //*self.ctl_fut.borrow_mut() =
                //    Some((Some(frame.clone()), Box::pin(self.inner.control.call(frame))));
                Either::Right(Either::Left(Ready::Ok(None)))
            }
            DispatchItem::Disconnect(err) => Either::Right(Either::Right(ControlResponse::new(
                ControlMessage::peer_gone(err),
                &self.inner,
            ))),
            DispatchItem::WBackPressureEnabled | DispatchItem::WBackPressureDisabled => {
                Either::Right(Either::Left(Ready::Ok(None)))
            }
        }
    }
}

pin_project_lite::pin_project! {
    /// Publish service response future
    pub(crate) struct PublishResponse<P: Service<Message>, C: Service<ControlMessage<P::Error>>> {
        stream: Stream,
        #[pin]
        state: PublishResponseState<P, C>,
        inner: Rc<Inner<C>>,
    }
}

pin_project_lite::pin_project! {
    #[project = PublishResponseStateProject]
    enum PublishResponseState<P: Service<Message>, C: Service<ControlMessage<P::Error>>> {
        Publish { #[pin] fut: P::Future },
        Control { #[pin] fut: ControlResponse<P::Error, C> },
    }
}

impl<P, C> PublishResponse<P, C>
where
    P: Service<Message, Response = ()>,
    P::Error: fmt::Debug,
    C: Service<ControlMessage<P::Error>, Response = ControlResult>,
    C::Error: fmt::Debug,
{
    fn new(fut: P::Future, stream: Stream, inner: &Rc<Inner<C>>) -> Self {
        Self {
            stream,
            inner: inner.clone(),
            state: PublishResponseState::Publish { fut },
        }
    }
}

impl<P, C> Future for PublishResponse<P, C>
where
    P: Service<Message, Response = ()>,
    P::Error: fmt::Debug,
    C: Service<ControlMessage<P::Error>, Response = ControlResult>,
    C::Error: fmt::Debug,
{
    type Output = Result<Option<Frame>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            PublishResponseStateProject::Publish { fut } => match fut.poll(cx) {
                Poll::Ready(Ok(_)) => Poll::Ready(Ok(None)),
                Poll::Ready(Err(e)) => {
                    this.state.set(PublishResponseState::Control {
                        fut: ControlResponse::new(
                            ControlMessage::app_error(e, this.stream.clone()),
                            &this.inner,
                        ),
                    });
                    return self.poll(cx);
                }
                Poll::Pending => return Poll::Pending,
            },
            PublishResponseStateProject::Control { fut } => fut.poll(cx),
        }
    }
}

pin_project_lite::pin_project! {
    /// Control service response future
    pub(crate) struct ControlResponse<E, Ctl: Service<ControlMessage<E>>> {
        #[pin]
        fut: Ctl::Future,
        inner: Rc<Inner<Ctl>>,
        _t: marker::PhantomData<E>,
    }
}

impl<E, Ctl> ControlResponse<E, Ctl>
where
    E: fmt::Debug,
    Ctl: Service<ControlMessage<E>, Response = ControlResult>,
{
    fn new(pkt: ControlMessage<E>, inner: &Rc<Inner<Ctl>>) -> Self {
        Self {
            fut: inner.control.call(pkt),
            inner: inner.clone(),
            _t: marker::PhantomData,
        }
    }
}

impl<E, Ctl> Future for ControlResponse<E, Ctl>
where
    E: fmt::Debug,
    Ctl: Service<ControlMessage<E>, Response = ControlResult>,
{
    type Output = Result<Option<Frame>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match ready!(this.fut.poll(cx)) {
            Ok(item) => Poll::Ready(Ok(item.frame)),
            Err(err) => {
                // we cannot handle control service errors, close connection
                Poll::Ready(Ok(Some(
                    GoAway::new(Reason::INTERNAL_ERROR)
                        .set_last_stream_id(this.inner.last_stream_id)
                        .into(),
                )))
            }
        }
    }
}
