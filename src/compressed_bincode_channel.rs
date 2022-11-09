use std::{
    convert::TryInto,
    marker::PhantomData,
    task::{Context, Poll},
    u16,
};

use bincode::Options as _;
use byteorder::{ByteOrder, LittleEndian};
use futures::{future, ready};
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
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), SendError> {
        future::poll_fn(|cx| self.poll_send(cx, msg)).await
    }

    /// Finish sending the current block of messages, compressing them and sending them over the
    /// reliable channel.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), reliable_channel::Error> {
        future::poll_fn(|cx| self.poll_flush(cx)).await
    }

    /// Receive a message.
    ///
    /// This method is cancel safe, it will never partially receive a message and will never drop a
    /// received message.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T, RecvError> {
        future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_send<T: Serialize>(
        &mut self,
        cx: &mut Context,
        msg: &T,
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

    pub fn poll_recv<T: DeserializeOwned>(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<T, RecvError>> {
        let bincode_config = self.bincode_config();

        loop {
            if self.recv_pos < self.recv_chunk.len() {
                let mut reader = &self.recv_chunk[self.recv_pos..];
                let msg = bincode_config.deserialize_from(&mut reader)?;
                self.recv_pos = self.recv_chunk.len() - reader.len();
                return Poll::Ready(Ok(msg));
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
                self.recv_chunk.resize(decompressed_len, 0);
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
pub struct CompressedTypedChannel<T> {
    channel: CompressedBincodeChannel,
    _phantom: PhantomData<T>,
}

impl<T> From<ReliableChannel> for CompressedTypedChannel<T> {
    fn from(channel: ReliableChannel) -> Self {
        Self::new(channel)
    }
}

impl<T> CompressedTypedChannel<T> {
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

    pub fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), reliable_channel::Error>> {
        self.channel.poll_flush(cx)
    }
}

impl<T: Serialize> CompressedTypedChannel<T> {
    pub async fn send(&mut self, msg: &T) -> Result<(), SendError> {
        self.channel.send(msg).await
    }

    pub fn poll_send(&mut self, cx: &mut Context, msg: &T) -> Poll<Result<(), SendError>> {
        self.channel.poll_send(cx, msg)
    }
}

impl<T: DeserializeOwned> CompressedTypedChannel<T> {
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        self.channel.recv().await
    }

    pub fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<T, RecvError>> {
        self.channel.poll_recv(cx)
    }
}
