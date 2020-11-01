use std::sync::mpsc::{sync_channel, SyncSender};
use comn::Chat;
use turbulence::MessageChannels;

fn main() {
    // Note that some of these clients may very well have long ago disconnected. In order to determine
    // whether or not a connection is live, you will have to implement some sort of heartbeat system.
    let mut clients = Vec::with_capacity(100);
    let (client_tx, client_rx) = sync_channel(100);

    smol::spawn(open_socket(comn::SERVER, 2500, client_tx)).detach();

    loop {
        // Add any new clients to our collection of channels
        if let Ok(channel) = client_rx.try_recv() {
            clients.push(channel);
        }

        // Check for chat messages from each client
        for channel in &mut clients {
            if let Some(Chat(msg)) = channel.recv() {
                println!("server got a message: {}", msg);

                // Messages that start with `I'm` get special treatment,
                // otherwise we just send their message back to them.
                let response = if msg.len() > 4 && &msg[..4] == "I'm " {
                    format!("Hi {}, I'm server!", &msg[4..])
                } else {
                    msg
                };

                // if .recv() didn't require &mut, I could loop over all the channels
                // again here and send the message to everyone. Instead, I'll just 
                // reply only to the person who sent the message.
                if let Some(Chat(rejected)) = channel.send(Chat(response)) {
                    println!("couldn't send chat message: {}", rejected)
                }
            }
        }
    }
}

// A UDP socket that accepts new connections for as long as it's open.
async fn open_socket(my_addr: &str, pool_size: usize, client_tx: SyncSender<MessageChannels>) {
    use comn::{channel_with_multiplexer, send_outgoing_to_socket, SimpleBufferPool};
    use std::collections::HashMap;
    use turbulence::{BufferPacketPool, Packet};

    let pool = BufferPacketPool::new(SimpleBufferPool(pool_size));
    let mut sockets_incoming = HashMap::with_capacity(100);

    let socket = smol::net::UdpSocket::bind(my_addr)
        .await
        .expect("couldn't bind to address");

    loop {
        let mut packet = comn::acquire_max(&pool);
        match socket.recv_from(&mut packet).await {
            Ok((len, addr)) => {
                let incoming = sockets_incoming.entry(addr).or_insert_with(|| {
                    let (channel, multiplexer) = channel_with_multiplexer(pool.clone());
                    let (incoming, outgoing) = multiplexer.start();
                    send_outgoing_to_socket(outgoing, socket.clone(), addr);
                    client_tx.send(channel).unwrap();
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
