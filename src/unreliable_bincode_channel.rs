use std::marker::PhantomData;

use bincode::Options as _;
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
    BincodeError(bincode::Error),
}

#[derive(Debug, Error)]
pub enum RecvError {
    #[error("unreliable channel error: {0}")]
    UnreliableChannelError(#[from] unreliable_channel::RecvError),
    /// Non-fatal error, message is skipped.
    #[error("bincode serialization error: {0}")]
    BincodeError(bincode::Error),
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
        self.finish_write().await?;

        let bincode_config = self.bincode_config();
        let mut w = &mut self.buffer[..];
        bincode_config
            .serialize_into(&mut w, msg)
            .map_err(SendError::BincodeError)?;

        let remaining = w.len();
        self.pending_write = (self.buffer.len() - remaining) as u16;

        Ok(())
    }

    /// Finish sending any unsent coalesced packets.
    ///
    /// This *must* be called to guarantee that any sent messages are actually sent to the outgoing
    /// packet stream.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), SendError> {
        self.finish_write().await?;
        Ok(self.channel.flush().await?)
    }

    /// Receive a deserializable message type as soon as the next message is available.
    ///
    /// This method is cancel safe, it will never partially read a message or drop received
    /// messages.
    pub async fn recv<'a, T: Deserialize<'a>>(&'a mut self) -> Result<T, RecvError> {
        let bincode_config = self.bincode_config();
        let msg = self.channel.recv().await?;
        bincode_config
            .deserialize(msg)
            .map_err(RecvError::BincodeError)
    }

    async fn finish_write(&mut self) -> Result<(), unreliable_channel::SendError> {
        if self.pending_write != 0 {
            self.channel
                .send(&self.buffer[0..self.pending_write as usize])
                .await?;
            self.pending_write = 0;
        }
        Ok(())
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

    pub async fn flush(&mut self) -> Result<(), SendError> {
        self.channel.flush().await
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
}
