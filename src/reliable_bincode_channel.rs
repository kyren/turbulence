use std::{
    marker::PhantomData,
    task::{Context, Poll},
    u16,
};

use bincode::Options as _;
use byteorder::{ByteOrder, LittleEndian};
use futures::{future, ready};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::reliable_channel::{self, ReliableChannel};

#[derive(Debug, Error)]
pub enum SendError {
    /// Fatal internal channel error.
    #[error("reliable channel error: {0}")]
    ReliableChannelError(#[from] reliable_channel::Error),
    /// Non-fatal error, message is unsent.
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
}

#[derive(Debug, Error)]
pub enum RecvError {
    /// Fatal internal channel error.
    #[error("reliable channel error: {0}")]
    ReliableChannelError(#[from] reliable_channel::Error),
    /// Fatal error, reading the next message would exceed the maximum buffer length, no progress
    /// can be made.
    #[error("received message exceeds the configured max message length")]
    PrefixTooLarge,
    /// Non-fatal error, message is skipped.
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
}

/// Wraps a `ReliableChannel` together with an internal buffer to allow easily sending message types
/// serialized with `bincode`.
///
/// Messages are guaranteed to arrive, and are guaranteed to be in order. Messages have a maximum
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
    /// calling this method. Without calling `flush`, any pending writes will not be sent until the
    /// next automatic sender task wakeup.
    ///
    /// This method is cancel safe, it will never partially send a message, and completes
    /// immediately upon successfully queuing a message to send.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), SendError> {
        future::poll_fn(|cx| self.poll_send_ready(cx)).await?;
        self.start_send(msg)?;
        Ok(())
    }

    /// Ensure that any previously sent messages are sent as soon as possible.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), reliable_channel::Error> {
        future::poll_fn(|cx| self.poll_flush(cx)).await
    }

    /// Read the next available incoming message.
    ///
    /// This method is cancel safe, it will never partially read a message or drop received
    /// messages.
    pub async fn recv<'a, T: Deserialize<'a>>(&'a mut self) -> Result<T, RecvError> {
        future::poll_fn(|cx| self.poll_recv_ready(cx)).await?;
        self.recv_next()
    }

    pub fn poll_send_ready(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), reliable_channel::Error>> {
        while self.write_pos < self.write_end {
            let len = ready!(self
                .channel
                .poll_write(cx, &self.write_buffer[self.write_pos..self.write_end]))?;
            self.write_pos += len;
        }
        Poll::Ready(Ok(()))
    }

    pub fn start_send<T: Serialize>(&mut self, msg: &T) -> Result<(), bincode::Error> {
        assert!(self.write_pos == self.write_end);

        self.write_pos = 0;
        self.write_end = 0;

        let bincode_config = self.bincode_config();
        let mut w = &mut self.write_buffer[2..];
        bincode_config.serialize_into(&mut w, msg)?;

        let remaining = w.len();
        self.write_end = self.write_buffer.len() - remaining;
        LittleEndian::write_u16(&mut self.write_buffer[0..2], (self.write_end - 2) as u16);

        Ok(())
    }

    pub fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), reliable_channel::Error>> {
        ready!(self.poll_send_ready(cx))?;
        self.channel.flush()?;
        Poll::Ready(Ok(()))
    }

    pub fn poll_recv<'a, T: Deserialize<'a>>(
        &'a mut self,
        cx: &mut Context,
    ) -> Poll<Result<T, RecvError>> {
        ready!(self.poll_recv_ready(cx))?;
        Poll::Ready(self.recv_next())
    }

    fn poll_recv_ready(&mut self, cx: &mut Context) -> Poll<Result<(), RecvError>> {
        if self.read_end < 2 {
            self.read_end = 2;
        }
        ready!(self.poll_finish_read(cx))?;

        let message_len = LittleEndian::read_u16(&self.read_buffer[0..2]);
        if message_len > self.max_message_len {
            return Poll::Ready(Err(RecvError::PrefixTooLarge));
        }
        self.read_end = message_len as usize + 2;
        ready!(self.poll_finish_read(cx))?;

        Poll::Ready(Ok(()))
    }

    fn recv_next<'a, T: Deserialize<'a>>(&'a mut self) -> Result<T, RecvError> {
        let bincode_config = self.bincode_config();
        let res = bincode_config.deserialize(&self.read_buffer[2..self.read_end]);
        self.read_pos = 0;
        self.read_end = 0;
        Ok(res?)
    }

    fn poll_finish_read(&mut self, cx: &mut Context) -> Poll<Result<(), reliable_channel::Error>> {
        while self.read_pos < self.read_end {
            let len = ready!(self
                .channel
                .poll_read(cx, &mut self.read_buffer[self.read_pos..self.read_end]))?;
            self.read_pos += len;
        }
        Poll::Ready(Ok(()))
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

    pub async fn flush(&mut self) -> Result<(), reliable_channel::Error> {
        self.channel.flush().await
    }

    pub fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), reliable_channel::Error>> {
        self.channel.poll_flush(cx)
    }

    pub fn poll_send_ready(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), reliable_channel::Error>> {
        self.channel.poll_send_ready(cx)
    }
}

impl<T: Serialize> ReliableTypedChannel<T> {
    pub async fn send(&mut self, msg: &T) -> Result<(), SendError> {
        self.channel.send(msg).await
    }

    pub fn start_send(&mut self, msg: &T) -> Result<(), bincode::Error> {
        self.channel.start_send(msg)
    }
}

impl<'a, T: Deserialize<'a>> ReliableTypedChannel<T> {
    pub async fn recv(&'a mut self) -> Result<T, RecvError> {
        self.channel.recv().await
    }

    pub fn poll_recv(&'a mut self, cx: &mut Context) -> Poll<Result<T, RecvError>> {
        self.channel.poll_recv(cx)
    }
}
