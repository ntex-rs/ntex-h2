use std::{fmt, future::poll_fn, future::Future, rc::Rc, task::Poll};

use ntex_io::DispatchItem;
use ntex_service::{Pipeline, Service, ServiceCtx};
use ntex_util::future::{join, join_all, Either};
use ntex_util::{spawn, HashMap};

use crate::connection::{Connection, RecvHalfConnection};
use crate::control::{Control, ControlAck};
use crate::error::{ConnectionError, OperationError, StreamErrorInner};
use crate::frame::{Frame, GoAway, Ping, Reason, Reset, StreamId};
use crate::{codec::Codec, message::Message, stream::StreamRef};

/// Amqp server dispatcher service.
pub(crate) struct Dispatcher<Ctl, Pub>
where
    Ctl: Service<Control<Pub::Error>>,
    Pub: Service<Message>,
{
    inner: Rc<Inner<Ctl, Pub>>,
    connection: RecvHalfConnection,
}

struct Inner<Ctl, Pub>
where
    Ctl: Service<Control<Pub::Error>>,
    Pub: Service<Message>,
{
    control: Ctl,
    publish: Pub,
    connection: Connection,
    last_stream_id: StreamId,
}

impl<Ctl, Pub> Dispatcher<Ctl, Pub>
where
    Ctl: Service<Control<Pub::Error>, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
{
    pub(crate) fn new(connection: Connection, control: Ctl, publish: Pub) -> Self {
        Dispatcher {
            connection: connection.recv_half(),
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
                control(Control::proto_error(err), &self.inner, ctx).await
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
            let _ = spawn(async move {
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
    Ctl: Service<Control<Pub::Error>, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
{
    type Response = Option<Frame>;
    type Error = ();

    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        let (res1, res2) = join(
            ctx.ready(&self.inner.publish),
            ctx.ready(&self.inner.control),
        )
        .await;

        res1.map_err(|_| ())?;
        res2.map_err(|_| ())?;
        Ok(())
    }

    async fn shutdown(&self) {
        let _ = Pipeline::new(&self.inner.control)
            .call(Control::terminated())
            .await;

        join(self.inner.publish.shutdown(), self.inner.control.shutdown()).await;

        self.connection.disconnect();
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
                        control(Control::proto_error(err), &self.inner, ctx).await
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
                    control(Control::go_away(frm), &self.inner, ctx).await
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
                control(Control::proto_error(err), &self.inner, ctx).await
            }
            DispatchItem::DecoderError(err) => {
                let err = ConnectionError::from(err);
                let streams = self.connection.proto_error(&err);
                self.handle_connection_error(streams, err.into());
                control(Control::proto_error(err), &self.inner, ctx).await
            }
            DispatchItem::KeepAliveTimeout => {
                log::warn!("did not receive pong response in time, closing connection");
                let streams = self.connection.ping_timeout();
                self.handle_connection_error(streams, ConnectionError::KeepaliveTimeout.into());
                control(
                    Control::proto_error(ConnectionError::KeepaliveTimeout),
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
                    Control::proto_error(ConnectionError::ReadTimeout),
                    &self.inner,
                    ctx,
                )
                .await
            }
            DispatchItem::Disconnect(err) => {
                let streams = self.connection.disconnect();
                self.handle_connection_error(streams, OperationError::Disconnected);
                control(Control::peer_gone(err), &self.inner, ctx).await
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
    C: Service<Control<P::Error>, Response = ControlAck>,
    C::Error: fmt::Debug,
{
    let result = if stream.is_remote() {
        let fut = ctx.call(&inner.publish, msg);
        let mut pinned = std::pin::pin!(fut);
        poll_fn(|cx| {
            match stream.poll_send_reset(cx) {
                Poll::Ready(Ok(())) | Poll::Ready(Err(_)) => {
                    log::trace!("Stream is closed {:?}", stream.id());
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => (),
            }
            pinned.as_mut().poll(cx)
        })
        .await
    } else {
        ctx.call(&inner.publish, msg).await
    };

    match result {
        Ok(_) => Ok(None),
        Err(e) => control(Control::app_error(e, stream), inner, ctx).await,
    }
}

async fn control<'f, Ctl, Pub>(
    pkt: Control<Pub::Error>,
    inner: &'f Inner<Ctl, Pub>,
    ctx: ServiceCtx<'f, Dispatcher<Ctl, Pub>>,
) -> Result<Option<Frame>, ()>
where
    Ctl: Service<Control<Pub::Error>, Response = ControlAck>,
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
