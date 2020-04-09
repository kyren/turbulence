pub mod channel_builder;
pub mod compressed_typed_channel;
pub mod event_watch;
pub mod message_channels;
pub mod packet;
pub mod packet_multiplexer;
pub mod reliable_bincode_channel;
pub mod reliable_channel;
pub mod spawn;
pub mod timer;
pub mod unreliable_bincode_channel;
pub mod unreliable_channel;
pub mod buffer;
pub mod wrap_cmp;
pub mod pool;

#[cfg(test)]
mod test_util;

pub use self::{
    channel_builder::ChannelBuilder,
    compressed_typed_channel::CompressedTypedChannel,
    message_channels::{
        MessageChannelMode, MessageChannelSettings, MessageChannels, MessageChannelsBuilder,
    },
    packet::{Packet, PacketPool, MAX_PACKET_LEN},
    packet_multiplexer::{
        ChannelStatistics, ChannelTotals, IncomingMultiplexedPackets, MuxPacket, MuxPacketPool,
        OutgoingMultiplexedPackets, PacketChannel, PacketMultiplexer, MAX_MUX_PACKET_LEN,
    },
    reliable_bincode_channel::{ReliableBincodeChannel, ReliableTypedChannel},
    reliable_channel::ReliableChannel,
    unreliable_bincode_channel::{UnreliableBincodeChannel, UnreliableTypedChannel},
    unreliable_channel::UnreliableChannel,
};
