use futures::channel::oneshot;
use serde::{Deserialize, Serialize};

use turbulence::{
    buffer::BufferPacketPool,
    runtime::Runtime,
    spsc,
    unreliable_bincode_channel::{UnreliableBincodeChannel, UnreliableTypedChannel},
    unreliable_channel::{Settings, UnreliableChannel},
};

mod util;

use self::util::{SimpleBufferPool, SimpleRuntime, SimpleRuntimeHandle};

#[test]
fn test_unreliable_bincode_channel() {
    const SETTINGS: Settings = Settings {
        bandwidth: 512,
        burst_bandwidth: 256,
    };

    let mut runtime = SimpleRuntime::new();
    let packet_pool = BufferPacketPool::new(SimpleBufferPool(1200));

    let (asend, arecv) = spsc::channel(8);
    let (bsend, brecv) = spsc::channel(8);

    let mut stream1 = UnreliableTypedChannel::new(UnreliableBincodeChannel::new(
        UnreliableChannel::new(
            runtime.handle(),
            packet_pool.clone(),
            SETTINGS,
            bsend,
            arecv,
        ),
        512,
    ));
    let mut stream2 = UnreliableTypedChannel::new(UnreliableBincodeChannel::new(
        UnreliableChannel::new(
            runtime.handle(),
            packet_pool.clone(),
            SETTINGS,
            asend,
            brecv,
        ),
        512,
    ));

    #[derive(Eq, PartialEq, Debug, Serialize, Deserialize)]
    struct MyMsg {
        a: u8,
        b: u8,
        c: u8,
    }

    async fn send(
        stream: &mut UnreliableTypedChannel<
            MyMsg,
            SimpleRuntimeHandle,
            BufferPacketPool<SimpleBufferPool>,
        >,
        val: u8,
        len: u8,
    ) {
        for i in 0..len {
            stream
                .send(&MyMsg {
                    a: val + i,
                    b: val + 1 + i,
                    c: val + 2 + i,
                })
                .await
                .unwrap();
        }
        stream.flush().await.unwrap();
    }

    async fn recv(
        stream: &mut UnreliableTypedChannel<
            MyMsg,
            SimpleRuntimeHandle,
            BufferPacketPool<SimpleBufferPool>,
        >,
        val: u8,
        len: u8,
    ) {
        for i in 0..len {
            assert_eq!(
                stream.recv().await.unwrap(),
                MyMsg {
                    a: val + i,
                    b: val + 1 + i,
                    c: val + 2 + i,
                }
            );
        }
    }

    let (a_done_send, mut a_done) = oneshot::channel();
    runtime.spawn(async move {
        send(&mut stream1, 42, 5).await;
        recv(&mut stream1, 17, 80).await;
        send(&mut stream1, 4, 70).await;
        recv(&mut stream1, 25, 115).await;
        recv(&mut stream1, 0, 0).await;
        recv(&mut stream1, 0, 0).await;
        send(&mut stream1, 64, 100).await;
        send(&mut stream1, 0, 0).await;
        send(&mut stream1, 64, 100).await;
        send(&mut stream1, 0, 0).await;
        send(&mut stream1, 0, 0).await;
        recv(&mut stream1, 0, 0).await;
        recv(&mut stream1, 99, 6).await;
        send(&mut stream1, 72, 40).await;
        send(&mut stream1, 82, 50).await;
        send(&mut stream1, 92, 60).await;
        let _ = a_done_send.send(stream1);
    });

    let (b_done_send, mut b_done) = oneshot::channel();
    runtime.spawn(async move {
        recv(&mut stream2, 42, 5).await;
        send(&mut stream2, 17, 80).await;
        recv(&mut stream2, 4, 70).await;
        send(&mut stream2, 25, 115).await;
        send(&mut stream2, 0, 0).await;
        send(&mut stream2, 0, 0).await;
        recv(&mut stream2, 64, 100).await;
        recv(&mut stream2, 0, 0).await;
        recv(&mut stream2, 64, 100).await;
        recv(&mut stream2, 0, 0).await;
        recv(&mut stream2, 0, 0).await;
        send(&mut stream2, 0, 0).await;
        send(&mut stream2, 99, 6).await;
        recv(&mut stream2, 72, 40).await;
        recv(&mut stream2, 82, 50).await;
        recv(&mut stream2, 92, 60).await;
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
