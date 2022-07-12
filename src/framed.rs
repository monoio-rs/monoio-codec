// Part of the helper functions and tests are borrowed from tokio-util.

use std::{
    borrow::{Borrow, BorrowMut},
    fmt,
    future::Future,
};

use bytes::{BufMut, BytesMut};
use monoio::{
    buf::{SliceMut, IoBufMut},
    io::{sink::Sink, stream::Stream, AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt},
};

use crate::{Decoder, Encoder};

const INITIAL_CAPACITY: usize = 8 * 1024;
const BACKPRESSURE_BOUNDARY: usize = INITIAL_CAPACITY;
const RESERVE: usize = 1024;

pub struct FramedInner<IO, Codec, S> {
    io: IO,
    codec: Codec,
    state: S,
}

#[derive(Debug)]
pub struct ReadState {
    state: State,
    buffer: BytesMut,
}

impl ReadState {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            state: State::Framing,
            buffer: BytesMut::with_capacity(capacity),
        }
    }
}

impl Default for ReadState {
    fn default() -> Self {
        Self::with_capacity(INITIAL_CAPACITY)
    }
}

#[derive(Debug)]
enum State {
    Framing,
    Pausing,
    Paused,
    Errored,
}

#[derive(Debug)]
pub struct WriteState {
    buffer: BytesMut,
}

impl Default for WriteState {
    fn default() -> Self {
        Self {
            buffer: BytesMut::with_capacity(INITIAL_CAPACITY),
        }
    }
}

#[derive(Debug, Default)]
pub struct RWState {
    read: ReadState,
    write: WriteState,
}

impl Borrow<ReadState> for RWState {
    fn borrow(&self) -> &ReadState {
        &self.read
    }
}
impl BorrowMut<ReadState> for RWState {
    fn borrow_mut(&mut self) -> &mut ReadState {
        &mut self.read
    }
}
impl Borrow<WriteState> for RWState {
    fn borrow(&self) -> &WriteState {
        &self.write
    }
}
impl BorrowMut<WriteState> for RWState {
    fn borrow_mut(&mut self) -> &mut WriteState {
        &mut self.write
    }
}

impl<IO, Codec, S> FramedInner<IO, Codec, S> {
    fn new(io: IO, codec: Codec, state: S) -> Self {
        Self { io, codec, state }
    }

    // In tokio there are 5 states. But since we use pure async here,
    // we do not need to return Pending so we don't need to save the state
    // when Penging returned. We only need to save state when return
    // `Option<Item>`.
    // We have 4 states: Framing, Pausing, Paused and Errored.
    async fn next_with(
        io: &mut IO,
        codec: &mut Codec,
        state: &mut S,
    ) -> Option<Result<Codec::Item, Codec::Error>>
    where
        IO: AsyncReadRent,
        Codec: Decoder,
        S: BorrowMut<ReadState>,
    {
        macro_rules! ok {
            ($result: expr, $state: expr) => {
                match $result {
                    Ok(x) => x,
                    Err(e) => {
                        *$state = State::Errored;
                        return Some(Err(e.into()));
                    }
                }
            };
        }

        let read_state: &mut ReadState = state.borrow_mut();
        let state = &mut read_state.state;
        let buffer = &mut read_state.buffer;

        loop {
            match state {
                // On framing, we will decode first. If the decoder needs more data,
                // we will do read and await it.
                // If we get an error or eof, we will transfer state.
                State::Framing => loop {
                    if !buffer.is_empty() {
                        if let Some(item) = ok!(codec.decode(buffer), state) {
                            return Some(Ok(item));
                        }
                    }
                    buffer.reserve(RESERVE);
                    let (begin, end) = {
                        let buffer_ptr = buffer.write_ptr();
                        let slice_to_write = buffer.chunk_mut();
                        let begin =
                            unsafe { slice_to_write.as_mut_ptr().offset_from(buffer_ptr) } as usize;
                        let end = begin + slice_to_write.len();
                        (begin, end)
                    };
                    let owned_buf = std::mem::replace(buffer, BytesMut::new());
                    let owned_slice = unsafe { SliceMut::new_unchecked(owned_buf, begin, end) };
                    let (result, owned_slice) = io.read(owned_slice).await;
                    *buffer = owned_slice.into_inner();
                    let n = ok!(result, state);
                    if n == 0 {
                        *state = State::Pausing;
                        break;
                    }
                },
                // On Pausing, we will loop decode_eof until None or Error.
                State::Pausing => {
                    return match ok!(codec.decode_eof(buffer), state) {
                        Some(item) => Some(Ok(item)),
                        None => {
                            // Buffer has no data, we can transfer to Paused.
                            *state = State::Paused;
                            None
                        }
                    };
                }
                // On Paused, we need to read directly.
                State::Paused => {
                    buffer.reserve(RESERVE);
                    let (begin, end) = {
                        let buffer_ptr = buffer.write_ptr();
                        let slice_to_write = buffer.chunk_mut();
                        let begin =
                            unsafe { slice_to_write.as_mut_ptr().offset_from(buffer_ptr) } as usize;
                        let end = begin + slice_to_write.len();
                        (begin, end)
                    };
                    let owned_buf = std::mem::replace(buffer, BytesMut::new());
                    let owned_slice = unsafe { SliceMut::new_unchecked(owned_buf, begin, end) };
                    let (result, owned_slice) = io.read(owned_slice).await;
                    *buffer = owned_slice.into_inner();
                    let n = ok!(result, state);
                    if n == 0 {
                        // still paused
                        return None;
                    }
                    // read something, then we move to framing state
                    *state = State::Framing;
                }
                // On Errored, we need to return None and trans to Paused.
                State::Errored => {
                    *state = State::Paused;
                    return None;
                }
            }
        }
    }
}

impl<IO, Codec, S> Stream for FramedInner<IO, Codec, S>
where
    IO: AsyncReadRent,
    Codec: Decoder,
    S: BorrowMut<ReadState>,
{
    type Item = Result<Codec::Item, Codec::Error>;

    type NextFuture<'a> = impl Future<Output = Option<Self::Item>> where Self: 'a;

    fn next(&mut self) -> Self::NextFuture<'_> {
        Self::next_with(&mut self.io, &mut self.codec, &mut self.state)
    }
}

impl<IO, Codec, S, Item> Sink<Item> for FramedInner<IO, Codec, S>
where
    IO: AsyncWriteRent,
    Codec: Encoder<Item>,
    S: BorrowMut<WriteState>,
{
    type Error = Codec::Error;

    type SendFuture<'a> = impl Future<Output = Result<(), Self::Error>> where Self: 'a;

    type FlushFuture<'a> = impl Future<Output = Result<(), Self::Error>> where Self: 'a;

    type CloseFuture<'a> = impl Future<Output = Result<(), Self::Error>> where Self: 'a;

    fn send(&mut self, item: Item) -> Self::SendFuture<'_> {
        async move {
            if self.state.borrow_mut().buffer.len() > BACKPRESSURE_BOUNDARY {
                self.flush().await?;
            }
            self.codec
                .encode(item, &mut self.state.borrow_mut().buffer)?;
            Ok(())
        }
    }

    fn flush(&mut self) -> Self::FlushFuture<'_> {
        async move {
            let WriteState { buffer } = self.state.borrow_mut();
            if buffer.is_empty() {
                return Ok(());
            }
            // This action does not allocate.
            let buf = std::mem::replace(buffer, BytesMut::new());
            let (result, buf) = self.io.write_all(buf).await;
            *buffer = buf;
            result?;
            buffer.clear();
            self.io.flush().await?;
            Ok(())
        }
    }

    fn close(&mut self) -> Self::CloseFuture<'_> {
        async move {
            self.flush().await?;
            self.io.shutdown().await?;
            Ok(())
        }
    }
}

pub struct Framed<IO, Codec> {
    inner: FramedInner<IO, Codec, RWState>,
}

pub struct FramedRead<IO, Codec> {
    inner: FramedInner<IO, Codec, ReadState>,
}

pub struct FramedWrite<IO, Codec> {
    inner: FramedInner<IO, Codec, WriteState>,
}

impl<IO, Codec> Framed<IO, Codec> {
    pub fn new(io: IO, codec: Codec) -> Self {
        Self {
            inner: FramedInner::new(io, codec, RWState::default()),
        }
    }

    pub fn with_capacity(io: IO, codec: Codec, capacity: usize) -> Self {
        Self {
            inner: FramedInner::new(
                io,
                codec,
                RWState {
                    read: ReadState::with_capacity(capacity),
                    write: Default::default(),
                },
            ),
        }
    }

    /// Returns a reference to the underlying I/O stream wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_ref(&self) -> &IO {
        &self.inner.io
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_mut(&mut self) -> &mut IO {
        &mut self.inner.io
    }

    /// Returns a reference to the underlying codec wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    pub fn codec(&self) -> &Codec {
        &self.inner.codec
    }

    /// Returns a mutable reference to the underlying codec wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    pub fn codec_mut(&mut self) -> &mut Codec {
        &mut self.inner.codec
    }

    /// Maps the codec `U` to `C`, preserving the read and write buffers
    /// wrapped by `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    pub fn map_codec<CodecNew, F>(self, map: F) -> Framed<IO, CodecNew>
    where
        F: FnOnce(Codec) -> CodecNew,
    {
        let FramedInner { io, codec, state } = self.inner;
        Framed {
            inner: FramedInner {
                io,
                codec: map(codec),
                state,
            },
        }
    }

    /// Returns a reference to the read buffer.
    pub fn read_buffer(&self) -> &BytesMut {
        &self.inner.state.read.buffer
    }

    /// Returns a mutable reference to the read buffer.
    pub fn read_buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.inner.state.read.buffer
    }

    /// Returns a reference to the write buffer.
    pub fn write_buffer(&self) -> &BytesMut {
        &self.inner.state.write.buffer
    }

    /// Returns a mutable reference to the write buffer.
    pub fn write_buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.inner.state.write.buffer
    }

    /// Consumes the `Framed`, returning its underlying I/O stream.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn into_inner(self) -> IO {
        self.inner.io
    }

    /// Equivalent to Stream::next but with custom codec.
    pub async fn next_with<C: Decoder>(
        &mut self,
        codec: &mut C,
    ) -> Option<Result<C::Item, C::Error>>
    where
        IO: AsyncReadRent,
    {
        FramedInner::next_with(&mut self.inner.io, codec, &mut self.inner.state).await
    }
}

impl<T, U> fmt::Debug for Framed<T, U>
where
    T: fmt::Debug,
    U: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Framed")
            .field("io", self.get_ref())
            .field("codec", self.codec())
            .finish()
    }
}

impl<IO, Codec> FramedRead<IO, Codec> {
    pub fn new(io: IO, decoder: Codec) -> Self {
        Self {
            inner: FramedInner::new(io, decoder, ReadState::default()),
        }
    }

    pub fn with_capacity(io: IO, codec: Codec, capacity: usize) -> Self {
        Self {
            inner: FramedInner::new(io, codec, ReadState::with_capacity(capacity)),
        }
    }

    /// Returns a reference to the underlying I/O stream wrapped by
    /// `FramedRead`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_ref(&self) -> &IO {
        &self.inner.io
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `FramedRead`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_mut(&mut self) -> &mut IO {
        &mut self.inner.io
    }

    /// Consumes the `FramedRead`, returning its underlying I/O stream.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn into_inner(self) -> IO {
        self.inner.io
    }

    /// Returns a reference to the underlying decoder.
    pub fn decoder(&self) -> &Codec {
        &self.inner.codec
    }

    /// Returns a mutable reference to the underlying decoder.
    pub fn decoder_mut(&mut self) -> &mut Codec {
        &mut self.inner.codec
    }

    /// Maps the decoder `D` to `C`, preserving the read buffer
    /// wrapped by `Framed`.
    pub fn map_decoder<CodecNew, F>(self, map: F) -> FramedRead<IO, CodecNew>
    where
        F: FnOnce(Codec) -> CodecNew,
    {
        let FramedInner { io, codec, state } = self.inner;
        FramedRead {
            inner: FramedInner {
                io,
                codec: map(codec),
                state,
            },
        }
    }

    /// Returns a reference to the read buffer.
    pub fn read_buffer(&self) -> &BytesMut {
        &self.inner.state.buffer
    }

    /// Returns a mutable reference to the read buffer.
    pub fn read_buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.inner.state.buffer
    }

    /// Equivalent to Stream::next but with custom codec.
    pub async fn next_with<C: Decoder>(
        &mut self,
        codec: &mut C,
    ) -> Option<Result<C::Item, C::Error>>
    where
        IO: AsyncReadRent,
    {
        FramedInner::next_with(&mut self.inner.io, codec, &mut self.inner.state).await
    }
}

impl<T, D> fmt::Debug for FramedRead<T, D>
where
    T: fmt::Debug,
    D: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FramedRead")
            .field("inner", &self.get_ref())
            .field("decoder", &self.decoder())
            .field("state", &self.inner.state.state)
            .field("buffer", &self.read_buffer())
            .finish()
    }
}

impl<IO, Codec> FramedWrite<IO, Codec> {
    pub fn new(io: IO, encoder: Codec) -> Self {
        Self {
            inner: FramedInner::new(io, encoder, WriteState::default()),
        }
    }

    /// Returns a reference to the underlying I/O stream wrapped by
    /// `FramedWrite`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_ref(&self) -> &IO {
        &self.inner.io
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `FramedWrite`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn get_mut(&mut self) -> &mut IO {
        &mut self.inner.io
    }

    /// Consumes the `FramedWrite`, returning its underlying I/O stream.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    pub fn into_inner(self) -> IO {
        self.inner.io
    }

    /// Returns a reference to the underlying encoder.
    pub fn encoder(&self) -> &Codec {
        &self.inner.codec
    }

    /// Returns a mutable reference to the underlying encoder.
    pub fn encoder_mut(&mut self) -> &mut Codec {
        &mut self.inner.codec
    }

    /// Maps the encoder `E` to `C`, preserving the write buffer
    /// wrapped by `Framed`.
    pub fn map_encoder<CodecNew, F>(self, map: F) -> FramedWrite<IO, CodecNew>
    where
        F: FnOnce(Codec) -> CodecNew,
    {
        let FramedInner { io, codec, state } = self.inner;
        FramedWrite {
            inner: FramedInner {
                io,
                codec: map(codec),
                state,
            },
        }
    }

    /// Returns a reference to the write buffer.
    pub fn write_buffer(&self) -> &BytesMut {
        &self.inner.state.buffer
    }

    /// Returns a mutable reference to the write buffer.
    pub fn write_buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.inner.state.buffer
    }
}

impl<T, U> fmt::Debug for FramedWrite<T, U>
where
    T: fmt::Debug,
    U: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FramedWrite")
            .field("inner", &self.get_ref())
            .field("encoder", &self.encoder())
            .field("buffer", &self.inner.state.buffer)
            .finish()
    }
}

impl<IO, Codec> Stream for Framed<IO, Codec>
where
    IO: AsyncReadRent,
    Codec: Decoder,
{
    type Item = <FramedInner<IO, Codec, RWState> as Stream>::Item;

    type NextFuture<'a> = <FramedInner<IO, Codec, RWState> as Stream>::NextFuture<'a>
        where Self: 'a;

    #[inline]
    fn next(&mut self) -> Self::NextFuture<'_> {
        (&mut self.inner).next()
    }
}

impl<IO, Codec> Stream for FramedRead<IO, Codec>
where
    IO: AsyncReadRent,
    Codec: Decoder,
{
    type Item = <FramedInner<IO, Codec, ReadState> as Stream>::Item;

    type NextFuture<'a> = <FramedInner<IO, Codec, ReadState> as Stream>::NextFuture<'a>
        where Self: 'a;

    #[inline]
    fn next(&mut self) -> Self::NextFuture<'_> {
        self.inner.next()
    }
}

impl<IO, Codec, Item> Sink<Item> for Framed<IO, Codec>
where
    IO: AsyncWriteRent,
    Codec: Encoder<Item>,
{
    type Error = <FramedInner<IO, Codec, RWState> as Sink<Item>>::Error;

    type SendFuture<'a> = <FramedInner<IO, Codec, RWState> as Sink<Item>>::SendFuture<'a>
    where
        Self: 'a;

    type FlushFuture<'a> = <FramedInner<IO, Codec, RWState> as Sink<Item>>::FlushFuture<'a>
    where
        Self: 'a;

    type CloseFuture<'a>=<FramedInner<IO, Codec, RWState> as Sink<Item>>::CloseFuture<'a>
    where
        Self: 'a;

    #[inline]
    fn send(&mut self, item: Item) -> Self::SendFuture<'_> {
        self.inner.send(item)
    }

    #[inline]
    fn flush(&mut self) -> Self::FlushFuture<'_> {
        self.inner.flush()
    }

    #[inline]
    fn close(&mut self) -> Self::CloseFuture<'_> {
        self.inner.close()
    }
}

impl<IO, Codec, Item> Sink<Item> for FramedWrite<IO, Codec>
where
    IO: AsyncWriteRent,
    Codec: Encoder<Item>,
{
    type Error = <FramedInner<IO, Codec, WriteState> as Sink<Item>>::Error;

    type SendFuture<'a> = <FramedInner<IO, Codec, WriteState> as Sink<Item>>::SendFuture<'a>
    where
        Self: 'a;

    type FlushFuture<'a> = <FramedInner<IO, Codec, WriteState> as Sink<Item>>::FlushFuture<'a>
    where
        Self: 'a;

    type CloseFuture<'a>=<FramedInner<IO, Codec, WriteState> as Sink<Item>>::CloseFuture<'a>
    where
        Self: 'a;

    #[inline]
    fn send(&mut self, item: Item) -> Self::SendFuture<'_> {
        self.inner.send(item)
    }

    #[inline]
    fn flush(&mut self) -> Self::FlushFuture<'_> {
        self.inner.flush()
    }

    #[inline]
    fn close(&mut self) -> Self::CloseFuture<'_> {
        self.inner.close()
    }
}
