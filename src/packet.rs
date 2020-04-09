use std::ops::{Deref, DerefMut};

use crate::buffer::{Buffer, BufferPool};

pub const MAX_PACKET_LEN: usize = 1200;

/// A pooled buffer with a static maximum length of `MAX_PACKET_LEN`.
///
/// `turbulence` uses unreliable, unordered message protocols as the bottom level of its networking,
/// and all such protocols support `MAX_PACKET_LEN` packets without fragmentation.
#[derive(Debug)]
pub struct Packet(Buffer);

impl Deref for Packet {
    type Target = Buffer;

    fn deref(&self) -> &Buffer {
        &self.0
    }
}

impl DerefMut for Packet {
    fn deref_mut(&mut self) -> &mut Buffer {
        &mut self.0
    }
}

#[derive(Debug, Clone)]
pub struct PacketPool(BufferPool);

impl PacketPool {
    pub fn new() -> PacketPool {
        PacketPool(BufferPool::new(MAX_PACKET_LEN))
    }

    /// Acquire an empty Packet
    pub fn acquire(&self) -> Packet {
        Packet(self.0.acquire())
    }

    /// Acquire a Packet set to its max length
    pub fn acquire_max(&self) -> Packet {
        Packet(self.0.acquire_max())
    }
}
