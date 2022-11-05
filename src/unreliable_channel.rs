use std::{
    convert::TryInto,
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};

use byteorder::{ByteOrder, LittleEndian};
use futures::{future, ready, SinkExt, StreamExt};
use thiserror::Error;

use crate::{
    bandwidth_limiter::BandwidthLimiter,
    packet::{Packet, PacketPool, MAX_PACKET_LEN},
    runtime::Runtime,
    spsc,
};

/// The maximum possible message length of an `UnreliableChannel` message for the largest possible
/// packet, based on the `MAX_PACKET_LEN`.
pub const MAX_MESSAGE_LEN: u16 = MAX_PACKET_LEN - 2;

#[derive(Debug, Error)]
/// Fatal error due to channel disconnection.
#[error("incoming or outgoing packet channel has been disconnected")]
pub struct Disconnected;

#[derive(Debug, Error)]
pub enum SendError {
    #[error(transparent)]
    Disconnected(#[from] Disconnected),
    /// Non-fatal error, message is unsent.
    #[error("sent message is larger than the maximum packet size")]
    TooBig,
}

#[derive(Debug, Error)]
pub enum RecvError {
    #[error(transparent)]
    Disconnected(#[from] Disconnected),
    /// Non-fatal error, the remainder of the incoming packet is dropped.
    #[error("incoming packet has bad message format")]
    BadFormat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The target outgoing bandwidth, in bytes / sec.
    pub bandwidth: u32,
    /// The maximum amount of bandwidth credit that can accumulate. This is the maximum bytes that
    /// will be sent in a single burst.
    pub burst_bandwidth: u32,
}

/// Turns a stream of unreliable, unordered packets into a stream of unreliable, unordered messages.
pub struct UnreliableChannel<R, P>
where
    R: Runtime,
    P: PacketPool,
{
    packet_pool: P,
    bandwidth_limiter: BandwidthLimiter<R>,
    incoming_packets: spsc::Receiver<P::Packet>,
    outgoing_packets: spsc::Sender<P::Packet>,
    out_packet: P::Packet,
    in_packet: Option<(P::Packet, usize)>,
    delay_until_available: Pin<Box<Option<R::Sleep>>>,
}

impl<R, P> UnreliableChannel<R, P>
where
    R: Runtime,
    P: PacketPool,
{
    pub fn new(
        runtime: R,
        mut packet_pool: P,
        settings: Settings,
        incoming: spsc::Receiver<P::Packet>,
        outgoing: spsc::Sender<P::Packet>,
    ) -> Self {
        let out_packet = packet_pool.acquire();
        UnreliableChannel {
            packet_pool,
            bandwidth_limiter: BandwidthLimiter::new(
                runtime,
                settings.bandwidth,
                settings.burst_bandwidth,
            ),
            incoming_packets: incoming,
            outgoing_packets: outgoing,
            out_packet,
            in_packet: None,
            delay_until_available: Box::pin(None),
        }
    }

    /// Write the given message to the channel.
    ///
    /// Messages are coalesced into larger packets before being sent, so in order to guarantee that
    /// the message is actually sent, you must call `flush`.
    ///
    /// Messages have a maximum size based on the size of the packets returned from the packet pool.
    /// Two bytes are used to encode the length of the message, so the maximum message length is
    /// `packet.len() - 2`, for whatever packet sizes are returned by the pool.
    ///
    /// This method is cancel safe, it will never partially send a message, and the future will
    /// complete immediately after writing a message.
    pub async fn send(&mut self, msg: &[u8]) -> Result<(), SendError> {
        future::poll_fn(|cx| self.poll_send(cx, msg)).await
    }

    /// Finish sending any unsent coalesced packets.
    ///
    /// This *must* be called to guarantee that any sent messages are actually sent to the outgoing
    /// packet stream.
    ///
    /// This method is cancel safe.
    pub async fn flush(&mut self) -> Result<(), Disconnected> {
        future::poll_fn(|cx| self.poll_flush(cx)).await
    }

    /// Receive a message into the provide buffer.
    ///
    /// If the received message fits into the provided buffer, this will return `Ok(message_len)`,
    /// otherwise it will return `Err(RecvError::TooBig)`.
    ///
    /// This method is cancel safe, it will never partially read a message or drop received
    /// messages.
    pub async fn recv<'a>(&'a mut self) -> Result<&'a [u8], RecvError> {
        // We have to split this into two separate function calls due to lifetime limitations of
        // `poll_fn`.
        future::poll_fn(|cx| self.poll_recv_packet(cx)).await?;
        self.next_msg()
    }

    pub fn poll_send(&mut self, cx: &mut Context, msg: &[u8]) -> Poll<Result<(), SendError>> {
        let msg_len: u16 = msg.len().try_into().map_err(|_| SendError::TooBig)?;

        let start = self.out_packet.len();
        if self.out_packet.capacity() - start < msg_len as usize + 2 {
            ready!(self.poll_flush(cx))?;

            if self.out_packet.capacity() < msg_len as usize + 2 {
                return Poll::Ready(Err(SendError::TooBig));
            }
        }

        let mut len = [0; 2];
        LittleEndian::write_u16(&mut len, msg_len);
        self.out_packet.extend(&len);
        self.out_packet.extend(&msg);

        Poll::Ready(Ok(()))
    }

    pub fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), Disconnected>> {
        if self.out_packet.is_empty() {
            return Poll::Ready(Ok(()));
        }

        if self.delay_until_available.is_none() {
            self.bandwidth_limiter.update_available();
            if let Some(delay) = self.bandwidth_limiter.delay_until_available() {
                self.delay_until_available.set(Some(delay));
            }
        }

        if let Some(delay) = self.delay_until_available.as_mut().as_pin_mut() {
            ready!(delay.poll(cx));
            self.delay_until_available.set(None);
        }

        ready!(self.outgoing_packets.poll_ready_unpin(cx)).map_err(|_| Disconnected)?;

        let out_packet = mem::replace(&mut self.out_packet, self.packet_pool.acquire());
        self.bandwidth_limiter.take_bytes(out_packet.len() as u32);
        self.outgoing_packets
            .start_send_unpin(out_packet)
            .map_err(|_| Disconnected)?;

        self.outgoing_packets
            .poll_flush_unpin(cx)
            .map_err(|_| Disconnected)
    }

    pub fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<&[u8], RecvError>> {
        ready!(self.poll_recv_packet(cx))?;
        Poll::Ready(self.next_msg())
    }

    // Make sure we are ready to call `next_msg`. Split into two methods due to poll_fn lifetime
    // limitations (which also prevents implementing a custom `Future`).
    fn poll_recv_packet(&mut self, cx: &mut Context) -> Poll<Result<(), RecvError>> {
        if let Some((packet, in_pos)) = &self.in_packet {
            if *in_pos == packet.len() {
                self.in_packet = None;
            }
        }

        if self.in_packet.is_none() {
            let packet = ready!(self.incoming_packets.poll_next_unpin(cx)).ok_or(Disconnected)?;
            self.in_packet = Some((packet, 0));
        }

        Poll::Ready(Ok(()))
    }

    // Must call `poll_recv_packet` beforehand until it completes.
    fn next_msg(&mut self) -> Result<&[u8], RecvError> {
        let (packet, in_pos) = self.in_packet.as_mut().unwrap();
        assert_ne!(*in_pos, packet.len());

        if *in_pos + 2 > packet.len() {
            *in_pos = packet.len();
            return Err(RecvError::BadFormat);
        }
        let length = LittleEndian::read_u16(&packet[*in_pos..*in_pos + 2]) as usize;
        *in_pos += 2;

        if *in_pos + length > packet.len() {
            *in_pos = packet.len();
            return Err(RecvError::BadFormat);
        }

        let msg = &packet[*in_pos..*in_pos + length];
        *in_pos += length;

        Ok(msg)
    }
}
