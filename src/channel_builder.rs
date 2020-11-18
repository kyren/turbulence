use crate::{
    compressed_bincode_channel::{CompressedBincodeChannel, CompressedTypedChannel},
    packet::PacketPool,
    packet_multiplexer::{
        ChannelStatistics, DuplicateChannel, MuxPacketPool, PacketChannel, PacketMultiplexer,
    },
    reliable_bincode_channel::{ReliableBincodeChannel, ReliableTypedChannel},
    reliable_channel::{self, ReliableChannel},
    runtime::Runtime,
    unreliable_bincode_channel::{UnreliableBincodeChannel, UnreliableTypedChannel},
    unreliable_channel::UnreliableChannel,
};

/// Helper that allows for easily opening different channel types on a `PacketMultiplexer`.
///
/// Contains a `MuxPacketPool` and a `Runtime` implemenentation that is used for each created
/// channel.
pub struct ChannelBuilder<R, P> {
    pub runtime: R,
    pub pool: MuxPacketPool<P>,
}

impl<R, P> ChannelBuilder<R, P>
where
    R: Runtime + 'static,
    P: PacketPool + Clone + Send + 'static,
    P::Packet: Unpin + Send,
{
    pub fn new(runtime: R, pool: P) -> Self {
        ChannelBuilder {
            runtime,
            pool: MuxPacketPool::new(pool),
        }
    }

    pub fn open_unreliable_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer<P::Packet>,
        channel: PacketChannel,
        buffer_size: usize,
    ) -> Result<(UnreliableChannel<MuxPacketPool<P>>, ChannelStatistics), DuplicateChannel> {
        let (sender, receiver, statistics) = multiplexer.open_channel(channel, buffer_size)?;
        Ok((
            UnreliableChannel::new(self.pool.clone(), receiver, sender),
            statistics,
        ))
    }

    pub fn open_unreliable_bincode_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer<P::Packet>,
        channel: PacketChannel,
        buffer_size: usize,
    ) -> Result<
        (
            UnreliableBincodeChannel<MuxPacketPool<P>>,
            ChannelStatistics,
        ),
        DuplicateChannel,
    > {
        let (sender, receiver, statistics) = multiplexer.open_channel(channel, buffer_size)?;
        Ok((
            UnreliableBincodeChannel::new(self.pool.clone(), receiver, sender),
            statistics,
        ))
    }

    pub fn open_unreliable_typed_channel<M>(
        &mut self,
        multiplexer: &mut PacketMultiplexer<P::Packet>,
        channel: PacketChannel,
        buffer_size: usize,
    ) -> Result<
        (
            UnreliableTypedChannel<M, MuxPacketPool<P>>,
            ChannelStatistics,
        ),
        DuplicateChannel,
    > {
        let (sender, receiver, statistics) = multiplexer.open_channel(channel, buffer_size)?;
        Ok((
            UnreliableTypedChannel::new(self.pool.clone(), receiver, sender),
            statistics,
        ))
    }

    pub fn open_reliable_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer<P::Packet>,
        channel: PacketChannel,
        buffer_size: usize,
        settings: reliable_channel::Settings,
    ) -> Result<(ReliableChannel, ChannelStatistics), DuplicateChannel> {
        let (sender, receiver, statistics) = multiplexer.open_channel(channel, buffer_size)?;
        Ok((
            ReliableChannel::new(
                self.runtime.clone(),
                self.pool.clone(),
                settings,
                receiver,
                sender,
            ),
            statistics,
        ))
    }

    pub fn open_reliable_bincode_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer<P::Packet>,
        channel: PacketChannel,
        buffer_size: usize,
        reliability_settings: reliable_channel::Settings,
        max_message_len: u16,
    ) -> Result<(ReliableBincodeChannel, ChannelStatistics), DuplicateChannel> {
        let (channel, statistics) =
            self.open_reliable_channel(multiplexer, channel, buffer_size, reliability_settings)?;
        Ok((
            ReliableBincodeChannel::new(channel, max_message_len),
            statistics,
        ))
    }

    pub fn open_reliable_typed_channel<M>(
        &mut self,
        multiplexer: &mut PacketMultiplexer<P::Packet>,
        channel: PacketChannel,
        buffer_size: usize,
        reliability_settings: reliable_channel::Settings,
        max_message_len: u16,
    ) -> Result<(ReliableTypedChannel<M>, ChannelStatistics), DuplicateChannel> {
        let (channel, statistics) = self.open_reliable_bincode_channel(
            multiplexer,
            channel,
            buffer_size,
            reliability_settings,
            max_message_len,
        )?;
        Ok((ReliableTypedChannel::new(channel), statistics))
    }

    pub fn open_compressed_bincode_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer<P::Packet>,
        channel: PacketChannel,
        buffer_size: usize,
        reliability_settings: reliable_channel::Settings,
        max_chunk_len: u16,
    ) -> Result<(CompressedBincodeChannel, ChannelStatistics), DuplicateChannel> {
        let (channel, statistics) =
            self.open_reliable_channel(multiplexer, channel, buffer_size, reliability_settings)?;
        Ok((
            CompressedBincodeChannel::new(channel, max_chunk_len),
            statistics,
        ))
    }

    pub fn open_compressed_typed_channel<M>(
        &mut self,
        multiplexer: &mut PacketMultiplexer<P::Packet>,
        channel: PacketChannel,
        buffer_size: usize,
        reliability_settings: reliable_channel::Settings,
        max_chunk_len: u16,
    ) -> Result<(CompressedTypedChannel<M>, ChannelStatistics), DuplicateChannel> {
        let (channel, statistics) = self.open_compressed_bincode_channel(
            multiplexer,
            channel,
            buffer_size,
            reliability_settings,
            max_chunk_len,
        )?;
        Ok((CompressedTypedChannel::new(channel), statistics))
    }
}
