use std::{
    collections::{hash_map, HashMap, HashSet},
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

use crate::packet::{Packet, PacketPool, MAX_PACKET_LEN};

pub const MAX_MUX_PACKET_LEN: usize = MAX_PACKET_LEN - 1;

pub type PacketChannel = u8;

/// A packet which has reserved space for a channel header
#[derive(Debug)]
pub struct MuxPacket(Packet);

impl MuxPacket {
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

impl Deref for MuxPacket {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0[1..]
    }
}

impl DerefMut for MuxPacket {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0[1..]
    }
}

#[derive(Clone, Debug)]
pub struct MuxPacketPool(PacketPool);

impl MuxPacketPool {
    pub fn new() -> MuxPacketPool {
        MuxPacketPool(PacketPool::new())
    }

    pub fn acquire(&self) -> MuxPacket {
        let mut packet = self.0.acquire();
        packet.resize(1, 0);
        MuxPacket(packet)
    }

    pub fn acquire_max(&self) -> MuxPacket {
        let mut packet = self.0.acquire_max();
        packet[0] = 0;
        MuxPacket(packet)
    }
}

impl From<PacketPool> for MuxPacketPool {
    fn from(pool: PacketPool) -> MuxPacketPool {
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
pub struct PacketMultiplexer {
    incoming: HashMap<PacketChannel, ChannelSender>,
    outgoing: SelectAll<ChannelReceiver>,
}

impl PacketMultiplexer {
    pub fn new() -> PacketMultiplexer {
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
    ) -> Result<(Sender<MuxPacket>, Receiver<MuxPacket>, ChannelStatistics), DuplicateChannel> {
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
    pub fn start(self) -> (IncomingMultiplexedPackets, OutgoingMultiplexedPackets) {
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

#[derive(Debug, Error)]
pub enum IncomingTrySendError {
    #[error("packet channel is full")]
    IsFull(Packet),
    #[error(transparent)]
    Error(#[from] IncomingError),
}

impl IncomingTrySendError {
    pub fn is_full(&self) -> bool {
        match self {
            IncomingTrySendError::IsFull(_) => true,
            _ => false,
        }
    }
}

pub struct IncomingMultiplexedPackets {
    incoming: HashMap<PacketChannel, ChannelSender>,
    to_send: Option<Packet>,
    to_flush: HashSet<PacketChannel>,
}

impl IncomingMultiplexedPackets {
    /// Attempt to send the given packet to the appropriate multiplexed channel.
    ///
    /// If a normal error occurs, returns `IncomingError::Error`, if the destination channel buffer
    /// is full, returns `IncomingTrySendError::IsFull`.
    pub fn try_send(&mut self, packet: Packet) -> Result<(), IncomingTrySendError> {
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

impl Sink<Packet> for IncomingMultiplexedPackets {
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

    fn start_send(mut self: Pin<&mut Self>, item: Packet) -> Result<(), Self::Error> {
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

pub struct OutgoingMultiplexedPackets {
    outgoing: SelectAll<ChannelReceiver>,
}

impl Stream for OutgoingMultiplexedPackets {
    type Item = Packet;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.outgoing).poll_next(cx)
    }
}

struct ChannelSender {
    sender: Sender<MuxPacket>,
    statistics: Arc<ChannelStatisticsData>,
}

struct ChannelReceiver {
    channel: PacketChannel,
    receiver: Receiver<MuxPacket>,
    statistics: Arc<ChannelStatisticsData>,
}

impl Stream for ChannelReceiver {
    type Item = Packet;

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

#[cfg(test)]
mod tests {
    use super::*;

    use futures::{
        executor::LocalPool,
        future::{self, Either},
        task::SpawnExt,
        SinkExt, StreamExt,
    };

    #[test]
    fn test_multiplexer() {
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        let mut multiplexer_a = PacketMultiplexer::new();
        let (mut sender4a, mut receiver4a, _) = multiplexer_a.open_channel(4, 8).unwrap();
        let (mut sender32a, mut receiver32a, _) = multiplexer_a.open_channel(32, 8).unwrap();

        let mut multiplexer_b = PacketMultiplexer::new();
        let (mut sender4b, mut receiver4b, _) = multiplexer_b.open_channel(4, 8).unwrap();
        let (mut sender32b, mut receiver32b, _) = multiplexer_b.open_channel(32, 8).unwrap();

        spawner
            .spawn(async move {
                let (mut a_incoming, mut a_outgoing) = multiplexer_a.start();
                let (mut b_incoming, mut b_outgoing) = multiplexer_b.start();
                loop {
                    match future::select(a_outgoing.next(), b_outgoing.next()).await {
                        Either::Left((Some(packet), _)) => {
                            b_incoming.send(packet).await.unwrap();
                        }
                        Either::Right((Some(packet), _)) => {
                            a_incoming.send(packet).await.unwrap();
                        }
                        Either::Left((None, _)) | Either::Right((None, _)) => break,
                    }
                }
            })
            .unwrap();

        spawner
            .spawn(async move {
                let packet_pool = MuxPacketPool::new();

                let mut packet = packet_pool.acquire_max();
                packet[0] = 17;
                sender4a.send(packet).await.unwrap();

                let mut packet = packet_pool.acquire_max();
                packet[0] = 18;
                sender4b.send(packet).await.unwrap();

                let mut packet = packet_pool.acquire_max();
                packet[0] = 19;
                sender32a.send(packet).await.unwrap();

                let mut packet = packet_pool.acquire_max();
                packet[0] = 20;
                sender32b.send(packet).await.unwrap();

                let packet = receiver4a.next().await.unwrap();
                assert_eq!(packet[0], 18);

                let packet = receiver4b.next().await.unwrap();
                assert_eq!(packet[0], 17);

                let packet = receiver32a.next().await.unwrap();
                assert_eq!(packet[0], 20);

                let packet = receiver32b.next().await.unwrap();
                assert_eq!(packet[0], 19);
            })
            .unwrap();

        pool.run();
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
