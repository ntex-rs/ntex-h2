use std::{fmt, marker::PhantomData, rc::Rc};

use ntex_service::{IntoServiceFactory, ServiceFactory};
use ntex_util::time::Seconds;

use crate::connection::Config;
use crate::control::{ControlMessage, ControlResult};
use crate::{consts, default::DefaultControlService, frame, frame::Settings, message::Message};

use super::service::{Server, ServerInner};

/// Builds server with custom configuration values.
///
/// Methods can be chained in order to set the configuration values.
///
/// New instances of `Builder` are obtained via [`Builder::new`].
///
/// See function level documentation for details on the various server
/// configuration settings.
///
/// [`Builder::new`]: struct.ServerBuilder.html#method.new
#[derive(Clone, Debug)]
pub struct ServerBuilder<E, Ctl = DefaultControlService> {
    control: Ctl,

    /// Time to keep locally reset streams around before reaping.
    pub(super) reset_stream_duration: Seconds,

    /// Maximum number of locally reset streams to keep at a time.
    pub(super) reset_stream_max: usize,

    /// Initial `Settings` frame to send as part of the handshake.
    pub(super) settings: Settings,

    /// Initial target window size for new connections.
    pub(super) initial_target_connection_window_size: u32,

    pub(super) handshake_timeout: Seconds,
    pub(super) disconnect_timeout: Seconds,
    pub(super) keepalive_timeout: Seconds,

    _t: PhantomData<E>,
}

// ===== impl Builder =====

impl<E> ServerBuilder<E> {
    /// Returns a new server builder instance initialized with default
    /// configuration values.
    ///
    /// Configuration methods can be chained on the return value.
    pub fn new() -> ServerBuilder<E> {
        ServerBuilder {
            control: DefaultControlService,
            reset_stream_duration: consts::DEFAULT_RESET_STREAM_SECS,
            reset_stream_max: consts::DEFAULT_RESET_STREAM_MAX,
            settings: Settings::default(),
            initial_target_connection_window_size: consts::DEFAULT_CONNECTION_WINDOW_SIZE,
            handshake_timeout: Seconds(5),
            disconnect_timeout: Seconds(3),
            keepalive_timeout: Seconds(120),
            _t: PhantomData,
        }
    }
}

impl<E: fmt::Debug, Ctl> ServerBuilder<E, Ctl> {
    /// Service to call with control frames
    pub fn control<S, F>(self, service: F) -> ServerBuilder<E, S>
    where
        F: IntoServiceFactory<S, ControlMessage<E>>,
        S: ServiceFactory<ControlMessage<E>, Response = ControlResult> + 'static,
        S::Error: fmt::Debug,
        S::InitError: fmt::Debug,
    {
        ServerBuilder {
            control: service.into_factory(),
            reset_stream_duration: self.reset_stream_duration,
            reset_stream_max: self.reset_stream_max,
            settings: self.settings,
            initial_target_connection_window_size: self.initial_target_connection_window_size,
            handshake_timeout: self.handshake_timeout,
            disconnect_timeout: self.disconnect_timeout,
            keepalive_timeout: self.keepalive_timeout,
            _t: PhantomData,
        }
    }

    /// Indicates the initial window size (in octets) for stream-level
    /// flow control for received data.
    ///
    /// The initial window of a stream is used as part of flow control. For more
    /// details, see [`FlowControl`].
    ///
    /// The default value is 65,535.
    ///
    /// [`FlowControl`]: ../struct.FlowControl.html
    pub fn initial_window_size(&mut self, size: u32) -> &mut Self {
        self.settings.set_initial_window_size(Some(size));
        self
    }

    /// Indicates the initial window size (in octets) for connection-level flow control
    /// for received data.
    ///
    /// The initial window of a connection is used as part of flow control. For more details,
    /// see [`FlowControl`].
    ///
    /// The default value is 1Mb.
    ///
    /// [`FlowControl`]: ../struct.FlowControl.html
    pub fn initial_connection_window_size(&mut self, size: u32) -> &mut Self {
        assert!(size <= consts::MAX_WINDOW_SIZE);
        self.initial_target_connection_window_size = size;
        self
    }

    /// Indicates the size (in octets) of the largest HTTP/2 frame payload that the
    /// configured server is able to accept.
    ///
    /// The sender may send data frames that are **smaller** than this value,
    /// but any data larger than `max` will be broken up into multiple `DATA`
    /// frames.
    ///
    /// The value **must** be between 16,384 and 16,777,215. The default value is 16,384.
    ///
    /// # Panics
    ///
    /// This function panics if `max` is not within the legal range specified
    /// above.
    pub fn max_frame_size(&mut self, max: u32) -> &mut Self {
        self.settings.set_max_frame_size(max);
        self
    }

    /// Sets the max size of received header frames.
    ///
    /// This advisory setting informs a peer of the maximum size of header list
    /// that the sender is prepared to accept, in octets. The value is based on
    /// the uncompressed size of header fields, including the length of the name
    /// and value in octets plus an overhead of 32 octets for each header field.
    ///
    /// This setting is also used to limit the maximum amount of data that is
    /// buffered to decode HEADERS frames.
    pub fn max_header_list_size(&mut self, max: u32) -> &mut Self {
        self.settings.set_max_header_list_size(Some(max));
        self
    }

    /// Sets the maximum number of concurrent streams.
    ///
    /// The maximum concurrent streams setting only controls the maximum number
    /// of streams that can be initiated by the remote peer. In other words,
    /// when this setting is set to 100, this does not limit the number of
    /// concurrent streams that can be created by the caller.
    ///
    /// It is recommended that this value be no smaller than 100, so as to not
    /// unnecessarily limit parallelism. However, any value is legal, including
    /// 0. If `max` is set to 0, then the remote will not be permitted to
    /// initiate streams.
    ///
    /// Note that streams in the reserved state, i.e., push promises that have
    /// been reserved but the stream has not started, do not count against this
    /// setting.
    ///
    /// Also note that if the remote *does* exceed the value set here, it is not
    /// a protocol level error. Instead, the `h2` library will immediately reset
    /// the stream.
    ///
    /// See [Section 5.1.2] in the HTTP/2 spec for more details.
    ///
    /// [Section 5.1.2]: https://http2.github.io/http2-spec/#rfc.section.5.1.2
    pub fn max_concurrent_streams(&mut self, max: u32) -> &mut Self {
        self.settings.set_max_concurrent_streams(Some(max));
        self
    }

    /// Sets the maximum number of concurrent locally reset streams.
    ///
    /// When a stream is explicitly reset by either calling
    /// [`SendResponse::send_reset`] or by dropping a [`SendResponse`] instance
    /// before completing the stream, the HTTP/2 specification requires that
    /// any further frames received for that stream must be ignored for "some
    /// time".
    ///
    /// In order to satisfy the specification, internal state must be maintained
    /// to implement the behavior. This state grows linearly with the number of
    /// streams that are locally reset.
    ///
    /// The `max_concurrent_reset_streams` setting configures sets an upper
    /// bound on the amount of state that is maintained. When this max value is
    /// reached, the oldest reset stream is purged from memory.
    ///
    /// Once the stream has been fully purged from memory, any additional frames
    /// received for that stream will result in a connection level protocol
    /// error, forcing the connection to terminate.
    ///
    /// The default value is 10.
    pub fn max_concurrent_reset_streams(&mut self, max: usize) -> &mut Self {
        self.reset_stream_max = max;
        self
    }

    /// Sets the maximum number of concurrent locally reset streams.
    ///
    /// When a stream is explicitly reset by either calling
    /// [`SendResponse::send_reset`] or by dropping a [`SendResponse`] instance
    /// before completing the stream, the HTTP/2 specification requires that
    /// any further frames received for that stream must be ignored for "some
    /// time".
    ///
    /// In order to satisfy the specification, internal state must be maintained
    /// to implement the behavior. This state grows linearly with the number of
    /// streams that are locally reset.
    ///
    /// The `reset_stream_duration` setting configures the max amount of time
    /// this state will be maintained in memory. Once the duration elapses, the
    /// stream state is purged from memory.
    ///
    /// Once the stream has been fully purged from memory, any additional frames
    /// received for that stream will result in a connection level protocol
    /// error, forcing the connection to terminate.
    ///
    /// The default value is 10 seconds.
    pub fn reset_stream_duration(&mut self, dur: Seconds) -> &mut Self {
        self.reset_stream_duration = dur;
        self
    }

    /// Enables the [extended CONNECT protocol].
    ///
    /// [extended CONNECT protocol]: https://datatracker.ietf.org/doc/html/rfc8441#section-4
    pub fn enable_connect_protocol(&mut self) -> &mut Self {
        self.settings.set_enable_connect_protocol(Some(1));
        self
    }

    /// Set handshake timeout.
    ///
    /// Hadnshake includes receiving preface and completing connection preparation.
    ///
    /// By default handshake timeuot is 5 seconds.
    pub fn handshake_timeout(&mut self, timeout: Seconds) -> &mut Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Set server connection disconnect timeout.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout(&mut self, val: Seconds) -> &mut Self {
        self.disconnect_timeout = val;
        self
    }

    /// Set keep-alive timeout.
    ///
    /// By default keep-alive time-out is set to 120 seconds.
    pub fn idle_timeout(&mut self, timeout: Seconds) -> &mut Self {
        self.keepalive_timeout = timeout;
        self
    }
}

impl<E, Ctl> ServerBuilder<E, Ctl>
where
    E: fmt::Debug,
    Ctl: ServiceFactory<ControlMessage<E>, Response = ControlResult> + 'static,
    Ctl::Error: fmt::Debug,
    Ctl::InitError: fmt::Debug,
{
    /// Creates a new configured HTTP/2 server.
    pub fn finish<F, Pub>(self, service: F) -> Server<Ctl, Pub>
    where
        F: IntoServiceFactory<Pub, Message>,
        Pub: ServiceFactory<Message, Response = (), Error = E> + 'static,
        Pub::InitError: fmt::Debug,
    {
        let settings = self.settings;
        let window_sz = settings
            .initial_window_size()
            .unwrap_or(frame::DEFAULT_INITIAL_WINDOW_SIZE);
        let window_sz_threshold = ((window_sz as f32) / 3.0) as u32;
        let connection_window_sz = self.initial_target_connection_window_size;
        let connection_window_sz_threshold = ((connection_window_sz as f32) / 4.0) as u32;

        let cfg = Config {
            settings,
            window_sz,
            window_sz_threshold,
            connection_window_sz,
            connection_window_sz_threshold,
            reset_duration: self.reset_stream_duration,
            reset_max: self.reset_stream_max,
        };

        Server::new(ServerInner {
            control: self.control,
            publish: service.into_factory(),
            config: Rc::new(cfg),
            keepalive_timeout: self.keepalive_timeout,
            handshake_timeout: self.handshake_timeout,
            disconnect_timeout: self.disconnect_timeout,
        })
    }
}

impl<E> Default for ServerBuilder<E> {
    fn default() -> ServerBuilder<E> {
        ServerBuilder::new()
    }
}
