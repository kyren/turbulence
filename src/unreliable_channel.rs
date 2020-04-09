use std::mem;

use byteorder::{ByteOrder, LittleEndian};
use futures::{
    channel::mpsc::{Receiver, Sender},
    SinkExt, StreamExt,
};
use thiserror::Error;

use crate::packet_multiplexer::{MuxPacket, MuxPacketPool, MAX_MUX_PACKET_LEN};

#[derive(Debug, Error)]
pub enum SendError {
    #[error("outgoing packet stream has been disconnected")]
    Disconnected,
    #[error("message length is larger than the largest supported")]
    TooBig,
}

#[derive(Debug, Error)]
pub enum RecvError {
    #[error("incoming packet stream has been disconnected")]
    Disconnected,
    #[error("incoming packet has bad message format")]
    BadFormat,
    #[error("message length is larger than the provided buffer")]
    TooBig,
}

pub const MAX_MESSAGE_LEN: usize = MAX_MUX_PACKET_LEN - 2;

/// Turns a stream of unreliable, unordered packets into a stream of unreliable, unordered messages.
///
/// This assumes that the packets are delivered unreliably and unordered, but it does no checking
/// for corruption: it assumes that if packets arrive that they are intact and uncorrupted.
pub struct UnreliableChannel {
    packet_pool: MuxPacketPool,
    incoming_packets: Receiver<MuxPacket>,
    outgoing_packets: Sender<MuxPacket>,
    out_packet: MuxPacket,
    in_packet: Option<(MuxPacket, usize)>,
}

impl UnreliableChannel {
    pub fn new(
        packet_pool: MuxPacketPool,
        incoming: Receiver<MuxPacket>,
        outgoing: Sender<MuxPacket>,
    ) -> UnreliableChannel {
        let out_packet = packet_pool.acquire();
        UnreliableChannel {
            packet_pool,
            incoming_packets: incoming,
            outgoing_packets: outgoing,
            out_packet,
            in_packet: None,
        }
    }

    /// Write the given message, which must be less than `MAX_MESSAGE_LEN` in size.
    ///
    /// Messages are coalesced into larger packets before being sent, so in order to guarantee that
    /// the message is actually sent, you must call `flush`.
    pub async fn send(&mut self, msg: &[u8]) -> Result<(), SendError> {
        if msg.len() > MAX_MESSAGE_LEN {
            return Err(SendError::TooBig);
        }

        let start = self.out_packet.len();
        if MAX_MUX_PACKET_LEN - start < msg.len() + 2 {
            self.flush().await?;
        }

        let mut len = [0; 2];
        LittleEndian::write_u16(&mut len, msg.len() as u16);
        self.out_packet.extend(&len);
        self.out_packet.extend(&msg);

        Ok(())
    }

    /// Finish sending any unsent coalesced packets.
    ///
    /// This *must* be called to guarantee that any sent messages are actually sent to the outgoing
    /// packet stream.
    pub async fn flush(&mut self) -> Result<(), SendError> {
        if !self.out_packet.is_empty() {
            let out_packet = mem::replace(&mut self.out_packet, self.packet_pool.acquire());
            self.outgoing_packets
                .send(out_packet)
                .await
                .map_err(|_| SendError::Disconnected)
        } else {
            Ok(())
        }
    }

    /// Receive a message into the provide buffer.
    ///
    /// If the received message fits into the provided buffer, this will return `Ok(message_len)`,
    /// otherwise it will return `Err(RecvError::TooBig)`.
    pub async fn recv(&mut self, msg: &mut [u8]) -> Result<usize, RecvError> {
        if self.in_packet.is_none() {
            let packet = self
                .incoming_packets
                .next()
                .await
                .ok_or(RecvError::Disconnected)?;
            self.in_packet = Some((packet, 0));
        }
        let (packet, in_pos) = self.in_packet.as_mut().unwrap();

        if *in_pos + 2 > packet.len() {
            return Err(RecvError::BadFormat);
        }
        let length = LittleEndian::read_u16(&packet[*in_pos..*in_pos + 2]) as usize;
        *in_pos += 2;

        if *in_pos + length > packet.len() {
            return Err(RecvError::BadFormat);
        }

        if length > msg.len() {
            return Err(RecvError::TooBig);
        }

        msg[0..length].copy_from_slice(&packet[*in_pos..*in_pos + length]);

        *in_pos += length;

        if *in_pos == packet.len() {
            self.in_packet = None;
        }

        Ok(length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::{channel::mpsc, executor::LocalPool, task::SpawnExt};

    #[test]
    fn test_unreliable_messages() {
        let packet_pool = MuxPacketPool::new();
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        let (asend, arecv) = mpsc::channel(8);
        let (bsend, brecv) = mpsc::channel(8);

        let mut stream1 = UnreliableChannel::new(packet_pool.clone(), arecv, bsend);
        let mut stream2 = UnreliableChannel::new(packet_pool.clone(), brecv, asend);

        async fn send(stream: &mut UnreliableChannel, val: u8, len: usize) {
            let msg1 = vec![val; len];
            stream.send(&msg1).await.unwrap();
            stream.flush().await.unwrap();
        }

        async fn recv(stream: &mut UnreliableChannel, val: u8, len: usize) {
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
}
