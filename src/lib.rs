#![allow(stable_features)]

mod async_codec;
mod framed;
pub mod length_delimited;
mod sync_codec;

pub use async_codec::{AsyncDecoder, AsyncEncoder};
#[deprecated]
pub use framed::StreamWithCodec as NextWithCodec;
pub use framed::{Framed, FramedRead, FramedWrite, SinkWithCodec, StreamWithCodec};
pub use sync_codec::{Decoded, Decoder, Encoder};
