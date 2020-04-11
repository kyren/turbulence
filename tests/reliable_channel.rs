use std::time::Duration;

use futures::channel::{mpsc, oneshot};
use rand::{rngs::SmallRng, thread_rng, SeedableRng};

use turbulence::{
    buffer::BufferPacketPool,
    reliable_channel::{ReliableChannel, Settings},
    runtime::Runtime,
};

mod util;

use self::util::{condition_link, LinkCondition, SimpleBufferPool, SimpleRuntime};

#[test]
fn test_reliable_stream() {
    const SETTINGS: Settings = Settings {
        bandwidth: 32768,
        recv_window_size: 16384,
        send_window_size: 16384,
        burst_bandwidth: 4096,
        init_send: 512,
        wakeup_time: Duration::from_millis(50),
        initial_rtt: Duration::from_millis(100),
        max_rtt: Duration::from_millis(2000),
        rtt_update_factor: 0.1,
        rtt_resend_factor: 1.5,
    };

    const CONDITION: LinkCondition = LinkCondition {
        loss: 0.4,
        duplicate: 0.1,
        delay: Duration::from_millis(30),
        jitter: Duration::from_millis(20),
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

    let mut stream1 = ReliableChannel::new(
        runtime.handle(),
        packet_pool.clone(),
        SETTINGS.clone(),
        arecv,
        bsend,
    );
    let mut stream2 = ReliableChannel::new(
        runtime.handle(),
        packet_pool.clone(),
        SETTINGS.clone(),
        brecv,
        asend,
    );

    const END_POS: usize = 86_753;
    const FLUSH_EVERY: usize = 2000;
    const SEND_DELAY_NEAR: usize = 30_000;
    const RECV_DELAY_NEAR: usize = 70_000;

    let (a_done_send, mut a_done) = oneshot::channel();
    runtime.spawn({
        let runtime_handle = runtime.handle();
        async move {
            let mut send_buffer = [0; 512];
            let mut c = 0;

            loop {
                for i in 0..send_buffer.len() {
                    send_buffer[i] = (c + i) as u8;
                }
                let len = stream1
                    .write(&send_buffer[0..send_buffer.len().min(END_POS - c)])
                    .await
                    .unwrap();

                if c % FLUSH_EVERY >= (c + len) % FLUSH_EVERY {
                    stream1.flush().await.unwrap();
                }

                if c < SEND_DELAY_NEAR && c + len > SEND_DELAY_NEAR {
                    runtime_handle.delay(Duration::from_secs(1)).await;
                }

                c += len;

                if c == END_POS {
                    stream1.flush().await.unwrap();
                    break;
                }
            }

            a_done_send.send(()).unwrap();
        }
    });

    let (b_done_send, mut b_done) = oneshot::channel();
    runtime.spawn({
        let runtime_handle = runtime.handle();
        async move {
            let mut recv_buffer = [0; 64];
            let mut c = 0;

            loop {
                let len = stream2.read(&mut recv_buffer).await.unwrap();
                for i in 0..len {
                    if recv_buffer[i] != (c + i) as u8 {
                        dbg!(c, i, c + i, len, &recv_buffer[..len]);
                        panic!();
                    }
                }

                if c < RECV_DELAY_NEAR && c + len >= RECV_DELAY_NEAR {
                    runtime_handle.delay(Duration::from_secs(2)).await;
                }

                c += len;

                if c == END_POS {
                    break;
                }
            }

            b_done_send.send(()).unwrap();
        }
    });

    let mut a_is_done = false;
    let mut b_is_done = false;
    for _ in 0..100_000 {
        a_is_done = a_is_done || a_done.try_recv().unwrap().is_some();
        b_is_done = b_is_done || b_done.try_recv().unwrap().is_some();

        if a_is_done && b_is_done {
            return;
        }

        runtime.run_until_stalled();
        runtime.advance_time(50);
    }

    panic!("didn't finish in time");
}
