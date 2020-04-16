use std::{convert::TryInto, mem};

use byteorder::{ByteOrder, LittleEndian};
use futures::{
    channel::mpsc::{Receiver, Sender},
    SinkExt, StreamExt,
};
use thiserror::Error;

use crate::packet::{Packet, PacketPool, MAX_PACKET_LEN};

/// The maximum possible message length of an `UnreliableChannel` message, based on the
/// `MAX_PACKET_LEN`.
pub const MAX_MESSAGE_LEN: usize = MAX_PACKET_LEN - 2;

#[derive(Debug, Error)]
pub enum SendError {
    #[error("outgoing packet stream has been disconnected")]
    Disconnected,
    #[error("message length is larger than the maximum packet size")]
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
    P: PacketPool,
{
    packet_pool: P,
    incoming_packets: Receiver<P::Packet>,
    outgoing_packets: Sender<P::Packet>,
    out_packet: P::Packet,
    in_packet: Option<(P::Packet, usize)>,
}

impl<P> UnreliableChannel<P>
where
    P: PacketPool,
{
    pub fn new(packet_pool: P, incoming: Receiver<P::Packet>, outgoing: Sender<P::Packet>) -> Self {
        let out_packet = packet_pool.acquire();
        UnreliableChannel {
            packet_pool,
            incoming_packets: incoming,
            outgoing_packets: outgoing,
            out_packet,
            in_packet: None,
        }
    }

    /// Write the given message to the channel.
    ///
    /// Messages are coalesced into larger packets before being sent, so in order to guarantee that
    /// the message is actually sent, you must call `flush`.
    pub async fn send(&mut self, msg: &[u8]) -> Result<(), SendError> {
        let msg_len: u16 = msg.len().try_into().map_err(|_| SendError::TooBig)?;

        let start = self.out_packet.len();
        if self.out_packet.capacity() - start < msg_len as usize + 2 {
            self.flush().await?;

            if self.out_packet.capacity() < msg_len as usize + 2 {
                return Err(SendError::TooBig);
            }
        }

        let mut len = [0; 2];
        LittleEndian::write_u16(&mut len, msg_len);
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
