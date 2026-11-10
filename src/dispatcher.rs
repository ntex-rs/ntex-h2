use std::{cell::Cell, future, rc::Rc, task::Poll};

use ntex_dispatcher::{DispatchItem, Reason as DispReason};
use ntex_error::Error;
use ntex_service::pipeline::Pipeline;
use ntex_service::{Ctx, Service};
use ntex_util::{HashMap, future::Either, future::join, spawn};

use crate::connection::{Connection, EitherError, RecvHalfConnection};
use crate::control::{Control, ControlAck};
use crate::error::{ConnectionError, OperationError, StreamError};
use crate::frame::{Frame, GoAway, Ping, Reason, Reset, StreamId};
use crate::message::Message;
use crate::{codec::Codec, stream::StreamRef};

/// Amqp server dispatcher service.
pub(crate) struct Dispatcher<Err, PErr> {
    inner: Rc<Inner<Err, PErr>>,
    connection: RecvHalfConnection,
}

struct Inner<Err, PErr> {
    publish: Pipeline<Message, (), PErr>,
    control: Pipeline<Control<PErr>, ControlAck, Err>,
    connection: Connection,
    disconnected: Cell<bool>,
}

impl<Err: 'static, PErr: 'static> Dispatcher<Err, PErr> {
    pub(crate) fn new(
        connection: Connection,
        publish: Pipeline<Message, (), PErr>,
        control: Pipeline<Control<PErr>, ControlAck, Err>,
    ) -> Self {
        Dispatcher {
            connection: connection.recv_half(),
            inner: Rc::new(Inner {
                publish,
                connection,
                control,
                disconnected: Cell::new(false),
            }),
        }
    }

    async fn handle_message(
        &self,
        result: Result<Option<(StreamRef, Message)>, EitherError>,
    ) -> Result<Option<Frame>, Err> {
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
                control(
                    Control::proto_error(err, self.connection.last_stream_id()),
                    &self.inner,
                )
                .await
            }
            Err(Either::Right(err)) => {
                let (stream, kind) = err.into_inner();

                if matches!(&*kind, StreamError::Reset(_)) {
                    // a received RST_STREAM must not be answered with
                    // another RST_STREAM (RFC 9113 §5.4.2)
                    stream.set_failed_stream(kind.clone().map(OperationError::from));
                } else {
                    log::error!(
                        "{}: Failed to handle frame, err: {kind:?} stream: {stream:?}",
                        stream.tag(),
                    );
                    // streams are removed once both sides are closed,
                    // a queried stream is always reset
                    stream.reset_silent(kind.reason());
                }
                publish(Message::error(kind, &stream), stream, &self.inner).await
            }
        }
    }

    async fn codec_error(&self, err: ConnectionError) -> Result<Option<Frame>, Err> {
        let err = Error::new(err, self.connection.service());
        let streams = self.connection.proto_error(&err);
        self.handle_connection_error(streams, err.clone().map(OperationError::from));
        control(
            Control::proto_error(err, self.connection.last_stream_id()),
            &self.inner,
        )
        .await
    }

    fn handle_connection_error(&self, streams: HashMap<StreamId, StreamRef>, err: Error<OperationError>) {
        if !streams.is_empty() {
            let publish = self.inner.publish.bind();
            spawn(async move {
                for stream in streams.into_values() {
                    let _ = publish.call(Message::disconnect(err.clone(), stream)).await;
                }
            });
        }
    }
}

impl<Err, PErr> Service<(), DispatchItem<Codec>> for Dispatcher<Err, PErr>
where
    Err: 'static,
    PErr: 'static,
{
    type Res = Option<Frame>;
    type Error = Err;

    #[inline]
    async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        let (res1, res2) = join(self.inner.publish.ready(), self.inner.control.ready()).await;

        if let Err(e) = res1 {
            if let Err(e) = res2 {
                Err(e)
            } else {
                // publish service is failed, streams cannot be processed.
                // control service decides on GOAWAY frame, connection is closed
                self.connection.disconnect();
                control(
                    Control::error(e, self.inner.connection.last_stream_id()),
                    &self.inner,
                )
                .await
                .map(|_| ())
            }
        } else {
            Ok(())
        }
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.inner.publish.shutdown().await;
        self.inner.control.shutdown().await;
        self.connection.disconnect();
    }

    #[allow(clippy::used_underscore_binding)]
    async fn call(
        &self,
        req: DispatchItem<Codec>,
        _: Ctx<'_, Self, ()>,
    ) -> Result<Self::Res, Self::Error> {
        #[cfg(feature = "trace")]
        log::debug!("{}: Handle h2 message: {req:?}", self.connection.tag());

        match req {
            DispatchItem::Item(frame) if let Err(err) = self.connection.check_first_frame(&frame) => {
                self.handle_message(Err(Either::Left(err))).await
            }
            DispatchItem::Item(frame) => match frame {
                Frame::Headers(hdrs) => self.handle_message(self.connection.recv_headers(hdrs)).await,
                Frame::Data(data) => self.handle_message(self.connection.recv_data(data)).await,
                Frame::Settings(settings) => match self.connection.recv_settings(settings) {
                    Err(Either::Left(err)) => {
                        let streams = self.connection.proto_error(&err);
                        self.handle_connection_error(streams, err.clone().map(OperationError::from));
                        control(
                            Control::proto_error(err, self.connection.last_stream_id()),
                            &self.inner,
                        )
                        .await
                    }
                    Err(Either::Right(errs)) => {
                        // handle stream errors
                        for err in errs {
                            let (stream, kind) = err.into_inner();
                            stream.set_failed_stream(kind.clone().map(OperationError::from));

                            self.connection.encode(Reset::new(stream.id(), kind.reason()));
                            let _ = publish(Message::error(kind, &stream), stream, &self.inner).await;
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
                        self.connection.recv_pong(&ping);
                        Ok(None)
                    } else {
                        Ok(Some(Ping::pong(ping.into_payload()).into()))
                    }
                }
                Frame::GoAway(frm) => {
                    log::trace!("{}: Processing GoAway: {:#?}", self.connection.tag(), frm);
                    let reason = frm.reason();
                    let streams = self
                        .connection
                        .recv_go_away(reason, frm.last_stream_id(), frm.data());
                    self.handle_connection_error(
                        streams,
                        Error::new(ConnectionError::GoAway(reason), self.connection.service()),
                    );
                    // remaining streams complete, connection closes when they are done
                    go_away(Control::go_away(frm), &self.inner).await
                }
                Frame::Invalid(frm) => self.handle_message(self.connection.recv_invalid_frame(frm)).await,
                Frame::Priority(_prio) => {
                    #[cfg(feature = "trace")]
                    log::debug!(
                        "{}: PRIORITY frame is not supported: {_prio:#?}",
                        self.connection.tag(),
                    );
                    Ok(None)
                }
            },
            DispatchItem::Stop(DispReason::Encoder(err)) => self.codec_error(err.into()).await,
            DispatchItem::Stop(DispReason::Decoder(err)) => self.codec_error(err.into()).await,
            DispatchItem::Stop(DispReason::KeepAlive) if self.connection.is_settings_timeout() => {
                log::warn!(
                    "{}: did not receive settings ack in time, closing connection",
                    self.connection.tag(),
                );
                let streams = self.connection.settings_timeout();
                let err: Error<ConnectionError> =
                    Error::new(ConnectionError::SettingsTimeout, self.connection.service());
                self.handle_connection_error(streams, err.clone().map(OperationError::from));
                control(
                    Control::proto_error(err, self.connection.last_stream_id()),
                    &self.inner,
                )
                .await
            }
            DispatchItem::Stop(DispReason::KeepAlive) => {
                log::warn!(
                    "{}: did not receive pong response in time, closing connection",
                    self.connection.tag(),
                );
                let streams = self.connection.ping_timeout();
                let err: Error<ConnectionError> =
                    Error::new(ConnectionError::KeepaliveTimeout, self.connection.service());
                self.handle_connection_error(streams, err.clone().map(OperationError::from));
                control(
                    Control::proto_error(err, self.connection.last_stream_id()),
                    &self.inner,
                )
                .await
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
                control(
                    Control::proto_error(err, self.connection.last_stream_id()),
                    &self.inner,
                )
                .await
            }
            DispatchItem::Stop(DispReason::WriteTimeout) => {
                log::warn!(
                    "{}: did not send write buffer in time, closing connection",
                    self.connection.tag(),
                );
                let streams = self.connection.write_timeout();
                let err: Error<ConnectionError> =
                    Error::new(ConnectionError::WriteTimeout, self.connection.service());
                self.handle_connection_error(streams, err.clone().map(OperationError::from));
                control(
                    Control::proto_error(err, self.connection.last_stream_id()),
                    &self.inner,
                )
                .await
            }
            DispatchItem::Stop(DispReason::Io(err)) => {
                let streams = self.connection.disconnect();
                self.handle_connection_error(
                    streams,
                    Error::new(OperationError::Disconnected, self.connection.service()),
                );
                control(Control::peer_gone(err), &self.inner).await
            }
            DispatchItem::Stop(DispReason::Service) => {
                // the dispatcher does not deliver any further items, release the
                // open streams so pending handlers do not block the shutdown
                let streams = self.connection.disconnect();
                self.handle_connection_error(
                    streams,
                    Error::new(OperationError::Disconnected, self.connection.service()),
                );
                self.inner.connection.encode(
                    GoAway::new(Reason::INTERNAL_ERROR)
                        .set_last_stream_id(self.inner.connection.last_stream_id()),
                );
                self.inner.connection.close();
                Ok(None)
            }
            DispatchItem::Control(_) => Ok(None),
        }
    }
}

async fn publish<Err, PErr>(
    msg: Message,
    stream: StreamRef,
    inner: &Inner<Err, PErr>,
) -> Result<Option<Frame>, Err>
where
    Err: 'static,
    PErr: 'static,
{
    // the final message of a reset stream is always published
    let result = if stream.is_remote() && !stream.has_error() {
        // a reset wakes all publish calls of the stream
        let io = inner.connection.io();
        let waiter = stream.on_reset();
        let fut = inner.publish.call(msg);
        let mut pinned = std::pin::pin!(fut);
        let mut first = true;
        let mut watch = true;
        let result = future::poll_fn(|cx| {
            if first {
                first = false;
                if let Poll::Ready(result) = pinned.as_mut().poll(cx) {
                    return Poll::Ready(Some(result));
                }

                let closed = waiter.poll_ready(cx).is_ready();
                // A reset or I/O close during the first poll predates registration.
                return if stream.has_error() {
                    Poll::Ready(None)
                } else {
                    if closed {
                        cx.waker().wake_by_ref();
                    }
                    Poll::Pending
                };
            }

            // the stream is reset during the call, the request body
            // can outlive the response
            while watch && waiter.poll_ready(cx).is_ready() {
                if stream.has_error() {
                    log::trace!("{}: Stream is closed {:?}", stream.tag(), stream.id());
                    return Poll::Ready(None);
                }
                // a closed io keeps the waiter ready
                watch = !io.is_closed();
            }
            pinned.as_mut().poll(cx).map(Some)
        })
        .await;

        // a stream reset by the local side gets the final message,
        // the publish call is dropped or completed
        match result {
            Some(Err(e)) => Err(e),
            _ => match stream.take_local_close() {
                Some(err) => inner.publish.call(Message::error(err, &stream)).await,
                None => Ok(()),
            },
        }
    } else {
        inner.publish.call(msg).await
    };

    match result {
        Ok(()) => Ok(None),
        Err(e) => control(Control::error(e, inner.connection.last_stream_id()), inner).await,
    }
}

impl<Err, PErr> Inner<Err, PErr> {
    fn can_disconnect(&self) -> bool {
        if self.disconnected.get() {
            false
        } else {
            self.disconnected.set(true);
            true
        }
    }
}

async fn control<Err, PErr>(pkt: Control<PErr>, inner: &Inner<Err, PErr>) -> Result<Option<Frame>, Err>
where
    Err: 'static,
    PErr: 'static,
{
    if inner.can_disconnect() {
        call_control(pkt, inner, true).await
    } else {
        // control service is already notified (graceful GOAWAY), use default response
        if let Some(frm) = pkt.ack().frame {
            inner.connection.encode(frm);
        }
        inner.connection.close();
        Ok(None)
    }
}

async fn go_away<Err, PErr>(pkt: Control<PErr>, inner: &Inner<Err, PErr>) -> Result<Option<Frame>, Err>
where
    Err: 'static,
    PErr: 'static,
{
    if inner.can_disconnect() {
        call_control(pkt, inner, false).await
    } else {
        Ok(None)
    }
}

async fn call_control<Err, PErr>(
    pkt: Control<PErr>,
    inner: &Inner<Err, PErr>,
    close: bool,
) -> Result<Option<Frame>, Err>
where
    Err: 'static,
    PErr: 'static,
{
    match inner.control.call(pkt).await {
        Ok(res) => {
            if let Some(frm) = res.frame {
                inner.connection.encode(frm);
            }
            if close {
                inner.connection.close();
            }
            Ok(None)
        }
        Err(err) => {
            // we cannot handle control service errors, close connection
            inner.connection.encode(
                GoAway::new(Reason::INTERNAL_ERROR).set_last_stream_id(inner.connection.last_stream_id()),
            );
            inner.connection.close();
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future, poll_fn},
        sync::{Arc, atomic::AtomicUsize, atomic::Ordering},
        task::{Context, Wake, Waker},
    };

    use ntex_bytes::Bytes;
    use ntex_http::{HeaderMap, Method, StatusCode};
    use ntex_io::{Handle, Io, IoContext, IoStream};
    use ntex_service::cfg::SharedCfg;
    use ntex_util::{channel::pool, time::Seconds};

    use super::*;
    use crate::{MessageKind, ServiceConfig, StreamEof, frame};

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct DropCount(Rc<Cell<usize>>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    struct Transport(Rc<Cell<Option<IoContext>>>);

    impl IoStream for Transport {
        fn start(self, ctx: IoContext) -> Box<dyn Handle> {
            self.0.set(Some(ctx));
            Box::new(self)
        }
    }

    impl Handle for Transport {}

    fn setup(publish: Pipeline<Message, (), ()>) -> (Inner<(), ()>, Io, IoContext) {
        let cfg = SharedCfg::new("SRV")
            .add(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_settings_timeout(Seconds::ZERO),
            )
            .build();
        let ctx = Rc::new(Cell::new(None));
        let io = Io::new(Transport(ctx.clone()), cfg.clone());
        let connection = Connection::new(
            true,
            io.get_ref(),
            Codec::default(),
            cfg.get(),
            false,
            false,
            pool::new(),
        );
        let inner = Inner {
            publish,
            control: Pipeline::new((), async |msg: Control<()>| Ok::<_, ()>(msg.ack())),
            connection,
            disconnected: Cell::new(false),
        };
        (inner, io, ctx.take().unwrap())
    }

    fn request(inner: &Inner<(), ()>, eof: bool) -> (StreamRef, Message) {
        inner
            .connection
            .recv_half()
            .recv_headers(frame::Headers::new(
                StreamId::from(1),
                frame::PseudoHeaders {
                    method: Some(Method::POST),
                    scheme: Some("http".into()),
                    path: Some("/".into()),
                    ..Default::default()
                },
                HeaderMap::new(),
                eof,
            ))
            .unwrap()
            .unwrap()
    }

    /// STATEMENT: An immediately completed response does not register a reset waiter or
    /// wake its own publish task.
    ///
    /// HOW:
    /// - Receive an end-of-stream request and use a handler that immediately completes
    ///   the response.
    /// - Poll publication once with a counting waker and assert it completes without
    ///   waking that task.
    #[ntex::test]
    async fn publish_ready_does_not_wake_itself() {
        let (inner, _io, _ctx) = setup(Pipeline::new((), async |msg: Message| {
            msg.stream
                .send_response(StatusCode::OK, HeaderMap::new(), true)
                .unwrap();
            Ok(())
        }));
        inner.publish.ready().await.unwrap();
        let (stream, msg) = request(&inner, true);
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        let mut fut = std::pin::pin!(publish(msg, stream, &inner));

        assert!(matches!(
            fut.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(None))
        ));
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
    }

    /// STATEMENT: A reset during the first handler poll cancels a pending call without
    /// losing the final local error, while a ready handler error retains precedence.
    ///
    /// HOW:
    /// - Reset an open request inside its handler and return `Pending`, `Ok`, or `Err`.
    /// - Poll publication once and assert it completes and drops the original handler.
    /// - Verify the pending and successful cases publish one `LocalReset` message,
    ///   whereas the error case notifies only the control service.
    #[ntex::test]
    async fn publish_reset_during_first_poll() {
        for result in [Poll::Pending, Poll::Ready(Ok(())), Poll::Ready(Err(()))] {
            let dropped = Rc::new(Cell::new(0));
            let errors = Rc::new(Cell::new(0));
            let controls = Rc::new(Cell::new(0));
            let dropped2 = dropped.clone();
            let errors2 = errors.clone();
            let controls2 = controls.clone();
            let (mut inner, _io, _ctx) =
                setup(Pipeline::new((), async move |msg: Message| match msg.kind {
                    MessageKind::Headers { .. } => {
                        let _guard = DropCount(dropped2.clone());
                        assert!(msg.stream.reset(Reason::CANCEL));
                        poll_fn(|_| result).await
                    }
                    MessageKind::Eof(StreamEof::Error(err)) => {
                        assert!(matches!(*err, StreamError::LocalReset(Reason::CANCEL)));
                        errors2.set(errors2.get() + 1);
                        Ok(())
                    }
                    _ => panic!("unexpected message"),
                }));
            inner.control = Pipeline::new((), async move |msg: Control<()>| {
                assert!(matches!(
                    msg,
                    Control::Disconnect(crate::control::Reason::Error(_))
                ));
                controls2.set(controls2.get() + 1);
                Ok::<_, ()>(msg.ack())
            });
            inner.publish.ready().await.unwrap();
            inner.control.ready().await.unwrap();
            let (stream, msg) = request(&inner, false);
            let mut fut = std::pin::pin!(publish(msg, stream, &inner));

            assert!(matches!(
                fut.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Ok(None))
            ));
            assert_eq!(dropped.get(), 1);
            let failed = matches!(result, Poll::Ready(Err(())));
            assert_eq!(errors.get(), usize::from(!failed));
            assert_eq!(controls.get(), usize::from(failed));
        }
    }

    /// STATEMENT: A pending publish call registers for later resets without being polled
    /// twice initially, and normal request EOF does not cancel it.
    ///
    /// HOW:
    /// - Poll a permanently pending handler once and assert its poll count is one.
    /// - Deliver request EOF, observe a wake, and poll again without dropping the handler.
    /// - Reset the stream, observe another wake, and assert publication completes without
    ///   polling the handler again and drops its guard.
    #[ntex::test]
    async fn publish_pending_waits_for_reset() {
        let polls = Rc::new(Cell::new(0));
        let dropped = Rc::new(Cell::new(0));
        let polls2 = polls.clone();
        let dropped2 = dropped.clone();
        let (inner, _io, _ctx) = setup(Pipeline::new((), async move |_: Message| {
            let _guard = DropCount(dropped2.clone());
            poll_fn(|_| {
                polls2.set(polls2.get() + 1);
                Poll::<Result<(), ()>>::Pending
            })
            .await
        }));
        inner.publish.ready().await.unwrap();
        let (stream, msg) = request(&inner, false);
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(publish(msg, stream.clone(), &inner));

        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(polls.get(), 1);
        let mut data = frame::Data::new(stream.id(), Bytes::new());
        data.set_end_stream();
        inner.connection.recv_half().recv_data(data).unwrap();
        assert!(wakes.0.swap(0, Ordering::Relaxed) > 0);
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(polls.get(), 2);
        assert_eq!(dropped.get(), 0);

        assert!(stream.reset(Reason::CANCEL));
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(None))));
        assert_eq!(polls.get(), 2);
        assert_eq!(dropped.get(), 1);
    }

    /// STATEMENT: Closing I/O during the first handler poll does not lose the wake
    /// needed to observe a subsequent connection failure.
    ///
    /// HOW:
    /// - Complete a response, report its transport as stopped, and leave the handler pending.
    /// - Assert the first publication poll schedules another poll despite the stream
    ///   already being removed from the connection.
    /// - Fail the connection and verify publication completes and drops the handler.
    #[ntex::test]
    async fn publish_io_closed_during_first_poll() {
        let dropped = Rc::new(Cell::new(0));
        let dropped2 = dropped.clone();
        let transport = Rc::new(Cell::new(None::<IoContext>));
        let transport2 = transport.clone();
        let (inner, _io, ctx) = setup(Pipeline::new((), async move |msg: Message| {
            let _guard = DropCount(dropped2.clone());
            msg.stream
                .send_response(StatusCode::OK, HeaderMap::new(), true)
                .unwrap();
            transport2.take().unwrap().stopped(None);
            future::pending::<Result<(), ()>>().await
        }));
        transport.set(Some(ctx));
        inner.publish.ready().await.unwrap();
        let (stream, msg) = request(&inner, true);
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(publish(msg, stream, &inner));

        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        assert_eq!(dropped.get(), 0);
        assert_eq!(inner.connection.recv_half().disconnect(), HashMap::default());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(None))));
        assert_eq!(dropped.get(), 1);
    }
}
