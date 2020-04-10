use std::{cmp::Ordering, collections::BinaryHeap, convert::TryInto, time::Duration};

use futures::future;
use rand::Rng;

use crate::{
    packet::{Packet, PacketPool},
    timer::Timer,
};

pub struct LinkCondition {
    /// Probability in the range [0.0, 1.0] of a packet being lost.
    pub loss: f64,
    /// Probability in the range [0.0, 1.0] of a packet being duplicated.
    ///
    /// Duplicated packets are delayed and jittered separately from their source packet.
    pub duplicate: f64,
    /// All packets are delayed by this amount plus any jitter.
    pub delay: Duration,
    /// Packets are delayed by a random fraction of the jitter.
    pub jitter: Duration,
}

pub struct DelayedPackets<R, T: Timer> {
    rng: R,
    timer: T,
    pool: PacketPool,
    condition: LinkCondition,
    epoch: T::Instant,
    heap: BinaryHeap<DelayedPacket>,
}

impl<R: Rng, T: Timer> DelayedPackets<R, T> {
    pub fn new(
        rng: R,
        timer: T,
        pool: PacketPool,
        condition: LinkCondition,
    ) -> DelayedPackets<R, T> {
        assert!(0.0 <= condition.loss && condition.loss <= 1.0);
        assert!(0.0 <= condition.duplicate && condition.duplicate <= 1.0);
        let epoch = timer.now();
        DelayedPackets {
            rng,
            timer,
            pool,
            condition,
            epoch,
            heap: BinaryHeap::new(),
        }
    }

    pub fn push(&mut self, packet: Packet) {
        if self.rng.gen::<f64>() > self.condition.loss {
            let delay = self.condition.delay.as_secs_f64();
            let jitter = self.condition.jitter.as_secs_f64();

            if self.rng.gen::<f64>() <= self.condition.duplicate {
                let delay = Duration::from_secs_f64(delay + jitter * self.rng.gen::<f64>());
                let mut dup_packet = self.pool.acquire();
                dup_packet.extend(&packet[..]);
                self.push_packet(delay, dup_packet);
            }

            let delay = Duration::from_secs_f64(delay + jitter * self.rng.gen::<f64>());
            self.push_packet(delay, packet);
        }
    }

    pub async fn next(&mut self) -> Packet {
        let now: u64 = self
            .timer
            .elapsed(self.epoch)
            .as_micros()
            .try_into()
            .unwrap();
        if let Some(delayed) = self.heap.peek() {
            if delayed.delay > now {
                self.timer
                    .delay(Duration::from_micros(delayed.delay - now))
                    .await;
            }
            self.heap.pop().unwrap().packet
        } else {
            future::pending().await
        }
    }

    fn push_packet(&mut self, delay: Duration, packet: Packet) {
        let delay = (self.timer.elapsed(self.epoch) + delay)
            .as_micros()
            .try_into()
            .unwrap();
        self.heap.push(DelayedPacket { delay, packet });
    }
}

struct DelayedPacket {
    delay: u64,
    packet: Packet,
}

impl PartialEq for DelayedPacket {
    fn eq(&self, other: &Self) -> bool {
        self.delay == other.delay
    }
}

impl Eq for DelayedPacket {}

impl PartialOrd for DelayedPacket {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DelayedPacket {
    fn cmp(&self, other: &Self) -> Ordering {
        self.delay.cmp(&other.delay).reverse()
    }
}
