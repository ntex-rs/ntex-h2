use std::time::Duration;

use ntex_util::time::Seconds;

use crate::{consts, frame, frame::Settings, frame::WindowSize};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    struct ConfigFlags: u8 {
        const HTTPS  =    0b0000_0010;
        const SHUTDOWN  = 0b0000_0100;
    }
}

#[derive(Copy, Clone, Debug)]
/// Http2 connection configuration
pub struct ServiceConfig {
    pub(crate) settings: Settings,
    /// Initial window size of locally initiated streams
    pub(crate) window_sz: WindowSize,
    pub(crate) window_sz_threshold: WindowSize,
    /// How long a locally reset stream should ignore frames
    pub(crate) reset_duration: Duration,
    /// Maximum number of locally reset streams to keep at a time
    pub(crate) reset_max: usize,
    /// Initial window size for new connections.
    pub(crate) connection_window_sz: WindowSize,
    pub(crate) connection_window_sz_threshold: WindowSize,
    /// Maximum number of remote initiated streams
    pub(crate) remote_max_concurrent_streams: Option<u32>,
    /// Limit number of continuation frames for headers
    pub(crate) max_header_continuations: usize,
    // /// If extended connect protocol is enabled.
    // pub extended_connect_protocol_enabled: bool,
    /// Connection timeouts
    pub(crate) handshake_timeout: Seconds,
    pub(crate) ping_timeout: Seconds,

    /// Config flags
    flags: ConfigFlags,
}

impl Default for &'static ServiceConfig {
    fn default() -> Self {
        thread_local! {
            static DEFAULT: &'static ServiceConfig = Box::leak(Box::new(ServiceConfig::default()));
        }
        DEFAULT.with(|cfg| *cfg)
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceConfig {
    /// Create configuration
    pub fn new() -> Self {
        let window_sz = frame::DEFAULT_INITIAL_WINDOW_SIZE;
        let window_sz_threshold = ((frame::DEFAULT_INITIAL_WINDOW_SIZE as f32) / 3.0) as u32;
        let connection_window_sz = consts::DEFAULT_CONNECTION_WINDOW_SIZE;
        let connection_window_sz_threshold =
            ((consts::DEFAULT_CONNECTION_WINDOW_SIZE as f32) / 4.0) as u32;

        let mut settings = Settings::default();
        settings.set_max_concurrent_streams(Some(256));
        settings.set_enable_push(false);
        settings.set_max_header_list_size(Some(consts::DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE));

        let flags = ConfigFlags::empty();

        ServiceConfig {
            flags,
            window_sz,
            window_sz_threshold,
            connection_window_sz,
            connection_window_sz_threshold,
            settings,
            reset_max: consts::DEFAULT_RESET_STREAM_MAX,
            reset_duration: consts::DEFAULT_RESET_STREAM_SECS.into(),
            remote_max_concurrent_streams: None,
            max_header_continuations: consts::DEFAULT_MAX_COUNTINUATIONS,
            handshake_timeout: Seconds(5),
            ping_timeout: Seconds(10),
        }
    }
}

impl ServiceConfig {
    /// Indicates the initial window size (in octets) for stream-level
    /// flow control for received data.
    ///
    /// The initial window of a stream is used as part of flow control. For more
    /// details, see [`FlowControl`].
    ///
    /// The default value is 65,535.
    pub fn initial_window_size(mut self, size: u32) -> Self {
        self.window_sz = size;
        self.window_sz_threshold = ((size as f32) / 3.0) as u32;
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
    pub fn initial_connection_window_size(mut self, size: u32) -> Self {
        assert!(size <= consts::MAX_WINDOW_SIZE);
        self.connection_window_sz = size;
        self.connection_window_sz_threshold = ((size as f32) / 4.0) as u32;
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
    pub fn max_frame_size(mut self, max: u32) -> Self {
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
    ///
    /// By default value is set to 48Kb.
    pub fn max_header_list_size(mut self, max: u32) -> Self {
        self.settings.set_max_header_list_size(Some(max));
        self
    }

    /// Sets the max number of continuation frames for HEADERS
    ///
    /// By default value is set to 5
    pub fn max_header_continuation_frames(mut self, max: usize) -> Self {
        self.max_header_continuations = max;
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
    pub fn max_concurrent_streams(mut self, max: u32) -> Self {
        self.remote_max_concurrent_streams = Some(max);
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
    /// The default value is 32.
    pub fn max_concurrent_reset_streams(mut self, val: usize) -> Self {
        self.reset_max = val;
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
    pub fn reset_stream_duration(mut self, dur: Seconds) -> Self {
        self.reset_duration = dur.into();
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
    pub fn handshake_timeout(mut self, timeout: Seconds) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Set ping timeout.
    ///
    /// By default ping time-out is set to 60 seconds.
    pub fn ping_timeout(mut self, timeout: Seconds) -> Self {
        self.ping_timeout = timeout;
        self
    }

    /// Check if service is shutting down.
    pub fn is_shutdown(&self) -> bool {
        self.flags.contains(ConfigFlags::SHUTDOWN)
    }

    /// Set service shutdown.
    pub fn shutdown(&self) {
        //let mut flags = self.0.flags.get();
        //flags.insert(ConfigFlags::SHUTDOWN);
        //self.0.flags.set(flags);
    }
}
