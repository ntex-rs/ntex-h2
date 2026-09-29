#![allow(
    clippy::new_without_default,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::missing_panics_doc
)]
use std::{cell::Cell, time::Duration};

use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::time::Seconds;

use crate::{consts, frame, frame::Settings, frame::WindowSize};

#[derive(Debug)]
/// HTTP/2 connection and service configuration.
pub struct ServiceConfig {
    pub(crate) settings: Settings,
    /// Initial window size of locally initiated streams
    pub(crate) window_sz: i32,
    pub(crate) window_sz_threshold: WindowSize,
    /// How long a locally reset stream should ignore frames
    pub(crate) reset_duration: Duration,
    /// Maximum number of locally reset streams to keep at a time
    pub(crate) reset_max: usize,
    /// Initial window size for new connections.
    pub(crate) connection_window_sz: i32,
    pub(crate) connection_window_sz_threshold: WindowSize,
    /// Maximum number of remote initiated streams
    pub(crate) remote_max_concurrent_streams: Option<u32>,
    /// Limit number of continuation frames for headers
    pub(crate) max_header_continuations: usize,
    /// Maximum number of headers
    pub(crate) max_headers: usize,
    /// Capacity availability timeout
    pub(crate) capacity_timeout: Option<Seconds>,
    /// Maximum number of in-flight publish calls
    pub(crate) max_inflight: u16,
    // /// If extended connect protocol is enabled.
    // pub extended_connect_protocol_enabled: bool,
    /// Connection timeouts
    pub(crate) handshake_timeout: Seconds,
    pub(crate) ping_timeout: Seconds,
    pub(crate) settings_timeout: Seconds,

    config: CfgContext,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig::new()
    }
}

impl Configuration for ServiceConfig {
    const NAME: &str = "Http/2 service configuration";

    fn ctx(&self) -> &CfgContext {
        &self.config
    }

    fn set_ctx(&mut self, ctx: CfgContext) {
        self.config = ctx;
    }
}

impl ServiceConfig {
    /// Creates a configuration with HTTP/2 defaults.
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

        ServiceConfig {
            window_sz,
            window_sz_threshold,
            connection_window_sz,
            connection_window_sz_threshold,
            settings,
            reset_max: consts::DEFAULT_RESET_STREAM_MAX,
            reset_duration: consts::DEFAULT_RESET_STREAM_SECS.into(),
            remote_max_concurrent_streams: Some(consts::DEFAULT_MAX_CONCURRENT_STREAMS),
            max_headers: consts::DEFAULT_MAX_HEADERS,
            max_header_continuations: consts::DEFAULT_MAX_COUNTINUATIONS,
            capacity_timeout: Some(consts::DEFAULT_CAPACITY_TIMEOUT),
            max_inflight: consts::DEFAULT_MAX_INFLIGHT,
            handshake_timeout: Seconds(5),
            ping_timeout: Seconds(10),
            settings_timeout: Seconds(5),
            config: CfgContext::default(),
        }
    }
}

impl ServiceConfig {
    #[must_use]
    /// Indicates the initial window size (in octets) for stream-level
    /// flow control for received data.
    ///
    /// The initial window of a stream is used as part of flow control. For more
    /// details, see [flow control](https://www.rfc-editor.org/rfc/rfc9113#section-5.2).
    ///
    /// The default value is 65,535.
    ///
    /// # Panics
    ///
    /// Panics if `size` is negative.
    pub fn set_initial_window_size(mut self, size: i32) -> Self {
        assert!((0..=consts::MAX_WINDOW_SIZE).contains(&size));

        self.window_sz = size;
        self.window_sz_threshold = ((size as f32) / 3.0) as u32;
        self.settings.set_initial_window_size(Some(size as u32));
        self
    }

    #[must_use]
    #[allow(clippy::missing_panics_doc)]
    /// Indicates the initial window size (in octets) for connection-level flow control
    /// for received data.
    ///
    /// The initial window of a connection is used as part of flow control. For more details,
    /// see [flow control](https://www.rfc-editor.org/rfc/rfc9113#section-5.2).
    ///
    /// The window is released when received data is consumed, so it bounds the amount of
    /// unconsumed data buffered for all streams of the connection.
    ///
    /// The connection window starts at 65,535 and can only be increased with
    /// `WINDOW_UPDATE` frames, smaller values do not shrink it.
    ///
    /// The default value is 4 MiB.
    ///
    /// # Panics
    ///
    /// Panics if `size` is negative.
    pub fn set_initial_connection_window_size(mut self, size: i32) -> Self {
        assert!((0..=consts::MAX_WINDOW_SIZE).contains(&size));
        self.connection_window_sz = size;
        self.connection_window_sz_threshold = ((size as f32) / 4.0) as u32;
        self
    }

    #[must_use]
    /// Indicates the size (in octets) of the largest HTTP/2 frame payload that
    /// the local endpoint is able to accept.
    ///
    /// The value is advertised to the peer with `SETTINGS_MAX_FRAME_SIZE`, the
    /// peer must split larger payloads into multiple frames. Frames sent to the
    /// peer are limited by the peer's own setting.
    ///
    /// The value **must** be between 16,384 and 16,777,215. The default value is 16,384.
    ///
    /// # Panics
    ///
    /// This function panics if `max` is not within the legal range specified
    /// above.
    pub fn set_max_frame_size(mut self, max: u32) -> Self {
        self.settings.set_max_frame_size(max);
        self
    }

    #[must_use]
    /// Set the maximum number of headers.
    ///
    /// When a request is received, the parser will reserve a buffer
    /// to store headers for optimal performance.
    ///
    /// If a header block contains more headers than the buffer size, the
    /// stream is reset with `REFUSED_STREAM` reason.
    ///
    /// The default is 96.
    pub fn set_max_headers(mut self, val: usize) -> Self {
        self.max_headers = val;
        self
    }

    #[must_use]
    /// Sets the maximum decoded header-list size.
    ///
    /// This advisory setting informs a peer of the maximum size of header list
    /// that the sender is prepared to accept, in octets. The value is based on
    /// the uncompressed size of header fields, including the length of the name
    /// and value in octets plus an overhead of 32 octets for each header field.
    ///
    /// This setting is also used to limit the maximum amount of data that is
    /// buffered to decode HEADERS frames.
    ///
    /// The default is 48 KiB.
    pub fn set_max_header_list_size(mut self, max: u32) -> Self {
        self.settings.set_max_header_list_size(Some(max));
        self
    }

    #[must_use]
    /// Sets the maximum number of continuation frames for one header block.
    ///
    /// The default is 5.
    pub fn set_max_header_continuation_frames(mut self, max: usize) -> Self {
        self.max_header_continuations = max;
        self
    }

    #[must_use]
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
    /// If the remote exceeds the value set here, the stream is reset with
    /// `REFUSED_STREAM`. Refused streams count toward the rapid-reset limit,
    /// once at least 10 streams have been opened and half of them are reset
    /// the connection is closed with `GOAWAY`
    /// ([`ConnectionError::StreamResetsLimit`](crate::ConnectionError::StreamResetsLimit)).
    ///
    /// The default value is 256.
    ///
    /// See [Section 5.1.2] in the HTTP/2 spec for more details.
    ///
    /// [Section 5.1.2]: https://www.rfc-editor.org/rfc/rfc9113#section-5.1.2
    pub fn set_max_concurrent_streams(mut self, max: u32) -> Self {
        self.remote_max_concurrent_streams = Some(max);
        self.settings.set_max_concurrent_streams(Some(max));
        self
    }

    #[must_use]
    /// Sets the maximum number of concurrent locally reset streams.
    ///
    /// When a stream is explicitly reset, or an unfinished stream handle is
    /// dropped, HTTP/2 requires further frames for that stream to be ignored
    /// for a period of time.
    ///
    /// In order to satisfy the specification, internal state must be maintained
    /// to implement the behavior. This state grows linearly with the number of
    /// streams that are locally reset.
    ///
    /// This setting configures an upper
    /// bound on the amount of state that is maintained. When this max value is
    /// reached, the oldest reset stream is purged from memory.
    ///
    /// Once the stream has been fully purged from memory, any additional `DATA`
    /// or `WINDOW_UPDATE` frames received for that stream will result in a
    /// connection level protocol error, forcing the connection to terminate.
    /// `RST_STREAM` and `PRIORITY` frames are ignored.
    ///
    /// The default value is 32.
    pub fn set_max_concurrent_reset_streams(mut self, val: usize) -> Self {
        self.reset_max = val;
        self
    }

    #[must_use]
    /// Sets how long locally reset stream state is retained.
    ///
    /// When a stream is explicitly reset, or an unfinished stream handle is
    /// dropped, HTTP/2 requires further frames for that stream to be ignored
    /// for a period of time.
    ///
    /// In order to satisfy the specification, internal state must be maintained
    /// to implement the behavior. This state grows linearly with the number of
    /// streams that are locally reset.
    ///
    /// The `reset_stream_duration` setting configures the max amount of time
    /// this state will be maintained in memory. Once the duration elapses, the
    /// stream state is purged from memory.
    ///
    /// Once the stream has been fully purged from memory, any additional `DATA`
    /// or `WINDOW_UPDATE` frames received for that stream will result in a
    /// connection level protocol error, forcing the connection to terminate.
    /// `RST_STREAM` and `PRIORITY` frames are ignored.
    ///
    /// The default value is 30 seconds.
    pub fn set_reset_stream_duration(mut self, dur: Seconds) -> Self {
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

    #[must_use]
    /// Sets the connection handshake timeout.
    ///
    /// For servers the handshake includes receiving the client preface and
    /// creating the request service. For clients created by
    /// [`client::Connector`](crate::client::Connector) it includes establishing
    /// the transport and creating the connection. The connections pool uses
    /// [`ClientBuilder::connect_timeout`](crate::client::ClientBuilder::connect_timeout)
    /// instead.
    ///
    /// A zero duration disables the timeout. The default is 5 seconds.
    pub fn set_handshake_timeout(mut self, timeout: Seconds) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    #[must_use]
    /// Sets the keep-alive ping timeout.
    ///
    /// A `PING` frame is sent to the peer every `timeout` interval. If the
    /// previous `PING` is not acknowledged by the next interval, the connection
    /// is closed and the open streams fail with
    /// [`ConnectionError::KeepaliveTimeout`](crate::ConnectionError::KeepaliveTimeout).
    ///
    /// A zero duration disables keep-alive pings. The default is 10 seconds.
    pub fn set_ping_timeout(mut self, timeout: Seconds) -> Self {
        self.ping_timeout = timeout;
        self
    }

    #[must_use]
    /// Sets the local settings acknowledgment timeout.
    ///
    /// If the peer does not acknowledge the local `SETTINGS` frame in time, the
    /// connection is closed with `SETTINGS_TIMEOUT` (RFC 9113 §6.5.3) and the
    /// open streams fail with
    /// [`ConnectionError::SettingsTimeout`](crate::ConnectionError::SettingsTimeout).
    ///
    /// A zero duration disables the timeout. The default is 5 seconds.
    pub fn set_settings_timeout(mut self, timeout: Seconds) -> Self {
        self.settings_timeout = timeout;
        self
    }

    #[must_use]
    /// Sets the send-capacity availability timeout.
    ///
    /// A stream that waits for send capacity longer than the timeout is reset with
    /// `CANCEL`, the waiter fails with `StreamError::CapacityTimeout`. The final
    /// message of a remote stream is published only if the stream has a publish
    /// call in flight, see [`StreamRef::reset`](crate::StreamRef::reset).
    ///
    /// A zero duration disables the timeout. The default is 5 seconds.
    pub fn set_capacity_timeout(mut self, timeout: Seconds) -> Self {
        if timeout.is_zero() {
            self.capacity_timeout = None;
        } else {
            self.capacity_timeout = Some(timeout);
        }
        self
    }

    #[must_use]
    /// Sets the maximum number of in-flight service calls of a connection.
    ///
    /// Every received HEADERS, DATA or trailers frame is published to the
    /// service, control events are counted as well. Once the limit is reached
    /// the connection stops processing incoming frames until a call completes.
    ///
    /// The value must be between 1 and 32,767. The default value is 16,384.
    ///
    /// # Panics
    ///
    /// Panics if `max` is not within the range specified above.
    pub fn set_max_inflight_messages(mut self, max: u16) -> Self {
        assert!(
            (1..=u16::MAX / 2).contains(&max),
            "max in-flight messages must be between 1 and 32767"
        );
        self.max_inflight = max;
        self
    }
}

thread_local! {
    static SHUTDOWN: Cell<bool> = const { Cell::new(false) };
}

// Current limitation, shutdown is thread global
impl ServiceConfig {
    /// Returns whether shutdown has been requested on the current thread.
    ///
    /// See [`ServiceConfig::shutdown`].
    pub fn is_shutdown(&self) -> bool {
        SHUTDOWN.with(Cell::get)
    }

    /// Requests shutdown for services running on the current thread.
    ///
    /// The flag is global for the thread and cannot be reset. Server
    /// connections check it when their dispatcher is polled next and then
    /// disconnect gracefully, new streams are refused with `REFUSED_STREAM` and
    /// the connection is closed after the open streams complete. Client
    /// connections do not check the flag.
    pub fn shutdown() {
        SHUTDOWN.with(|v| v.set(true));
    }
}
