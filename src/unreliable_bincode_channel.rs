use std::marker::PhantomData;

use futures::channel::mpsc::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    packet::PacketPool,
    unreliable_channel::{self, UnreliableChannel, MAX_MESSAGE_LEN},
};

#[derive(Debug, Error)]
pub enum SendError {
    #[error("outgoing packet stream has been disconnected")]
    Disconnected,
    #[error("bincode serialization error: {0}")]
    BincodeError(bincode::Error),
}

#[derive(Debug, Error)]
pub enum RecvError {
    #[error("incoming packet stream has been disconnected")]
    Disconnected,
    #[error("incoming packet has bad message format")]
    BadFormat,
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
    buffer: Vec<u8>,
    bincode_config: bincode::Config,
}

impl<P> UnreliableBincodeChannel<P>
where
    P: PacketPool,
{
    pub fn new(packet_pool: P, incoming: Receiver<P::Packet>, outgoing: Sender<P::Packet>) -> Self {
        let mut bincode_config = bincode::config();
        bincode_config.limit(MAX_MESSAGE_LEN as u64);
        UnreliableBincodeChannel {
            channel: UnreliableChannel::new(packet_pool, incoming, outgoing),
            buffer: Vec::new(),
            bincode_config,
        }
    }

    /// Write the given serializable message type to the channel.
    ///
    /// Messages are coalesced into larger packets before being sent, so in order to guarantee that
    /// the message is actually sent, you must call `flush`.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), SendError> {
        self.buffer.clear();
        self.bincode_config
            .serialize_into(&mut self.buffer, msg)
            .map_err(SendError::BincodeError)?;
        self.channel
            .send(&self.buffer)
            .await
            .map_err(from_inner_send_err)
    }

    /// Finish sending any unsent coalesced packets.
    ///
    /// This *must* be called to guarantee that any sent messages are actually sent to the outgoing
    /// packet stream.
    pub async fn flush(&mut self) -> Result<(), SendError> {
        self.channel.flush().await.map_err(from_inner_send_err)
    }

    /// Receive a deserializable message type as soon as the next message is available.
    pub async fn recv<'a, T: Deserialize<'a>>(&'a mut self) -> Result<T, RecvError> {
        self.buffer.resize(MAX_MESSAGE_LEN, 0);
        let len = self
            .channel
            .recv(&mut self.buffer[..])
            .await
            .map_err(from_inner_recv_err)?;
        self.buffer.truncate(len);
        self.bincode_config
            .deserialize(&self.buffer)
            .map_err(RecvError::BincodeError)
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
    pub fn new(packet_pool: P, incoming: Receiver<P::Packet>, outgoing: Sender<P::Packet>) -> Self {
        UnreliableTypedChannel {
            channel: UnreliableBincodeChannel::new(packet_pool, incoming, outgoing),
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

fn from_inner_send_err(err: unreliable_channel::SendError) -> SendError {
    match err {
        unreliable_channel::SendError::Disconnected => SendError::Disconnected,
        unreliable_channel::SendError::TooBig => {
            unreachable!("messages that are too large are caught by bincode configuration")
        }
    }
}

fn from_inner_recv_err(err: unreliable_channel::RecvError) -> RecvError {
    match err {
        unreliable_channel::RecvError::Disconnected => RecvError::Disconnected,
        unreliable_channel::RecvError::BadFormat => RecvError::BadFormat,
        unreliable_channel::RecvError::TooBig => {
            unreachable!("messages that are too large are caught by bincode configuration")
        }
    }
}
