#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

mod async_codec;
mod framed;

pub use tokio_util::codec::Decoder;
pub use tokio_util::codec::Encoder;

pub use async_codec::{AsyncDecoder, AsyncEncoder};
pub use framed::{Framed, FramedRead, FramedWrite};
