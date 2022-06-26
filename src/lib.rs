//! An asynchronous, HTTP/2 server and client implementation.
//!
//! This library implements the [HTTP/2] specification. The implementation is
//! asynchronous, using [futures] as the basis for the API. The implementation
//! is also decoupled from TCP or TLS details. The user must handle ALPN and
//! HTTP/1.1 upgrades themselves.
//!
//! # Getting started
//!
//! Add the following to your `Cargo.toml` file:
//!
//! ```toml
//! [dependencies]
//! ntex-h2 = "0.1"
//! ```
//!
//! # Layout
//!
//! The crate is split into [`client`] and [`server`] modules. Types that are
//! common to both clients and servers are located at the root of the crate.
//!
//! See module level documentation for more details on how to use `h2`.
//!
//! # Handshake
//!
//! Both the client and the server require a connection to already be in a state
//! ready to start the HTTP/2 handshake. This library does not provide
//! facilities to do this.
//!
//! There are three ways to reach an appropriate state to start the HTTP/2
//! handshake.
//!
//! * Opening an HTTP/1.1 connection and performing an [upgrade].
//! * Opening a connection with TLS and use ALPN to negotiate the protocol.
//! * Open a connection with prior knowledge, i.e. both the client and the
//!   server assume that the connection is immediately ready to start the
//!   HTTP/2 handshake once opened.
//!
//! Once the connection is ready to start the HTTP/2 handshake, it can be
//! passed to [`server::handshake`] or [`client::handshake`]. At this point, the
//! library will start the handshake process, which consists of:
//!
//! * The client sends the connection preface (a predefined sequence of 24
//! octets).
//! * Both the client and the server sending a SETTINGS frame.
//!
//! See the [Starting HTTP/2] in the specification for more details.
//!
//! # Flow control
//!
//! [Flow control] is a fundamental feature of HTTP/2. The `h2` library
//! exposes flow control to the user.
//!
//! An HTTP/2 client or server may not send unlimited data to the peer. When a
//! stream is initiated, both the client and the server are provided with an
//! initial window size for that stream.  A window size is the number of bytes
//! the endpoint can send to the peer. At any point in time, the peer may
//! increase this window size by sending a `WINDOW_UPDATE` frame. Once a client
//! or server has sent data filling the window for a stream, no further data may
//! be sent on that stream until the peer increases the window.
//!
//! There is also a **connection level** window governing data sent across all
//! streams.
//!
//! Managing flow control for inbound data is done through [`FlowControl`].
//! Managing flow control for outbound data is done through [`SendStream`]. See
//! the struct level documentation for those two types for more details.
//!
//! [HTTP/2]: https://http2.github.io/
//! [futures]: https://docs.rs/futures/
//! [Starting HTTP/2]: http://httpwg.org/specs/rfc7540.html#starting
//! [upgrade]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Protocol_upgrade_mechanism

// #![deny(missing_debug_implementations, missing_docs)]
#![cfg_attr(test, deny(warnings))]
#![deny(rust_2018_idioms)]

macro_rules! proto_err {
    (conn: $($msg:tt)+) => {
        log::debug!("connection error PROTOCOL_ERROR -- {};", format_args!($($msg)+))
    };
    (stream: $($msg:tt)+) => {
        log::debug!("stream error PROTOCOL_ERROR -- {};", format_args!($($msg)+))
    };
}

mod codec;
mod config;
mod connection;
mod consts;
mod control;
mod default;
mod dispatcher;
mod error;
mod message;
mod stream;
mod window;

pub mod client;
pub mod frame;
pub mod hpack;
pub mod server;

//#[cfg(fuzzing)]
//pub mod fuzz_bridge;

pub use self::codec::Codec;
pub use self::config::Config;
pub use self::control::{ControlMessage, ControlResult};
pub use self::message::{Message, MessageKind, StreamEof};
pub use self::stream::{Capacity, Stream, StreamRef};
pub use crate::error::{ConnectionError, EncoderError, OperationError, StreamError};
