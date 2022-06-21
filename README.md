# ntex-h2

HTTP/2 client & server implementation for Rust.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Crates.io](https://img.shields.io/crates/v/ntex-h2.svg)](https://crates.io/crates/ntex-h2)
[![Documentation](https://docs.rs/ntex-h2/badge.svg)][dox]
[![CI](https://github.com/ntex-rs/ntex-h2/actions/workflows/CI.yml/badge.svg)](https://github.com/ntex-rs/ntex-h2/actions/workflows/CI.yml)

More information about this crate can be found in the [crate documentation][dox].

[dox]: https://docs.rs/ntex-h2

## Features

* Client and server HTTP/2 implementation.
* Implements part of HTTP/2 specification (priority and push are not supported).
* Passes [h2spec](https://github.com/summerwind/h2spec).

### Original

This is fork of [h2 crate](https://crates.io/crates/h2) optimized for ntex framework

[h2spec]: https://github.com/summerwind/h2spec
