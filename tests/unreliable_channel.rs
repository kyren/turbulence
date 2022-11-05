use futures::channel::oneshot;

use turbulence::{
    buffer::BufferPacketPool,
    runtime::Runtime,
    spsc,
    unreliable_channel::{Settings, UnreliableChannel},
};

mod util;

use self::util::{SimpleBufferPool, SimpleRuntime, SimpleRuntimeHandle};

#[test]
fn test_unreliable_channel() {
    const SETTINGS: Settings = Settings {
        bandwidth: 512,
        burst_bandwidth: 256,
    };

    let mut runtime = SimpleRuntime::new();
    let packet_pool = BufferPacketPool::new(SimpleBufferPool(1200));

    let (asend, arecv) = spsc::channel(8);
    let (bsend, brecv) = spsc::channel(8);

    let mut stream1 = UnreliableChannel::new(
        runtime.handle(),
        packet_pool.clone(),
        SETTINGS,
        arecv,
        bsend,
    );
    let mut stream2 = UnreliableChannel::new(
        runtime.handle(),
        packet_pool.clone(),
        SETTINGS,
        brecv,
        asend,
    );

    async fn send(
        stream: &mut UnreliableChannel<SimpleRuntimeHandle, BufferPacketPool<SimpleBufferPool>>,
        val: u8,
        len: usize,
    ) {
        let msg1 = vec![val; len];
        stream.send(&msg1).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn recv(
        stream: &mut UnreliableChannel<SimpleRuntimeHandle, BufferPacketPool<SimpleBufferPool>>,
        val: u8,
        len: usize,
    ) {
        assert_eq!(stream.recv().await.unwrap(), vec![val; len].as_slice());
    }

    let (a_done_send, mut a_done) = oneshot::channel();
    runtime.spawn(async move {
        send(&mut stream1, 42, 5).await;
        recv(&mut stream1, 17, 800).await;
        send(&mut stream1, 4, 700).await;
        recv(&mut stream1, 25, 1150).await;
        recv(&mut stream1, 0, 0).await;
        recv(&mut stream1, 0, 0).await;
        send(&mut stream1, 64, 1000).await;
        send(&mut stream1, 0, 0).await;
        send(&mut stream1, 64, 1000).await;
        send(&mut stream1, 0, 0).await;
        send(&mut stream1, 0, 0).await;
        recv(&mut stream1, 0, 0).await;
        recv(&mut stream1, 99, 64).await;
        send(&mut stream1, 72, 400).await;
        send(&mut stream1, 82, 500).await;
        send(&mut stream1, 92, 600).await;
        let _ = a_done_send.send(stream1);
    });

    let (b_done_send, mut b_done) = oneshot::channel();
    runtime.spawn(async move {
        recv(&mut stream2, 42, 5).await;
        send(&mut stream2, 17, 800).await;
        recv(&mut stream2, 4, 700).await;
        send(&mut stream2, 25, 1150).await;
        send(&mut stream2, 0, 0).await;
        send(&mut stream2, 0, 0).await;
        recv(&mut stream2, 64, 1000).await;
        recv(&mut stream2, 0, 0).await;
        recv(&mut stream2, 64, 1000).await;
        recv(&mut stream2, 0, 0).await;
        recv(&mut stream2, 0, 0).await;
        send(&mut stream2, 0, 0).await;
        send(&mut stream2, 99, 64).await;
        recv(&mut stream2, 72, 400).await;
        recv(&mut stream2, 82, 500).await;
        recv(&mut stream2, 92, 600).await;
        let _ = b_done_send.send(stream2);
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
