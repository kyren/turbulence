use criterion::{criterion_group, criterion_main, Criterion};
use futures::{
    future::{self, Either},
    Future, SinkExt, StreamExt,
};
use serde::{Deserialize, Serialize};
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use turbulence::{
    buffer::{BufferPacketPool, BufferPool},
    message_channels::{MessageChannelMode, MessageChannelSettings, MessageChannelsBuilder},
    packet_multiplexer::PacketMultiplexer,
    runtime::Runtime,
};

/// A smol::Timer wrapped to produce `()` instead of `Instant`,
/// for compatibility with the turbulence::Runtime trait.
pub struct Timer(pub smol::Timer);
impl Future for Timer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        use smol::future::FutureExt;
        self.0.poll(cx).map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Default)]
/// Facilitates using Smol's global executor with turbulence
pub struct GlobalSmolRuntime;
impl turbulence::Runtime for GlobalSmolRuntime {
    type Instant = Instant;
    type Sleep = Timer;

    fn spawn<F: Future<Output = ()> + Send + 'static>(&self, fut: F) {
        smol::spawn(fut).detach()
    }

    fn now(&self) -> Self::Instant {
        Instant::now()
    }

    fn elapsed(&self, instant: Self::Instant) -> Duration {
        instant.elapsed()
    }

    fn duration_between(&self, earlier: Self::Instant, later: Self::Instant) -> Duration {
        later.duration_since(earlier)
    }

    fn sleep(&self, d: Duration) -> Self::Sleep {
        Timer(smol::Timer::after(d))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SimpleBufferPool(pub usize);

impl BufferPool for SimpleBufferPool {
    type Buffer = Box<[u8]>;

    fn acquire(&self) -> Self::Buffer {
        vec![0; self.0].into_boxed_slice()
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct NoRelyInt(i32);

const NO_RELY_INT: MessageChannelSettings = MessageChannelSettings {
    channel: 0,
    channel_mode: MessageChannelMode::Unreliable,
    message_buffer_size: 8,
    packet_buffer_size: 8,
};

pub fn unreliable_message_channel_benchmark(c: &mut Criterion) {
    let runtime = GlobalSmolRuntime;
    let pool = BufferPacketPool::new(SimpleBufferPool(32));

    // Set up two packet multiplexers, one for our sending "A" side and one for our receiving "B"
    // side.  They should both have exactly the same message types registered.

    let mut multiplexer_a = PacketMultiplexer::new();
    let mut builder_a = MessageChannelsBuilder::new(runtime, pool);
    builder_a.register::<NoRelyInt>(NO_RELY_INT).unwrap();
    let mut channels_a = builder_a.build(&mut multiplexer_a);

    let mut multiplexer_b = PacketMultiplexer::new();
    let mut builder_b = MessageChannelsBuilder::new(runtime, pool);
    builder_b.register::<NoRelyInt>(NO_RELY_INT).unwrap();
    let mut channels_b = builder_b.build(&mut multiplexer_b);

    // Spawn a task that simulates a perfect network connection, and takes outgoing packets from
    // each multiplexer and gives it to the other.
    runtime.spawn(async move {
        // We need to send packets bidirectionally from A -> B and B -> A, because reliable message
        // channels must have a way to send acknowledgments.
        let (mut a_incoming, mut a_outgoing) = multiplexer_a.start();
        let (mut b_incoming, mut b_outgoing) = multiplexer_b.start();
        loop {
            // How to best send packets from the multiplexer to the internet and vice versa is
            // somewhat complex.  This is not a great example of how to do it.
            //
            // See tests/message_channels.rs for a breakdown.
            match future::select(a_outgoing.next(), b_outgoing.next()).await {
                Either::Left((Some(packet), _)) => {
                    b_incoming.send(packet).await.unwrap();
                }
                Either::Right((Some(packet), _)) => {
                    a_incoming.send(packet).await.unwrap();
                }
                Either::Left((None, _)) | Either::Right((None, _)) => break,
            }
        }
    });

    c.bench_function("unreliable messages", |b| {
        b.iter(|| {
            smol::block_on(async {
                // Now send some traffic across...

                // Since our underlying simulated network is perfect, our unreliable message will always
                // arrive.
                channels_a.async_send(NoRelyInt(20)).await.unwrap();
                channels_a.async_send(NoRelyInt(30)).await.unwrap();
                channels_a.async_send(NoRelyInt(21)).await.unwrap();
                channels_a.async_send(NoRelyInt(31)).await.unwrap();
                channels_a.async_send(NoRelyInt(22)).await.unwrap();
                channels_a.async_send(NoRelyInt(32)).await.unwrap();
                channels_a.flush::<NoRelyInt>();

                assert_eq!(channels_b.async_recv::<NoRelyInt>().await.unwrap().0, 20);
                assert_eq!(channels_b.async_recv::<NoRelyInt>().await.unwrap().0, 30);
                assert_eq!(channels_b.async_recv::<NoRelyInt>().await.unwrap().0, 21);
                assert_eq!(channels_b.async_recv::<NoRelyInt>().await.unwrap().0, 31);
                assert_eq!(channels_b.async_recv::<NoRelyInt>().await.unwrap().0, 22);
                assert_eq!(channels_b.async_recv::<NoRelyInt>().await.unwrap().0, 32);
            })
        })
    });
}

criterion_group!(benches, unreliable_message_channel_benchmark);
criterion_main!(benches);
