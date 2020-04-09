use crate::{
    compressed_typed_channel::CompressedTypedChannel,
    packet_multiplexer::{
        ChannelStatistics, DuplicateChannel, MuxPacketPool, PacketChannel, PacketMultiplexer,
    },
    reliable_bincode_channel::{ReliableBincodeChannel, ReliableTypedChannel},
    reliable_channel::{self, ReliableChannel},
    spawn::Spawn,
    timer::Timer,
    unreliable_bincode_channel::{UnreliableBincodeChannel, UnreliableTypedChannel},
    unreliable_channel::UnreliableChannel,
};

/// Helper that allows for easily opening different channel types on a `PacketMultiplexer`.
///
/// Contains a `MuxPacketPool`, a `Spawn` implemenentation, and a `Timer` implementation that is
/// used for each created channel.
pub struct ChannelBuilder<S, T> {
    pub pool: MuxPacketPool,
    pub spawner: S,
    pub timer: T,
}

impl<S: Spawn, T: Timer> ChannelBuilder<S, T> {
    pub fn new(packet_pool: MuxPacketPool, spawner: S, timer: T) -> Self {
        ChannelBuilder {
            pool: packet_pool,
            spawner,
            timer,
        }
    }

    pub fn open_unreliable_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer,
        channel: PacketChannel,
        buffer_size: usize,
    ) -> Result<(UnreliableChannel, ChannelStatistics), DuplicateChannel> {
        let (sender, receiver, statistics) = multiplexer.open_channel(channel, buffer_size)?;
        Ok((
            UnreliableChannel::new(self.pool.clone(), receiver, sender),
            statistics,
        ))
    }

    pub fn open_unreliable_bincode_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer,
        channel: PacketChannel,
        buffer_size: usize,
    ) -> Result<(UnreliableBincodeChannel, ChannelStatistics), DuplicateChannel> {
        let (sender, receiver, statistics) = multiplexer.open_channel(channel, buffer_size)?;
        Ok((
            UnreliableBincodeChannel::new(self.pool.clone(), receiver, sender),
            statistics,
        ))
    }

    pub fn open_unreliable_typed_channel<M>(
        &mut self,
        multiplexer: &mut PacketMultiplexer,
        channel: PacketChannel,
        buffer_size: usize,
    ) -> Result<(UnreliableTypedChannel<M>, ChannelStatistics), DuplicateChannel> {
        let (sender, receiver, statistics) = multiplexer.open_channel(channel, buffer_size)?;
        Ok((
            UnreliableTypedChannel::new(self.pool.clone(), receiver, sender),
            statistics,
        ))
    }

    pub fn open_reliable_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer,
        channel: PacketChannel,
        buffer_size: usize,
        settings: reliable_channel::Settings,
    ) -> Result<(ReliableChannel, ChannelStatistics), DuplicateChannel> {
        let (sender, receiver, statistics) = multiplexer.open_channel(channel, buffer_size)?;
        Ok((
            ReliableChannel::new(
                settings,
                self.timer.clone(),
                self.pool.clone(),
                receiver,
                sender,
                &self.spawner,
            ),
            statistics,
        ))
    }

    pub fn open_reliable_bincode_channel(
        &mut self,
        multiplexer: &mut PacketMultiplexer,
        channel: PacketChannel,
        buffer_size: usize,
        reliability_settings: reliable_channel::Settings,
        max_message_len: usize,
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
        multiplexer: &mut PacketMultiplexer,
        channel: PacketChannel,
        buffer_size: usize,
        reliability_settings: reliable_channel::Settings,
        max_message_len: usize,
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

    pub fn open_compressed_typed_channel<M>(
        &mut self,
        multiplexer: &mut PacketMultiplexer,
        channel: PacketChannel,
        buffer_size: usize,
        reliability_settings: reliable_channel::Settings,
        max_chunk_len: usize,
    ) -> Result<(CompressedTypedChannel<M>, ChannelStatistics), DuplicateChannel> {
        let (channel, statistics) =
            self.open_reliable_channel(multiplexer, channel, buffer_size, reliability_settings)?;
        Ok((
            CompressedTypedChannel::new(channel, max_chunk_len),
            statistics,
        ))
    }
}
