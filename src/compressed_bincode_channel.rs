use std::{
    convert::TryInto,
    marker::PhantomData,
    task::{Context, Poll},
    u16,
};

use bincode::Options as _;
use byteorder::{ByteOrder, LittleEndian};
use futures::{future, ready, task};
use serde::{de::DeserializeOwned, Serialize};
use snap::raw::{decompress_len, max_compress_len, Decoder as SnapDecoder, Encoder as SnapEncoder};
use thiserror::Error;

use crate::reliable_channel::{self, ReliableChannel};

/// The maximum serialized length of a `CompressedBincodeChannel` message. This also serves as
/// the maximum size of a compressed chunk of messages, but it is guaranteed that any message <=
/// `MAX_MESSAGE_LEN` can be sent, even if it cannot be compressed.
pub const MAX_MESSAGE_LEN: u16 = u16::MAX;

#[derive(Debug, Error)]
pub enum SendError {
    /// Fatal internal channel error.
    #[error("reliable channel error error: {0}")]
    ReliableChannelError(#[from] reliable_channel::Error),
    /// Non-fatal error, no message is sent.
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
}

#[derive(Debug, Error)]
pub enum RecvError {
    /// Fatal internal channel error.
    #[error("reliable channel error error: {0}")]
    ReliableChannelError(#[from] reliable_channel::Error),
    /// Fatal error, indicates corruption or protocol mismatch.
    #[error("Snappy serialization error: {0}")]
    SnapError(#[from] snap::Error),
    /// Fatal error, stream becomes desynchronized, individual serialized types are not length
    /// prefixed.
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
}

/// Wraps a `ReliableMessageChannel` and reliably sends a single message type serialized with
/// `bincode` and compressed with `snap`.
///
/// Messages are written in large blocks to aid compression. Messages are serialized end to end, and
/// when a block reaches the maximum configured size (or `flush` is called), the block is compressed
/// and sent as a single message.
///
/// This saves space from the compression and also from the reduced message header overhead per
/// individual message.
pub struct CompressedBincodeChannel {
    channel: ReliableChannel,

    send_chunk: Vec<u8>,

    write_buffer: Vec<u8>,
    write_pos: usize,

    read_buffer: Vec<u8>,
    read_pos: usize,

    recv_chunk: Vec<u8>,
    recv_pos: usize,

    encoder: SnapEncoder,
    decoder: SnapDecoder,
}

impl From<ReliableChannel> for CompressedBincodeChannel {
    fn from(channel: ReliableChannel) -> Self {
        Self::new(channel)
    }
}

impl CompressedBincodeChannel {
    pub fn new(channel: ReliableChannel) -> Self {
        CompressedBincodeChannel {
            channel,
            send_chunk: Vec::new(),
            write_buffer: Vec::new(),
            write_pos: 0,
            read_buffer: Vec::new(),
            read_pos: 0,
            recv_chunk: Vec::new(),
            recv_pos: 0,
            encoder: SnapEncoder::new(),
            decoder: SnapDecoder::new(),
        }
    }

    pub fn into_inner(self) -> ReliableChannel {
        self.channel
    }

    /// Send the given message.
    ///
    /// This method is cancel safe, it will never partially send a message, and completes
    /// immediately upon successfully queuing a message to send.
    pub async fn send<M: Serialize>(&mut self, msg: &M) -> Result<(), SendError> {
        future::poll_fn(|cx| self.poll_send(cx, msg)).await
    }

    pub fn try_send<M: Serialize>(&mut self, msg: &M) -> Result<bool, SendError> {
        match self.poll_send(&mut Context::from_waker(task::noop_waker_ref()), msg) {
            Poll::Pending => Ok(false),
            Poll::Ready(Ok(())) => Ok(true),
            Poll::Ready(Err(err)) => Err(err),
        }
    }

    /// Finish sending the current block of messages, compressing them and sending them over the
    /// reliable channel.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), reliable_channel::Error> {
        future::poll_fn(|cx| self.poll_flush(cx)).await
    }

    pub fn try_flush(&mut self) -> Result<bool, reliable_channel::Error> {
        match self.poll_flush(&mut Context::from_waker(task::noop_waker_ref())) {
            Poll::Pending => Ok(false),
            Poll::Ready(Ok(())) => Ok(true),
            Poll::Ready(Err(err)) => Err(err),
        }
    }

    /// Receive a message.
    ///
    /// This method is cancel safe, it will never partially receive a message and will never drop a
    /// received message.
    pub async fn recv<M: DeserializeOwned>(&mut self) -> Result<M, RecvError> {
        future::poll_fn(|cx| self.poll_recv_ready(cx)).await?;
        Ok(self.recv_next()?)
    }

    pub fn try_recv<M: DeserializeOwned>(&mut self) -> Result<Option<M>, RecvError> {
        match self.poll_recv::<M>(&mut Context::from_waker(task::noop_waker_ref())) {
            Poll::Pending => Ok(None),
            Poll::Ready(Ok(val)) => Ok(Some(val)),
            Poll::Ready(Err(err)) => Err(err),
        }
    }

    pub fn poll_send<M: Serialize>(
        &mut self,
        cx: &mut Context,
        msg: &M,
    ) -> Poll<Result<(), SendError>> {
        let bincode_config = self.bincode_config();

        let serialized_len = bincode_config.serialized_size(msg)?;
        if self.send_chunk.len() as u64 + serialized_len > MAX_MESSAGE_LEN as u64 {
            ready!(self.poll_write_send_chunk(cx))?;
        }

        bincode_config.serialize_into(&mut self.send_chunk, msg)?;

        Poll::Ready(Ok(()))
    }

    pub fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), reliable_channel::Error>> {
        ready!(self.poll_write_send_chunk(cx))?;
        ready!(self.poll_finish_write(cx))?;
        self.channel.flush()?;
        Poll::Ready(Ok(()))
    }

    pub fn poll_recv<M: DeserializeOwned>(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<M, RecvError>> {
        ready!(self.poll_recv_ready(cx))?;
        Poll::Ready(Ok(self.recv_next::<M>()?))
    }

    fn poll_recv_ready(&mut self, cx: &mut Context) -> Poll<Result<(), RecvError>> {
        loop {
            if self.recv_pos < self.recv_chunk.len() {
                return Poll::Ready(Ok(()));
            }

            if self.read_pos < 3 {
                self.read_buffer.resize(3, 0);
                ready!(self.poll_finish_read(cx))?;
            }

            let compressed = self.read_buffer[0] != 0;
            let chunk_len = LittleEndian::read_u16(&self.read_buffer[1..3]);
            self.read_buffer.resize(chunk_len as usize + 3, 0);
            ready!(self.poll_finish_read(cx))?;

            if compressed {
                let decompressed_len = decompress_len(&self.read_buffer[3..])?;
                self.recv_chunk
                    .resize(decompressed_len.min(MAX_MESSAGE_LEN as usize), 0);
                self.decoder
                    .decompress(&self.read_buffer[3..], &mut self.recv_chunk)?;
            } else {
                self.recv_chunk.resize(chunk_len as usize, 0);
                self.recv_chunk.copy_from_slice(&self.read_buffer[3..]);
            }

            self.recv_pos = 0;
            self.read_pos = 0;
        }
    }

    fn recv_next<M: DeserializeOwned>(&mut self) -> Result<M, bincode::Error> {
        let bincode_config = self.bincode_config();
        let mut reader = &self.recv_chunk[self.recv_pos..];
        let msg = bincode_config.deserialize_from(&mut reader)?;
        self.recv_pos = self.recv_chunk.len() - reader.len();
        Ok(msg)
    }

    fn poll_write_send_chunk(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), reliable_channel::Error>> {
        if !self.send_chunk.is_empty() {
            ready!(self.poll_finish_write(cx))?;

            self.write_pos = 0;
            self.write_buffer
                .resize(max_compress_len(self.send_chunk.len()) + 3, 0);
            // Should not error, `write_buffer` is correctly sized and is less than `2^32 - 1`
            let compressed_len = self
                .encoder
                .compress(&self.send_chunk, &mut self.write_buffer[3..])
                .expect("unexpected snap encoder error");
            self.write_buffer.truncate(compressed_len + 3);
            if compressed_len >= self.send_chunk.len() {
                // If our compressed size is worse than our uncompressed size, write the original
                // chunk.
                self.write_buffer.truncate(self.send_chunk.len() + 3);
                self.write_buffer[3..].copy_from_slice(&self.send_chunk);
                // An initial 0 means uncompressed
                self.write_buffer[0] = 0;
                LittleEndian::write_u16(
                    &mut self.write_buffer[1..3],
                    (self.send_chunk.len()).try_into().unwrap(),
                );
            } else {
                // An initial 1 means compressed
                self.write_buffer[0] = 1;
                LittleEndian::write_u16(
                    &mut self.write_buffer[1..3],
                    (compressed_len).try_into().unwrap(),
                );
            }

            self.send_chunk.clear();
        }

        Poll::Ready(Ok(()))
    }

    fn poll_finish_write(&mut self, cx: &mut Context) -> Poll<Result<(), reliable_channel::Error>> {
        while self.write_pos < self.write_buffer.len() {
            let len = ready!(self
                .channel
                .poll_write(cx, &self.write_buffer[self.write_pos..]))?;
            self.write_pos += len;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_finish_read(&mut self, cx: &mut Context) -> Poll<Result<(), reliable_channel::Error>> {
        while self.read_pos < self.read_buffer.len() {
            let len = ready!(self
                .channel
                .poll_read(cx, &mut self.read_buffer[self.read_pos..]))?;
            self.read_pos += len;
        }
        Poll::Ready(Ok(()))
    }

    fn bincode_config(&self) -> impl bincode::Options + Copy {
        bincode::options().with_limit(MAX_MESSAGE_LEN as u64)
    }
}

/// Wrapper over an `CompressedBincodeChannel` that only allows a single message type.
pub struct CompressedTypedChannel<M> {
    channel: CompressedBincodeChannel,
    _phantom: PhantomData<M>,
}

impl<M> From<ReliableChannel> for CompressedTypedChannel<M> {
    fn from(channel: ReliableChannel) -> Self {
        Self::new(channel)
    }
}

impl<M> CompressedTypedChannel<M> {
    pub fn new(channel: ReliableChannel) -> Self {
        CompressedTypedChannel {
            channel: CompressedBincodeChannel::new(channel),
            _phantom: PhantomData,
        }
    }

    pub fn into_inner(self) -> ReliableChannel {
        self.channel.into_inner()
    }

    pub async fn flush(&mut self) -> Result<(), reliable_channel::Error> {
        self.channel.flush().await
    }

    pub fn try_flush(&mut self) -> Result<bool, reliable_channel::Error> {
        self.channel.try_flush()
    }

    pub fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), reliable_channel::Error>> {
        self.channel.poll_flush(cx)
    }
}

impl<M: Serialize> CompressedTypedChannel<M> {
    pub async fn send(&mut self, msg: &M) -> Result<(), SendError> {
        self.channel.send(msg).await
    }

    pub fn try_send(&mut self, msg: &M) -> Result<bool, SendError> {
        self.channel.try_send(msg)
    }

    pub fn poll_send(&mut self, cx: &mut Context, msg: &M) -> Poll<Result<(), SendError>> {
        self.channel.poll_send(cx, msg)
    }
}

impl<M: DeserializeOwned> CompressedTypedChannel<M> {
    pub async fn recv(&mut self) -> Result<M, RecvError> {
        self.channel.recv::<M>().await
    }

    pub fn try_recv(&mut self) -> Result<Option<M>, RecvError> {
        self.channel.try_recv::<M>()
    }

    pub fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<M, RecvError>> {
        self.channel.poll_recv::<M>(cx)
    }
}
