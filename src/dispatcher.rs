use std::{cell, fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use ntex_io::DispatchItem;
use ntex_rt::spawn;
use ntex_service::{Container, ContainerCall, Service, ServiceCtx};
use ntex_util::future::{join_all, BoxFuture, Either, Ready};
use ntex_util::{ready, HashMap};

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
    shutdown: cell::RefCell<Shutdown<BoxFuture<'static, ()>>>,
}

enum Shutdown<F> {
    NotSet,
    Done,
    InProcess(F),
}

struct Inner<Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>>,
    Pub: Service<Message>,
{
    control: Container<Ctl>,
    publish: Container<Pub>,
    connection: Rc<ConnectionState>,
    last_stream_id: StreamId,
}

type ServiceFut<'f, Pub, Ctl> = Either<
    PublishResponse<'f, Pub, Ctl>,
    Either<Ready<Option<Frame>, ()>, ControlResponse<'f, Ctl, Pub>>,
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
                control: Container::new(control),
                publish: Container::new(publish),
                last_stream_id: 0.into(),
                connection: connection.get_state(),
            }),
            connection,
        }
    }

    fn handle_message(
        &self,
        result: Result<Option<(StreamRef, Message)>, Either<ConnectionError, StreamErrorInner>>,
    ) -> ServiceFut<'_, Pub, Ctl> {
        match result {
            Ok(Some((stream, msg))) => Either::Left(PublishResponse::new(msg, stream, &self.inner)),
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
                    Message::error(kind, &stream),
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
                let futs = streams
                    .into_values()
                    .map(|stream| inner.publish.call(Message::disconnect(err.clone(), stream)));
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
    type Future<'f> = Either<
        PublishResponse<'f, Pub, Ctl>,
        Either<Ready<Self::Response, Self::Error>, ControlResponse<'f, Ctl, Pub>>,
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

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut shutdown = self.shutdown.borrow_mut();
        if matches!(&*shutdown, &Shutdown::NotSet) {
            let inner = self.inner.clone();
            *shutdown = Shutdown::InProcess(Box::pin(async move {
                let _ = inner.control.call(ControlMessage::terminated()).await;
            }));
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
            let res1 = self.inner.publish.poll_shutdown(cx);
            let res2 = self.inner.control.poll_shutdown(cx);
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

    fn call<'a>(
        &'a self,
        request: DispatchItem<Codec>,
        _: ServiceCtx<'a, Self>,
    ) -> Self::Future<'a> {
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
                            let inner = self.inner.clone();
                            ntex_rt::spawn(async move {
                                let _ = PublishResponse::<Pub, Ctl>::new(
                                    Message::error(kind, &stream),
                                    stream,
                                    &inner,
                                )
                                .await;
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
                log::warn!("did not receive pong response in time, closing connection");
                let streams = self.connection.ping_timeout();
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
    pub(crate) struct PublishResponse<'f, P: Service<Message>, C: Service<ControlMessage<P::Error>>> {
        stream: StreamRef,
        #[pin]
        state: PublishResponseState<'f, P, C>,
        inner: &'f Inner<C, P>,
    }
}

pin_project_lite::pin_project! {
    #[project = PublishResponseStateProject]
    enum PublishResponseState<'f, P: Service<Message>, C: Service<ControlMessage<P::Error>>>
    where P: 'f
    {
        Publish { #[pin] fut: ContainerCall<'f, P, Message> },
        Control { #[pin] fut: ControlResponse<'f, C, P> },
    }
}

impl<'f, P, C> PublishResponse<'f, P, C>
where
    P: Service<Message, Response = ()>,
    P::Error: fmt::Debug,
    C: Service<ControlMessage<P::Error>, Response = ControlResult>,
    C::Error: fmt::Debug,
{
    fn new(msg: Message, stream: StreamRef, inner: &'f Inner<C, P>) -> Self {
        let state = PublishResponseState::Publish {
            fut: inner.publish.container_call(msg),
        };
        Self {
            state,
            stream,
            inner,
        }
    }
}

impl<'f, P, C> Future for PublishResponse<'f, P, C>
where
    P: Service<Message, Response = ()>,
    P::Error: fmt::Debug,
    C: Service<ControlMessage<P::Error>, Response = ControlResult>,
    C::Error: fmt::Debug,
{
    type Output = Result<Option<Frame>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        loop {
            return match this.state.as_mut().project() {
                PublishResponseStateProject::Publish { fut } => {
                    if this.stream.is_remote() {
                        match this.stream.poll_send_reset(cx) {
                            Poll::Ready(Ok(())) | Poll::Ready(Err(_)) => {
                                log::trace!("Stream is closed {:?}", this.stream.id());
                                return Poll::Ready(Ok(None));
                            }
                            Poll::Pending => (),
                        }
                    }

                    match fut.poll(cx) {
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
                                //this = self.as_mut().project();
                                continue;
                            }
                        }
                        Poll::Pending => Poll::Pending,
                    }
                }
                PublishResponseStateProject::Control { fut } => fut.poll(cx),
            };
        }
    }
}

pin_project_lite::pin_project! {
    /// Control service response future
    pub(crate) struct ControlResponse<'f, Ctl: Service<ControlMessage<Pub::Error>>, Pub>
    where
        Ctl: Service<ControlMessage<Pub::Error>>,
        Ctl: 'f,
        Pub: Service<Message>,
        Pub: 'f,
    {
        #[pin]
        fut: ContainerCall<'f, Ctl, ControlMessage<Pub::Error>>,
        inner: &'f Inner<Ctl, Pub>,
    }
}

impl<'f, Ctl, Pub> ControlResponse<'f, Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult>,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message>,
    Pub::Error: fmt::Debug,
{
    fn new(pkt: ControlMessage<Pub::Error>, inner: &'f Inner<Ctl, Pub>) -> Self {
        Self {
            fut: inner.control.container_call(pkt),
            inner,
        }
    }
}

impl<'f, Ctl, Pub> Future for ControlResponse<'f, Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult>,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message>,
    Pub::Error: fmt::Debug,
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
