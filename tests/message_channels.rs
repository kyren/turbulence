use std::time::Duration;

use futures::{
    channel::oneshot,
    future::{self, Either},
    SinkExt, StreamExt,
};
use serde::{Deserialize, Serialize};

use turbulence::{
    buffer::BufferPacketPool,
    message_channels::{MessageChannelMode, MessageChannelSettings, MessageChannelsBuilder},
    packet_multiplexer::PacketMultiplexer,
    reliable_channel,
    runtime::Spawn,
    unreliable_channel,
};

mod util;

use self::util::{SimpleBufferPool, SimpleRuntime};

// Define two message types, `Message1` and `Message2`

// `Message1` is a reliable message on channel "0" that has a maximum bandwidth of 4KB/s

#[derive(Serialize, Deserialize)]
struct Message1(i32);

const MESSAGE1_SETTINGS: MessageChannelSettings = MessageChannelSettings {
    channel: 0,
    channel_mode: MessageChannelMode::Reliable(reliable_channel::Settings {
        bandwidth: 4096,
        burst_bandwidth: 1024,
        recv_window_size: 1024,
        send_window_size: 1024,
        init_send: 512,
        resend_time: Duration::from_millis(100),
        initial_rtt: Duration::from_millis(200),
        max_rtt: Duration::from_secs(2),
        rtt_update_factor: 0.1,
        rtt_resend_factor: 1.5,
    }),
    message_buffer_size: 8,
    packet_buffer_size: 8,
};

// `Message2` is an unreliable message type on channel "1"

#[derive(Serialize, Deserialize)]
struct Message2(i32);

const MESSAGE2_SETTINGS: MessageChannelSettings = MessageChannelSettings {
    channel: 1,
    channel_mode: MessageChannelMode::Unreliable(unreliable_channel::Settings {
        bandwidth: 4096,
        burst_bandwidth: 1024,
    }),
    message_buffer_size: 8,
    packet_buffer_size: 8,
};

#[test]
fn test_message_channels() {
    let mut runtime = SimpleRuntime::new();
    let pool = BufferPacketPool::new(SimpleBufferPool(32));

    // Set up two packet multiplexers, one for our sending "A" side and one for our receiving "B"
    // side. They should both have exactly the same message types registered.

    let mut multiplexer_a = PacketMultiplexer::new();
    let mut builder_a = MessageChannelsBuilder::new(runtime.handle(), runtime.handle(), pool);
    builder_a.register::<Message1>(MESSAGE1_SETTINGS).unwrap();
    builder_a.register::<Message2>(MESSAGE2_SETTINGS).unwrap();
    let mut channels_a = builder_a.build(&mut multiplexer_a);

    let mut multiplexer_b = PacketMultiplexer::new();
    let mut builder_b = MessageChannelsBuilder::new(runtime.handle(), runtime.handle(), pool);
    builder_b.register::<Message1>(MESSAGE1_SETTINGS).unwrap();
    builder_b.register::<Message2>(MESSAGE2_SETTINGS).unwrap();
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
            // somewhat complex. This is not a great example of how to do it.
            //
            // Calling `x_incoming.send(packet).await` here is using `IncomingMultiplexedPackets`
            // `Sink` implementation, which forwards to the incoming spsc channel for whatever
            // channel this packet is for. `turbulence` *only* uses sync channels with static
            // size, so it is expected that this buffer might be full. You might want to instead
            // use `IncomingMultiplexedPackets::try_send` here and if the incoming buffer is full,
            // simply drop the packet. A full buffer means some level of the pipeline cannot keep
            // up, and dropping the packet rather than blocking on delivering here means that
            // a backup on one channel will not potentially block other channels from receiving
            // packets.
            //
            // On the outgoing side, since `turbulence` assumes an unreliable transport, it also
            // assumes that the actual outgoing transport can send at more or less an arbitrary
            // rate. For this reason, the different internal channel types *block* on sending
            // outgoing packets. It is assumed that the outgoing packet buffer would only be full
            // under very high, temporary CPU load on the host, and they block to let the task that
            // actually sends packets catch up. This assumption works if the outgoing stream is only
            // really CPU bound: that it is not harmful to block on outgoing packets because we're
            // cooperating with a task that will send UDP packets as fast as it can anyway, so we
            // won't be blocking for long (and it's better not to burn up even more CPU making more
            // packets that might not be sent).
            //
            // So why the difference, why drop incoming packets but block on outgoing packets? Well,
            // this again assumes that the task that sends packets is utterly simple, that it is a
            // task that just calls `sendto` or equivalent as fast as it can. On the incoming side
            // the pipeline is much longer, and will usually include the actual main game loop.
            // "Blocking" in this case may simply mean only processing a maximum number of incoming
            // messages per tick, or something along those lines. In that case, since "blocking" is
            // not a function of purely CPU load, dropping incoming packets for fairness and latency
            // may be reasonable. On the outgoing side, we're not assuming that we may have somehow
            // accidentally *sent* too much data, we of course assume that we are following our
            // *own* rules, so the only cause of a backup should be very high CPU load.
            //
            // Since this test unrealistically assumes perfect delivery of an unreliable channel,
            // and since this is all hard to simulate in an example with no actual network involved,
            // we just provide perfect instant delivery. None of the subtlety of doing this in a
            // real project is captured in this simplistic example.
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

    let (is_done_send, mut is_done_recv) = oneshot::channel();
    runtime.spawn(async move {
        // Now send some traffic across...

        // We're using the async `MessageChannels` API, but in a game you might use the sync API.
        channels_a.async_send(Message1(42)).await.unwrap();
        channels_a.flush::<Message1>();
        assert_eq!(channels_b.async_recv::<Message1>().await.unwrap().0, 42);

        // Since our underlying simulated network is perfect, our unreliable message will always
        // arrive.
        channels_a.async_send(Message2(13)).await.unwrap();
        channels_a.flush::<Message2>();
        assert_eq!(channels_b.async_recv::<Message2>().await.unwrap().0, 13);

        // Each message channel is independent of the others, and they all have their own
        // independent instances of message coalescing and reliability protocols.

        channels_a.async_send(Message1(20)).await.unwrap();
        channels_a.async_send(Message2(30)).await.unwrap();
        channels_a.async_send(Message1(21)).await.unwrap();
        channels_a.async_send(Message2(31)).await.unwrap();
        channels_a.async_send(Message1(22)).await.unwrap();
        channels_a.async_send(Message2(32)).await.unwrap();
        channels_a.flush::<Message1>();
        channels_a.flush::<Message2>();

        assert_eq!(channels_b.async_recv::<Message1>().await.unwrap().0, 20);
        assert_eq!(channels_b.async_recv::<Message1>().await.unwrap().0, 21);
        assert_eq!(channels_b.async_recv::<Message1>().await.unwrap().0, 22);

        assert_eq!(channels_b.async_recv::<Message2>().await.unwrap().0, 30);
        assert_eq!(channels_b.async_recv::<Message2>().await.unwrap().0, 31);
        assert_eq!(channels_b.async_recv::<Message2>().await.unwrap().0, 32);

        is_done_send.send(()).unwrap();
    });

    for _ in 0..100_000 {
        if is_done_recv.try_recv().unwrap().is_some() {
            return;
        }

        runtime.run_until_stalled();
        runtime.advance_time(50);
    }

    panic!("didn't finish in time");
}
