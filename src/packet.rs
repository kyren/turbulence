use std::ops::{Deref, DerefMut};

/// The maximum usable packet size by `turbulence`.
///
/// It is not useful for an implementation of `PacketPool` to return packets with larger capacity
/// than this, `turbulence` may not be able to use the entire packet capacity otherwise.
pub const MAX_PACKET_LEN: u16 = 32768;

/// A trait for packet buffers used by `turbulence`.
pub trait Packet: Deref<Target = [u8]> + DerefMut {
    /// Resizes the packet to the given length, which must be at most the static capacity.
    fn resize(&mut self, len: usize, val: u8);

    fn extend(&mut self, other: &[u8]) {
        let cur_len = self.len();
        let new_len = cur_len + other.len();
        self.resize(new_len, 0);
        self[cur_len..new_len].copy_from_slice(other);
    }

    fn truncate(&mut self, len: usize) {
        let len = len.min(self.len());
        self.resize(len, 0);
    }

    fn clear(&mut self) {
        self.resize(0, 0);
    }
}

/// Trait for packet allocation and pooling.
///
/// All packets that are allocated from `turbulence` are allocated through this interface.
///
/// Packets must implement the `Packet` trait and should all have the same capacity: the MTU for
/// whatever the underlying transport is, up to `MAX_PACKET_LEN` in size.
pub trait PacketPool {
    type Packet: Packet;

    /// Static maximum capacity packets returned by this pool.
    fn capacity(&self) -> usize;

    fn acquire(&mut self) -> Self::Packet;
}
