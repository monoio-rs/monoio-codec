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

If you already have an codec(in tokio trait), you can use our `Framed` to make it works in monoio. If you haven't, maybe you can try to write encoder and decoder in pure async way.