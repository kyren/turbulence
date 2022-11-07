use std::{
    any::{type_name, Any, TypeId},
    collections::{hash_map, HashMap, HashSet},
    error::Error,
    task::{Context, Poll},
};

use futures::{
    future::{self, BoxFuture, RemoteHandle},
    ready, select,
    stream::FuturesUnordered,
    FutureExt, SinkExt, StreamExt, TryFutureExt,
};
use rustc_hash::FxHashMap;
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

use crate::{
    event_watch,
    packet::PacketPool,
    packet_multiplexer::{ChannelStatistics, PacketChannel, PacketMultiplexer},
    reliable_channel,
    runtime::Runtime,
    spsc::{self, TryRecvError},
    unreliable_channel, CompressedBincodeChannel, CompressedTypedChannel, MuxPacketPool,
    ReliableBincodeChannel, ReliableChannel, ReliableTypedChannel, UnreliableBincodeChannel,
    UnreliableChannel, UnreliableTypedChannel,
};

// TODO: Message channels are currently always full-duplex, because the unreliable / reliable
// channels backing them are always full-duplex. We could add configuration to limit a channel to
// send or receive only, and to error if the remote sends to a send-only channel.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageChannelSettings {
    pub channel: PacketChannel,
    pub channel_mode: MessageChannelMode,
    /// The buffer size for the spsc channel of messages that transports messages of this type to /
    /// from the network task.
    pub message_buffer_size: usize,
    /// The buffer size for the spsc channel of packets for this message type that transports
    /// packets to / from the packet multiplexer.
    pub packet_buffer_size: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageChannelMode {
    Unreliable {
        settings: unreliable_channel::Settings,
        max_message_len: u16,
    },
    Reliable {
        settings: reliable_channel::Settings,
        max_message_len: u16,
    },
    Compressed {
        settings: reliable_channel::Settings,
        max_chunk_len: u16,
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
#[error("network task for message type {type_name:?} has errored: {error}")]
pub struct ChannelTaskError {
    pub type_name: &'static str,
    pub error: TaskError,
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
    R: Runtime + Clone + 'static,
    P: PacketPool + Clone + Send + 'static,
    P::Packet: Send,
{
    /// Register this message type on the constructed `MessageChannels`, using the given channel
    /// settings.
    ///
    /// Can only be called once per message type, will error if it is called with the same message
    /// type or channel number more than once.
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
        let Self {
            runtime,
            pool,
            register_fns,
            ..
        } = self;
        let mut channels_map = ChannelsMap::default();
        let mut tasks: FuturesUnordered<_> = register_fns
            .into_iter()
            .map(|(_, (type_name, settings, register_fn))| {
                register_fn(
                    settings,
                    runtime.clone(),
                    pool.clone(),
                    multiplexer,
                    &mut channels_map,
                )
                .map_err(move |error| ChannelTaskError { type_name, error })
            })
            .collect();

        let (remote, remote_handle) = async move {
            match tasks.next().await {
                None => ChannelTaskError {
                    type_name: "none",
                    error: "no channel tasks to run".to_owned().into(),
                },
                Some(Ok(())) => panic!("channel tasks only return errors"),
                Some(Err(err)) => err,
            }
        }
        .remote_handle();
        runtime.spawn(remote);

        MessageChannels {
            disconnected: false,
            task: remote_handle,
            channels: channels_map,
        }
    }
}

#[derive(Debug, Error)]
#[error("no such message type `{0}` registered")]
pub struct MessageTypeUnregistered(&'static str);

#[derive(Debug, Error)]
#[error("`MessageChannels` instance has become disconnected")]
pub struct MessageChannelsDisconnected;

#[derive(Debug, Error)]
pub enum TryAsyncMessageError {
    #[error(transparent)]
    Unregistered(#[from] MessageTypeUnregistered),
    #[error(transparent)]
    Disconnected(#[from] MessageChannelsDisconnected),
}

/// Manages a set of channels through a packet multiplexer, where each channel is associated with
/// exactly one message type.
///
/// Acts as a bridge between the sync and async worlds. Provides sync methods to send and receive
/// messages that do not block or error. Has simplified error handling, is if any of the backing
/// tasks end in an error or if the backing packet channels are dropped, the `MessageChannels` will
/// permanently go into a "disconnected" state.
///
/// Additionally still provides async versions of methods to send and receive messages that share
/// the same simplified error handling, which may be useful during startup or shutdown.
#[derive(Debug)]
pub struct MessageChannels {
    disconnected: bool,
    task: RemoteHandle<ChannelTaskError>,
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
    /// disconnected. If it is not disconnected, it will become disconnected by calling this and
    /// return that error.
    pub async fn recv_err(self) -> ChannelTaskError {
        drop(self.channels);
        self.task.await
    }

    /// Send the given message on the channel associated with its message type.
    ///
    /// In order to ensure delivery, `flush` should be called for the same message type to
    /// immediately send any buffered messages.
    ///
    /// If the spsc channel for this message type is full, will return the message that was sent
    /// back to the caller. If the message was successfully put onto the outgoing spsc channel, will
    /// return None.
    ///
    /// # Panics
    /// Panics if this message type was not registered with the `MessageChannelsBuilder` used to
    /// build this `MessageChannels` instance.
    pub fn send<M: ChannelMessage>(&mut self, message: M) -> Option<M> {
        self.try_send(message).unwrap()
    }

    /// Like `MessageChannels::send` but errors instead of panicking when the message type is
    /// unregistered.
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

    /// Any async version of `MessageChannels::send`, sends the given message on the
    /// channel associated with its message type but waits if the channel is full. Like
    /// `MessageChannels::send`, `MessageChannels::flush` must still be called afterwards in order
    /// to ensure delivery.
    ///
    /// This method is cancel safe, it will never partially send a message, though canceling it may
    /// or may not buffer a message to be sent.
    ///
    /// # Panics
    /// Panics if this message type is not registered.
    pub async fn async_send<M: ChannelMessage>(
        &mut self,
        message: M,
    ) -> Result<(), MessageChannelsDisconnected> {
        self.try_async_send(message).await.map_err(|e| match e {
            TryAsyncMessageError::Unregistered(e) => panic!("{}", e),
            TryAsyncMessageError::Disconnected(e) => e,
        })
    }

    /// Like `MessageChannels::async_send` but errors instead of panicking when the message type is
    /// unregistered.
    pub async fn try_async_send<M: ChannelMessage>(
        &mut self,
        message: M,
    ) -> Result<(), TryAsyncMessageError> {
        let channels = self.channels.get_mut::<M>()?;

        if self.disconnected {
            Err(MessageChannelsDisconnected.into())
        } else {
            let res = channels.outgoing_sender.send(message).await;

            if res.is_err() {
                self.disconnected = true;
                Err(MessageChannelsDisconnected.into())
            } else {
                Ok(())
            }
        }
    }

    /// Immediately send any buffered messages for this message type. Messages may not be delivered
    /// unless `flush` is called after any `send` calls.
    ///
    /// # Panics
    /// Panics if this message type was not registered with the `MessageChannelsBuilder` used to
    /// build this `MessageChannels` instance.
    pub fn flush<M: ChannelMessage>(&mut self) {
        self.try_flush::<M>().unwrap();
    }

    /// Like `MessageChannels::flush` but errors instead of panicking when the message type is
    /// unregistered.
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

    /// Like `MessageChannels::recv` but errors instead of panicking when the message type is
    /// unregistered.
    pub fn try_recv<M: ChannelMessage>(&mut self) -> Result<Option<M>, MessageTypeUnregistered> {
        let channels = self.channels.get_mut::<M>()?;

        Ok(if self.disconnected {
            None
        } else {
            match channels.incoming_receiver.try_recv() {
                Ok(msg) => Some(msg),
                Err(err) => {
                    if err.is_disconnected() {
                        self.disconnected = true;
                    }
                    None
                }
            }
        })
    }

    /// Any async version of `MessageChannels::receive`, receives an incoming message on the channel
    /// associated with its message type but waits if there is no message available.
    ///
    /// This method is cancel safe, it will never partially read a message or drop received
    /// messages.
    ///
    /// # Panics
    /// Panics if this message type is not registered.
    pub async fn async_recv<M: ChannelMessage>(
        &mut self,
    ) -> Result<M, MessageChannelsDisconnected> {
        self.try_async_recv().await.map_err(|e| match e {
            TryAsyncMessageError::Unregistered(e) => panic!("{}", e),
            TryAsyncMessageError::Disconnected(e) => e,
        })
    }

    /// Like `MessageChannels::async_recv` but errors instead of panicking when the message type is
    /// unregistered.
    pub async fn try_async_recv<M: ChannelMessage>(&mut self) -> Result<M, TryAsyncMessageError> {
        let channels = self.channels.get_mut::<M>()?;

        if self.disconnected {
            Err(MessageChannelsDisconnected.into())
        } else if let Some(message) = channels.incoming_receiver.next().await {
            Ok(message)
        } else {
            self.disconnected = true;
            Err(MessageChannelsDisconnected.into())
        }
    }

    pub fn statistics<M: ChannelMessage>(&self) -> &ChannelStatistics {
        self.try_statistics::<M>().unwrap()
    }

    pub fn try_statistics<M: ChannelMessage>(
        &self,
    ) -> Result<&ChannelStatistics, MessageTypeUnregistered> {
        Ok(&self.channels.get::<M>()?.statistics)
    }
}

type ChannelTask = BoxFuture<'static, Result<(), TaskError>>;
type RegisterFn<R, P> = fn(
    MessageChannelSettings,
    R,
    P,
    &mut PacketMultiplexer<<P as PacketPool>::Packet>,
    &mut ChannelsMap,
) -> ChannelTask;

#[derive(Debug, Error)]
#[error("channel has been disconnected")]
struct ChannelDisconnected;

struct ChannelSet<M> {
    outgoing_sender: spsc::Sender<M>,
    incoming_receiver: spsc::Receiver<M>,
    flush_sender: event_watch::Sender,
    statistics: ChannelStatistics,
}

#[derive(Debug, Default)]
struct ChannelsMap(FxHashMap<TypeId, Box<dyn Any + Send + Sync>>);

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
            .ok_or_else(|| MessageTypeUnregistered(type_name::<M>()))?
            .downcast_ref()
            .unwrap())
    }

    fn get_mut<M: ChannelMessage>(
        &mut self,
    ) -> Result<&mut ChannelSet<M>, MessageTypeUnregistered> {
        Ok(self
            .0
            .get_mut(&TypeId::of::<M>())
            .ok_or_else(|| MessageTypeUnregistered(type_name::<M>()))?
            .downcast_mut()
            .unwrap())
    }
}

fn register_message_type<R, P, M>(
    settings: MessageChannelSettings,
    runtime: R,
    packet_pool: P,
    multiplexer: &mut PacketMultiplexer<P::Packet>,
    channels_map: &mut ChannelsMap,
) -> ChannelTask
where
    R: Runtime + Clone + 'static,
    P: PacketPool + Clone + Send + 'static,
    P::Packet: Send,
    M: ChannelMessage,
{
    let (incoming_sender, incoming_receiver) = spsc::channel::<M>(settings.message_buffer_size);
    let (outgoing_sender, outgoing_receiver) = spsc::channel::<M>(settings.message_buffer_size);

    let (flush_sender, flush_receiver) = event_watch::channel();

    let (channel_sender, channel_receiver, statistics) = multiplexer
        .open_channel(settings.channel, settings.packet_buffer_size)
        .expect("duplicate packet channel");

    let packet_pool = MuxPacketPool::new(packet_pool);

    let channel_task = match settings.channel_mode {
        MessageChannelMode::Unreliable {
            settings: unreliable_settings,
            max_message_len,
        } => channel_task(
            UnreliableTypedChannel::new(UnreliableBincodeChannel::new(
                UnreliableChannel::new(
                    runtime,
                    packet_pool,
                    unreliable_settings,
                    channel_sender,
                    channel_receiver,
                ),
                max_message_len,
            )),
            incoming_sender,
            outgoing_receiver,
            flush_receiver,
        )
        .boxed(),
        MessageChannelMode::Reliable {
            settings: reliable_settings,
            max_message_len,
        } => channel_task(
            ReliableTypedChannel::new(ReliableBincodeChannel::new(
                ReliableChannel::new(
                    runtime,
                    packet_pool,
                    reliable_settings,
                    channel_sender,
                    channel_receiver,
                ),
                max_message_len,
            )),
            incoming_sender,
            outgoing_receiver,
            flush_receiver,
        )
        .boxed(),
        MessageChannelMode::Compressed {
            settings: reliable_settings,
            max_chunk_len,
        } => channel_task(
            CompressedTypedChannel::new(CompressedBincodeChannel::new(
                ReliableChannel::new(
                    runtime,
                    packet_pool,
                    reliable_settings,
                    channel_sender,
                    channel_receiver,
                ),
                max_chunk_len,
            )),
            incoming_sender,
            outgoing_receiver,
            flush_receiver,
        )
        .boxed(),
    };

    channels_map.insert(ChannelSet::<M> {
        outgoing_sender,
        flush_sender,
        incoming_receiver,
        statistics,
    });

    channel_task
}

trait MessageBincodeChannel<M: ChannelMessage> {
    fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<M, TaskError>>;
    fn poll_send(&mut self, cx: &mut Context, msg: &M) -> Poll<Result<(), TaskError>>;
    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), TaskError>>;
}

impl<M, R, P> MessageBincodeChannel<M> for UnreliableTypedChannel<M, R, P>
where
    M: ChannelMessage,
    R: Runtime,
    P: PacketPool,
{
    fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<M, TaskError>> {
        UnreliableTypedChannel::poll_recv(self, cx).map_err(|e| e.into())
    }

    fn poll_send(&mut self, cx: &mut Context, msg: &M) -> Poll<Result<(), TaskError>> {
        ready!(self.poll_send_ready(cx))?;
        Poll::Ready(Ok(self.start_send(msg)?))
    }

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), TaskError>> {
        UnreliableTypedChannel::poll_flush(self, cx).map_err(|e| e.into())
    }
}

impl<M: ChannelMessage> MessageBincodeChannel<M> for ReliableTypedChannel<M> {
    fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<M, TaskError>> {
        ReliableTypedChannel::poll_recv(self, cx).map_err(|e| e.into())
    }

    fn poll_send(&mut self, cx: &mut Context, msg: &M) -> Poll<Result<(), TaskError>> {
        ready!(self.poll_send_ready(cx))?;
        Poll::Ready(Ok(self.start_send(msg)?))
    }

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), TaskError>> {
        ReliableTypedChannel::poll_flush(self, cx).map_err(|e| e.into())
    }
}

impl<M: ChannelMessage> MessageBincodeChannel<M> for CompressedTypedChannel<M> {
    fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<M, TaskError>> {
        CompressedTypedChannel::poll_recv(self, cx).map_err(|e| e.into())
    }

    fn poll_send(&mut self, cx: &mut Context, msg: &M) -> Poll<Result<(), TaskError>> {
        CompressedTypedChannel::poll_send(self, cx, msg).map_err(|e| e.into())
    }

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), TaskError>> {
        CompressedTypedChannel::poll_flush(self, cx).map_err(|e| e.into())
    }
}

async fn channel_task<M: ChannelMessage>(
    mut channel: impl MessageBincodeChannel<M>,
    mut incoming_message_sender: spsc::Sender<M>,
    mut outgoing_message_receiver: spsc::Receiver<M>,
    mut flush_receiver: event_watch::Receiver,
) -> Result<(), TaskError> {
    enum Next<M> {
        Incoming(M),
        Outgoing(M),
        Flush,
    }

    loop {
        let next = {
            select! {
                incoming = future::poll_fn(|cx| channel.poll_recv(cx)).fuse() => {
                    Next::Incoming(incoming?)
                }
                outgoing = outgoing_message_receiver.next().fuse() => {
                    Next::Outgoing(outgoing.ok_or(ChannelDisconnected)?)
                }
                _ = flush_receiver.wait().fuse() => Next::Flush,
            }
        };

        match next {
            Next::Incoming(incoming) => incoming_message_sender.send(incoming).await?,
            Next::Outgoing(outgoing) => {
                future::poll_fn(|cx| channel.poll_send(cx, &outgoing)).await?
            }
            Next::Flush => loop {
                match outgoing_message_receiver.try_recv() {
                    Ok(outgoing) => future::poll_fn(|cx| channel.poll_send(cx, &outgoing)).await?,
                    Err(TryRecvError::Disconnected) => return Err(ChannelDisconnected.into()),
                    Err(TryRecvError::Empty) => {
                        future::poll_fn(|cx| channel.poll_flush(cx)).await?;
                        break;
                    }
                }
            },
        }
    }
}
