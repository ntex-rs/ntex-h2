# ntex-h2

HTTP/2 client & server implementation for Rust.

[![License](https://img.shields.io/crates/l/ntex-h2.svg)](https://github.com/ntex-rs/ntex-h2#license)
[![Crates.io](https://img.shields.io/crates/v/ntex-h2.svg)](https://crates.io/crates/ntex-h2)
[![Documentation](https://img.shields.io/docsrs/ntex-h2/latest)](https://docs.rs/ntex-h2) 
[![Version](https://img.shields.io/badge/rustc-1.88+-lightgray.svg)](https://releases.rs/docs/1.88.0/) 
[![CI](https://github.com/ntex-rs/ntex-h2/actions/workflows/CI.yml/badge.svg)](https://github.com/ntex-rs/ntex-h2/actions/workflows/CI.yml) 
[![codecov](https://codecov.io/gh/ntex-rs/ntex-h2/branch/main/graph/badge.svg)](https://codecov.io/gh/ntex-rs/ntex-h2) 

More information about this crate can be found in the [crate documentation][dox]
and in the [HTTP/2 section][guide] of the ntex framework guide.

[dox]: https://docs.rs/ntex-h2
[guide]: https://docs.ntex.rs/14-h2.html

## Features

* Client and server HTTP/2 implementation.
* Implements part of HTTP/2 specification (priority and push are not supported).
* Passes [h2spec](https://github.com/summerwind/h2spec).

[h2spec]: https://github.com/summerwind/h2spec

## License

This project is licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

Code derived from the h2 crate is also covered by its original MIT license
([LICENSE-h2](LICENSE-h2)).
