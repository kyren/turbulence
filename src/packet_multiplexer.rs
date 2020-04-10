use std::{
    collections::{hash_map, HashMap, HashSet},
    fmt,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    u8,
};

use futures::{
    channel::mpsc::{self, Receiver, Sender},
    stream::SelectAll,
    Sink, Stream,
};
use thiserror::Error;

use crate::packet::{BufferPool, Packet, PacketPool};

pub type PacketChannel = u8;

/// A wrapper over a `Packet` that reserves the first byte for the channel.
#[derive(Debug)]
pub struct MuxPacket<B>(Packet<B>);

impl<B> MuxPacket<B>
where
    B: Deref<Target = [u8]> + DerefMut,
{
    pub fn capacity(&self) -> usize {
        self.0.capacity() - 1
    }

    pub fn clear(&mut self) {
        self.0.truncate(1);
    }

    pub fn resize(&mut self, len: usize, val: u8) {
        self.0.resize(len + 1, val);
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len + 1);
    }

    pub fn extend(&mut self, other: &[u8]) {
        self.0.extend(other);
    }

    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.0.as_mut_slice()
    }
}

impl<B> Deref for MuxPacket<B>
where
    B: Deref<Target = [u8]>,
{
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0[1..]
    }
}

impl<B> DerefMut for MuxPacket<B>
where
    B: Deref<Target = [u8]> + DerefMut,
{
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0[1..]
    }
}

#[derive(Clone, Debug)]
pub struct MuxPacketPool<B>(PacketPool<B>);

impl<B> MuxPacketPool<B> {
    pub fn new(buffer_pool: B) -> Self {
        MuxPacketPool(PacketPool::new(buffer_pool))
    }
}

impl<B: BufferPool> MuxPacketPool<B> {
    pub fn acquire(&self) -> MuxPacket<B::Buffer> {
        let mut packet = self.0.acquire();
        packet.resize(1, 0);
        MuxPacket(packet)
    }
}

impl<B> From<PacketPool<B>> for MuxPacketPool<B> {
    fn from(pool: PacketPool<B>) -> MuxPacketPool<B> {
        MuxPacketPool(pool)
    }
}

#[derive(Debug, Error)]
#[error("packet channel has already been opened")]
pub struct DuplicateChannel;

#[derive(Copy, Clone, Debug)]
pub struct ChannelTotals {
    pub packets: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct ChannelStatistics(Arc<ChannelStatisticsData>);

impl ChannelStatistics {
    pub fn incoming_totals(&self) -> ChannelTotals {
        ChannelTotals {
            packets: self.0.incoming_packets.load(Ordering::Relaxed),
            bytes: self.0.incoming_bytes.load(Ordering::Relaxed),
        }
    }

    pub fn outgoing_totals(&self) -> ChannelTotals {
        ChannelTotals {
            packets: self.0.outgoing_packets.load(Ordering::Relaxed),
            bytes: self.0.outgoing_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Routes packets marked with a channel header from a single `Sink` / `Stream` pair to a set of
/// `Sink` / `Stream` pairs for each channel.
pub struct PacketMultiplexer<B> {
    incoming: HashMap<PacketChannel, ChannelSender<B>>,
    outgoing: SelectAll<ChannelReceiver<B>>,
}

impl<B> PacketMultiplexer<B>
where
    B: Deref<Target = [u8]> + DerefMut + Unpin,
{
    pub fn new() -> PacketMultiplexer<B> {
        PacketMultiplexer {
            incoming: HashMap::new(),
            outgoing: SelectAll::new(),
        }
    }

    /// Open a multiplexed packet channel, producing a sender for outgoing `MuxPacket`s on this
    /// channel, and a receiver for incoming `MuxPacket`s on this channel.
    ///
    /// The `buffer_size` parameter controls the buffer size requested when creating the MPSC
    /// futures channels for the returned `Sender` and `Receiver`.
    pub fn open_channel(
        &mut self,
        channel: PacketChannel,
        buffer_size: usize,
    ) -> Result<
        (
            Sender<MuxPacket<B>>,
            Receiver<MuxPacket<B>>,
            ChannelStatistics,
        ),
        DuplicateChannel,
    > {
        let statistics = Arc::new(ChannelStatisticsData::default());
        match self.incoming.entry(channel) {
            hash_map::Entry::Occupied(_) => Err(DuplicateChannel),
            hash_map::Entry::Vacant(vacant) => {
                let (incoming_sender, incoming_receiver) = mpsc::channel(buffer_size);
                let (outgoing_sender, outgoing_receiver) = mpsc::channel(buffer_size);
                vacant.insert(ChannelSender {
                    sender: incoming_sender,
                    statistics: Arc::clone(&statistics),
                });
                self.outgoing.push(ChannelReceiver {
                    channel,
                    receiver: outgoing_receiver,
                    statistics: Arc::clone(&statistics),
                });
                Ok((
                    outgoing_sender,
                    incoming_receiver,
                    ChannelStatistics(statistics),
                ))
            }
        }
    }

    /// Start multiplexing packets to all opened channels.
    ///
    /// Returns an `IncomingMultiplexedPackets` which is a `Sink` for incoming packets, and an
    /// `OutgoingMultiplexedPackets` which is a `Stream` for outgoing packets.
    pub fn start(self) -> (IncomingMultiplexedPackets<B>, OutgoingMultiplexedPackets<B>) {
        (
            IncomingMultiplexedPackets {
                incoming: self.incoming,
                to_send: None,
                to_flush: HashSet::new(),
            },
            OutgoingMultiplexedPackets {
                outgoing: self.outgoing,
            },
        )
    }
}

#[derive(Debug, Error)]
pub enum IncomingError {
    #[error("packet received for unopened channel")]
    UnknownPacketChannel,
    #[error("channel receiver has been dropped")]
    ChannelReceiverDropped,
}

#[derive(Error)]
pub enum IncomingTrySendError<B> {
    #[error("packet channel is full")]
    IsFull(Packet<B>),
    #[error(transparent)]
    Error(#[from] IncomingError),
}

impl<B> fmt::Debug for IncomingTrySendError<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IncomingTrySendError::IsFull(_) => write!(f, "IncomingTrySendError::IsFull"),
            IncomingTrySendError::Error(err) => f
                .debug_tuple("IncomingTrySendError::Error")
                .field(err)
                .finish(),
        }
    }
}

impl<B> IncomingTrySendError<B> {
    pub fn is_full(&self) -> bool {
        match self {
            IncomingTrySendError::IsFull(_) => true,
            _ => false,
        }
    }
}

pub struct IncomingMultiplexedPackets<B> {
    incoming: HashMap<PacketChannel, ChannelSender<B>>,
    to_send: Option<Packet<B>>,
    to_flush: HashSet<PacketChannel>,
}

impl<B> IncomingMultiplexedPackets<B>
where
    B: Deref<Target = [u8]> + Unpin,
{
    /// Attempt to send the given packet to the appropriate multiplexed channel.
    ///
    /// If a normal error occurs, returns `IncomingError::Error`, if the destination channel buffer
    /// is full, returns `IncomingTrySendError::IsFull`.
    pub fn try_send(&mut self, packet: Packet<B>) -> Result<(), IncomingTrySendError<B>> {
        let channel = packet[0];
        let incoming = self
            .incoming
            .get_mut(&channel)
            .ok_or(IncomingError::UnknownPacketChannel)?;

        let mux_packet_len = (packet.len() - 1) as u64;
        incoming.sender.try_send(MuxPacket(packet)).map_err(|e| {
            if e.is_full() {
                IncomingTrySendError::IsFull(e.into_inner().0)
            } else {
                IncomingError::ChannelReceiverDropped.into()
            }
        })?;
        incoming.statistics.mark_incoming_packet(mux_packet_len);

        Ok(())
    }
}

impl<B> Sink<Packet<B>> for IncomingMultiplexedPackets<B>
where
    B: Deref<Target = [u8]> + Unpin,
{
    type Error = IncomingError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        if let Some(packet) = self.to_send.take() {
            let channel = packet[0];
            let incoming = &mut self
                .incoming
                .get_mut(&channel)
                .ok_or(IncomingError::UnknownPacketChannel)?;
            match incoming.sender.poll_ready(cx) {
                Poll::Pending => {
                    self.to_send = Some(packet);
                    Poll::Pending
                }
                Poll::Ready(Ok(())) => {
                    let mux_packet_len = (packet.len() - 1) as u64;
                    incoming
                        .sender
                        .start_send(MuxPacket(packet))
                        .map_err(|_| IncomingError::ChannelReceiverDropped)?;
                    incoming.statistics.mark_incoming_packet(mux_packet_len);
                    self.to_flush.insert(channel);
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(_)) => Poll::Ready(Err(IncomingError::ChannelReceiverDropped)),
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: Packet<B>) -> Result<(), Self::Error> {
        assert!(self.to_send.is_none());
        self.to_send = Some(item);
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        if self.as_mut().poll_ready(cx)?.is_pending() {
            return Poll::Pending;
        }
        while let Some(&channel) = self.to_flush.iter().next() {
            let sender = Pin::new(
                &mut self
                    .incoming
                    .get_mut(&channel)
                    .ok_or(IncomingError::UnknownPacketChannel)?
                    .sender,
            );
            if sender
                .poll_flush(cx)
                .map_err(|_| IncomingError::ChannelReceiverDropped)?
                .is_pending()
            {
                return Poll::Pending;
            }
            self.to_flush.remove(&channel);
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.poll_flush(cx)
    }
}

pub struct OutgoingMultiplexedPackets<B> {
    outgoing: SelectAll<ChannelReceiver<B>>,
}

impl<B> Stream for OutgoingMultiplexedPackets<B>
where
    B: Deref<Target = [u8]> + DerefMut + Unpin,
{
    type Item = Packet<B>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.outgoing).poll_next(cx)
    }
}

struct ChannelSender<B> {
    sender: Sender<MuxPacket<B>>,
    statistics: Arc<ChannelStatisticsData>,
}

struct ChannelReceiver<B> {
    channel: PacketChannel,
    receiver: Receiver<MuxPacket<B>>,
    statistics: Arc<ChannelStatisticsData>,
}

impl<B> Stream for ChannelReceiver<B>
where
    B: Deref<Target = [u8]> + DerefMut + Unpin,
{
    type Item = Packet<B>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.receiver).poll_next(cx) {
            Poll::Ready(Some(packet)) => {
                let mut packet = packet.0;
                packet[0] = self.channel;
                self.statistics
                    .mark_outgoing_packet((packet.len() - 1) as u64);
                Poll::Ready(Some(packet))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug, Default)]
struct ChannelStatisticsData {
    incoming_packets: AtomicU64,
    incoming_bytes: AtomicU64,

    outgoing_packets: AtomicU64,
    outgoing_bytes: AtomicU64,
}

impl ChannelStatisticsData {
    fn mark_incoming_packet(&self, len: u64) {
        self.incoming_packets.fetch_add(1, Ordering::Relaxed);
        self.incoming_bytes.fetch_add(len, Ordering::Relaxed);
    }

    fn mark_outgoing_packet(&self, len: u64) {
        self.outgoing_packets.fetch_add(1, Ordering::Relaxed);
        self.outgoing_bytes.fetch_add(len, Ordering::Relaxed);
    }
}
