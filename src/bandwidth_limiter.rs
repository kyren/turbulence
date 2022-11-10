use std::time::Duration;

use crate::runtime::Timer;

pub struct BandwidthLimiter<T: Timer> {
    bandwidth: u32,
    burst_bandwidth: u32,
    bytes_available: f64,
    last_calculation: T::Instant,
}

impl<T: Timer> BandwidthLimiter<T> {
    /// The `burst_bandwidth` is the maximum amount of bandwidth credit that can accumulate.
    pub fn new(timer: &T, bandwidth: u32, burst_bandwidth: u32) -> Self {
        let last_calculation = timer.now();
        BandwidthLimiter {
            bandwidth,
            burst_bandwidth,
            bytes_available: burst_bandwidth as f64,
            last_calculation,
        }
    }

    /// Delay until a time where there will be bandwidth available.
    pub fn delay_until_available(&self, timer: &T) -> Option<T::Sleep> {
        if self.bytes_available < 0. {
            Some(timer.sleep(Duration::from_secs_f64(
                (-self.bytes_available) / self.bandwidth as f64,
            )))
        } else {
            None
        }
    }

    /// Actually update the amount of available bandwidth. Additional available bytes are not added
    /// until this method is called to add them.
    pub fn update_available(&mut self, timer: &T) {
        let now = timer.now();
        self.bytes_available += timer
            .duration_between(self.last_calculation, now)
            .as_secs_f64()
            * self.bandwidth as f64;
        self.bytes_available = self.bytes_available.min(self.burst_bandwidth as f64);
        self.last_calculation = now;
    }

    /// The bandwidth limiter only needs to limit outgoing packets being sent at all, not their
    /// size, so this returns true if a non-negative amount of bytes is available. If a packet is
    /// sent that is larger than the available bytes, the available bytes will go negative and this
    /// will no longer return true.
    pub fn bytes_available(&self) -> bool {
        self.bytes_available >= 0.
    }

    /// Record that bytes were sent, possibly going into bandwidth debt.
    pub fn take_bytes(&mut self, bytes: u32) {
        self.bytes_available -= bytes as f64
    }
}
