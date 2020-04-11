use futures::{channel::mpsc, executor::LocalPool, task::SpawnExt};
use serde::{Deserialize, Serialize};

use turbulence::{
    packet_multiplexer::MuxPacketPool, unreliable_bincode_channel::UnreliableTypedChannel,
};

mod util;

use self::util::SimpleBufferPool;

#[test]
fn test_unreliable_bincode_channel() {
    let packet_pool = MuxPacketPool::new(SimpleBufferPool(1200));
    let mut pool = LocalPool::new();
    let spawner = pool.spawner();

    let (asend, arecv) = mpsc::channel(8);
    let (bsend, brecv) = mpsc::channel(8);

    let mut stream1 = UnreliableTypedChannel::new(packet_pool.clone(), arecv, bsend);
    let mut stream2 = UnreliableTypedChannel::new(packet_pool.clone(), brecv, asend);

    #[derive(Eq, PartialEq, Debug, Serialize, Deserialize)]
    struct MyMsg {
        a: u8,
        b: u8,
        c: u8,
    }

    async fn send(stream: &mut UnreliableTypedChannel<MyMsg, SimpleBufferPool>, val: u8, len: u8) {
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

    async fn recv(stream: &mut UnreliableTypedChannel<MyMsg, SimpleBufferPool>, val: u8, len: u8) {
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

    spawner
        .spawn(async move {
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
        })
        .unwrap();

    spawner
        .spawn(async move {
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
        })
        .unwrap();

    pool.run();
}
