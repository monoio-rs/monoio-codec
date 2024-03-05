// Part of the helper functions and tests are borrowed from tokio-util.

use std::{
    borrow::{Borrow, BorrowMut},
    fmt,
    future::Future,
};

use bytes::{Buf, BufMut, BytesMut};
use monoio::{
    buf::{IoBuf, IoBufMut, IoVecBuf, IoVecBufMut, IoVecWrapperMut, SliceMut},
    io::{sink::Sink, stream::Stream, AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt},
    BufResult,
};

use crate::{Decoded, Decoder, Encoder};

const INITIAL_CAPACITY: usize = 8 * 1024;
const BACKPRESSURE_BOUNDARY: usize = INITIAL_CAPACITY;
const RESERVE: usize = 4096;

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
    #[inline]
    fn with_capacity(capacity: usize) -> Self {
        Self {
            state: State::Framing(None),
            buffer: BytesMut::with_capacity(capacity),
        }
    }
}

impl Default for ReadState {
    #[inline]
    fn default() -> Self {
        Self::with_capacity(INITIAL_CAPACITY)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Framing(Option<usize>),
    Pausing,
    Paused,
    Errored,
}

#[derive(Debug)]
pub struct WriteState {
    buffer: BytesMut,
}

impl Default for WriteState {
    #[inline]
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
    #[inline]
    fn borrow(&self) -> &ReadState {
        &self.read
    }
}
impl BorrowMut<ReadState> for RWState {
    #[inline]
    fn borrow_mut(&mut self) -> &mut ReadState {
        &mut self.read
    }
}
impl Borrow<WriteState> for RWState {
    #[inline]
    fn borrow(&self) -> &WriteState {
        &self.write
    }
}
impl BorrowMut<WriteState> for RWState {
    #[inline]
    fn borrow_mut(&mut self) -> &mut WriteState {
        &mut self.write
    }
}

impl<IO, Codec, S> FramedInner<IO, Codec, S> {
    #[inline]
    const fn new(io: IO, codec: Codec, state: S) -> Self {
        Self { io, codec, state }
    }

    async fn peek_data<'a, 'b>(io: &'b mut IO, state: &'a mut S) -> std::io::Result<&'a mut [u8]>
    where
        IO: AsyncReadRent,
        S: BorrowMut<ReadState>,
    {
        let read_state: &mut ReadState = state.borrow_mut();
        let state = &mut read_state.state;
        let buffer = &mut read_state.buffer;

        if !buffer.is_empty() {
            return Ok(buffer.as_mut());
        }
        buffer.reserve(RESERVE);

        macro_rules! ok {
            ($result: expr, $state: expr) => {
                match $result {
                    Ok(x) => x,
                    Err(e) => {
                        *$state = State::Errored;
                        return Err(e);
                    }
                }
            };
        }

        // Read data
        let end = buffer.capacity();
        let owned_buf = std::mem::take(buffer);
        let owned_slice = unsafe { SliceMut::new_unchecked(owned_buf, 0, end) };
        let (result, owned_slice) = io.read(owned_slice).await;
        *buffer = owned_slice.into_inner();
        let n = ok!(result, state);
        if n == 0 {
            *state = State::Paused;
        }
        Ok(buffer.as_mut())
    }

    // In tokio there are 5 states. But since we use pure async here,
    // we do not need to return Pending so we don't need to save the state
    // when Pending returned. We only need to save state when return
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
                State::Framing(hint) => loop {
                    if !matches!(hint, Some(size) if buffer.len() < *size) && !buffer.is_empty() {
                        // If we get a Some hint and the buffer length is less than it, we do not
                        // decode. If the buffer is empty, we we do not decode.
                        *hint = match ok!(codec.decode(buffer), state) {
                            Decoded::Some(item) => {
                                // When we decoded something, we should clear the hint.
                                *hint = None;
                                return Some(Ok(item));
                            }
                            Decoded::Insufficient => None,
                            Decoded::InsufficientAtLeast(size) => Some(size),
                        };
                    }

                    let reserve = match *hint {
                        Some(size) if size > buffer.len() => RESERVE.max(size - buffer.len()),
                        _ => RESERVE,
                    };
                    buffer.reserve(reserve);
                    let (begin, end) = {
                        let buffer_ptr = buffer.write_ptr();
                        let slice_to_write = buffer.chunk_mut();
                        let begin =
                            unsafe { slice_to_write.as_mut_ptr().offset_from(buffer_ptr) } as usize;
                        let end = begin + slice_to_write.len();
                        (begin, end)
                    };
                    let owned_buf = std::mem::take(buffer);
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
                        Decoded::Some(item) => Some(Ok(item)),
                        _ => {
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
                    let owned_buf = std::mem::take(buffer);
                    let owned_slice = unsafe { SliceMut::new_unchecked(owned_buf, begin, end) };
                    let (result, owned_slice) = io.read(owned_slice).await;
                    *buffer = owned_slice.into_inner();
                    let n = ok!(result, state);
                    if n == 0 {
                        // still paused
                        return None;
                    }
                    // read something, then we move to framing state
                    *state = State::Framing(None);
                }
                // On Errored, we need to return None and trans to Paused.
                State::Errored => {
                    *state = State::Paused;
                    return None;
                }
            }
        }
    }

    async fn flush(io: &mut IO, state: &mut S) -> std::io::Result<()>
    where
        IO: AsyncWriteRent,
        S: BorrowMut<WriteState>,
    {
        let WriteState { buffer } = state.borrow_mut();
        if buffer.is_empty() {
            return Ok(());
        }
        // This action does not allocate.
        let buf = std::mem::take(buffer);
        let (result, buf) = io.write_all(buf).await;
        *buffer = buf;
        result?;
        buffer.clear();
        io.flush().await?;
        Ok(())
    }

    #[inline]
    async fn send_with<Item>(
        io: &mut IO,
        codec: &mut Codec,
        state: &mut S,
        item: Item,
    ) -> Result<(), Codec::Error>
    where
        IO: AsyncWriteRent,
        Codec: Encoder<Item>,
        S: BorrowMut<WriteState>,
    {
        if state.borrow_mut().buffer.len() >= BACKPRESSURE_BOUNDARY {
            Self::flush(io, state).await?;
        }
        codec.encode(item, &mut state.borrow_mut().buffer)?;
        Ok(())
    }
}

impl<IO, Codec, S> AsyncReadRent for FramedInner<IO, Codec, S>
where
    IO: AsyncReadRent,
    S: BorrowMut<ReadState>,
{
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> BufResult<usize, T> {
        let read_state: &mut ReadState = self.state.borrow_mut();
        let state = &mut read_state.state;
        let buffer = &mut read_state.buffer;

        if buf.bytes_total() == 0 {
            return (Ok(0), buf);
        }

        // Copy existing data if there is some.
        let to_copy = buf.bytes_total().min(buffer.len());
        if to_copy != 0 {
            unsafe {
                buf.write_ptr()
                    .copy_from_nonoverlapping(buffer.as_ptr(), to_copy);
                buf.set_init(to_copy);
            }
            buffer.advance(to_copy);
            return (Ok(to_copy), buf);
        }

        // Read to buf directly if buf size is bigger than some threshold.
        if buf.bytes_total() > INITIAL_CAPACITY {
            let (res, buf) = self.io.read(buf).await;
            return match res {
                Ok(0) => {
                    *state = State::Pausing;
                    (Ok(0), buf)
                }
                Ok(n) => (Ok(n), buf),
                Err(e) => {
                    *state = State::Errored;
                    (Err(e), buf)
                }
            };
        }
        // Read to inner buffer and copy to buf.
        buffer.reserve(INITIAL_CAPACITY);
        let owned_buffer = std::mem::take(buffer);
        let (res, owned_buffer) = self.io.read(owned_buffer).await;
        *buffer = owned_buffer;
        match res {
            Ok(0) => {
                *state = State::Pausing;
                return (Ok(0), buf);
            }
            Err(e) => {
                *state = State::Errored;
                return (Err(e), buf);
            }
            _ => (),
        }
        let to_copy = buf.bytes_total().min(buffer.len());
        unsafe {
            buf.write_ptr()
                .copy_from_nonoverlapping(buffer.as_ptr(), to_copy);
            buf.set_init(to_copy);
        }
        buffer.advance(to_copy);
        (Ok(to_copy), buf)
    }

    async fn readv<T: IoVecBufMut>(&mut self, mut buf: T) -> BufResult<usize, T> {
        let slice = match IoVecWrapperMut::new(buf) {
            Ok(slice) => slice,
            Err(buf) => return (Ok(0), buf),
        };

        let (result, slice) = self.read(slice).await;
        buf = slice.into_inner();
        if let Ok(n) = result {
            unsafe { buf.set_init(n) };
        }
        (result, buf)
    }
}

impl<IO, Codec, S> Stream for FramedInner<IO, Codec, S>
where
    IO: AsyncReadRent,
    Codec: Decoder,
    S: BorrowMut<ReadState>,
{
    type Item = Result<Codec::Item, Codec::Error>;

    #[inline]
    async fn next(&mut self) -> Option<Self::Item> {
        Self::next_with(&mut self.io, &mut self.codec, &mut self.state).await
    }
}

impl<IO, Codec, S> AsyncWriteRent for FramedInner<IO, Codec, S>
where
    IO: AsyncWriteRent,
    S: BorrowMut<WriteState>,
{
    async fn write<T: monoio::buf::IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let WriteState { buffer } = self.state.borrow_mut();
        if buffer.len() >= BACKPRESSURE_BOUNDARY || buf.bytes_init() >= INITIAL_CAPACITY {
            // flush buffer
            if let Err(e) = AsyncWriteRent::flush(self).await {
                return (Err(e), buf);
            }
            // write directly
            return self.io.write_all(buf).await;
        }
        // copy to buffer
        let cap = buffer.capacity() - buffer.len();
        let size = buf.bytes_init().min(cap);
        let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), size) };
        buffer.extend_from_slice(slice);
        (Ok(size), buf)
    }

    #[inline]
    async fn writev<T: monoio::buf::IoVecBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let slice = match monoio::buf::IoVecWrapper::new(buf) {
            Ok(slice) => slice,
            Err(buf) => return (Ok(0), buf),
        };

        let (result, slice) = self.write(slice).await;
        (result, slice.into_inner())
    }

    #[inline]
    async fn flush(&mut self) -> std::io::Result<()> {
        FramedInner::<_, Codec, _>::flush(&mut self.io, &mut self.state).await
    }

    #[inline]
    async fn shutdown(&mut self) -> std::io::Result<()> {
        AsyncWriteRent::flush(self).await?;
        self.io.shutdown().await?;
        Ok(())
    }
}

impl<IO, Codec, S, Item> Sink<Item> for FramedInner<IO, Codec, S>
where
    IO: AsyncWriteRent,
    Codec: Encoder<Item>,
    S: BorrowMut<WriteState>,
{
    type Error = Codec::Error;

    #[inline]
    async fn send(&mut self, item: Item) -> Result<(), Self::Error> {
        if self.state.borrow_mut().buffer.len() >= BACKPRESSURE_BOUNDARY {
            FramedInner::<_, Codec, _>::flush(&mut self.io, &mut self.state).await?;
        }
        self.codec
            .encode(item, &mut self.state.borrow_mut().buffer)?;
        Ok(())
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), Self::Error> {
        AsyncWriteRent::flush(self).await?;
        Ok(())
    }

    #[inline]
    async fn close(&mut self) -> Result<(), Self::Error> {
        AsyncWriteRent::shutdown(self).await?;
        Ok(())
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
    #[inline]
    pub fn new(io: IO, codec: Codec) -> Self {
        Self {
            inner: FramedInner::new(io, codec, RWState::default()),
        }
    }

    #[inline]
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
    #[inline]
    pub fn get_ref(&self) -> &IO {
        &self.inner.io
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    #[inline]
    pub fn get_mut(&mut self) -> &mut IO {
        &mut self.inner.io
    }

    /// Returns a reference to the underlying codec wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    #[inline]
    pub fn codec(&self) -> &Codec {
        &self.inner.codec
    }

    /// Returns a mutable reference to the underlying codec wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    #[inline]
    pub fn codec_mut(&mut self) -> &mut Codec {
        &mut self.inner.codec
    }

    /// Maps the codec `U` to `C`, preserving the read and write buffers
    /// wrapped by `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    #[inline]
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
    #[inline]
    pub fn read_buffer(&self) -> &BytesMut {
        &self.inner.state.read.buffer
    }

    /// Returns a mutable reference to the read buffer.
    #[inline]
    pub fn read_buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.inner.state.read.buffer
    }

    /// Returns io and a mutable reference to the read buffer.
    #[inline]
    pub fn read_state_mut(&mut self) -> (&mut IO, &mut BytesMut) {
        (&mut self.inner.io, &mut self.inner.state.read.buffer)
    }

    /// Returns a reference to the write buffer.
    #[inline]
    pub fn write_buffer(&self) -> &BytesMut {
        &self.inner.state.write.buffer
    }

    /// Returns a mutable reference to the write buffer.
    #[inline]
    pub fn write_buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.inner.state.write.buffer
    }

    /// Consumes the `Framed`, returning its underlying I/O stream.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise
    /// being worked with.
    #[inline]
    pub fn into_inner(self) -> IO {
        self.inner.io
    }

    /// Equivalent to Stream::next but with custom codec.
    #[inline]
    pub async fn next_with<C: Decoder>(
        &mut self,
        codec: &mut C,
    ) -> Option<Result<C::Item, C::Error>>
    where
        IO: AsyncReadRent,
    {
        FramedInner::next_with(&mut self.inner.io, codec, &mut self.inner.state).await
    }

    /// Await some new data.
    /// Useful to do read timeout.
    pub fn peek_data(&mut self) -> impl Future<Output = std::io::Result<&mut [u8]>>
    where
        IO: AsyncReadRent,
    {
        FramedInner::<_, Codec, _>::peek_data(&mut self.inner.io, &mut self.inner.state)
    }

    /// Equivalent to Sink::send but with custom codec.
    #[inline]
    pub async fn send_with<C: Encoder<Item>, Item>(
        &mut self,
        codec: &mut C,
        item: Item,
    ) -> Result<(), C::Error>
    where
        IO: AsyncWriteRent,
        C: Encoder<Item>,
    {
        FramedInner::send_with(&mut self.inner.io, codec, &mut self.inner.state, item).await
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

    /// Returns io and a mutable reference to the read buffer.
    pub fn read_state_mut(&mut self) -> (&mut IO, &mut BytesMut) {
        (&mut self.inner.io, &mut self.inner.state.buffer)
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

    /// Await some new data.
    /// Useful to do read timeout.
    pub fn peek_data(&mut self) -> impl Future<Output = std::io::Result<&mut [u8]>>
    where
        IO: AsyncReadRent,
    {
        FramedInner::<_, Codec, _>::peek_data(&mut self.inner.io, &mut self.inner.state)
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

    #[inline]
    async fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().await
    }
}

impl<IO, Codec> Stream for FramedRead<IO, Codec>
where
    IO: AsyncReadRent,
    Codec: Decoder,
{
    type Item = <FramedInner<IO, Codec, ReadState> as Stream>::Item;

    #[inline]
    async fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().await
    }
}

impl<IO, Codec, Item> Sink<Item> for Framed<IO, Codec>
where
    IO: AsyncWriteRent,
    Codec: Encoder<Item>,
{
    type Error = <FramedInner<IO, Codec, RWState> as Sink<Item>>::Error;

    #[inline]
    async fn send(&mut self, item: Item) -> Result<(), Self::Error> {
        self.inner.send(item).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Sink::flush(&mut self.inner).await
    }

    #[inline]
    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}

impl<IO, Codec, Item> Sink<Item> for FramedWrite<IO, Codec>
where
    IO: AsyncWriteRent,
    Codec: Encoder<Item>,
{
    type Error = <FramedInner<IO, Codec, WriteState> as Sink<Item>>::Error;

    #[inline]
    async fn send(&mut self, item: Item) -> Result<(), Self::Error> {
        self.inner.send(item).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Sink::flush(&mut self.inner).await
    }

    #[inline]
    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}

pub trait StreamWithCodec<T> {
    type Item;

    fn next_with<'a>(&'a mut self, codec: &'a mut T) -> impl Future<Output = Option<Self::Item>>;
}

pub trait SinkWithCodec<T, Item>
where
    T: Encoder<Item>,
{
    fn send_with<'a>(
        &'a mut self,
        codec: &'a mut T,
        item: Item,
    ) -> impl Future<Output = Result<(), T::Error>>;

    fn flush(&mut self) -> impl Future<Output = Result<(), T::Error>>;
}

impl<Codec: Decoder, IO: AsyncReadRent, AnyCodec> StreamWithCodec<Codec>
    for FramedRead<IO, AnyCodec>
{
    type Item = Result<Codec::Item, Codec::Error>;

    #[inline]
    async fn next_with<'a>(&'a mut self, codec: &'a mut Codec) -> Option<Self::Item> {
        FramedInner::next_with(&mut self.inner.io, codec, &mut self.inner.state).await
    }
}

impl<Codec: Decoder, IO: AsyncReadRent, AnyCodec> StreamWithCodec<Codec> for Framed<IO, AnyCodec> {
    type Item = Result<Codec::Item, Codec::Error>;

    #[inline]
    async fn next_with<'a>(&'a mut self, codec: &'a mut Codec) -> Option<Self::Item> {
        FramedInner::next_with(&mut self.inner.io, codec, &mut self.inner.state).await
    }
}

impl<Codec: Encoder<Item>, IO: AsyncWriteRent, AnyCodec, Item> SinkWithCodec<Codec, Item>
    for FramedWrite<IO, AnyCodec>
{
    #[inline]
    async fn send_with<'a>(
        &'a mut self,
        codec: &'a mut Codec,
        item: Item,
    ) -> Result<(), Codec::Error> {
        FramedInner::send_with(&mut self.inner.io, codec, &mut self.inner.state, item).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), Codec::Error> {
        FramedInner::<_, (), _>::flush(&mut self.inner.io, &mut self.inner.state)
            .await
            .map_err(|e| e.into())
    }
}

impl<Codec: Encoder<Item>, IO: AsyncWriteRent, AnyCodec, Item> SinkWithCodec<Codec, Item>
    for Framed<IO, AnyCodec>
{
    #[inline]
    async fn send_with<'a>(
        &'a mut self,
        codec: &'a mut Codec,
        item: Item,
    ) -> Result<(), Codec::Error> {
        FramedInner::send_with(&mut self.inner.io, codec, &mut self.inner.state, item).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), Codec::Error> {
        FramedInner::<_, (), _>::flush(&mut self.inner.io, &mut self.inner.state)
            .await
            .map_err(|e| e.into())
    }
}

impl<IO: AsyncReadRent, Codec> AsyncReadRent for Framed<IO, Codec> {
    #[inline]
    async fn read<T: IoBufMut>(&mut self, buf: T) -> BufResult<usize, T> {
        self.inner.read(buf).await
    }

    #[inline]
    async fn readv<T: IoVecBufMut>(&mut self, buf: T) -> BufResult<usize, T> {
        self.inner.readv(buf).await
    }
}

impl<IO: AsyncReadRent, Codec> AsyncReadRent for FramedRead<IO, Codec> {
    #[inline]
    async fn read<T: IoBufMut>(&mut self, buf: T) -> BufResult<usize, T> {
        self.inner.read(buf).await
    }

    #[inline]
    async fn readv<T: IoVecBufMut>(&mut self, buf: T) -> BufResult<usize, T> {
        self.inner.readv(buf).await
    }
}

impl<IO: AsyncWriteRent, Codec> AsyncWriteRent for Framed<IO, Codec> {
    #[inline]
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        self.inner.write(buf).await
    }

    #[inline]
    async fn writev<T: IoVecBuf>(&mut self, buf_vec: T) -> BufResult<usize, T> {
        self.inner.writev(buf_vec).await
    }

    #[inline]
    async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }

    #[inline]
    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }
}

impl<IO: AsyncWriteRent, Codec> AsyncWriteRent for FramedWrite<IO, Codec> {
    #[inline]
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        self.inner.write(buf).await
    }

    #[inline]
    async fn writev<T: IoVecBuf>(&mut self, buf_vec: T) -> BufResult<usize, T> {
        self.inner.writev(buf_vec).await
    }

    #[inline]
    async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }

    #[inline]
    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }
}
