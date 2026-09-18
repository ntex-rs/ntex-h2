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
//! ntex-h2 = "4"
//! ```
//!
//! # Layout
//!
//! The crate is split into [`client`] and [`server`] modules. Types that are
//! common to both clients and servers are located at the root of the crate.
//!
//! Use [`server::Server`] to accept HTTP/2 connections, [`client::Client`] for
//! pooled client connections, or [`client::SimpleClient`] for one established
//! transport.
//!
//! # Handshake
//!
//! Both clients and servers require a transport that is already ready for the
//! HTTP/2 connection preface. The caller is responsible for negotiating HTTP/2
//! before passing the transport to this crate.
//!
//! There are three ways to reach an appropriate state to start the HTTP/2
//! handshake.
//!
//! * Open an HTTP/1.1 connection and perform an [upgrade].
//! * Open a TLS connection and use ALPN to negotiate HTTP/2.
//! * Open a connection with prior knowledge, where both the client and the
//!   server assume that the connection is immediately ready to start the
//!   HTTP/2 handshake once opened.
//!
//! Once the transport is ready, pass it to [`server::Server`] or construct a
//! [`client::SimpleClient`]. The connection setup consists of:
//!
//! * The client sends the connection preface (a predefined sequence of 24 octets).
//! * Both endpoints send a SETTINGS frame.
//!
//! See the [Starting HTTP/2] in the specification for more details.
//!
//! # Flow control
//!
//! [Flow control] is a fundamental feature of HTTP/2. This crate
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
//! Inbound flow-control capacity is represented by [`Capacity`]. Consuming or
//! dropping a capacity value releases receive-window capacity to the peer.
//! Outbound flow control is handled by [`StreamRef::send_capacity`] and
//! [`client::SendStream::send_capacity`].
//!
//! [HTTP/2]: https://www.rfc-editor.org/rfc/rfc9113
//! [futures]: https://docs.rs/futures/
//! [Starting HTTP/2]: https://www.rfc-editor.org/rfc/rfc9113#section-3
//! [Flow control]: https://www.rfc-editor.org/rfc/rfc9113#section-5.2
//! [upgrade]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/Protocol_upgrade_mechanism
#![deny(clippy::pedantic)]
#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::missing_fields_in_debug,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::too_many_lines,
    clippy::unused_async_trait_impl
)]

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
pub mod control;
mod default;
mod dispatcher;
mod error;
mod message;
mod stream;
mod timer;
mod window;

pub mod client;
pub mod frame;
pub mod hpack;
pub mod server;

pub use self::codec::Codec;
pub use self::config::ServiceConfig;
pub use self::control::{Control, ControlAck};
pub use self::message::{Message, MessageKind, StreamEof};
pub use self::stream::{Capacity, Stream, StreamData, StreamRef};
pub use crate::error::{ConnectionError, EncoderError, OperationError, StreamError};
