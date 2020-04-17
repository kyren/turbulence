use std::{
    any::{type_name, Any, TypeId},
    collections::{hash_map, HashMap, HashSet},
    error::Error,
};

use futures::{
    channel::{mpsc, oneshot},
    future::{self, BoxFuture},
    select,
    stream::FuturesUnordered,
    FutureExt, SinkExt, StreamExt, TryFutureExt,
};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

use crate::{
    channel_builder::ChannelBuilder,
    event_watch,
    packet::PacketPool,
    packet_multiplexer::{ChannelStatistics, PacketChannel, PacketMultiplexer},
    reliable_channel,
    runtime::Runtime,
};

// TODO: Message channels are currently always full-duplex, because the unreliable / reliable
// channels backing them are always full-duplex.  We could add configuration to limit a channel to
// send or receive only, and to error if the remote sends to a send-only channel.
#[derive(Debug)]
pub struct MessageChannelSettings {
    pub channel: PacketChannel,
    pub channel_mode: MessageChannelMode,
    /// The buffer size for the mpsc channel of messages that transports messages of this type to /
    /// from the network task.
    pub message_buffer_size: usize,
    /// The buffer size for the mpsc channel of packets for this message type that transports
    /// packets to / from the packet multiplexer.
    pub packet_buffer_size: usize,
}

#[derive(Debug)]
pub enum MessageChannelMode {
    Unreliable,
    Reliable {
        reliability_settings: reliable_channel::Settings,
        max_message_len: usize,
    },
    Compressed {
        reliability_settings: reliable_channel::Settings,
        max_chunk_len: usize,
    },
}

pub trait ChannelMessage: Serialize + DeserializeOwned + Send + Sync + 'static {}

impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> ChannelMessage for T {}

#[derive(Debug, Error)]
pub enum ChannelAlreadyRegistered {
    #[error("message type already registered")]
    MessageType,
    #[error("channel already registered")]
    Channel,
}

pub type TaskError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Error)]
#[error("network task for message type {type_name:?} has errored: {source}")]
pub struct ChannelTaskError {
    pub type_name: &'static str,
    pub source: TaskError,
}

pub struct MessageChannelsBuilder<R, P>
where
    R: Runtime,
    P: PacketPool,
{
    runtime: R,
    pool: P,
    channels: HashSet<PacketChannel>,
    register_fns: HashMap<TypeId, (&'static str, MessageChannelSettings, RegisterFn<R, P>)>,
}

impl<R, P> MessageChannelsBuilder<R, P>
where
    R: Runtime,
    P: PacketPool,
{
    pub fn new(runtime: R, pool: P) -> Self {
        MessageChannelsBuilder {
            runtime,
            pool,
            channels: HashSet::new(),
            register_fns: HashMap::new(),
        }
    }
}

impl<R, P> MessageChannelsBuilder<R, P>
where
    R: Runtime + 'static,
    P: PacketPool + Clone + Send + 'static,
    P::Packet: Unpin + Send,
{
    /// Register this message type on the constructed `MessageChannels`, using the given channel
    /// settings.
    ///
    /// Can only be called once per message type, will error if it is called with the same message
    /// type more than once.
    pub fn register<M: ChannelMessage>(
        &mut self,
        settings: MessageChannelSettings,
    ) -> Result<(), ChannelAlreadyRegistered> {
        if !self.channels.insert(settings.channel) {
            return Err(ChannelAlreadyRegistered::Channel);
        }

        match self.register_fns.entry(TypeId::of::<M>()) {
            hash_map::Entry::Occupied(_) => Err(ChannelAlreadyRegistered::MessageType),
            hash_map::Entry::Vacant(vacant) => {
                vacant.insert((type_name::<M>(), settings, register_message_type::<R, P, M>));
                Ok(())
            }
        }
    }

    /// Build a `MessageChannels` instance that can send and receive all of the registered message
    /// types via channels on the given packet multiplexer.
    pub fn build(self, multiplexer: &mut PacketMultiplexer<P::Packet>) -> MessageChannels {
        let mut channel_builder = ChannelBuilder::new(self.runtime, self.pool);
        let mut channels_map = ChannelsMap::default();
        let mut tasks: FuturesUnordered<_> = self
            .register_fns
            .into_iter()
            .map(|(_, (type_name, settings, register_fn))| {
                register_fn(
                    settings,
                    multiplexer,
                    &mut channel_builder,
                    &mut channels_map,
                )
                .map_err(move |source| ChannelTaskError { type_name, source })
            })
            .collect();

        let (error_sender, error_receiver) = oneshot::channel();
        channel_builder.runtime.spawn({
            async move {
                if let Some(res) = tasks.next().await {
                    let _ = error_sender.send(res.unwrap_err());
                }
            }
        });

        MessageChannels {
            disconnected: false,
            task_error: error_receiver,
            channels: channels_map,
        }
    }
}

#[derive(Debug, Error)]
#[error("no such message type registered")]
pub struct MessageTypeUnregistered;

#[derive(Debug, Error)]
pub enum AsyncMessageError {
    #[error(transparent)]
    Unregistered(#[from] MessageTypeUnregistered),
    #[error("message channel has been disconnected")]
    Disconnected,
}

/// Manages a set of channels through a packet multiplexer, where each channel is associated with
/// exactly one message type.
///
/// Acts as a bridge between the sync and async worlds.  Provides sync methods to send and receive
/// messages that do not block or error.  The only error condition is if any of the backing tasks
/// end or if the backing packet channels are dropped, the `MessageChannels` will permanently go
/// into a "disconnected" state.
///
/// Additionally still provides async versions of methods to send and receive messages that share
/// the same simplified error handling, which may be useful during startup or shutdown.
#[derive(Debug)]
pub struct MessageChannels {
    disconnected: bool,
    task_error: oneshot::Receiver<ChannelTaskError>,
    channels: ChannelsMap,
}

impl MessageChannels {
    /// Returns whether this `MessageChannels` has become disconnected because the backing network
    /// task has errored.
    ///
    /// Once it has become disconnected, a `MessageChannels` is permanently in this errored state.
    /// You can receive the error from the task by calling `MessageChannels::recv_err`.
    pub fn is_connected(&self) -> bool {
        !self.disconnected
    }

    /// Consume this `MessageChannels` and receive the networking task shutdown error.
    ///
    /// If this `MessageChannels` is disconnected, returns the error that caused it to become
    /// disconnected.  If it is not disconnected, it will become disconnected by calling this and
    /// return that error.
    pub async fn recv_err(self) -> ChannelTaskError {
        drop(self.channels);
        self.task_error.await.expect("task has panicked")
    }

    /// Send the given message on the channel associated with its message type.
    ///
    /// In order to ensure delivery, `flush` should be called for the same message type to
    /// immediately send any buffered messages.
    ///
    /// If the mpsc channel for this message type is full, will return the message that was sent
    /// back to the caller.  If the message was successfully put onto the outgoing mpsc channel,
    /// will return None.
    ///
    /// # Panics
    /// Panics if this message type was not registered with the `MessageChannelsBuilder` used to
    /// build this `MessageChannels` instance.
    pub fn send<M: ChannelMessage>(&mut self, message: M) -> Option<M> {
        self.try_send(message).unwrap()
    }

    /// Like `MessageChannels::send` but errors instead of panicking.
    pub fn try_send<M: ChannelMessage>(
        &mut self,
        message: M,
    ) -> Result<Option<M>, MessageTypeUnregistered> {
        let channels = self.channels.get_mut::<M>()?;

        Ok(if self.disconnected {
            Some(message)
        } else if let Err(err) = channels.outgoing_sender.try_send(message) {
            if err.is_disconnected() {
                self.disconnected = true;
            }
            Some(err.into_inner())
        } else {
            None
        })
    }

    /// Any async version of `MessageChannels::send`, sends the given message on the channel
    /// associated with its message type but waits if the channel is full.  Like
    /// `MessageChannels::send`, `MessageChannels::flush` must still be called afterwards in order
    /// to ensure delivery.
    ///
    /// # Panics
    /// Panics if this message type is not registered, or if the channel task has panicked.
    pub async fn async_send<M: ChannelMessage>(&mut self, message: M) {
        self.try_async_send(message).await.unwrap()
    }

    /// Like `MessageChannels::async_send` but errors instead of panicking.
    pub async fn try_async_send<M: ChannelMessage>(
        &mut self,
        message: M,
    ) -> Result<(), AsyncMessageError> {
        let channels = self.channels.get_mut::<M>()?;

        if self.disconnected {
            Err(AsyncMessageError::Disconnected)
        } else {
            let res = async {
                future::poll_fn(|cx| channels.outgoing_sender.poll_ready(cx)).await?;
                channels.outgoing_sender.start_send(message)
            }
            .await;

            if res.is_err() {
                self.disconnected = true;
                Err(AsyncMessageError::Disconnected)
            } else {
                Ok(())
            }
        }
    }

    /// Immediately send any buffered messages for this message type.  Messages may not be delivered
    /// unless `flush` is called after any `send` calls.
    ///
    /// # Panics
    /// Panics if this message type was not registered with the `MessageChannelsBuilder` used to
    /// build this `MessageChannels` instance.
    pub fn flush<M: ChannelMessage>(&mut self) {
        self.try_flush::<M>().unwrap();
    }

    /// Like `MessageChannels::flush` but errors instead of panicking.
    pub fn try_flush<M: ChannelMessage>(&mut self) -> Result<(), MessageTypeUnregistered> {
        self.channels.get_mut::<M>()?.flush_sender.signal();
        Ok(())
    }

    /// Receive an incoming message on the channel associated with this mesage type, if one is
    /// available.
    ///
    /// # Panics
    /// Panics if this message type was not registered with the `MessageChannelsBuilder` used to
    /// build this `MessageChannels` instance.
    pub fn recv<M: ChannelMessage>(&mut self) -> Option<M> {
        self.try_recv().unwrap()
    }

    /// Like `MessageChannels::recv` but errors instead of panicking.
    pub fn try_recv<M: ChannelMessage>(&mut self) -> Result<Option<M>, MessageTypeUnregistered> {
        let channels = self.channels.get_mut::<M>()?;

        Ok(if self.disconnected {
            None
        } else {
            match channels.incoming_receiver.try_next() {
                Ok(None) => {
                    self.disconnected = true;
                    None
                }
                Ok(Some(msg)) => Some(msg),
                Err(_) => None,
            }
        })
    }

    /// Any async version of `MessageChannels::receive`, receives an incoming message on the channel
    /// associated with its message type but waits if there is no message available.
    ///
    /// # Panics
    /// Panics if this message type is not registered, or if the channel task has panicked.
    pub async fn async_recv<M: ChannelMessage>(&mut self) -> M {
        self.try_async_recv().await.unwrap()
    }

    /// Like `MessageChannels::async_recv` but errors instead of panicking.
    pub async fn try_async_recv<M: ChannelMessage>(&mut self) -> Result<M, AsyncMessageError> {
        let channels = self.channels.get_mut::<M>()?;

        if self.disconnected {
            Err(AsyncMessageError::Disconnected)
        } else if let Some(message) = channels.incoming_receiver.next().await {
            Ok(message)
        } else {
            self.disconnected = true;
            Err(AsyncMessageError::Disconnected)
        }
    }

    pub fn try_statistics<M: ChannelMessage>(
        &self,
    ) -> Result<&ChannelStatistics, MessageTypeUnregistered> {
        Ok(&self.channels.get::<M>()?.statistics)
    }

    pub fn statistics<M: ChannelMessage>(&self) -> &ChannelStatistics {
        self.try_statistics::<M>().unwrap()
    }
}

type ChannelTask = BoxFuture<'static, Result<(), TaskError>>;
type RegisterFn<R, P> = fn(
    MessageChannelSettings,
    &mut PacketMultiplexer<<P as PacketPool>::Packet>,
    &mut ChannelBuilder<R, P>,
    &mut ChannelsMap,
) -> ChannelTask;

#[derive(Debug, Error)]
#[error("channel has been disconnected")]
struct ChannelDisconnected;

struct ChannelSet<M> {
    outgoing_sender: mpsc::Sender<M>,
    incoming_receiver: mpsc::Receiver<M>,
    flush_sender: event_watch::Sender,
    statistics: ChannelStatistics,
}

#[derive(Debug, Default)]
struct ChannelsMap(HashMap<TypeId, Box<dyn Any + Send + Sync>>);

impl ChannelsMap {
    fn insert<M: ChannelMessage>(&mut self, channel_set: ChannelSet<M>) -> bool {
        self.0
            .insert(TypeId::of::<M>(), Box::new(channel_set))
            .is_none()
    }

    fn get<M: ChannelMessage>(&self) -> Result<&ChannelSet<M>, MessageTypeUnregistered> {
        Ok(self
            .0
            .get(&TypeId::of::<M>())
            .ok_or(MessageTypeUnregistered)?
            .downcast_ref()
            .unwrap())
    }

    fn get_mut<M: ChannelMessage>(
        &mut self,
    ) -> Result<&mut ChannelSet<M>, MessageTypeUnregistered> {
        Ok(self
            .0
            .get_mut(&TypeId::of::<M>())
            .ok_or(MessageTypeUnregistered)?
            .downcast_mut()
            .unwrap())
    }
}

fn register_message_type<R, P, M>(
    settings: MessageChannelSettings,
    multiplexer: &mut PacketMultiplexer<P::Packet>,
    builder: &mut ChannelBuilder<R, P>,
    channels_map: &mut ChannelsMap,
) -> ChannelTask
where
    R: Runtime + 'static,
    P: PacketPool + Clone + Send + 'static,
    P::Packet: Unpin + Send,
    M: ChannelMessage,
{
    enum Next<M> {
        Incoming(M),
        Outgoing(M),
        Flush,
    }

    let (mut incoming_message_sender, incoming_message_receiver) =
        mpsc::channel::<M>(settings.message_buffer_size);
    let (outgoing_message_sender, mut outgoing_message_receiver) =
        mpsc::channel::<M>(settings.message_buffer_size);

    let (flush_sender, mut flush_receiver) = event_watch::channel();

    // TODO: Ideally, you would want all the channel types to implement a single trait and not have
    // to repeat this task implementation for all of them.  Unfortunately, for the time being, doing
    // so would require that the typed channels not use async / await or that the trait would box
    // the returned futures, because rust doesn't yet support async / await in traits.
    let (channel_task, statistics) = match settings.channel_mode {
        MessageChannelMode::Unreliable => {
            let (mut channel, statistics) = builder
                .open_unreliable_typed_channel(
                    multiplexer,
                    settings.channel,
                    settings.packet_buffer_size,
                )
                .expect("duplicate packet channel");
            let task = async move {
                loop {
                    let next = {
                        select! {
                            incoming = channel.recv().fuse() => Next::Incoming(incoming?),
                            outgoing = outgoing_message_receiver.next().fuse() => Next::Outgoing(outgoing.ok_or(ChannelDisconnected)?),
                            flush = flush_receiver.wait().fuse() => Next::Flush,
                        }
                    };

                    match next {
                        Next::Incoming(incoming) => {
                            incoming_message_sender.send(incoming).await?;
                        }
                        Next::Outgoing(outgoing) => {
                            channel.send(&outgoing).await?;
                        }
                        Next::Flush => {
                            while let Ok(outgoing) = outgoing_message_receiver.try_next() {
                                let outgoing = outgoing.ok_or(ChannelDisconnected)?;
                                channel.send(&outgoing).await?;
                            }
                            channel.flush().await?;
                        }
                    }
                }
            }
            .boxed();
            (task, statistics)
        }
        MessageChannelMode::Reliable {
            reliability_settings,
            max_message_len,
        } => {
            let (mut channel, statistics) = builder
                .open_reliable_typed_channel(
                    multiplexer,
                    settings.channel,
                    settings.packet_buffer_size,
                    reliability_settings,
                    max_message_len,
                )
                .expect("duplicate packet channel");
            let task = async move {
                loop {
                    let next = {
                        select! {
                            incoming = channel.recv().fuse() => Next::Incoming(incoming?),
                            outgoing = outgoing_message_receiver.next().fuse() => {
                                Next::Outgoing(outgoing.ok_or(ChannelDisconnected)?)
                            }
                            flush = flush_receiver.wait().fuse() => Next::Flush,
                        }
                    };

                    match next {
                        Next::Incoming(incoming) => {
                            incoming_message_sender.send(incoming).await?;
                        }
                        Next::Outgoing(outgoing) => {
                            channel.send(&outgoing).await?;
                        }
                        Next::Flush => {
                            while let Ok(outgoing) = outgoing_message_receiver.try_next() {
                                let outgoing = outgoing.ok_or(ChannelDisconnected)?;
                                channel.send(&outgoing).await?;
                            }
                            channel.flush().await?;
                        }
                    }
                }
            }
            .boxed();
            (task, statistics)
        }
        MessageChannelMode::Compressed {
            reliability_settings,
            max_chunk_len,
        } => {
            let (mut channel, statistics) = builder
                .open_compressed_typed_channel(
                    multiplexer,
                    settings.channel,
                    settings.packet_buffer_size,
                    reliability_settings,
                    max_chunk_len,
                )
                .expect("duplicate packet channel");
            let task = async move {
                loop {
                    let next = {
                        select! {
                            incoming = channel.recv().fuse() => Next::Incoming(incoming?),
                            outgoing = outgoing_message_receiver.next().fuse() => {
                                Next::Outgoing(outgoing.ok_or(ChannelDisconnected)?)
                            }
                            flush = flush_receiver.wait().fuse() => Next::Flush,
                        }
                    };

                    match next {
                        Next::Incoming(incoming) => {
                            incoming_message_sender.send(incoming).await?;
                        }
                        Next::Outgoing(outgoing) => {
                            channel.send(&outgoing).await?;
                        }
                        Next::Flush => {
                            while let Ok(outgoing) = outgoing_message_receiver.try_next() {
                                let outgoing = outgoing.ok_or(ChannelDisconnected)?;
                                channel.send(&outgoing).await?;
                            }
                            channel.flush().await?;
                        }
                    }
                }
            }
            .boxed();
            (task, statistics)
        }
    };

    channels_map.insert(ChannelSet::<M> {
        outgoing_sender: outgoing_message_sender,
        flush_sender: flush_sender,
        incoming_receiver: incoming_message_receiver,
        statistics,
    });

    channel_task
}
