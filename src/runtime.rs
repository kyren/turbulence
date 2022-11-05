use std::{future::Future, time::Duration};

/// Trait for async runtime functionality needed by `turbulence`.
///
/// This is designed so that it can be implemented on multiple platforms with multiple runtimes,
/// including `wasm32-unknown-unknown`, where `std::time::Instant` is unavailable.
pub trait Runtime: Clone + Send + Sync {
    type Instant: Copy + Send + Sync;
    type Sleep: Future<Output = ()> + Send;

    /// This is similar to the `futures::task::Spawn` trait, but it is generic in the spawned
    /// future, which is better for backends like tokio.
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static;

    /// Return the current instant.
    fn now(&self) -> Self::Instant;
    /// Return the time elapsed since the given instant.
    fn elapsed(&self, instant: Self::Instant) -> Duration;

    /// Similarly to `std::time::Instant::duration_since`, may panic if `later` comes before
    /// `earlier`.
    fn duration_between(&self, earlier: Self::Instant, later: Self::Instant) -> Duration;

    /// Create a future which resolves after the given time has passed.
    fn sleep(&self, duration: Duration) -> Self::Sleep;
}

impl<'a, R: Runtime> Runtime for &'a R {
    type Instant = R::Instant;
    type Sleep = R::Sleep;

    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        (**self).spawn(future);
    }

    fn now(&self) -> Self::Instant {
        (**self).now()
    }

    fn elapsed(&self, instant: Self::Instant) -> Duration {
        (**self).elapsed(instant)
    }

    fn duration_between(&self, earlier: Self::Instant, later: Self::Instant) -> Duration {
        (**self).duration_between(earlier, later)
    }

    fn sleep(&self, duration: Duration) -> Self::Sleep {
        (**self).sleep(duration)
    }
}
