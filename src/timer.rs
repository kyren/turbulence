use std::{future::Future, time::Duration};

use futures::Stream;

/// Interface for time related functionality that can be implemented on multiple platforms with
/// multiple futures runtimes, including `wasm32-unknown-unknown`, where `std::time::Instant` is
/// unavailable.
pub trait Timer: Clone + Send + Sync + 'static {
    type Instant: Copy + Send + Sync;
    type Delay: Future<Output = ()> + Unpin + Send;
    type Interval: Stream<Item = Self::Instant> + Unpin + Send;

    fn now(&self) -> Self::Instant;
    fn elapsed(&self, instant: Self::Instant) -> Duration;

    /// Similarly to `std::time::Instant::duration_since`, will panic if `later` comes before
    /// `earlier`.
    fn duration_between(&self, earlier: Self::Instant, later: Self::Instant) -> Duration;

    /// Create a future which resolves after the given time has passed.
    fn delay(&self, duration: Duration) -> Self::Delay;

    /// Create a stream which produces values continuously at the given interval.
    fn interval(&self, duration: Duration) -> Self::Interval;
}
