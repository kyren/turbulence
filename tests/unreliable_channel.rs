use futures::{channel::mpsc, executor::LocalPool, task::SpawnExt};

use turbulence::{packet_multiplexer::MuxPacketPool, unreliable_channel::UnreliableChannel};

mod util;

use self::util::SimpleBufferPool;

#[test]
fn test_unreliable_channel() {
    let packet_pool = MuxPacketPool::new(SimpleBufferPool(1200));
    let mut pool = LocalPool::new();
    let spawner = pool.spawner();

    let (asend, arecv) = mpsc::channel(8);
    let (bsend, brecv) = mpsc::channel(8);

    let mut stream1 = UnreliableChannel::new(packet_pool.clone(), arecv, bsend);
    let mut stream2 = UnreliableChannel::new(packet_pool.clone(), brecv, asend);

    async fn send(stream: &mut UnreliableChannel<SimpleBufferPool>, val: u8, len: usize) {
        let msg1 = vec![val; len];
        stream.send(&msg1).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn recv(stream: &mut UnreliableChannel<SimpleBufferPool>, val: u8, len: usize) {
        let mut msg2 = vec![0; len];
        assert_eq!(stream.recv(&mut msg2).await.unwrap(), len);
        assert_eq!(msg2, vec![val; len]);
    }

    spawner
        .spawn(async move {
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
        })
        .unwrap();

    spawner
        .spawn(async move {
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
        })
        .unwrap();

    pool.run();
}
