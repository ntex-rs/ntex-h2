use std::{cell, fmt, future::poll_fn, future::Future, rc::Rc, task::Context, task::Poll};

use ntex_io::DispatchItem;
use ntex_rt::spawn;
use ntex_service::{Pipeline, Service, ServiceCtx};
use ntex_util::future::{join_all, BoxFuture, Either};
use ntex_util::HashMap;

use crate::connection::{Connection, RecvHalfConnection};
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
    connection: RecvHalfConnection,
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
    control: Ctl,
    publish: Pub,
    connection: Connection,
    last_stream_id: StreamId,
}

impl<Ctl, Pub> Dispatcher<Ctl, Pub>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
{
    pub(crate) fn new(connection: Connection, control: Ctl, publish: Pub) -> Self {
        Dispatcher {
            connection: connection.recv_half(),
            shutdown: cell::RefCell::new(Shutdown::NotSet),
            inner: Rc::new(Inner {
                control,
                publish,
                connection,
                last_stream_id: 0.into(),
            }),
        }
    }

    async fn handle_message<'f>(
        &'f self,
        result: Result<Option<(StreamRef, Message)>, Either<ConnectionError, StreamErrorInner>>,
        ctx: ServiceCtx<'f, Self>,
    ) -> Result<Option<Frame>, ()> {
        match result {
            Ok(Some((stream, msg))) => publish(msg, stream, &self.inner, ctx).await,
            Ok(None) => Ok(None),
            Err(Either::Left(err)) => {
                self.connection.proto_error(&err);
                control(ControlMessage::proto_error(err), &self.inner, ctx).await
            }
            Err(Either::Right(err)) => {
                let (stream, kind) = err.into_inner();
                stream.set_failed_stream(kind.into());

                self.connection
                    .encode(Reset::new(stream.id(), kind.reason()));
                publish(Message::error(kind, &stream), stream, &self.inner, ctx).await
            }
        }
    }

    fn handle_connection_error(&self, streams: HashMap<StreamId, StreamRef>, err: OperationError) {
        if !streams.is_empty() {
            let inner = self.inner.clone();
            spawn(async move {
                let p = Pipeline::new(&inner.publish);
                let futs = streams
                    .into_values()
                    .map(|stream| p.call(Message::disconnect(err.clone(), stream)));
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
                let _ = Pipeline::new(&inner.control)
                    .call(ControlMessage::terminated())
                    .await;
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

    async fn call(
        &self,
        request: DispatchItem<Codec>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        log::debug!("Handle h2 message: {:?}", request);

        match request {
            DispatchItem::Item(frame) => match frame {
                Frame::Headers(hdrs) => {
                    self.handle_message(self.connection.recv_headers(hdrs), ctx)
                        .await
                }
                Frame::Data(data) => {
                    self.handle_message(self.connection.recv_data(data), ctx)
                        .await
                }
                Frame::Settings(settings) => match self.connection.recv_settings(settings) {
                    Err(Either::Left(err)) => {
                        self.connection.proto_error(&err);
                        control(ControlMessage::proto_error(err), &self.inner, ctx).await
                    }
                    Err(Either::Right(errs)) => {
                        // handle stream errors
                        for err in errs {
                            let (stream, kind) = err.into_inner();
                            stream.set_failed_stream(kind.into());

                            self.connection
                                .encode(Reset::new(stream.id(), kind.reason()));
                            let _ = publish::<Pub, Ctl>(
                                Message::error(kind, &stream),
                                stream,
                                &self.inner,
                                ctx,
                            )
                            .await;
                        }
                        Ok(None)
                    }
                    Ok(_) => Ok(None),
                },
                Frame::WindowUpdate(update) => {
                    self.handle_message(
                        self.connection.recv_window_update(update).map(|_| None),
                        ctx,
                    )
                    .await
                }
                Frame::Reset(reset) => {
                    self.handle_message(self.connection.recv_rst_stream(reset).map(|_| None), ctx)
                        .await
                }
                Frame::Ping(ping) => {
                    log::trace!("processing PING: {:#?}", ping);
                    if ping.is_ack() {
                        self.connection.recv_pong(ping);
                        Ok(None)
                    } else {
                        Ok(Some(Ping::pong(ping.into_payload()).into()))
                    }
                }
                Frame::GoAway(frm) => {
                    log::trace!("processing GoAway: {:#?}", frm);
                    let reason = frm.reason();
                    let streams = self.connection.recv_go_away(reason, frm.data());
                    self.handle_connection_error(streams, ConnectionError::GoAway(reason).into());
                    control(ControlMessage::go_away(frm), &self.inner, ctx).await
                }
                Frame::Priority(prio) => {
                    log::debug!("PRIORITY frame is not supported: {:#?}", prio);
                    Ok(None)
                }
            },
            DispatchItem::EncoderError(err) => {
                let err = ConnectionError::from(err);
                let streams = self.connection.proto_error(&err);
                self.handle_connection_error(streams, err.into());
                control(ControlMessage::proto_error(err), &self.inner, ctx).await
            }
            DispatchItem::DecoderError(err) => {
                let err = ConnectionError::from(err);
                let streams = self.connection.proto_error(&err);
                self.handle_connection_error(streams, err.into());
                control(ControlMessage::proto_error(err), &self.inner, ctx).await
            }
            DispatchItem::KeepAliveTimeout => {
                log::warn!("did not receive pong response in time, closing connection");
                let streams = self.connection.ping_timeout();
                self.handle_connection_error(streams, ConnectionError::KeepaliveTimeout.into());
                control(
                    ControlMessage::proto_error(ConnectionError::KeepaliveTimeout),
                    &self.inner,
                    ctx,
                )
                .await
            }
            DispatchItem::ReadTimeout => {
                log::warn!("did not receive complete frame in time, closing connection");
                let streams = self.connection.read_timeout();
                self.handle_connection_error(streams, ConnectionError::ReadTimeout.into());
                control(
                    ControlMessage::proto_error(ConnectionError::ReadTimeout),
                    &self.inner,
                    ctx,
                )
                .await
            }
            DispatchItem::Disconnect(err) => {
                let streams = self.connection.disconnect();
                self.handle_connection_error(streams, OperationError::Disconnected);
                control(ControlMessage::peer_gone(err), &self.inner, ctx).await
            }
            DispatchItem::WBackPressureEnabled | DispatchItem::WBackPressureDisabled => Ok(None),
        }
    }
}

async fn publish<'f, P, C>(
    msg: Message,
    stream: StreamRef,
    inner: &'f Inner<C, P>,
    ctx: ServiceCtx<'f, Dispatcher<C, P>>,
) -> Result<Option<Frame>, ()>
where
    P: Service<Message, Response = ()>,
    P::Error: fmt::Debug,
    C: Service<ControlMessage<P::Error>, Response = ControlResult>,
    C::Error: fmt::Debug,
{
    let fut = ctx.call(&inner.publish, msg);
    let mut pinned = std::pin::pin!(fut);
    let result = poll_fn(|cx| {
        if stream.is_remote() {
            match stream.poll_send_reset(cx) {
                Poll::Ready(Ok(())) | Poll::Ready(Err(_)) => {
                    log::trace!("Stream is closed {:?}", stream.id());
                    return Poll::Ready(Ok(None));
                }
                Poll::Pending => (),
            }
        }

        match pinned.as_mut().poll(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(None)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await;

    match result {
        Ok(v) => Ok(v),
        Err(e) => control(ControlMessage::app_error(e, stream), inner, ctx).await,
    }
}

async fn control<'f, Ctl, Pub>(
    pkt: ControlMessage<Pub::Error>,
    inner: &'f Inner<Ctl, Pub>,
    ctx: ServiceCtx<'f, Dispatcher<Ctl, Pub>>,
) -> Result<Option<Frame>, ()>
where
    Ctl: Service<ControlMessage<Pub::Error>, Response = ControlResult>,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message>,
    Pub::Error: fmt::Debug,
{
    match ctx.call(&inner.control, pkt).await {
        Ok(res) => {
            if let Some(Frame::Reset(ref rst)) = res.frame {
                if !rst.stream_id().is_zero() {
                    inner.connection.rst_stream(rst.stream_id(), rst.reason());
                }
            }
            if let Some(frm) = res.frame {
                inner.connection.encode(frm);
            }
            if res.disconnect {
                inner.connection.close();
            }
        }
        Err(err) => {
            log::error!("control service has failed with {:?}", err);
            // we cannot handle control service errors, close connection
            inner.connection.encode(
                GoAway::new(Reason::INTERNAL_ERROR).set_last_stream_id(inner.last_stream_id),
            );
            inner.connection.close();
        }
    }
    Ok(None)
}
