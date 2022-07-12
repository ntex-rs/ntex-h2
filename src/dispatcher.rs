use std::{cell, fmt, future::Future, marker, pin::Pin, rc::Rc, task::Context, task::Poll};

use ntex_io::DispatchItem;
use ntex_rt::spawn;
use ntex_service::Service;
use ntex_util::{future::join_all, future::Either, future::Ready, ready, HashMap};

use crate::connection::{Connection, ConnectionState};
use crate::control::{ControlMessage, ControlResult};
use crate::error::{ConnectionError, OperationError, StreamErrorInner};
use crate::frame::{Frame, GoAway, Ping, Reason, Reset, StreamId};
use crate::{codec::Codec, message::Message, stream::StreamRef};

/// Amqp server dispatcher service.
pub(crate) struct Dispatcher<Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>>,
    Pub: Service<Message>,
{
    inner: Rc<Inner<Ctl, Pub>>,
    connection: Connection,
    shutdown: cell::RefCell<Shutdown<Ctl::Future>>,
}

enum Shutdown<F> {
    NotSet,
    Done,
    InProcess(Pin<Box<F>>),
}

struct Inner<Ctl, Pub> {
    control: Ctl,
    publish: Pub,
    connection: Rc<ConnectionState>,
    last_stream_id: StreamId,
}

type ServiceFut<Pub, Ctl, E> = Either<
    PublishResponse<Pub, Ctl>,
    Either<Ready<Option<Frame>, ()>, ControlResponse<E, Ctl, Pub>>,
>;

impl<Ctl, Pub> Dispatcher<Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
{
    pub(crate) fn new(connection: Connection, control: Ctl, publish: Pub) -> Self {
        Dispatcher {
            shutdown: cell::RefCell::new(Shutdown::NotSet),
            inner: Rc::new(Inner {
                control,
                publish,
                last_stream_id: 0.into(),
                connection: connection.get_state(),
            }),
            connection,
        }
    }

    fn handle_message(
        &self,
        result: Result<Option<(StreamRef, Message)>, Either<ConnectionError, StreamErrorInner>>,
    ) -> ServiceFut<Pub, Ctl, Pub::Error> {
        match result {
            Ok(Some((stream, msg))) => Either::Left(PublishResponse::new(
                self.inner.publish.call(msg),
                stream,
                &self.inner,
            )),
            Ok(None) => Either::Right(Either::Left(Ready::Ok(None))),
            Err(Either::Left(err)) => {
                self.connection.proto_error(&err);
                Either::Right(Either::Right(ControlResponse::new(
                    ControlMessage::proto_error(err),
                    &self.inner,
                )))
            }
            Err(Either::Right(err)) => {
                let (stream, kind) = err.into_inner();
                stream.set_failed_stream(kind.into());

                let st = self.connection.state();
                let _ = st
                    .io
                    .encode(Reset::new(stream.id(), kind.reason()).into(), &st.codec);
                Either::Left(PublishResponse::new(
                    self.inner.publish.call(Message::error(kind, &stream)),
                    stream,
                    &self.inner,
                ))
            }
        }
    }

    fn handle_connection_error(&self, streams: HashMap<StreamId, StreamRef>, err: OperationError) {
        if !streams.is_empty() {
            let inner = self.inner.clone();
            spawn(async move {
                let futs = streams.into_iter().map(move |(_, stream)| {
                    inner.publish.call(Message::disconnect(err.clone(), stream))
                });
                let _ = join_all(futs).await;
            });
        }
    }
}

impl<Ctl, Pub> Service<DispatchItem<Codec>> for Dispatcher<Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
{
    type Response = Option<Frame>;
    type Error = ();
    type Future = Either<
        PublishResponse<Pub, Ctl>,
        Either<Ready<Self::Response, Self::Error>, ControlResponse<Pub::Error, Ctl, Pub>>,
    >;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // check publish service readiness
        let res1 = self.inner.publish.poll_ready(cx);

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
            let res1 = self.inner.publish.poll_shutdown(cx, is_error);
            let res2 = self.inner.control.poll_shutdown(cx, is_error);
            if res1.is_pending() || res2.is_pending() {
                Poll::Pending
            } else {
                self.connection.disconnect();
                Poll::Ready(())
            }
        } else {
            Poll::Pending
        }
    }

    fn call(&self, request: DispatchItem<Codec>) -> Self::Future {
        log::debug!("Handle h2 message: {:?}", request);

        match request {
            DispatchItem::Item(frame) => match frame {
                Frame::Headers(hdrs) => self.handle_message(self.connection.recv_headers(hdrs)),
                Frame::Data(data) => self.handle_message(self.connection.recv_data(data)),
                Frame::Settings(settings) => match self.connection.recv_settings(settings) {
                    Err(Either::Left(err)) => {
                        self.connection.proto_error(&err);
                        Either::Right(Either::Right(ControlResponse::new(
                            ControlMessage::proto_error(err),
                            &self.inner,
                        )))
                    }
                    Err(Either::Right(errs)) => {
                        // handle stream errors
                        for err in errs {
                            let (stream, kind) = err.into_inner();
                            stream.set_failed_stream(kind.into());

                            let st = self.connection.state();
                            let _ = st
                                .io
                                .encode(Reset::new(stream.id(), kind.reason()).into(), &st.codec);
                            let fut = PublishResponse::<Pub, Ctl>::new(
                                self.inner.publish.call(Message::error(kind, &stream)),
                                stream,
                                &self.inner,
                            );
                            ntex_rt::spawn(async move {
                                let _ = fut.await;
                            });
                        }
                        Either::Right(Either::Left(Ready::Ok(None)))
                    }
                    Ok(_) => Either::Right(Either::Left(Ready::Ok(None))),
                },
                Frame::WindowUpdate(update) => {
                    self.handle_message(self.connection.recv_window_update(update).map(|_| None))
                }
                Frame::Reset(reset) => {
                    self.handle_message(self.connection.recv_rst_stream(reset).map(|_| None))
                }
                Frame::Ping(ping) => {
                    log::trace!("processing PING: {:#?}", ping);
                    if ping.is_ack() {
                        self.connection.recv_pong(ping);
                        Either::Right(Either::Left(Ready::Ok(None)))
                    } else {
                        Either::Right(Either::Left(Ready::Ok(Some(
                            Ping::pong(ping.into_payload()).into(),
                        ))))
                    }
                }
                Frame::GoAway(frm) => {
                    log::trace!("processing GoAway: {:#?}", frm);
                    let reason = frm.reason();
                    let streams = self.connection.recv_go_away(reason, frm.data());
                    self.handle_connection_error(streams, ConnectionError::GoAway(reason).into());
                    Either::Right(Either::Right(ControlResponse::new(
                        ControlMessage::go_away(frm),
                        &self.inner,
                    )))
                }
                Frame::Priority(prio) => {
                    log::debug!("PRIORITY frame is not supported: {:#?}", prio);
                    Either::Right(Either::Left(Ready::Ok(None)))
                }
            },
            DispatchItem::EncoderError(err) => {
                let err = ConnectionError::from(err);
                let streams = self.connection.proto_error(&err);
                self.handle_connection_error(streams, err.into());
                Either::Right(Either::Right(ControlResponse::new(
                    ControlMessage::proto_error(err),
                    &self.inner,
                )))
            }
            DispatchItem::DecoderError(err) => {
                let err = ConnectionError::from(err);
                let streams = self.connection.proto_error(&err);
                self.handle_connection_error(streams, err.into());
                Either::Right(Either::Right(ControlResponse::new(
                    ControlMessage::proto_error(err),
                    &self.inner,
                )))
            }
            DispatchItem::KeepAliveTimeout => {
                let streams = self
                    .connection
                    .proto_error(&ConnectionError::KeepaliveTimeout);
                self.handle_connection_error(streams, ConnectionError::KeepaliveTimeout.into());
                Either::Right(Either::Right(ControlResponse::new(
                    ControlMessage::proto_error(ConnectionError::KeepaliveTimeout),
                    &self.inner,
                )))
            }
            DispatchItem::Disconnect(err) => {
                let streams = self.connection.disconnect();
                self.handle_connection_error(streams, OperationError::Disconnected);
                Either::Right(Either::Right(ControlResponse::new(
                    ControlMessage::peer_gone(err),
                    &self.inner,
                )))
            }
            DispatchItem::WBackPressureEnabled | DispatchItem::WBackPressureDisabled => {
                Either::Right(Either::Left(Ready::Ok(None)))
            }
        }
    }
}

pin_project_lite::pin_project! {
    /// Publish service response future
    pub(crate) struct PublishResponse<P: Service<Message>, C: Service<ControlMessage<P::Error>>> {
        stream: StreamRef,
        #[pin]
        state: PublishResponseState<P, C>,
        inner: Rc<Inner<C, P>>,
    }
}

pin_project_lite::pin_project! {
    #[project = PublishResponseStateProject]
    enum PublishResponseState<P: Service<Message>, C: Service<ControlMessage<P::Error>>> {
        Publish { #[pin] fut: P::Future },
        Control { #[pin] fut: ControlResponse<P::Error, C, P> },
    }
}

impl<P, C> PublishResponse<P, C>
where
    P: Service<Message, Response = ()>,
    P::Error: fmt::Debug,
    C: Service<ControlMessage<P::Error>, Response = ControlResult>,
    C::Error: fmt::Debug,
{
    fn new(fut: P::Future, stream: StreamRef, inner: &Rc<Inner<C, P>>) -> Self {
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
                    if this.inner.connection.is_disconnected() {
                        Poll::Ready(Ok(None))
                    } else {
                        this.state.set(PublishResponseState::Control {
                            fut: ControlResponse::new(
                                ControlMessage::app_error(e, this.stream.clone()),
                                this.inner,
                            ),
                        });
                        self.poll(cx)
                    }
                }
                Poll::Pending => Poll::Pending,
            },
            PublishResponseStateProject::Control { fut } => fut.poll(cx),
        }
    }
}

pin_project_lite::pin_project! {
    /// Control service response future
    pub(crate) struct ControlResponse<E, Ctl: Service<ControlMessage<E>>, Pub> {
        #[pin]
        fut: Ctl::Future,
        inner: Rc<Inner<Ctl, Pub>>,
        _t: marker::PhantomData<E>,
    }
}

impl<E, Ctl, Pub> ControlResponse<E, Ctl, Pub>
where
    E: fmt::Debug,
    Ctl: Service<ControlMessage<E>, Response = ControlResult>,
    Ctl::Error: fmt::Debug,
{
    fn new(pkt: ControlMessage<E>, inner: &Rc<Inner<Ctl, Pub>>) -> Self {
        Self {
            fut: inner.control.call(pkt),
            inner: inner.clone(),
            _t: marker::PhantomData,
        }
    }
}

impl<E, Ctl, Pub> Future for ControlResponse<E, Ctl, Pub>
where
    E: fmt::Debug,
    Ctl: Service<ControlMessage<E>, Response = ControlResult>,
    Ctl::Error: fmt::Debug,
{
    type Output = Result<Option<Frame>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match ready!(this.fut.poll(cx)) {
            Ok(res) => {
                if let Some(Frame::Reset(ref rst)) = res.frame {
                    if !rst.stream_id().is_zero() {
                        this.inner
                            .connection
                            .rst_stream(rst.stream_id(), rst.reason());
                    }
                }
                if let Some(frm) = res.frame {
                    let _ = this
                        .inner
                        .connection
                        .io
                        .encode(frm, &this.inner.connection.codec);
                }
                if res.disconnect {
                    this.inner.connection.io.close();
                }
            }
            Err(err) => {
                log::error!("control service has failed with {:?}", err);
                // we cannot handle control service errors, close connection
                let _ = this.inner.connection.io.encode(
                    GoAway::new(Reason::INTERNAL_ERROR)
                        .set_last_stream_id(this.inner.last_stream_id)
                        .into(),
                    &this.inner.connection.codec,
                );
                this.inner.connection.io.close();
            }
        }
        Poll::Ready(Ok(None))
    }
}
