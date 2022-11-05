use std::time::Duration;

use futures::channel::oneshot;
use rand::{rngs::SmallRng, thread_rng, SeedableRng};

use turbulence::{
    buffer::BufferPacketPool,
    reliable_channel::{ReliableChannel, Settings},
    runtime::Runtime,
    spsc,
};

mod util;

use self::util::{condition_link, LinkCondition, SimpleBufferPool, SimpleRuntime};

#[test]
fn test_reliable_stream() {
    const SETTINGS: Settings = Settings {
        bandwidth: 32768,
        burst_bandwidth: 4096,
        recv_window_size: 16384,
        send_window_size: 16384,
        init_send: 512,
        resend_time: Duration::from_millis(50),
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

    let (asend, acondrecv) = spsc::channel(2);
    let (acondsend, arecv) = spsc::channel(2);
    condition_link(
        CONDITION,
        runtime.handle(),
        packet_pool.clone(),
        SmallRng::from_rng(thread_rng()).unwrap(),
        acondrecv,
        acondsend,
    );

    let (bsend, bcondrecv) = spsc::channel(2);
    let (bcondsend, brecv) = spsc::channel(2);
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
        SETTINGS,
        arecv,
        bsend,
    );
    let mut stream2 = ReliableChannel::new(
        runtime.handle(),
        packet_pool.clone(),
        SETTINGS,
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
                    runtime_handle.sleep(Duration::from_secs(1)).await;
                }

                c += len;

                if c == END_POS {
                    stream1.flush().await.unwrap();
                    break;
                }
            }

            let _ = a_done_send.send(stream1);
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
                        panic!();
                    }
                }

                if c < RECV_DELAY_NEAR && c + len >= RECV_DELAY_NEAR {
                    runtime_handle.sleep(Duration::from_secs(2)).await;
                }

                c += len;

                if c == END_POS {
                    break;
                }
            }

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
