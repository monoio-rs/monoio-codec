// Part of the helper functions and tests are borrowed from tokio-util.

#![allow(stable_features)]
#![feature(type_alias_impl_trait)]

use std::{collections::VecDeque, io};

use bytes::{Buf, BytesMut};
use monoio::{
    buf::RawBuf,
    io::{stream::Stream, AsyncReadRent},
};
use monoio_codec::{Decoder, FramedRead};

macro_rules! mock {
    ($($x:expr,)*) => {{
        let mut v = VecDeque::new();
        v.extend(vec![$($x),*]);
        Mock { calls: v }
    }};
}

macro_rules! assert_read_next {
    ($e:expr, $n:expr) => {{
        assert_eq!($e.next().await.unwrap().unwrap(), $n);
    }};
}

macro_rules! assert_read_next_none {
    ($e:expr) => {{
        assert!($e.next().await.is_none());
    }};
}

macro_rules! assert_read_next_err {
    ($e:expr) => {{
        assert!($e.next().await.unwrap().is_err());
    }};
}

struct U32Decoder;

impl Decoder for U32Decoder {
    type Item = u32;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> io::Result<Option<u32>> {
        if buf.len() < 4 {
            return Ok(None);
        }

        let n = buf.split_to(4).get_u32();
        Ok(Some(n))
    }
}

struct U64Decoder;

impl Decoder for U64Decoder {
    type Item = u64;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> io::Result<Option<u64>> {
        if buf.len() < 8 {
            return Ok(None);
        }

        let n = buf.split_to(8).get_u64();
        Ok(Some(n))
    }
}

#[monoio::test_all]
async fn read_multi_frame_in_packet() {
    let mock = mock! {
        Ok(b"\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x02".to_vec()),
    };

    let mut framed = FramedRead::new(mock, U32Decoder);
    assert_read_next!(framed, 0);
    assert_read_next!(framed, 1);
    assert_read_next!(framed, 2);
    assert!(framed.next().await.is_none());
}

#[monoio::test_all]
async fn read_multi_frame_across_packets() {
    let mock = mock! {
        Ok(b"\x00\x00\x00\x00".to_vec()),
        Ok(b"\x00\x00\x00\x01".to_vec()),
        Ok(b"\x00\x00\x00\x02".to_vec()),
    };

    let mut framed = FramedRead::new(mock, U32Decoder);
    assert_read_next!(framed, 0);
    assert_read_next!(framed, 1);
    assert_read_next!(framed, 2);
    assert_read_next_none!(framed);
}

#[monoio::test_all]
async fn read_multi_frame_in_packet_after_codec_changed() {
    let mock = mock! {
        Ok(b"\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00\x08".to_vec()),
    };
    let mut framed = FramedRead::new(mock, U32Decoder);
    assert_read_next!(framed, 0x04);

    let mut framed = framed.map_decoder(|_| U64Decoder);
    assert_read_next!(framed, 0x08);
    assert_read_next_none!(framed);
}

#[monoio::test_all]
async fn read_err() {
    let mock = mock! {
        Err(io::Error::new(io::ErrorKind::Other, "")),
    };

    let mut framed = FramedRead::new(mock, U32Decoder);
    assert_eq!(
        framed.next().await.unwrap().unwrap_err().kind(),
        io::ErrorKind::Other
    );
}

#[monoio::test_all]
async fn read_partial_then_err() {
    let mock = mock! {
        Ok(b"\x00\x00".to_vec()),
        Err(io::Error::new(io::ErrorKind::Other, "")),
    };

    let mut framed = FramedRead::new(mock, U32Decoder);
    assert_eq!(
        framed.next().await.unwrap().unwrap_err().kind(),
        io::ErrorKind::Other
    );
}

#[monoio::test_all]
async fn huge_size() {
    let data = &[0; 32 * 1024][..];
    let mut framed = FramedRead::new(data, BigDecoder);

    assert_read_next!(framed, 0);
    assert_read_next_none!(framed);

    struct BigDecoder;
    impl Decoder for BigDecoder {
        type Item = u32;
        type Error = io::Error;

        fn decode(&mut self, buf: &mut BytesMut) -> io::Result<Option<u32>> {
            if buf.len() < 32 * 1024 {
                return Ok(None);
            }
            buf.advance(32 * 1024);
            Ok(Some(0))
        }
    }
}

#[monoio::test_all]
async fn data_remaining_is_error() {
    let slice = &[0; 5][..];
    let mut framed = FramedRead::new(slice, U32Decoder);

    assert_read_next!(framed, 0);
    assert_read_next_err!(framed);
}

#[monoio::test_all]
async fn multi_frames_on_eof() {
    struct MyDecoder(Vec<u32>);

    impl Decoder for MyDecoder {
        type Item = u32;
        type Error = io::Error;

        fn decode(&mut self, _buf: &mut BytesMut) -> io::Result<Option<u32>> {
            unreachable!();
        }

        fn decode_eof(&mut self, _buf: &mut BytesMut) -> io::Result<Option<u32>> {
            if self.0.is_empty() {
                return Ok(None);
            }

            Ok(Some(self.0.remove(0)))
        }
    }

    let mut framed = FramedRead::new(mock!(), MyDecoder(vec![0, 1, 2, 3]));
    assert_read_next!(framed, 0);
    assert_read_next!(framed, 1);
    assert_read_next!(framed, 2);
    assert_read_next!(framed, 3);
    assert_read_next_none!(framed);
}

#[monoio::test_all]
async fn read_eof_then_resume() {
    let mock = mock! {
        Ok(b"\x00\x00\x00\x01".to_vec()),
        Ok(b"".to_vec()),
        Ok(b"\x00\x00\x00\x02".to_vec()),
        Ok(b"".to_vec()),
        Ok(b"\x00\x00\x00\x03".to_vec()),
    };
    let mut framed = FramedRead::new(mock, U32Decoder);
    assert_read_next!(framed, 1);
    assert_read_next_none!(framed);
    assert_read_next!(framed, 2);
    assert_read_next_none!(framed);
    assert_read_next!(framed, 3);
    assert_read_next_none!(framed);
    assert_read_next_none!(framed);
}

struct Mock {
    calls: VecDeque<io::Result<Vec<u8>>>,
}

use monoio::buf::{IoBufMut, IoVecBufMut};

impl AsyncReadRent for Mock {
    type ReadFuture<'a, B> = impl std::future::Future<Output = monoio::BufResult<usize, B>> + 'a where
        B: IoBufMut + 'a;
    type ReadvFuture<'a, B> = impl std::future::Future<Output = monoio::BufResult<usize, B>> + 'a where
        B: IoVecBufMut + 'a;

    fn read<T: IoBufMut>(&mut self, mut buf: T) -> Self::ReadFuture<'_, T> {
        async {
            match self.calls.pop_front() {
                Some(Ok(data)) => {
                    let n = data.len();
                    debug_assert!(buf.bytes_total() >= n);
                    unsafe {
                        buf.write_ptr().copy_from_nonoverlapping(data.as_ptr(), n);
                        buf.set_init(n)
                    }
                    (Ok(n), buf)
                }
                Some(Err(e)) => (Err(e), buf),
                None => (Ok(0), buf),
            }
        }
    }

    fn readv<T: IoVecBufMut>(&mut self, mut buf: T) -> Self::ReadvFuture<'_, T> {
        async move {
            let n = match unsafe { RawBuf::new_from_iovec_mut(&mut buf) } {
                Some(raw_buf) => self.read(raw_buf).await.0,
                None => Ok(0),
            };
            if let Ok(n) = n {
                unsafe { buf.set_init(n) };
            }
            (n, buf)
        }
    }
}
