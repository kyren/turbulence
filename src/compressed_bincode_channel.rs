use std::{convert::TryInto, marker::PhantomData, u16};

use bincode::Options as _;
use byteorder::{ByteOrder, LittleEndian};
use serde::{de::DeserializeOwned, Serialize};
use snap::raw::{decompress_len, max_compress_len, Decoder as SnapDecoder, Encoder as SnapEncoder};
use thiserror::Error;

use crate::reliable_channel::{self, ReliableChannel};

#[derive(Debug, Error)]
pub enum Error {
    #[error("reliable channel error error: {0}")]
    ReliableChannelError(#[from] reliable_channel::Error),
    #[error("received chunk has exceeded the configured max chunk length")]
    ChunkTooLarge,
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
    #[error("Snappy serialization error: {0}")]
    SnapError(#[from] snap::Error),
}

/// Wraps a `ReliableMessageChannel` and reliably sends a single message type serialized with
/// `bincode` and compressed with `snap`.
///
/// Messages are written in large blocks to aid compression.  Messages are serialized end to end,
/// and when a block reaches the maximum configured size (or `flush` is called), the block is
/// compressed and sent as a single message.
///
/// This saves space from the compression and also from the reduced message header overhead per
/// individual message.
pub struct CompressedBincodeChannel {
    channel: ReliableChannel,
    max_chunk_len: u16,

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

impl CompressedBincodeChannel {
    /// The `max_chunk_len` parameter describes the maximum buffer size of a combined message block
    /// before it is automatically sent.
    ///
    /// An individual message may be no more than `max_chunk_len` in length.
    pub fn new(channel: ReliableChannel, max_chunk_len: u16) -> Self {
        CompressedBincodeChannel {
            channel,
            max_chunk_len,
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

    /// Send the given message.
    ///
    /// This method is cancel safe, it will never partially send a message, though canceling it may
    /// or may not buffer a message to be sent.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), Error> {
        let bincode_config = self.bincode_config();

        let serialized_len = bincode_config.serialized_size(msg)?;
        if self.send_chunk.len() as u64 + serialized_len > self.max_chunk_len as u64 {
            self.write_send_chunk().await?;
        }

        bincode_config.serialize_into(&mut self.send_chunk, msg)?;

        Ok(())
    }

    /// Finish sending the current block of messages, compressing them and sending them over the
    /// reliable channel.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), Error> {
        self.write_send_chunk().await?;
        self.finish_write().await?;
        self.channel.flush().await?;
        Ok(())
    }

    /// Receive a message.
    ///
    /// This method is cancel safe, it will never partially receive a message and will never drop a
    /// received message.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T, Error> {
        let bincode_config = self.bincode_config();

        loop {
            if self.recv_pos < self.recv_chunk.len() {
                let mut reader = &self.recv_chunk[self.recv_pos..];
                let msg = bincode_config.deserialize_from(&mut reader)?;
                self.recv_pos = self.recv_chunk.len() - reader.len();
                return Ok(msg);
            }

            if self.read_pos < 3 {
                self.read_buffer.resize(3, 0);
                self.finish_read().await?;
            }

            let compressed = self.read_buffer[0] != 0;
            let chunk_len = LittleEndian::read_u16(&self.read_buffer[1..3]);
            if chunk_len > self.max_chunk_len {
                return Err(Error::ChunkTooLarge);
            }
            self.read_buffer.resize(chunk_len as usize + 3, 0);
            self.finish_read().await?;

            if compressed {
                let decompressed_len = decompress_len(&self.read_buffer[3..])?;
                if decompressed_len > self.max_chunk_len as usize {
                    return Err(Error::ChunkTooLarge);
                }
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

    async fn write_send_chunk(&mut self) -> Result<(), Error> {
        if !self.send_chunk.is_empty() {
            self.finish_write().await?;

            self.write_pos = 0;
            self.write_buffer
                .resize(max_compress_len(self.send_chunk.len()) + 3, 0);
            let compressed_len = self
                .encoder
                .compress(&self.send_chunk, &mut self.write_buffer[3..])?;
            self.write_buffer.truncate(compressed_len + 3);
            if compressed_len >= self.send_chunk.len() {
                // If our compressed size is worse than our uncompressed size, write the original chunk
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

        Ok(())
    }

    async fn finish_write(&mut self) -> Result<(), Error> {
        while self.write_pos < self.write_buffer.len() {
            let len = self
                .channel
                .write(&self.write_buffer[self.write_pos..])
                .await?;
            self.write_pos += len;
        }
        Ok(())
    }

    async fn finish_read(&mut self) -> Result<(), Error> {
        while self.read_pos < self.read_buffer.len() {
            let len = self
                .channel
                .read(&mut self.read_buffer[self.read_pos..])
                .await?;
            self.read_pos += len;
        }
        Ok(())
    }

    fn bincode_config(&self) -> impl bincode::Options + Copy {
        bincode::options().with_limit(self.max_chunk_len as u64)
    }
}

/// Wrapper over an `CompressedBincodeChannel` that only allows a single message type.
pub struct CompressedTypedChannel<T> {
    channel: CompressedBincodeChannel,
    _phantom: PhantomData<T>,
}

impl<T> CompressedTypedChannel<T> {
    pub fn new(channel: CompressedBincodeChannel) -> Self {
        CompressedTypedChannel {
            channel,
            _phantom: PhantomData,
        }
    }

    pub async fn flush(&mut self) -> Result<(), Error> {
        self.channel.flush().await
    }
}

impl<T: Serialize> CompressedTypedChannel<T> {
    pub async fn send(&mut self, msg: &T) -> Result<(), Error> {
        self.channel.send(msg).await
    }
}

impl<T: DeserializeOwned> CompressedTypedChannel<T> {
    pub async fn recv(&mut self) -> Result<T, Error> {
        self.channel.recv().await
    }
}
