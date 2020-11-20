use std::{marker::PhantomData, u16};

use bincode::Options as _;
use byteorder::{ByteOrder, LittleEndian};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::reliable_channel::{self, ReliableChannel};

#[derive(Debug, Error)]
pub enum Error {
    #[error("reliable channel error: {0}")]
    ReliableChannelError(#[from] reliable_channel::Error),
    #[error("received message exceeds the configured max message length")]
    PrefixTooLarge,
    #[error("an error has been encountered that has caused the stream to shutdown")]
    Shutdown,
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
}

/// Wraps a `ReliableChannel` together with an internal buffer to allow easily sending message types
/// serialized with `bincode`.
///
/// Messages are guaranteed to arrive, and are guaranteed to be in order.  Messages have a maximum
/// length, but this maximum size can be larger than the size of an individual packet.
pub struct ReliableBincodeChannel {
    channel: ReliableChannel,
    max_message_len: u16,

    write_buffer: Box<[u8]>,
    write_pos: usize,
    write_end: usize,

    read_buffer: Box<[u8]>,
    read_pos: usize,
    read_end: usize,
}

impl ReliableBincodeChannel {
    /// Create a new `ReliableBincodeChannel` with a maximum message size of `max_message_len`.
    pub fn new(channel: ReliableChannel, max_message_len: u16) -> Self {
        ReliableBincodeChannel {
            channel,
            max_message_len,
            write_buffer: vec![0; 2 + max_message_len as usize].into_boxed_slice(),
            write_pos: 0,
            write_end: 0,
            read_buffer: vec![0; 2 + max_message_len as usize].into_boxed_slice(),
            read_pos: 0,
            read_end: 0,
        }
    }

    /// Write the given message to the reliable channel.
    ///
    /// In order to ensure that messages are sent in a timely manner, `flush` must be called after
    /// calling this method.  Without calling `flush`, any pending writes will not be sent until the
    /// next automatic sender task wakeup.
    ///
    /// This method is cancel safe, it will never partially send a message, though canceling it may
    /// or may not buffer a message to be sent.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), Error> {
        self.finish_write().await?;
        self.write_pos = 0;
        self.write_end = 2;
        let bincode_config = self.bincode_config();
        let mut w = &mut self.write_buffer[2..];
        match bincode_config.serialize_into(&mut w, msg) {
            Ok(()) => {
                let remaining = w.len();
                self.write_end = self.write_buffer.len() - remaining;
                LittleEndian::write_u16(&mut self.write_buffer[0..2], (self.write_end - 2) as u16);
                self.finish_write().await?;
                Ok(())
            }
            Err(err) => {
                self.write_pos = 0;
                self.write_end = 0;
                Err(err.into())
            }
        }
    }

    /// Ensure that any previously sent messages are sent as soon as possible.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), Error> {
        self.finish_write().await?;
        Ok(self.channel.flush().await?)
    }

    /// Read the next available incoming message.
    ///
    /// This method is cancel safe, it will never partially read a message or drop received
    /// messages.
    pub async fn recv<'a, T: Deserialize<'a>>(&'a mut self) -> Result<T, Error> {
        if self.read_end < 2 {
            self.read_end = 2;
        }
        self.finish_read().await?;

        let message_len = LittleEndian::read_u16(&self.read_buffer[0..2]);
        if message_len > self.max_message_len {
            return Err(Error::PrefixTooLarge);
        }
        self.read_end = message_len as usize + 2;
        self.finish_read().await?;

        let bincode_config = self.bincode_config();
        let res = bincode_config.deserialize(&self.read_buffer[2..self.read_end]);
        self.read_pos = 0;
        self.read_end = 0;
        res.map_err(|e| e.into())
    }

    async fn finish_write(&mut self) -> Result<(), Error> {
        while self.write_pos < self.write_end {
            let len = self
                .channel
                .write(&self.write_buffer[self.write_pos..self.write_end])
                .await?;
            self.write_pos += len;
        }
        Ok(())
    }

    async fn finish_read(&mut self) -> Result<(), Error> {
        while self.read_pos < self.read_end {
            let len = self
                .channel
                .read(&mut self.read_buffer[self.read_pos..self.read_end])
                .await?;
            self.read_pos += len;
        }
        Ok(())
    }

    fn bincode_config(&self) -> impl bincode::Options + Copy {
        bincode::options().with_limit(self.max_message_len as u64)
    }
}

/// Wrapper over an `ReliableBincodeChannel` that only allows a single message type.
pub struct ReliableTypedChannel<T> {
    channel: ReliableBincodeChannel,
    _phantom: PhantomData<T>,
}

impl<T> ReliableTypedChannel<T> {
    pub fn new(channel: ReliableBincodeChannel) -> Self {
        ReliableTypedChannel {
            channel,
            _phantom: PhantomData,
        }
    }

    pub async fn flush(&mut self) -> Result<(), Error> {
        self.channel.flush().await
    }
}

impl<T: Serialize> ReliableTypedChannel<T> {
    pub async fn send(&mut self, msg: &T) -> Result<(), Error> {
        self.channel.send(msg).await
    }
}

impl<'a, T: Deserialize<'a>> ReliableTypedChannel<T> {
    pub async fn recv(&'a mut self) -> Result<T, Error> {
        self.channel.recv().await
    }
}
