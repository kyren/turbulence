use std::ops::{Deref, DerefMut};

pub use crate::packet::{Packet, PacketPool};

/// A trait for implementing `PacketPool` more easily using an allocator for statically sized buffers.
pub trait BufferPool {
    type Buffer: Deref<Target = [u8]> + DerefMut;

    fn acquire(&mut self) -> Self::Buffer;
}

/// Turns a `BufferPool` implementation into something that implements `PacketPool`.
#[derive(Debug, Copy, Clone, Default)]
pub struct BufferPacketPool<B>(B);

impl<B> BufferPacketPool<B> {
    pub fn new(buffer_pool: B) -> Self {
        BufferPacketPool(buffer_pool)
    }
}

impl<B: BufferPool> PacketPool for BufferPacketPool<B> {
    type Packet = BufferPacket<B::Buffer>;

    fn acquire(&mut self) -> Self::Packet {
        BufferPacket {
            buffer: self.0.acquire(),
            len: 0,
        }
    }
}

#[derive(Debug)]
pub struct BufferPacket<B> {
    buffer: B,
    len: usize,
}

impl<B> Packet for BufferPacket<B>
where
    B: Deref<Target = [u8]> + DerefMut,
{
    fn capacity(&self) -> usize {
        self.buffer.len()
    }

    fn resize(&mut self, len: usize, val: u8) {
        assert!(len <= self.buffer.len());
        for i in self.len..len {
            self.buffer[i] = val;
        }
        self.len = len;
    }
}

impl<B> Deref for BufferPacket<B>
where
    B: Deref<Target = [u8]>,
{
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buffer[0..self.len]
    }
}

impl<B> DerefMut for BufferPacket<B>
where
    B: Deref<Target = [u8]> + DerefMut,
{
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.buffer[0..self.len]
    }
}
