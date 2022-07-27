#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

mod async_codec;
mod framed;

pub use async_codec::{AsyncDecoder, AsyncEncoder};
pub use framed::{Framed, FramedRead, FramedWrite};
pub use tokio_util::codec::{Decoder, Encoder};
