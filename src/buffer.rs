use std::ops::{Deref, DerefMut};

use crate::pool::{Init, Pool, Pooled};

/// A pooled buffer with a static capacity.
#[derive(Debug)]
pub struct Buffer(Pooled<BufferData>);

impl Deref for Buffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl Buffer {
    /// Constructs a new buffer not associated with any `BufferPool`.
    pub fn new(capacity: usize) -> Buffer {
        Buffer(Pooled::new(BufferData(Vec::with_capacity(capacity))))
    }

    /// Static capacity of this buffer
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Resizes the buffer to the given length, panicking if the length is larger than the static
    /// buffer capacity.
    pub fn resize(&mut self, len: usize, val: u8) {
        assert!(len <= self.0.capacity());
        self.0.resize(len, val);
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }

    pub fn extend(&mut self, other: &[u8]) {
        assert!(self.0.len() + other.len() <= self.0.capacity());
        self.0.extend_from_slice(other);
    }

    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.0.as_mut_slice()
    }
}

/// A pool of static capacity buffers.  When a `Buffer` is dropped, it is automatically returned to
/// the pool.
#[derive(Clone, Debug)]
pub struct BufferPool {
    buffer_capacity: usize,
    pool: Pool<BufferData>,
}

impl BufferPool {
    /// All created buffers will have the given static capacity.
    pub fn new(buffer_capacity: usize) -> BufferPool {
        BufferPool {
            buffer_capacity,
            pool: Pool::new(),
        }
    }

    /// Acquire an empty buffer from the pool.
    pub fn acquire(&self) -> Buffer {
        Buffer(
            self.pool
                .acquire_with(|| BufferData(Vec::with_capacity(self.buffer_capacity))),
        )
    }

    /// Acquire a buffer from the pool resized to its max capacity and filled with zeroes.
    pub fn acquire_max(&self) -> Buffer {
        let mut buffer = self.acquire();
        // If this ever becomes a performance problem, we could ensure that buffers are initialized
        // on creation and provide an interface that simply calls `Vec::set_len` instead of resize.
        // This can be done with a "safe" interface, in that we only have to initialize pooled
        // buffers once, and returned data in the buffer would be "initialized" but "unspecified".
        // You wouldn't be able to get the random contents of memory, only the random contents of
        // previous pooled buffers.  This may be "safe" in the rust sense, but it would still be
        // dangerous, so let's not do it until it's actually necessary.
        buffer.resize(buffer.capacity(), 0);
        buffer
    }
}

#[derive(Debug)]
pub struct BufferData(Vec<u8>);

impl Init for BufferData {
    fn initialize(&mut self) {
        self.0.clear();
    }
}

impl Deref for BufferData {
    type Target = Vec<u8>;

    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl DerefMut for BufferData {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}
