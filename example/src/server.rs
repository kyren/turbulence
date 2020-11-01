use comn::Chat;
use turbulence::MessageChannels;

async fn handle_client(mut channel: MessageChannels) {
    // Note that we use the async versions of the MessageChannels messages here,
    // because each client gets their own task on the server, so we don't mind blocking.
    // On the client, we used the sync versions, because not blocking was desirable.
    loop {
        if let Ok(Chat(msg)) = channel.async_recv().await {
            println!("server got a message: {}", msg);

            // Messages that start with `I'm` get special treatment,
            // otherwise we just send their message back to them.
            let response = if msg.len() > 4 && &msg[..4] == "I'm " {
                format!("Hi {}, I'm server!", &msg[4..])
            } else {
                msg
            };

            channel.async_send(Chat(response)).await.unwrap();
        }
    }
}

fn main() {
    // Note that connections here is not a tally of live connections, just a count of total
    // lifetime connections. In order to determine whether or not a connection is live, you will
    // have to implement some sort of heartbeat system.
    let mut connections = 0;

    smol::block_on(open_socket(comn::SERVER, 2500, move |channel| {
        connections += 1;
        println!("connections: {}", connections);

        // It's important that processing the messages from the client happens in another task,
        // so that this one is freed up to keep accepting more clients.
        smol::spawn(handle_client(channel)).detach();
    }));
}

// A UDP socket that accepts new connections for as long as it's open.
async fn open_socket(
    my_addr: &str,
    pool_size: usize,
    mut channel_handler: impl FnMut(MessageChannels) + Send + 'static,
) {
    use comn::{channel_with_multiplexer, send_outgoing_to_socket, SimpleBufferPool};
    use std::collections::HashMap;
    use turbulence::{BufferPacketPool, Packet};

    let pool = BufferPacketPool::new(SimpleBufferPool(pool_size));
    let mut sockets_incoming = HashMap::with_capacity(100);

    let socket = smol::block_on(async {
        smol::net::UdpSocket::bind(my_addr)
            .await
            .expect("couldn't bind to address")
    });

    loop {
        let mut packet = comn::acquire_max(&pool);
        match socket.recv_from(&mut packet).await {
            Ok((len, addr)) => {
                let incoming = sockets_incoming.entry(addr).or_insert_with(|| {
                    let (channel, multiplexer) = channel_with_multiplexer(pool.clone());
                    let (incoming, outgoing) = multiplexer.start();
                    send_outgoing_to_socket(outgoing, socket.clone(), addr);
                    channel_handler(channel);
                    incoming
                });
                packet.truncate(len);
                if let Err(e) = incoming.try_send(packet) {
                    println!("couldn't send packet: {}", e);
                }
            }
            Err(e) => println!("couldn't recieve packet from UDP socket: {}", e),
        };
    }
}
