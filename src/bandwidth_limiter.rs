use std::time::Duration;

use crate::Runtime;

pub struct BandwidthLimiter<R: Runtime> {
    runtime: R,
    bandwidth: u32,
    burst_bandwidth: u32,
    bytes_available: f64,
    last_calculation: R::Instant,
}

impl<R: Runtime> BandwidthLimiter<R> {
    /// The `burst_bandwidth` is the maximum amount of bandwidth credit that can accumulate.
    pub fn new(runtime: R, bandwidth: u32, burst_bandwidth: u32) -> BandwidthLimiter<R> {
        let last_calculation = runtime.now();
        BandwidthLimiter {
            runtime,
            bandwidth,
            burst_bandwidth,
            bytes_available: burst_bandwidth as f64,
            last_calculation,
        }
    }

    /// Delay until a time where there will be bandwidth available.
    pub fn delay_until_available(&self) -> Option<R::Sleep> {
        if self.bytes_available < 0. {
            Some(self.runtime.sleep(Duration::from_secs_f64(
                (-self.bytes_available) / self.bandwidth as f64,
            )))
        } else {
            None
        }
    }

    /// Actually update the amount of available bandwidth. Additional available bytes are not added
    /// until this method is called to add them.
    pub fn update_available(&mut self) {
        let now = self.runtime.now();
        self.bytes_available += self
            .runtime
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
