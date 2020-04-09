use std::{
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    ptr,
    sync::{Arc, Weak},
};

use crossbeam::queue::SegQueue;

/// A sharable pool of items of type `T`, where values are returned to the pool on drop.
#[derive(Debug)]
pub struct Pool<T>(Arc<SegQueue<T>>);

/// Trait for pooled items that allows them to change state when they re-enter the pool.
pub trait Init {
    /// Re-initialize this value before it is returned to the pool.  Default implementation does
    /// nothing.
    fn initialize(&mut self) {}
}

impl<T> Clone for Pool<T> {
    fn clone(&self) -> Pool<T> {
        Pool(Arc::clone(&self.0))
    }
}

impl<T: Init> Pool<T> {
    /// Construct a new empty pool.
    pub fn new() -> Pool<T> {
        Pool(Arc::new(SegQueue::new()))
    }

    /// Acquire an item from the pool, or construct a new default one if one is not available.
    pub fn acquire(&self) -> Pooled<T>
    where
        T: Default,
    {
        let item = self.0.pop().ok().unwrap_or_default();
        Pooled(ManuallyDrop::new(item), Arc::downgrade(&self.0))
    }

    pub fn acquire_with<F>(&self, f: F) -> Pooled<T>
    where
        F: FnOnce() -> T,
    {
        let item = self.0.pop().ok().unwrap_or_else(f);
        Pooled(ManuallyDrop::new(item), Arc::downgrade(&self.0))
    }
}

/// A wrapper around a pooled item.  On drop, the item is returned to its source pool.
#[derive(Debug)]
pub struct Pooled<T: Init>(ManuallyDrop<T>, Weak<SegQueue<T>>);

impl<T: Init> Pooled<T> {
    /// Constructs a new `Pooled` type not associated with a parent pool.
    pub fn new(item: T) -> Pooled<T> {
        Pooled(ManuallyDrop::new(item), Weak::new())
    }
}

impl<T: Init> Deref for Pooled<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Init> DerefMut for Pooled<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: Init> Drop for Pooled<T> {
    fn drop(&mut self) {
        // SAFE: Avoids the overhead of `Option` in `Pooled<T>`.  Safe because the contained value
        // is either dropped OR moved back into the pool without being dropped, never both.
        if let Some(pool) = self.1.upgrade() {
            let mut item = unsafe { ManuallyDrop::into_inner(ptr::read(&self.0)) };
            item.initialize();
            pool.push(item);
        } else {
            unsafe {
                ManuallyDrop::drop(&mut self.0);
            }
        }
    }
}
