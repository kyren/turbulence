use futures::{
    executor::LocalPool,
    future::{self, Either},
    task::SpawnExt,
    SinkExt, StreamExt,
};

use turbulence::{
    buffer::BufferPacketPool,
    packet::{Packet, PacketPool},
    packet_multiplexer::{MuxPacketPool, PacketMultiplexer},
};

mod util;

use self::util::SimpleBufferPool;

#[test]
fn test_multiplexer() {
    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    let mut packet_pool = MuxPacketPool::new(BufferPacketPool::new(SimpleBufferPool(32)));

    let mut multiplexer_a = PacketMultiplexer::new();
    let (mut sender4a, mut receiver4a, _) = multiplexer_a.open_channel(4, 8).unwrap();
    let (mut sender32a, mut receiver32a, _) = multiplexer_a.open_channel(32, 8).unwrap();

    let mut multiplexer_b = PacketMultiplexer::new();
    let (mut sender4b, mut receiver4b, _) = multiplexer_b.open_channel(4, 8).unwrap();
    let (mut sender32b, mut receiver32b, _) = multiplexer_b.open_channel(32, 8).unwrap();

    spawner
        .spawn(async move {
            let (mut a_incoming, mut a_outgoing) = multiplexer_a.start();
            let (mut b_incoming, mut b_outgoing) = multiplexer_b.start();
            loop {
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
        })
        .unwrap();

    spawner
        .spawn(async move {
            let mut packet = packet_pool.acquire();
            packet.resize(1, 17);
            sender4a.send(packet).await.unwrap();

            let mut packet = packet_pool.acquire();
            packet.resize(1, 18);
            sender4b.send(packet).await.unwrap();

            let mut packet = packet_pool.acquire();
            packet.resize(1, 19);
            sender32a.send(packet).await.unwrap();

            let mut packet = packet_pool.acquire();
            packet.resize(1, 20);
            sender32b.send(packet).await.unwrap();

            let packet = receiver4a.next().await.unwrap();
            assert_eq!(packet[0], 18);

            let packet = receiver4b.next().await.unwrap();
            assert_eq!(packet[0], 17);

            let packet = receiver32a.next().await.unwrap();
            assert_eq!(packet[0], 20);

            let packet = receiver32b.next().await.unwrap();
            assert_eq!(packet[0], 19);
        })
        .unwrap();

    pool.run();
}
