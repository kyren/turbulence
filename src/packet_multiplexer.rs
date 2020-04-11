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

use crate::packet::{Packet, PacketPool};

pub type PacketChannel = u8;

/// A wrapper over a `Packet` that reserves the first byte for the channel.
#[derive(Debug)]
pub struct MuxPacket<P>(P);

impl<P> Packet for MuxPacket<P>
where
    P: Packet,
{
    fn capacity(&self) -> usize {
        self.0.capacity() - 1
    }

    fn resize(&mut self, len: usize, val: u8) {
        self.0.resize(len + 1, val);
    }
}

impl<P> Deref for MuxPacket<P>
where
    P: Packet,
{
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0[1..]
    }
}

impl<P> DerefMut for MuxPacket<P>
where
    P: Packet,
{
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0[1..]
    }
}

#[derive(Clone, Debug)]
pub struct MuxPacketPool<P>(P);

impl<P> MuxPacketPool<P> {
    pub fn new(packet_pool: P) -> Self {
        MuxPacketPool(packet_pool)
    }
}

impl<P> PacketPool for MuxPacketPool<P>
where
    P: PacketPool,
{
    type Packet = MuxPacket<P::Packet>;

    fn acquire(&self) -> MuxPacket<P::Packet> {
        let mut packet = self.0.acquire();
        packet.resize(1, 0);
        MuxPacket(packet)
    }
}

impl<P> From<P> for MuxPacketPool<P> {
    fn from(pool: P) -> MuxPacketPool<P> {
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
///
/// Also monitors bandwidth on each channel independently, and returns a `ChannelStatistics` handle
/// to query bandwidth totals for that specific channel.
pub struct PacketMultiplexer<P> {
    incoming: HashMap<PacketChannel, ChannelSender<P>>,
    outgoing: SelectAll<ChannelReceiver<P>>,
}

impl<P> PacketMultiplexer<P>
where
    P: Packet + Unpin,
{
    pub fn new() -> PacketMultiplexer<P> {
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
            Sender<MuxPacket<P>>,
            Receiver<MuxPacket<P>>,
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
    pub fn start(self) -> (IncomingMultiplexedPackets<P>, OutgoingMultiplexedPackets<P>) {
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
pub enum IncomingTrySendError<P> {
    #[error("packet channel is full")]
    IsFull(P),
    #[error(transparent)]
    Error(#[from] IncomingError),
}

impl<P> fmt::Debug for IncomingTrySendError<P> {
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

impl<P> IncomingTrySendError<P> {
    pub fn is_full(&self) -> bool {
        match self {
            IncomingTrySendError::IsFull(_) => true,
            _ => false,
        }
    }
}

pub struct IncomingMultiplexedPackets<P> {
    incoming: HashMap<PacketChannel, ChannelSender<P>>,
    to_send: Option<P>,
    to_flush: HashSet<PacketChannel>,
}

impl<P> IncomingMultiplexedPackets<P>
where
    P: Packet + Unpin,
{
    /// Attempt to send the given packet to the appropriate multiplexed channel.
    ///
    /// If a normal error occurs, returns `IncomingError::Error`, if the destination channel buffer
    /// is full, returns `IncomingTrySendError::IsFull`.
    pub fn try_send(&mut self, packet: P) -> Result<(), IncomingTrySendError<P>> {
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

impl<P> Sink<P> for IncomingMultiplexedPackets<P>
where
    P: Packet + Unpin,
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

    fn start_send(mut self: Pin<&mut Self>, item: P) -> Result<(), Self::Error> {
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

pub struct OutgoingMultiplexedPackets<P> {
    outgoing: SelectAll<ChannelReceiver<P>>,
}

impl<P> Stream for OutgoingMultiplexedPackets<P>
where
    P: Packet + Unpin,
{
    type Item = P;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.outgoing).poll_next(cx)
    }
}

struct ChannelSender<P> {
    sender: Sender<MuxPacket<P>>,
    statistics: Arc<ChannelStatisticsData>,
}

struct ChannelReceiver<P> {
    channel: PacketChannel,
    receiver: Receiver<MuxPacket<P>>,
    statistics: Arc<ChannelStatisticsData>,
}

impl<P> Stream for ChannelReceiver<P>
where
    P: Packet + Unpin,
{
    type Item = P;

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
