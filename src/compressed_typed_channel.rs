use std::{convert::TryInto, marker::PhantomData, u16};

use byteorder::{ByteOrder, LittleEndian};
use serde::{de::DeserializeOwned, Serialize};
use snap::raw::{decompress_len, max_compress_len, Decoder as SnapDecoder, Encoder as SnapEncoder};
use thiserror::Error;

use crate::reliable_channel::{self, ReliableChannel};

#[derive(Debug, Error)]
pub enum Error {
    #[error("reliable channel error error: {0}")]
    ReliableChannelError(#[from] reliable_channel::Error),
    #[error("chunk has exceeded the configured max chunk length")]
    ChunkTooLarge,
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
    #[error("Snappy serialization error: {0}")]
    SnapError(#[from] snap::Error),
}

/// The maximum supported length for a message chunk sent over a `CompressedTypedChannel`.
pub const MAX_CHUNK_LEN: usize = u16::MAX as usize + 1;

/// Wraps a `ReliableMessageChannel` and reliably sends a single message type serialized with
/// `bincode` and compressed with `snap`.
///
/// Messages are written in large blocks to aid compression.  Messages are serialized end to end,
/// and when a block reaches the maximum configured size (or `flush` is called), the block is
/// compressed and sent as a single message.
///
/// This saves space from the compression and also from the reduced message header overhead per
/// individual message.
pub struct CompressedTypedChannel<T> {
    channel: ReliableChannel,
    bincode_config: bincode::Config,
    max_chunk_len: usize,

    send_chunk: Vec<u8>,

    write_buffer: Vec<u8>,
    write_pos: usize,

    read_buffer: Vec<u8>,
    read_pos: usize,

    recv_chunk: Vec<u8>,
    recv_pos: usize,

    encoder: SnapEncoder,
    decoder: SnapDecoder,

    _phantom: PhantomData<T>,
}

impl<T> CompressedTypedChannel<T> {
    /// The `max_chunk_len` parameter describes the maximum buffer size of a combined message block
    /// before it is automatically sent.
    ///
    /// An individual message may be no more than `max_chunk_len` in length.
    pub fn new(channel: ReliableChannel, max_chunk_len: usize) -> Self {
        let mut bincode_config = bincode::config();
        assert!(max_chunk_len > 1);
        assert!(max_chunk_len <= MAX_CHUNK_LEN as usize);
        bincode_config.limit(max_chunk_len as u64);
        CompressedTypedChannel {
            channel,
            bincode_config,
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
            _phantom: PhantomData,
        }
    }

    pub async fn flush(&mut self) -> Result<(), Error> {
        self.write_send_chunk().await?;
        self.finish_write().await?;
        self.channel.flush().await?;
        Ok(())
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
                    (self.send_chunk.len() - 1).try_into().unwrap(),
                );
            } else {
                // An initial 1 means compressed
                self.write_buffer[0] = 1;
                LittleEndian::write_u16(
                    &mut self.write_buffer[1..3],
                    (compressed_len - 1).try_into().unwrap(),
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
}

impl<T: Serialize> CompressedTypedChannel<T> {
    pub async fn send(&mut self, msg: &T) -> Result<(), Error> {
        let serialized_len = self.bincode_config.serialized_size(msg)?;
        if self.send_chunk.len() + serialized_len as usize > self.max_chunk_len {
            self.write_send_chunk().await?;
        }

        self.bincode_config
            .serialize_into(&mut self.send_chunk, msg)?;

        Ok(())
    }
}

impl<T: DeserializeOwned> CompressedTypedChannel<T> {
    pub async fn recv(&mut self) -> Result<T, Error> {
        loop {
            if self.recv_pos < self.recv_chunk.len() {
                let mut reader = &self.recv_chunk[self.recv_pos..];
                let msg = self.bincode_config.deserialize_from(&mut reader)?;
                self.recv_pos = self.recv_chunk.len() - reader.len();
                return Ok(msg);
            }

            if self.read_pos < 3 {
                self.read_buffer.resize(3, 0);
                self.finish_read().await?;
            }

            let compressed = self.read_buffer[0] != 0;
            let chunk_len = LittleEndian::read_u16(&self.read_buffer[1..3]) as usize + 1;
            if chunk_len > self.max_chunk_len {
                return Err(Error::ChunkTooLarge);
            }
            self.read_buffer.resize(chunk_len as usize + 3, 0);
            self.finish_read().await?;

            if compressed {
                let decompressed_len = decompress_len(&self.read_buffer[3..])?;
                if decompressed_len > self.max_chunk_len {
                    return Err(Error::ChunkTooLarge);
                }
                self.recv_chunk.resize(decompressed_len, 0);
                self.decoder
                    .decompress(&self.read_buffer[3..], &mut self.recv_chunk)?;
            } else {
                self.recv_chunk.resize(chunk_len, 0);
                self.recv_chunk.copy_from_slice(&self.read_buffer[3..]);
            }

            self.recv_pos = 0;
            self.read_pos = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use futures::channel::{mpsc, oneshot};
    use rand::{rngs::SmallRng, thread_rng, RngCore, SeedableRng};

    use crate::{
            packet_multiplexer::MuxPacketPool,
            reliable_channel::{self, ReliableChannel},
            test_util::{condition_link, LinkCondition, SimpleExecutor, TestTimer},
        spawn::Spawn,
    };

    #[test]
    fn test_compressed_typed_channel() {
        const SETTINGS: reliable_channel::Settings = reliable_channel::Settings {
            bandwidth: 2048,
            recv_window_size: 512,
            send_window_size: 512,
            burst_bandwidth: 512,
            init_send: 256,
            wakeup_time: Duration::from_millis(50),
            initial_rtt: Duration::from_millis(100),
            max_rtt: Duration::from_millis(2000),
            rtt_update_factor: 0.1,
            rtt_resend_factor: 1.5,
        };

        const CONDITION: LinkCondition = LinkCondition {
            loss: 0.2,
            duplicate: 0.05,
            delay: Duration::from_millis(40),
            jitter: Duration::from_millis(10),
        };

        let packet_pool = MuxPacketPool::new();
        let mut executor = SimpleExecutor::default();
        let mut timer = TestTimer::new();

        let (asend, acondrecv) = mpsc::channel(2);
        let (acondsend, arecv) = mpsc::channel(2);
        condition_link(
            CONDITION,
            executor.spawner(),
            timer.handle(),
            packet_pool.clone(),
            SmallRng::from_rng(thread_rng()).unwrap(),
            acondrecv,
            acondsend,
        );

        let (bsend, bcondrecv) = mpsc::channel(2);
        let (bcondsend, brecv) = mpsc::channel(2);
        condition_link(
            CONDITION,
            executor.spawner(),
            timer.handle(),
            packet_pool.clone(),
            SmallRng::from_rng(thread_rng()).unwrap(),
            bcondrecv,
            bcondsend,
        );

        let mut stream1 = CompressedTypedChannel::<Vec<u8>>::new(
            ReliableChannel::new(
                SETTINGS.clone(),
                timer.handle(),
                packet_pool.clone(),
                arecv,
                bsend,
                executor.spawner(),
            ),
            1024,
        );
        let mut stream2 = CompressedTypedChannel::<Vec<u8>>::new(
            ReliableChannel::new(
                SETTINGS.clone(),
                timer.handle(),
                packet_pool.clone(),
                brecv,
                asend,
                executor.spawner(),
            ),
            1024,
        );

        let (a_done_send, mut a_done) = oneshot::channel();
        executor.spawn({
            async move {
                for i in 0..100 {
                    let send_val = vec![i as u8 + 13; i + 25];
                    stream1.send(&send_val).await.unwrap();
                }
                stream1.flush().await.unwrap();

                for i in 0..100 {
                    let recv_val = stream1.recv().await.unwrap();
                    assert_eq!(recv_val.len(), i + 17);
                }

                a_done_send.send(()).unwrap();
            }
        });

        let (b_done_send, mut b_done) = oneshot::channel();
        executor.spawn({
            async move {
                for i in 0..100 {
                    let recv_val = stream2.recv().await.unwrap();
                    assert_eq!(recv_val, vec![i as u8 + 13; i + 25].as_slice());
                }

                for i in 0..100 {
                    let mut send_val = vec![0; i + 17];
                    rand::thread_rng().fill_bytes(&mut send_val);
                    stream2.send(&send_val).await.unwrap();
                }
                stream2.flush().await.unwrap();

                b_done_send.send(()).unwrap();
            }
        });

        let mut a_is_done = false;
        let mut b_is_done = false;
        loop {
            a_is_done = a_is_done || a_done.try_recv().unwrap().is_some();
            b_is_done = b_is_done || b_done.try_recv().unwrap().is_some();

            if a_is_done && b_is_done {
                break;
            }

            executor.run_until_stalled();
            timer.advance(50);
        }
    }
}
