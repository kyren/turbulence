use std::{
    marker::PhantomData,
    task::{Context, Poll},
};

use bincode::Options as _;
use futures::{future, ready};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    packet::PacketPool,
    runtime::Runtime,
    unreliable_channel::{self, UnreliableChannel, MAX_MESSAGE_LEN},
};

#[derive(Debug, Error)]
pub enum SendError {
    #[error("unreliable channel error: {0}")]
    UnreliableChannelError(#[from] unreliable_channel::SendError),
    /// Non-fatal error, message is unsent.
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
}

#[derive(Debug, Error)]
pub enum RecvError {
    #[error("unreliable channel error: {0}")]
    UnreliableChannelError(#[from] unreliable_channel::RecvError),
    /// Non-fatal error, message is skipped.
    #[error("bincode serialization error: {0}")]
    BincodeError(#[from] bincode::Error),
}

/// Wraps an `UnreliableChannel` together with an internal buffer to allow easily sending message
/// types serialized with `bincode`.
///
/// Just like the underlying channel, messages are not guaranteed to arrive, nor are they guaranteed
/// to arrive in order.
pub struct UnreliableBincodeChannel<R, P>
where
    R: Runtime,
    P: PacketPool,
{
    channel: UnreliableChannel<R, P>,
    buffer: Box<[u8]>,
    pending_write: u16,
}

impl<R, P> UnreliableBincodeChannel<R, P>
where
    R: Runtime,
    P: PacketPool,
{
    /// Create a new `UnreliableBincodeChannel` with the given max message size.
    ///
    /// The maximum message size is always limited by the underlying `UnreliableChannel` maximum
    /// message size regardless of the `max_message_len` setting, but this can be used to restrict
    /// the intermediate buffer used to serialize messages.
    pub fn new(channel: UnreliableChannel<R, P>, max_message_len: u16) -> Self {
        UnreliableBincodeChannel {
            channel,
            buffer: vec![0; max_message_len.min(MAX_MESSAGE_LEN) as usize].into_boxed_slice(),
            pending_write: 0,
        }
    }

    /// Write the given serializable message type to the channel.
    ///
    /// Messages are coalesced into larger packets before being sent, so in order to guarantee that
    /// the message is actually sent, you must call `flush`.
    ///
    /// This method is cancel safe, it will never partially send a message, and completes
    /// immediately upon successfully queuing a message to send.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), SendError> {
        future::poll_fn(|cx| self.poll_send_ready(cx)).await?;
        self.start_send(msg)?;
        Ok(())
    }

    /// Finish sending any unsent coalesced packets.
    ///
    /// This *must* be called to guarantee that any sent messages are actually sent to the outgoing
    /// packet stream.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), unreliable_channel::SendError> {
        future::poll_fn(|cx| self.poll_flush(cx)).await
    }

    /// Receive a deserializable message type as soon as the next message is available.
    ///
    /// This method is cancel safe, it will never partially read a message or drop received
    /// messages.
    pub async fn recv<'a, T: Deserialize<'a>>(&'a mut self) -> Result<T, RecvError> {
        let bincode_config = self.bincode_config();
        let msg = self.channel.recv().await?;
        Ok(bincode_config.deserialize(msg)?)
    }

    pub fn poll_send_ready(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), unreliable_channel::SendError>> {
        if self.pending_write != 0 {
            ready!(self
                .channel
                .poll_send(cx, &self.buffer[0..self.pending_write as usize]))?;
            self.pending_write = 0;
        }
        Poll::Ready(Ok(()))
    }

    pub fn start_send<T: Serialize>(&mut self, msg: &T) -> Result<(), bincode::Error> {
        assert!(self.pending_write == 0);

        let bincode_config = self.bincode_config();
        let mut w = &mut self.buffer[..];
        bincode_config.serialize_into(&mut w, msg)?;

        let remaining = w.len();
        self.pending_write = (self.buffer.len() - remaining) as u16;

        Ok(())
    }

    pub fn poll_flush(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), unreliable_channel::SendError>> {
        ready!(self.poll_send_ready(cx))?;
        ready!(self.channel.poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }

    pub fn poll_recv<'a, T: Deserialize<'a>>(
        &'a mut self,
        cx: &mut Context,
    ) -> Poll<Result<T, RecvError>> {
        let bincode_config = self.bincode_config();
        let msg = ready!(self.channel.poll_recv(cx))?;
        Poll::Ready(Ok(bincode_config.deserialize(msg)?))
    }

    fn bincode_config(&self) -> impl bincode::Options + Copy {
        bincode::options().with_limit(self.buffer.len() as u64)
    }
}

/// Wrapper over an `UnreliableBincodeChannel` that only allows a single message type.
pub struct UnreliableTypedChannel<T, R, P>
where
    R: Runtime,
    P: PacketPool,
{
    channel: UnreliableBincodeChannel<R, P>,
    _phantom: PhantomData<T>,
}

impl<T, R, P> UnreliableTypedChannel<T, R, P>
where
    R: Runtime,
    P: PacketPool,
{
    pub fn new(channel: UnreliableBincodeChannel<R, P>) -> Self {
        UnreliableTypedChannel {
            channel,
            _phantom: PhantomData,
        }
    }

    pub async fn flush(&mut self) -> Result<(), unreliable_channel::SendError> {
        self.channel.flush().await
    }

    pub fn poll_flush(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), unreliable_channel::SendError>> {
        self.channel.poll_flush(cx)
    }

    pub fn poll_send_ready(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), unreliable_channel::SendError>> {
        self.channel.poll_send_ready(cx)
    }
}

impl<T, R, P> UnreliableTypedChannel<T, R, P>
where
    T: Serialize,
    R: Runtime,
    P: PacketPool,
{
    pub async fn send(&mut self, msg: &T) -> Result<(), SendError> {
        self.channel.send(msg).await
    }

    pub fn start_send(&mut self, msg: &T) -> Result<(), bincode::Error> {
        self.channel.start_send(msg)
    }
}

impl<'a, T, R, P> UnreliableTypedChannel<T, R, P>
where
    T: Deserialize<'a>,
    R: Runtime,
    P: PacketPool,
{
    pub async fn recv(&'a mut self) -> Result<T, RecvError> {
        self.channel.recv().await
    }

    pub fn poll_recv(&'a mut self, cx: &mut Context) -> Poll<Result<T, RecvError>> {
        self.channel.poll_recv(cx)
    }
}
