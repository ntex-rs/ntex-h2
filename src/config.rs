use std::{cell::Cell, fmt, rc::Rc, time::Duration};

use ntex_io::DispatcherConfig;
use ntex_util::{channel::pool, time::Seconds};

use crate::{consts, frame, frame::Settings, frame::WindowSize};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    struct ConfigFlags: u8 {
        const SERVER = 0b0000_0001;
        const HTTPS  = 0b0000_0010;
    }
}

#[derive(Clone)]
pub struct Config(pub(crate) Rc<ConfigInner>);

/// Http2 connection configuration
pub(crate) struct ConfigInner {
    /// Initial window size of locally initiated streams
    pub(crate) window_sz: Cell<WindowSize>,
    pub(crate) window_sz_threshold: Cell<WindowSize>,
    /// How long a locally reset stream should ignore frames
    pub(crate) reset_duration: Cell<Duration>,
    /// Maximum number of locally reset streams to keep at a time
    pub(crate) reset_max: Cell<usize>,
    pub(crate) settings: Cell<Settings>,
    /// Initial window size for new connections.
    pub(crate) connection_window_sz: Cell<WindowSize>,
    pub(crate) connection_window_sz_threshold: Cell<WindowSize>,
    /// Maximum number of remote initiated streams
    pub(crate) remote_max_concurrent_streams: Cell<Option<u32>>,
    // /// If extended connect protocol is enabled.
    // pub extended_connect_protocol_enabled: bool,
    /// Connection timeouts
    pub(crate) handshake_timeout: Cell<Seconds>,
    pub(crate) client_timeout: Cell<Seconds>,
    pub(crate) ping_timeout: Cell<Seconds>,
    pub(crate) dispatcher_config: DispatcherConfig,

    /// Config flags
    flags: Cell<ConfigFlags>,

    pub(crate) pool: pool::Pool<()>,
}

impl Config {
    /// Create configuration for server
    pub fn server() -> Self {
        Config::new(true)
    }

    /// Create configuration for client
    pub fn client() -> Self {
        Config::new(false)
    }

    fn new(server: bool) -> Self {
        let window_sz = Cell::new(frame::DEFAULT_INITIAL_WINDOW_SIZE);
        let window_sz_threshold =
            Cell::new(((frame::DEFAULT_INITIAL_WINDOW_SIZE as f32) / 3.0) as u32);
        let connection_window_sz = Cell::new(consts::DEFAULT_CONNECTION_WINDOW_SIZE);
        let connection_window_sz_threshold =
            Cell::new(((consts::DEFAULT_CONNECTION_WINDOW_SIZE as f32) / 4.0) as u32);

        let mut settings = Settings::default();
        settings.set_max_concurrent_streams(Some(256));
        settings.set_enable_push(false);
        settings.set_max_header_list_size(Some(consts::DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE));

        let flags = if server {
            Cell::new(ConfigFlags::SERVER)
        } else {
            Cell::new(ConfigFlags::empty())
        };

        let dispatcher_config = DispatcherConfig::default();
        dispatcher_config
            .set_keepalive_timeout(Seconds(0))
            .set_disconnect_timeout(Seconds(3));

        Config(Rc::new(ConfigInner {
            flags,
            window_sz,
            window_sz_threshold,
            connection_window_sz,
            connection_window_sz_threshold,
            dispatcher_config,
            settings: Cell::new(settings),
            reset_max: Cell::new(consts::DEFAULT_RESET_STREAM_MAX),
            reset_duration: Cell::new(consts::DEFAULT_RESET_STREAM_SECS.into()),
            remote_max_concurrent_streams: Cell::new(None),
            client_timeout: Cell::new(Seconds(0)),
            handshake_timeout: Cell::new(Seconds(5)),
            ping_timeout: Cell::new(Seconds(10)),
            pool: pool::new(),
        }))
    }
}

impl Config {
    /// Indicates the initial window size (in octets) for stream-level
    /// flow control for received data.
    ///
    /// The initial window of a stream is used as part of flow control. For more
    /// details, see [`FlowControl`].
    ///
    /// The default value is 65,535.
    pub fn initial_window_size(&self, size: u32) -> &Self {
        self.0.window_sz.set(size);
        self.0.window_sz_threshold.set(((size as f32) / 3.0) as u32);

        let mut s = self.0.settings.get();
        s.set_initial_window_size(Some(size));
        self.0.settings.set(s);
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
    pub fn initial_connection_window_size(&self, size: u32) -> &Self {
        assert!(size <= consts::MAX_WINDOW_SIZE);
        self.0.connection_window_sz.set(size);
        self.0
            .connection_window_sz_threshold
            .set(((size as f32) / 4.0) as u32);
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
    pub fn max_frame_size(&self, max: u32) -> &Self {
        let mut s = self.0.settings.get();
        s.set_max_frame_size(max);
        self.0.settings.set(s);
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
    ///
    /// By default value is set to 48Kb.
    pub fn max_header_list_size(&self, max: u32) -> &Self {
        let mut s = self.0.settings.get();
        s.set_max_header_list_size(Some(max));
        self.0.settings.set(s);
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
    pub fn max_concurrent_streams(&self, max: u32) -> &Self {
        self.0.remote_max_concurrent_streams.set(Some(max));
        let mut s = self.0.settings.get();
        s.set_max_concurrent_streams(Some(max));
        self.0.settings.set(s);
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
    /// The default value is 30.
    pub fn max_concurrent_reset_streams(&self, max: usize) -> &Self {
        self.0.reset_max.set(max);
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
    /// The default value is 30 seconds.
    pub fn reset_stream_duration(&self, dur: Seconds) -> &Self {
        self.0.reset_duration.set(dur.into());
        self
    }

    // /// Enables the [extended CONNECT protocol].
    // ///
    // /// [extended CONNECT protocol]: https://datatracker.ietf.org/doc/html/rfc8441#section-4
    // pub fn enable_connect_protocol(&self) -> &Self {
    //     let mut s = self.0.settings.get();
    //     s.set_enable_connect_protocol(Some(1));
    //     self.0.settings.set(s);
    //     self
    // }

    /// Set handshake timeout.
    ///
    /// Hadnshake includes receiving preface and completing connection preparation.
    ///
    /// By default handshake timeuot is 5 seconds.
    pub fn handshake_timeout(&self, timeout: Seconds) -> &Self {
        self.0.handshake_timeout.set(timeout);
        self
    }

    /// Set server client timeout for first request.
    ///
    /// Defines a timeout for reading client request header. If a client does not transmit
    /// the entire set headers within this time, the request is terminated with
    /// the 408 (Request Time-out) error.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 3 seconds.
    pub fn client_timeout(&self, timeout: Seconds) -> &Self {
        self.0.client_timeout.set(timeout);
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
    pub fn disconnect_timeout(&self, val: Seconds) -> &Self {
        self.0.dispatcher_config.set_disconnect_timeout(val);
        self
    }

    /// Set ping timeout.
    ///
    /// By default ping time-out is set to 60 seconds.
    pub fn ping_timeout(&self, timeout: Seconds) -> &Self {
        self.0.ping_timeout.set(timeout);
        self
    }

    /// Check if configuration defined for server.
    pub fn is_server(&self) -> bool {
        self.0.flags.get().contains(ConfigFlags::SERVER)
    }

    pub(crate) fn inner(&self) -> &ConfigInner {
        self.0.as_ref()
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("window_sz", &self.0.window_sz.get())
            .field("window_sz_threshold", &self.0.window_sz_threshold.get())
            .field("reset_duration", &self.0.reset_duration.get())
            .field("reset_max", &self.0.reset_max.get())
            .field("connection_window_sz", &self.0.connection_window_sz.get())
            .field(
                "connection_window_sz_threshold",
                &self.0.connection_window_sz_threshold.get(),
            )
            .field(
                "remote_max_concurrent_streams",
                &self.0.remote_max_concurrent_streams.get(),
            )
            .field("settings", &self.0.settings.get())
            .finish()
    }
}

impl fmt::Debug for ConfigInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("window_sz", &self.window_sz.get())
            .field("window_sz_threshold", &self.window_sz_threshold.get())
            .field("reset_duration", &self.reset_duration.get())
            .field("reset_max", &self.reset_max.get())
            .field("connection_window_sz", &self.connection_window_sz.get())
            .field(
                "connection_window_sz_threshold",
                &self.connection_window_sz_threshold.get(),
            )
            .field(
                "remote_max_concurrent_streams",
                &self.remote_max_concurrent_streams.get(),
            )
            .field("settings", &self.settings.get())
            .finish()
    }
}
