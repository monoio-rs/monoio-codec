#![allow(stable_features)]
#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]
#![feature(impl_trait_in_assoc_type)]

mod async_codec;
mod framed;
pub mod length_delimited;
mod sync_codec;

pub use async_codec::{AsyncDecoder, AsyncEncoder};
pub use framed::{Framed, FramedRead, FramedWrite, NextWithCodec};
pub use sync_codec::{Decoded, Decoder, Encoder};
