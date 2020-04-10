use std::mem;

use byteorder::{ByteOrder, LittleEndian};
use futures::{
    channel::mpsc::{Receiver, Sender},
    SinkExt, StreamExt,
};
use thiserror::Error;

use crate::{
    packet::BufferPool,
    packet_multiplexer::{MuxPacket, MuxPacketPool},
};

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

/// Turns a stream of unreliable, unordered packets into a stream of unreliable, unordered messages.
pub struct UnreliableChannel<P>
where
    P: BufferPool,
{
    packet_pool: MuxPacketPool<P>,
    incoming_packets: Receiver<MuxPacket<P::Buffer>>,
    outgoing_packets: Sender<MuxPacket<P::Buffer>>,
    out_packet: MuxPacket<P::Buffer>,
    in_packet: Option<(MuxPacket<P::Buffer>, usize)>,
}

impl<P> UnreliableChannel<P>
where
    P: BufferPool,
{
    pub fn new(
        packet_pool: MuxPacketPool<P>,
        incoming: Receiver<MuxPacket<P::Buffer>>,
        outgoing: Sender<MuxPacket<P::Buffer>>,
    ) -> Self {
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
        let start = self.out_packet.len();
        if self.out_packet.capacity() - start < msg.len() + 2 {
            self.flush().await?;
        }

        if self.out_packet.capacity() - start < msg.len() + 2 {
            return Err(SendError::TooBig);
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
