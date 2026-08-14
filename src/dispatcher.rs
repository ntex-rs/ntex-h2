use std::{cell::Cell, fmt, future::Future, future::poll_fn, rc::Rc, task::Context, task::Poll};

use ntex_dispatcher::{DispatchItem, Reason as DispReason};
use ntex_error::Error;
use ntex_service::{Pipeline, Service, ServiceCtx};
use ntex_util::{HashMap, future::Either, future::join, spawn};

use crate::connection::{Connection, EitherError, RecvHalfConnection};
use crate::control::{Control, ControlAck};
use crate::error::{ConnectionError, OperationError, StreamError};
use crate::frame::{Frame, FrameError, GoAway, Ping, Reason, Reset, StreamId};
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
    control: Pipeline<Ctl, Ctl::Data>,
    publish: Pipeline<Pub, Pub::Data>,
    connection: Connection,
    last_stream_id: StreamId,
    disconnected: Cell<bool>,
}

impl<Ctl, Pub> Dispatcher<Ctl, Pub>
where
    Ctl: Service<Control<Pub::Error>, Response = ControlAck> + 'static,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message, Response = ()> + 'static,
    Pub::Error: fmt::Debug,
{
    pub(crate) fn new(
        connection: Connection,
        control: Ctl,
        control_data: Ctl::Data,
        publish: Pub,
        publish_data: Pub::Data,
    ) -> Self {
        Dispatcher {
            connection: connection.recv_half(),
            inner: Rc::new(Inner {
                publish: Pipeline::new(publish, publish_data),
                connection,
                control: Pipeline::new(control, control_data),
                last_stream_id: 0.into(),
                disconnected: Cell::new(false),
            }),
        }
    }

    async fn handle_message<'f>(
        &'f self,
        result: Result<Option<(StreamRef, Message)>, EitherError>,
    ) -> Result<Option<Frame>, ()> {
        match result {
            Ok(Some((stream, msg))) => publish(msg, stream, &self.inner).await,
            Ok(None) => Ok(None),
            Err(Either::Left(err)) => {
                log::error!(
                    "{}: Connection failed during message handling: {err:?}",
                    self.connection.tag()
                );
                let streams = self.connection.proto_error(&err);
                self.handle_connection_error(streams, err.clone().map(OperationError::from));
                control(Control::proto_error(err), &self.inner).await
            }
            Err(Either::Right(err)) => {
                let (stream, kind) = err.into_inner();

                if matches!(&*kind, StreamError::Reset(_)) {
                    stream.set_failed_stream(kind.clone().map(OperationError::from));
                } else {
                    log::error!(
                        "{}: Failed to handle frame, err: {kind:?} stream: {stream:?}",
                        stream.tag(),
                    );
                }

                if !stream.reset(kind.reason()) {
                    self.connection
                        .encode(Reset::new(stream.id(), kind.reason()));
                }
                publish(Message::error(kind, &stream), stream, &self.inner).await
            }
        }
    }

    fn handle_connection_error(
        &self,
        streams: HashMap<StreamId, StreamRef>,
        err: Error<OperationError>,
    ) {
        if !streams.is_empty() {
            let inner = self.inner.clone();
            spawn(async move {
                let publish = inner.publish.clone();
                for stream in streams.into_values() {
                    let _ = publish.call(Message::disconnect(err.clone(), stream)).await;
                }
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
    type Data = ();

    #[inline]
    async fn ready(&self, _: &Self::Data, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        let (res1, res2) = join(self.inner.publish.ready(), self.inner.control.ready()).await;

        if let Err(e) = res1 {
            if res2.is_err() {
                Err(())
            } else {
                match self
                    .inner
                    .control
                    .call_nowait(Control::error(e, None))
                    .await
                {
                    Ok(_) => {
                        self.connection.disconnect();
                        Ok(())
                    }
                    Err(_) => Err(()),
                }
            }
        } else {
            res2.map_err(|_| ())
        }
    }

    fn poll(&self, _: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        if let Err(e) = self.inner.publish.poll(cx) {
            let inner = self.inner.clone();
            let con = self.connection.connection();
            ntex_util::spawn(async move {
                if inner
                    .control
                    .call_nowait(Control::error(e, None))
                    .await
                    .is_ok()
                {
                    con.close();
                }
            });
        }
        self.inner.control.poll(cx).map_err(|_| ())
    }

    async fn shutdown(&self, _: &Self::Data) {
        let _ = self.inner.control.clone().call(Control::terminated()).await;

        join(self.inner.publish.shutdown(), self.inner.control.shutdown()).await;

        self.connection.disconnect();
    }

    #[allow(clippy::used_underscore_binding)]
    async fn call(
        &self,
        request: DispatchItem<Codec>,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        #[cfg(feature = "trace")]
        log::debug!("{}: Handle h2 message: {request:?}", self.connection.tag());

        match request {
            DispatchItem::Item(frame) => match frame {
                Frame::Headers(hdrs) => {
                    self.handle_message(self.connection.recv_headers(hdrs))
                        .await
                }
                Frame::Data(data) => self.handle_message(self.connection.recv_data(data)).await,
                Frame::Settings(settings) => match self.connection.recv_settings(settings) {
                    Err(Either::Left(err)) => {
                        let streams = self.connection.proto_error(&err);
                        self.handle_connection_error(
                            streams,
                            err.clone().map(OperationError::from),
                        );
                        control(Control::proto_error(err), &self.inner).await
                    }
                    Err(Either::Right(errs)) => {
                        // handle stream errors
                        for err in errs {
                            let (stream, kind) = err.into_inner();
                            stream.set_failed_stream(kind.clone().map(OperationError::from));

                            self.connection
                                .encode(Reset::new(stream.id(), kind.reason()));
                            let _ = publish::<Pub, Ctl>(
                                Message::error(kind, &stream),
                                stream,
                                &self.inner,
                            )
                            .await;
                        }
                        Ok(None)
                    }
                    Ok(()) => Ok(None),
                },
                Frame::WindowUpdate(update) => {
                    self.handle_message(self.connection.recv_window_update(update).map(|()| None))
                        .await
                }
                Frame::Reset(reset) => {
                    self.handle_message(self.connection.recv_rst_stream(reset).map(|()| None))
                        .await
                }
                Frame::Ping(ping) => {
                    #[cfg(feature = "trace")]
                    log::trace!("{}: Processing PING: {:#?}", self.connection.tag(), ping);
                    if ping.is_ack() {
                        self.connection.recv_pong(ping);
                        Ok(None)
                    } else {
                        Ok(Some(Ping::pong(ping.into_payload()).into()))
                    }
                }
                Frame::GoAway(frm) => {
                    log::trace!("{}: Processing GoAway: {:#?}", self.connection.tag(), frm);
                    let reason = frm.reason();
                    let streams = self.connection.recv_go_away(reason, frm.data());
                    self.handle_connection_error(
                        streams,
                        Error::new(ConnectionError::GoAway(reason), self.connection.service()),
                    );
                    control(Control::go_away(frm), &self.inner).await
                }
                Frame::Priority(_prio) => {
                    #[cfg(feature = "trace")]
                    log::debug!(
                        "{}: PRIORITY frame is not supported: {_prio:#?}",
                        self.connection.tag(),
                    );
                    Ok(None)
                }
            },
            DispatchItem::Stop(DispReason::Encoder(err)) => {
                let err = Error::new(ConnectionError::from(err), self.connection.service());
                let streams = self.connection.proto_error(&err);
                self.handle_connection_error(streams, err.clone().map(OperationError::from));
                control(Control::proto_error(err), &self.inner).await
            }
            DispatchItem::Stop(DispReason::Decoder(err)) => {
                let err = if let FrameError::TooManyHeaders(id) = err {
                    log::warn!("{}: TOO Many headers: {id:?}", self.connection.tag());
                    self.connection.drop_stream(id);
                    self.connection
                        .encode(Reset::new(id, Reason::REFUSED_STREAM));
                    if let Err(err) = self.connection.update_rst_count() {
                        err
                    } else {
                        return Ok(None);
                    }
                } else {
                    Error::new(ConnectionError::from(err), self.connection.service())
                };
                let streams = self.connection.proto_error(&err);
                self.handle_connection_error(streams, err.clone().map(OperationError::from));
                control(Control::proto_error(err), &self.inner).await
            }
            DispatchItem::Stop(DispReason::KeepAliveTimeout) => {
                log::warn!(
                    "{}: did not receive pong response in time, closing connection",
                    self.connection.tag(),
                );
                let streams = self.connection.ping_timeout();
                let err: Error<ConnectionError> =
                    Error::new(ConnectionError::KeepaliveTimeout, self.connection.service());
                self.handle_connection_error(streams, err.clone().map(OperationError::from));
                control(Control::proto_error(err), &self.inner).await
            }
            DispatchItem::Stop(DispReason::ReadTimeout) => {
                log::warn!(
                    "{}: did not receive complete frame in time, closing connection",
                    self.connection.tag(),
                );
                let streams = self.connection.read_timeout();
                let err: Error<ConnectionError> =
                    Error::new(ConnectionError::ReadTimeout, self.connection.service());
                self.handle_connection_error(streams, err.clone().map(OperationError::from));
                control(Control::proto_error(err), &self.inner).await
            }
            DispatchItem::Stop(DispReason::Io(err)) => {
                let streams = self.connection.disconnect();
                self.handle_connection_error(
                    streams,
                    Error::new(OperationError::Disconnected, self.connection.service()),
                );
                control(Control::peer_gone(err), &self.inner).await
            }
            DispatchItem::Control(_) => Ok(None),
        }
    }
}

async fn publish<'f, P, C>(
    msg: Message,
    stream: StreamRef,
    inner: &'f Inner<C, P>,
) -> Result<Option<Frame>, ()>
where
    P: Service<Message, Response = ()>,
    P::Error: fmt::Debug,
    C: Service<Control<P::Error>, Response = ControlAck>,
    C::Error: fmt::Debug,
{
    let publish = inner.publish.clone();
    let result = if stream.is_remote() {
        let fut = publish.call(msg);
        let mut pinned = std::pin::pin!(fut);
        poll_fn(|cx| {
            if let Poll::Ready(Ok(()) | Err(_)) = stream.poll_send_reset(cx) {
                log::trace!("{}: Stream is closed {:?}", stream.tag(), stream.id());
                return Poll::Ready(Ok(()));
            }
            pinned.as_mut().poll(cx)
        })
        .await
    } else {
        publish.call(msg).await
    };

    match result {
        Ok(()) => Ok(None),
        Err(e) => control(Control::error(e, Some(&stream)), inner).await,
    }
}

impl<Ctl, Pub> Inner<Ctl, Pub>
where
    Ctl: Service<Control<Pub::Error>>,
    Pub: Service<Message>,
{
    fn can_disconnect(&self) -> bool {
        if self.disconnected.get() {
            false
        } else {
            self.disconnected.set(true);
            true
        }
    }
}

async fn control<'f, Ctl, Pub>(
    pkt: Control<Pub::Error>,
    inner: &'f Inner<Ctl, Pub>,
) -> Result<Option<Frame>, ()>
where
    Ctl: Service<Control<Pub::Error>, Response = ControlAck>,
    Ctl::Error: fmt::Debug,
    Pub: Service<Message>,
    Pub::Error: fmt::Debug,
{
    if inner.can_disconnect() {
        match inner.control.clone().call(pkt).await {
            Ok(res) => {
                if let Some(frm) = res.frame {
                    inner.connection.encode(frm);
                }
                inner.connection.close();
            }
            Err(err) => {
                log::error!(
                    "{}: control service has failed with {err:?}",
                    inner.connection.tag()
                );
                // we cannot handle control service errors, close connection
                inner.connection.encode(
                    GoAway::new(Reason::INTERNAL_ERROR).set_last_stream_id(inner.last_stream_id),
                );
                inner.connection.close();
            }
        }
    }
    Ok(None)
}
