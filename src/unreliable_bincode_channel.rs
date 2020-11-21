use std::marker::PhantomData;

use bincode::Options as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    packet::PacketPool,
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
pub struct UnreliableBincodeChannel<P>
where
    P: PacketPool,
{
    channel: UnreliableChannel<P>,
    buffer: Box<[u8]>,
}

impl<P> UnreliableBincodeChannel<P>
where
    P: PacketPool,
{
    /// Create a new `UnreliableBincodeChannel` with the given max message size.
    ///
    /// The maximum message size is always limited by the underlying `UnreliableChannel` maximum
    /// message size regardless of the `max_message_len` setting, but this can be used to restrict
    /// the intermediate buffer used to serialize messages.
    pub fn new(channel: UnreliableChannel<P>, max_message_len: u16) -> Self {
        UnreliableBincodeChannel {
            channel,
            buffer: vec![0; max_message_len.min(MAX_MESSAGE_LEN) as usize].into_boxed_slice(),
        }
    }

    /// Write the given serializable message type to the channel.
    ///
    /// Messages are coalesced into larger packets before being sent, so in order to guarantee that
    /// the message is actually sent, you must call `flush`.
    ///
    /// This method is cancel safe, it will never partially send a message, though canceling it may
    /// or may not buffer a message to be sent.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), SendError> {
        let bincode_config = self.bincode_config();
        let mut w = &mut self.buffer[..];
        bincode_config
            .serialize_into(&mut w, msg)
            .map_err(SendError::BincodeError)?;
        let remaining = w.len();
        let written = self.buffer.len() - remaining;
        Ok(self.channel.send(&self.buffer[0..written]).await?)
    }

    /// Finish sending any unsent coalesced packets.
    ///
    /// This *must* be called to guarantee that any sent messages are actually sent to the outgoing
    /// packet stream.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), SendError> {
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

    fn bincode_config(&self) -> impl bincode::Options + Copy {
        bincode::options().with_limit(self.buffer.len() as u64)
    }
}

/// Wrapper over an `UnreliableBincodeChannel` that only allows a single message type.
pub struct UnreliableTypedChannel<T, P>
where
    P: PacketPool,
{
    channel: UnreliableBincodeChannel<P>,
    _phantom: PhantomData<T>,
}

impl<T, P> UnreliableTypedChannel<T, P>
where
    P: PacketPool,
{
    pub fn new(channel: UnreliableBincodeChannel<P>) -> Self {
        UnreliableTypedChannel {
            channel,
            _phantom: PhantomData,
        }
    }

    pub async fn flush(&mut self) -> Result<(), SendError> {
        self.channel.flush().await
    }
}

impl<T, P> UnreliableTypedChannel<T, P>
where
    T: Serialize,
    P: PacketPool,
{
    pub async fn send(&mut self, msg: &T) -> Result<(), SendError> {
        self.channel.send(msg).await
    }
}

impl<'a, T, P> UnreliableTypedChannel<T, P>
where
    T: Deserialize<'a>,
    P: PacketPool,
{
    pub async fn recv(&'a mut self) -> Result<T, RecvError> {
        self.channel.recv().await
    }
}
