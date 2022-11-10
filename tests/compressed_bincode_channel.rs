use std::time::Duration;

use futures::channel::oneshot;
use rand::{rngs::SmallRng, thread_rng, RngCore, SeedableRng};

use turbulence::{
    buffer::BufferPacketPool,
    compressed_bincode_channel::CompressedBincodeChannel,
    reliable_channel::{ReliableChannel, Settings},
    runtime::Spawn,
    spsc,
};

mod util;

use self::util::{condition_link, LinkCondition, SimpleBufferPool, SimpleRuntime};

#[test]
fn test_compressed_bincode_channel() {
    const SETTINGS: Settings = Settings {
        bandwidth: 2048,
        recv_window_size: 512,
        send_window_size: 512,
        burst_bandwidth: 512,
        init_send: 256,
        resend_time: Duration::from_millis(50),
        initial_rtt: Duration::from_millis(100),
        max_rtt: Duration::from_millis(2000),
        rtt_update_factor: 0.1,
        rtt_resend_factor: 1.5,
    };

    const CONDITION: LinkCondition = LinkCondition {
        loss: 0.2,
        duplicate: 0.05,
        delay: Duration::from_millis(40),
        jitter: Duration::from_millis(10),
    };

    let packet_pool = BufferPacketPool::new(SimpleBufferPool(1000));
    let mut runtime = SimpleRuntime::new();

    let (asend, acondrecv) = spsc::channel(2);
    let (acondsend, arecv) = spsc::channel(2);
    condition_link(
        CONDITION,
        runtime.handle(),
        runtime.handle(),
        packet_pool.clone(),
        SmallRng::from_rng(thread_rng()).unwrap(),
        acondsend,
        acondrecv,
    );

    let (bsend, bcondrecv) = spsc::channel(2);
    let (bcondsend, brecv) = spsc::channel(2);
    condition_link(
        CONDITION,
        runtime.handle(),
        runtime.handle(),
        packet_pool.clone(),
        SmallRng::from_rng(thread_rng()).unwrap(),
        bcondsend,
        bcondrecv,
    );

    let mut stream1 = CompressedBincodeChannel::new(ReliableChannel::new(
        runtime.handle(),
        runtime.handle(),
        packet_pool.clone(),
        SETTINGS.clone(),
        bsend,
        arecv,
    ));
    let mut stream2 = CompressedBincodeChannel::new(ReliableChannel::new(
        runtime.handle(),
        runtime.handle(),
        packet_pool.clone(),
        SETTINGS.clone(),
        asend,
        brecv,
    ));

    let (a_done_send, mut a_done) = oneshot::channel();
    runtime.spawn({
        async move {
            for i in 0..100 {
                let send_val = vec![i as u8 + 13; i + 25];
                stream1.send(&send_val).await.unwrap();
            }
            stream1.flush().await.unwrap();

            for i in 0..100 {
                let recv_val = stream1.recv::<Vec<u8>>().await.unwrap();
                assert_eq!(recv_val.len(), i + 17);
            }

            let _ = a_done_send.send(stream1);
        }
    });

    let (b_done_send, mut b_done) = oneshot::channel();
    runtime.spawn({
        async move {
            for i in 0..100 {
                let recv_val = stream2.recv::<Vec<u8>>().await.unwrap();
                assert_eq!(recv_val, vec![i as u8 + 13; i + 25].as_slice());
            }

            for i in 0..100 {
                let mut send_val = vec![0; i + 17];
                rand::thread_rng().fill_bytes(&mut send_val);
                stream2.send(&send_val).await.unwrap();
            }
            stream2.flush().await.unwrap();

            let _ = b_done_send.send(stream2);
        }
    });

    let mut a_done_stream = None;
    let mut b_done_stream = None;
    for _ in 0..100_000 {
        a_done_stream = a_done_stream.or_else(|| a_done.try_recv().unwrap());
        b_done_stream = b_done_stream.or_else(|| b_done.try_recv().unwrap());

        if a_done_stream.is_some() && b_done_stream.is_some() {
            return;
        }

        runtime.run_until_stalled();
        runtime.advance_time(50);
    }

    panic!("didn't finish in time");
}
