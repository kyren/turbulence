use turbulence::MessageChannels;

mod comn;
use comn::Chat;

fn main() {
    let mut channel = direct_socket(comn::CLIENT, comn::SERVER, 1024);
    let input = message_input_channel();

    // This loop is supposed to model a game loop like you know and love.
    // Each iteration through it can be thought of as "one frame", although
    // we're not blocking on vsync or anything like that, so it runs as
    // quickly as your CPU can handle.
    loop {
        // Here we check for any pending messages from the server and process them all.
        // We simply print them here, but you'll probably want to use them to modify your
        // Client application's state.
        while let Some(Chat(m)) = channel.recv() {
            println!("from server: {}", m);
        }

        // Here we poll for inputs from the command line, and send them, if any, to the server.
        while let Ok(msg) = input.try_recv() {
            match channel.send(Chat(msg)) {
                // You can read more about rejected messages in MessageChannel's documentation.
                Some(Chat(rejected)) => println!("couldn't send {}", rejected),
                None => println!("sent!"),
            }
        }
    }
}

// Spawns a background thread that turns command line input into events send down a smol::channel.
// Checking for events from the Receiver this returns allows us to simulate polling for keyboard
// inputs every frame without pulling in any dependencies.
fn message_input_channel() -> smol::channel::Receiver<String> {
    let (tx, rx) = smol::channel::unbounded();

    smol::spawn(async move {
        loop {
            println!("Enter Chat Message: ");
            let message = smol::unblock(|| {
                let mut msg = String::new();
                std::io::stdin().read_line(&mut msg).unwrap();
                msg.trim().to_string()
            })
            .await;
            if !message.is_empty() {
                tx.send(message).await.unwrap();
            }
        }
    })
    .detach();

    rx
}

// Returns a MessageChannels corresponding to a UDP socket that only accepts messages from,
// and sends messages to, a single address.
fn direct_socket(
    my_addr: &'static str,
    remote_addr: &'static str,
    pool_size: usize,
) -> MessageChannels {
    use comn::{channel_with_multiplexer, send_outgoing_to_socket, SimpleBufferPool};
    use turbulence::{BufferPacketPool, Packet};

    let pool = BufferPacketPool::new(SimpleBufferPool(pool_size));
    let (channel, multiplexer) = channel_with_multiplexer(pool.clone());

    let socket = smol::block_on(async {
        let s = smol::net::UdpSocket::bind(my_addr)
            .await
            .expect("couldn't bind to address");
        s.connect(remote_addr)
            .await
            .expect("connect function failed");
        s
    });

    let (mut incoming, outgoing) = multiplexer.start();
    send_outgoing_to_socket(outgoing, socket.clone(), remote_addr.parse().unwrap());

    smol::spawn(async move {
        loop {
            let mut packet = comn::acquire_max(&pool);
            match socket.recv(&mut packet).await {
                Ok(len) => {
                    packet.truncate(len);
                    if let Err(e) = incoming.try_send(packet) {
                        println!("couldn't send packet: {}", e);
                    }
                }
                Err(e) => println!("couldn't recieve packet from UDP socket: {}", e),
            };
        }
    })
    .detach();

    channel
}
