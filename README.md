# Monoio-codec

[![Crates.io][crates-badge]][crates-url]
[![MIT/Apache-2 licensed][license-badge]][license-url]

[crates-badge]: https://img.shields.io/crates/v/monoio-codec.svg
[crates-url]: https://crates.io/crates/monoio-codec
[license-badge]: https://img.shields.io/crates/l/monoio-codec.svg
[license-url]: LICENSE-MIT

This crate provides 2 utils:
1. `Framed`, `FramedRead` and `FramedWrite`: Like the same things in tokio-util, but with monoio pure async `AsyncReadRent`, `AsyncWriteRent`, `Sink` and `Stream`.
2. `AsyncEncoder`, `AsyncDecoder`: Trait for encode and decode in async streaming way.

If the target codec is designed such that all required data lengths can be read from the header at low cost, using synchronous codec can lead to better performance. If not, you can use our async codec trait.

Note: These 2 modes can be used at the same time because our `Framed` is also a `BufIo`. It implements `AsyncReadRent` and `AsyncWriteRent`. If users know how much data to read, read directly from it can avoid data copy or buffer growth.
