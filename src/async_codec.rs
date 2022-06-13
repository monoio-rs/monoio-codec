use std::io;

pub trait AsyncEncoder<Item> {
    type Error: From<io::Error>;
    type EncodeFuture<'a>: std::future::Future<Output = Result<(), Self::Error>>
    where
        Self: 'a;

    fn encode<IO>(&mut self, item: Item, dst: IO) -> Self::EncodeFuture<'_>;
}

pub trait AsyncDecoder {
    type Item;
    type Error: From<io::Error>;
    type DecodeFuture<'a>: std::future::Future<Output = Result<Option<Self::Item>, Self::Error>>
    where
        Self: 'a;

    fn decode<IO>(&mut self, src: IO) -> Self::DecodeFuture<'_>;
}
