use std::time::Duration;

use futures::channel::{mpsc, oneshot};
use rand::{rngs::SmallRng, thread_rng, SeedableRng};

use turbulence::{
    buffer::BufferPacketPool,
    reliable_bincode_channel::ReliableBincodeChannel,
    reliable_channel::{ReliableChannel, Settings},
    runtime::Runtime,
};

mod util;

use self::util::{condition_link, LinkCondition, SimpleBufferPool, SimpleRuntime};

#[test]
fn test_reliable_bincode_channel() {
    const SETTINGS: Settings = Settings {
        bandwidth: 2048,
        recv_window_size: 512,
        send_window_size: 512,
        burst_bandwidth: 512,
        init_send: 256,
        wakeup_time: Duration::from_millis(50),
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

    let (asend, acondrecv) = mpsc::channel(2);
    let (acondsend, arecv) = mpsc::channel(2);
    condition_link(
        CONDITION,
        runtime.handle(),
        packet_pool.clone(),
        SmallRng::from_rng(thread_rng()).unwrap(),
        acondrecv,
        acondsend,
    );

    let (bsend, bcondrecv) = mpsc::channel(2);
    let (bcondsend, brecv) = mpsc::channel(2);
    condition_link(
        CONDITION,
        runtime.handle(),
        packet_pool.clone(),
        SmallRng::from_rng(thread_rng()).unwrap(),
        bcondrecv,
        bcondsend,
    );

    let mut stream1 = ReliableBincodeChannel::new(
        ReliableChannel::new(
            runtime.handle(),
            SETTINGS.clone(),
            packet_pool.clone(),
            arecv,
            bsend,
        ),
        1024,
    );
    let mut stream2 = ReliableBincodeChannel::new(
        ReliableChannel::new(
            runtime.handle(),
            SETTINGS.clone(),
            packet_pool.clone(),
            brecv,
            asend,
        ),
        1024,
    );

    let (a_done_send, mut a_done) = oneshot::channel();
    runtime.spawn({
        async move {
            for i in 0..100 {
                let send_val = vec![i as u8 + 42; i + 25];
                stream1.send(&send_val).await.unwrap();
            }
            stream1.flush().await.unwrap();

            for i in 0..100 {
                let recv_val = stream1.recv::<&[u8]>().await.unwrap();
                assert_eq!(recv_val, vec![i as u8 + 64; i + 50].as_slice());
            }

            a_done_send.send(()).unwrap();
        }
    });

    let (b_done_send, mut b_done) = oneshot::channel();
    runtime.spawn({
        async move {
            for i in 0..100 {
                let recv_val = stream2.recv::<&[u8]>().await.unwrap();
                assert_eq!(recv_val, vec![i as u8 + 42; i + 25].as_slice());
            }

            for i in 0..100 {
                let send_val = vec![i as u8 + 64; i + 50];
                stream2.send(&send_val).await.unwrap();
            }
            stream2.flush().await.unwrap();

            b_done_send.send(()).unwrap();
        }
    });

    let mut a_is_done = false;
    let mut b_is_done = false;
    loop {
        a_is_done = a_is_done || a_done.try_recv().unwrap().is_some();
        b_is_done = b_is_done || b_done.try_recv().unwrap().is_some();

        if a_is_done && b_is_done {
            break;
        }

        runtime.run_until_stalled();
        runtime.advance_time(50);
    }
}
