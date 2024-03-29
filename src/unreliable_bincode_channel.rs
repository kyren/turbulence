use std::{
    marker::PhantomData,
    task::{Context, Poll},
};

use bincode::Options as _;
use futures::{future, ready, task};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    packet::PacketPool,
    runtime::Timer,
    unreliable_channel::{self, UnreliableChannel},
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
pub struct UnreliableBincodeChannel<T, P>
where
    T: Timer,
    P: PacketPool,
{
    channel: UnreliableChannel<T, P>,
    pending_write: Vec<u8>,
}

impl<T, P> From<UnreliableChannel<T, P>> for UnreliableBincodeChannel<T, P>
where
    T: Timer,
    P: PacketPool,
{
    fn from(channel: UnreliableChannel<T, P>) -> Self {
        Self::new(channel)
    }
}

impl<T, P> UnreliableBincodeChannel<T, P>
where
    T: Timer,
    P: PacketPool,
{
    pub fn new(channel: UnreliableChannel<T, P>) -> Self {
        UnreliableBincodeChannel {
            channel,
            pending_write: Vec::new(),
        }
    }

    pub fn into_inner(self) -> UnreliableChannel<T, P> {
        self.channel
    }

    /// Maximum allowed message length based on the packet capacity of the provided `PacketPool`.
    ///
    /// Will never be greater than `MAX_PACKET_LEN - 2`.
    pub fn max_message_len(&self) -> u16 {
        self.channel.max_message_len()
    }

    /// Write the given serializable message type to the channel.
    ///
    /// Messages are coalesced into larger packets before being sent, so in order to guarantee that
    /// the message is actually sent, you must call `flush`.
    ///
    /// This method is cancel safe, it will never partially send a message, and completes
    /// immediately upon successfully queuing a message to send.
    pub async fn send<M: Serialize>(&mut self, msg: &M) -> Result<(), SendError> {
        future::poll_fn(|cx| self.poll_send_ready(cx)).await?;
        self.start_send(msg)?;
        Ok(())
    }

    pub fn try_send<M: Serialize>(&mut self, msg: &M) -> Result<bool, SendError> {
        if self.try_send_ready()? {
            self.start_send(msg)?;
            Ok(true)
        } else {
            Ok(false)
        }
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

    pub fn try_flush(&mut self) -> Result<bool, unreliable_channel::SendError> {
        match self.poll_flush(&mut Context::from_waker(task::noop_waker_ref())) {
            Poll::Pending => Ok(false),
            Poll::Ready(Ok(())) => Ok(true),
            Poll::Ready(Err(err)) => Err(err),
        }
    }

    /// Receive a deserializable message type as soon as the next message is available.
    ///
    /// This method is cancel safe, it will never partially read a message or drop received
    /// messages.
    pub async fn recv<'a, M: Deserialize<'a>>(&'a mut self) -> Result<M, RecvError> {
        let bincode_config = self.bincode_config();
        let msg = self.channel.recv().await?;
        Ok(bincode_config.deserialize(msg)?)
    }

    pub fn try_recv<'a, M: Deserialize<'a>>(&'a mut self) -> Result<Option<M>, RecvError> {
        match self.poll_recv::<M>(&mut Context::from_waker(task::noop_waker_ref())) {
            Poll::Pending => Ok(None),
            Poll::Ready(Ok(val)) => Ok(Some(val)),
            Poll::Ready(Err(err)) => Err(err),
        }
    }

    pub fn poll_send_ready(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), unreliable_channel::SendError>> {
        if !self.pending_write.is_empty() {
            ready!(self.channel.poll_send(cx, &self.pending_write))?;
            self.pending_write.clear();
        }
        Poll::Ready(Ok(()))
    }

    pub fn try_send_ready(&mut self) -> Result<bool, unreliable_channel::SendError> {
        match self.poll_send_ready(&mut Context::from_waker(task::noop_waker_ref())) {
            Poll::Pending => Ok(false),
            Poll::Ready(Ok(())) => Ok(true),
            Poll::Ready(Err(err)) => Err(err),
        }
    }

    pub fn start_send<M: Serialize>(&mut self, msg: &M) -> Result<(), bincode::Error> {
        assert!(self.pending_write.is_empty());

        let bincode_config = self.bincode_config();
        bincode_config.serialize_into(&mut self.pending_write, msg)?;

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

    pub fn poll_recv<'a, M: Deserialize<'a>>(
        &'a mut self,
        cx: &mut Context,
    ) -> Poll<Result<M, RecvError>> {
        let bincode_config = self.bincode_config();
        let msg = ready!(self.channel.poll_recv(cx))?;
        Poll::Ready(Ok(bincode_config.deserialize::<M>(msg)?))
    }

    fn bincode_config(&self) -> impl bincode::Options + Copy {
        bincode::options().with_limit(self.max_message_len() as u64)
    }
}

/// Wrapper over an `UnreliableBincodeChannel` that only allows a single message type.
pub struct UnreliableTypedChannel<T, P, M>
where
    T: Timer,
    P: PacketPool,
{
    channel: UnreliableBincodeChannel<T, P>,
    _phantom: PhantomData<M>,
}

impl<T, P, M> From<UnreliableChannel<T, P>> for UnreliableTypedChannel<T, P, M>
where
    T: Timer,
    P: PacketPool,
{
    fn from(channel: UnreliableChannel<T, P>) -> Self {
        Self::new(channel)
    }
}

impl<T, P, M> UnreliableTypedChannel<T, P, M>
where
    T: Timer,
    P: PacketPool,
{
    pub fn new(channel: UnreliableChannel<T, P>) -> Self {
        Self {
            channel: UnreliableBincodeChannel::new(channel),
            _phantom: PhantomData,
        }
    }

    pub fn into_inner(self) -> UnreliableChannel<T, P> {
        self.channel.into_inner()
    }

    pub async fn flush(&mut self) -> Result<(), unreliable_channel::SendError> {
        self.channel.flush().await
    }

    pub fn try_flush(&mut self) -> Result<bool, unreliable_channel::SendError> {
        self.channel.try_flush()
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

    pub fn try_send_ready(&mut self) -> Result<bool, unreliable_channel::SendError> {
        self.channel.try_send_ready()
    }
}

impl<T, P, M> UnreliableTypedChannel<T, P, M>
where
    T: Timer,
    P: PacketPool,
    M: Serialize,
{
    pub async fn send(&mut self, msg: &M) -> Result<(), SendError> {
        self.channel.send(msg).await
    }

    pub fn try_send(&mut self, msg: &M) -> Result<bool, SendError> {
        self.channel.try_send(msg)
    }

    pub fn start_send(&mut self, msg: &M) -> Result<(), bincode::Error> {
        self.channel.start_send(msg)
    }
}

impl<'a, T, P, M> UnreliableTypedChannel<T, P, M>
where
    T: Timer,
    P: PacketPool,
    M: Deserialize<'a>,
{
    pub async fn recv(&'a mut self) -> Result<M, RecvError> {
        self.channel.recv::<M>().await
    }

    pub fn try_recv(&'a mut self) -> Result<Option<M>, RecvError> {
        self.channel.try_recv::<M>()
    }

    pub fn poll_recv(&'a mut self, cx: &mut Context) -> Poll<Result<M, RecvError>> {
        self.channel.poll_recv::<M>(cx)
    }
}
