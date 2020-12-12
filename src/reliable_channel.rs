use std::{
    collections::HashMap,
    future::Future,
    i16,
    num::Wrapping,
    pin::Pin,
    sync::Arc,
    task::{Poll, Waker},
    time::Duration,
    u32,
};

use byteorder::{ByteOrder, LittleEndian};
use futures::{
    channel::mpsc,
    future::{self, Fuse, FusedFuture, RemoteHandle},
    lock::{Mutex, MutexGuard},
    pin_mut, select, FutureExt, StreamExt,
};
use thiserror::Error;

use crate::{
    packet::{Packet, PacketPool},
    runtime::Runtime,
    windows::{stream_gt, AckResult, RecvWindow, SendWindow, StreamPos},
};

/// All reliable channel errors are fatal.  Once any error is returned all further reliable channel
/// method calls will return `Error::Shutdown` errors.
#[derive(Debug, Error)]
pub enum Error {
    #[error("incoming or outgoing packet channel has been disconnected")]
    Disconnected,
    #[error("remote endpoint has violated the reliability protocol")]
    ProtocolError,
    #[error("an error has been encountered that has caused the channel to shutdown")]
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// The target outgoing bandwidth, in bytes / sec.
    ///
    /// This is the target bandwidth usage for all sent packets, not the target bandwidth for the
    /// actual underlying stream.  Both sends and resends (but not currently acks) count against
    /// this bandwidth limit, so this is designed to limit the amount of traffic this channel
    /// produces.
    pub bandwidth: u32,
    /// The size of the incoming ring buffer
    pub recv_window_size: u32,
    /// The size of the outgoing ring buffer
    pub send_window_size: u32,
    /// The maximum available bytes to send in a single burst
    pub burst_bandwidth: u32,
    /// The sending side of a channel will always send a constant amount of bytes more than what it
    /// believes the remote's recv window actually is, to avoid stalling the connection.  This
    /// controls the amount past the recv window which will be sent, and also the initial amount of
    /// data that will be sent when the connection starts up.
    pub init_send: u32,
    /// The transmission task for the channel will wake up at this rate to do resends, if not woken
    /// up to send other data.
    pub resend_time: Duration,
    /// The initial estimate for the RTT.
    pub initial_rtt: Duration,
    /// The maximum reasonable RTT which will be used as an upper bound for packet RTT values.
    pub max_rtt: Duration,
    /// The computed RTT for each received acknowledgment will be mixed with the RTT estimate by
    /// this factor.
    pub rtt_update_factor: f64,
    /// Resends will occur if an acknowledgment is not received within this multiplicative factor of
    /// the estimated RTT.
    pub rtt_resend_factor: f64,
}

/// Turns a stream of unreliable, unordered packets into a reliable in-order stream of data.
///
/// All methods on `ReliableChannel` are always cancel safe, they return immediately once any amount
/// of work is done, so canceling the returned futures makes them have no effect.
pub struct ReliableChannel {
    // TODO: It would be nicer to use `BiLock` once it is stable in `futures`, and would allow
    // `ReliableChannel` to implement `AsyncRead` and `AsyncWrite`.
    shared: Arc<Mutex<Shared>>,
    task: Fuse<RemoteHandle<Error>>,
}

impl ReliableChannel {
    pub fn new<R, P>(
        runtime: R,
        packet_pool: P,
        settings: Settings,
        incoming: mpsc::Receiver<P::Packet>,
        outgoing: mpsc::Sender<P::Packet>,
    ) -> Self
    where
        R: Runtime + 'static,
        P: PacketPool + Send + 'static,
        P::Packet: Send,
    {
        assert!(settings.bandwidth != 0);
        assert!(settings.recv_window_size != 0);
        assert!(settings.recv_window_size != 0);
        assert!(settings.burst_bandwidth != 0);
        assert!(settings.init_send != 0);
        assert!(settings.rtt_update_factor > 0.);
        assert!(settings.rtt_resend_factor > 0.);

        let resend_timer = runtime.sleep(settings.resend_time).fuse();

        let shared = Arc::new(Mutex::new(Shared {
            send_window: SendWindow::new(settings.send_window_size, Wrapping(0)),
            send_ready: None,
            write_ready: None,
            recv_window: RecvWindow::new(settings.recv_window_size, Wrapping(0)),
            read_ready: None,
        }));

        let bandwidth_limiter = BandwidthLimiter::new(
            runtime.clone(),
            settings.bandwidth,
            settings.burst_bandwidth,
            settings.burst_bandwidth,
        );
        let remote_recv_available = settings.init_send;
        let rtt_estimate = settings.initial_rtt.as_secs_f64();

        let task = Task {
            settings,
            runtime: runtime.clone(),
            packet_pool,
            incoming,
            outgoing,
            resend_timer,
            remote_recv_available,
            unacked_ranges: HashMap::new(),
            rtt_estimate,
            bandwidth_limiter,
        };
        let (remote, remote_handle) = {
            let shared = Arc::clone(&shared);
            async move { task.main_loop(shared).await.unwrap_err() }
        }
        .remote_handle();

        runtime.spawn(remote);

        ReliableChannel {
            shared,
            task: remote_handle.fuse(),
        }
    }

    /// Write the given data to the reliable channel and return once any nonzero amount of data has
    /// been written.
    ///
    /// In order to ensure that data is written to the channel in a timely manner,
    /// `ReliableChannel::flush` must be called.
    pub async fn write(&mut self, data: &[u8]) -> Result<usize, Error> {
        if self.task.is_terminated() {
            return Err(Error::Shutdown);
        }

        let shared = &self.shared;
        let mut shared_lock_future = shared.lock();
        let mut write_done =
            future::poll_fn(|cx| match Pin::new(&mut shared_lock_future).poll(cx) {
                Poll::Ready(mut shared_guard) => {
                    let len = shared_guard.send_window.write(data);
                    if len > 0 {
                        Poll::Ready(len)
                    } else {
                        shared_guard.write_ready = Some(cx.waker().clone());
                        if let Some(send_ready) = shared_guard.send_ready.take() {
                            send_ready.wake();
                        }
                        shared_lock_future = shared.lock();
                        Poll::Pending
                    }
                }
                Poll::Pending => Poll::Pending,
            })
            .fuse();

        select! {
            len = write_done => Ok(len),
            error = &mut self.task => Err(error),
        }
    }

    /// Ensure that any previously written data is sent in a timely manner.
    ///
    /// Returns once the sending task has been notified to wake up and will send the written data
    /// promptly.  Does *not* actually wait for outgoing packets to be sent before returning.
    pub async fn flush(&mut self) -> Result<(), Error> {
        if self.task.is_terminated() {
            return Err(Error::Shutdown);
        }

        let mut shared = self.shared.lock().await;
        if let Some(send_ready) = shared.send_ready.take() {
            send_ready.wake();
        }
        Ok(())
    }

    /// Read any available data.  Returns once at least one byte of data has been read.
    pub async fn read(&mut self, data: &mut [u8]) -> Result<usize, Error> {
        if self.task.is_terminated() {
            return Err(Error::Shutdown);
        }

        let shared = &self.shared;
        let mut shared_lock_future = shared.lock();
        let mut read_done =
            future::poll_fn(|cx| match Pin::new(&mut shared_lock_future).poll(cx) {
                Poll::Ready(mut shared_guard) => {
                    let len = shared_guard.recv_window.read(data);
                    if len > 0 {
                        Poll::Ready(len)
                    } else {
                        shared_guard.read_ready = Some(cx.waker().clone());
                        shared_lock_future = shared.lock();
                        Poll::Pending
                    }
                }
                Poll::Pending => Poll::Pending,
            })
            .fuse();

        select! {
            len = read_done => Ok(len),
            error = &mut self.task => Err(error),
        }
    }
}

struct Shared {
    send_window: SendWindow,
    send_ready: Option<Waker>,
    write_ready: Option<Waker>,

    recv_window: RecvWindow,
    read_ready: Option<Waker>,
}

struct UnackedRange<I> {
    start: StreamPos,
    end: StreamPos,
    last_sent: Option<I>,
    retransmit: bool,
}

struct Task<R, P>
where
    R: Runtime,
    P: PacketPool,
{
    runtime: R,
    settings: Settings,
    packet_pool: P,
    incoming: mpsc::Receiver<P::Packet>,
    outgoing: mpsc::Sender<P::Packet>,

    resend_timer: Fuse<R::Sleep>,
    remote_recv_available: u32,
    unacked_ranges: HashMap<StreamPos, UnackedRange<R::Instant>>,
    rtt_estimate: f64,
    bandwidth_limiter: BandwidthLimiter<R>,
}

impl<R, P> Task<R, P>
where
    R: Runtime,
    P: PacketPool,
{
    async fn main_loop(mut self, shared: Arc<Mutex<Shared>>) -> Result<(), Error> {
        loop {
            enum WakeReason<'a, P> {
                ResendTimer,
                IncomingPacket(P),
                SendAvailable(MutexGuard<'a, Shared>),
            }

            self.bandwidth_limiter.update_available();

            let wake_reason = {
                let bandwidth_limiter = &self.bandwidth_limiter;
                let resend_timer = &mut self.resend_timer;

                let resend_timer = async {
                    if !resend_timer.is_terminated() {
                        resend_timer.await;
                    }
                    // Don't bother waking up for the resend timer until we have bandwidth available
                    // to do resends.
                    bandwidth_limiter.delay_until_available().await;
                }
                .fuse();
                pin_mut!(resend_timer);

                let remote_recv_available = self.remote_recv_available;
                let send_available = async {
                    if remote_recv_available == 0 {
                        // Don't wake up at all for sending new data if we couldn't send anything
                        // anyway.
                        future::pending::<()>().await;
                    }

                    // Don't wake up for sending new data until we have bandwidth available.
                    bandwidth_limiter.delay_until_available().await;

                    let mut shared_lock_future = shared.lock();
                    future::poll_fn(|cx| match Pin::new(&mut shared_lock_future).poll(cx) {
                        Poll::Ready(mut shared_guard) => {
                            if shared_guard.send_window.send_available() > 0 {
                                Poll::Ready(shared_guard)
                            } else {
                                shared_guard.send_ready = Some(cx.waker().clone());
                                shared_lock_future = shared.lock();
                                Poll::Pending
                            }
                        }
                        Poll::Pending => Poll::Pending,
                    })
                    .await
                }
                .fuse();
                pin_mut!(send_available);

                select! {
                    _ = resend_timer => WakeReason::ResendTimer,
                    incoming_packet = self.incoming.next() => {
                        WakeReason::IncomingPacket(incoming_packet.ok_or(Error::Disconnected)?)
                    },
                    shared = send_available => WakeReason::SendAvailable(shared),
                }
            };

            self.bandwidth_limiter.update_available();

            match wake_reason {
                WakeReason::ResendTimer => {
                    let mut shared = shared.lock().await;
                    self.resend(&mut *shared).await?;
                    self.resend_timer = self.runtime.sleep(self.settings.resend_time).fuse();
                }
                WakeReason::IncomingPacket(packet) => {
                    let mut shared = shared.lock().await;
                    self.recv_packet(&mut *shared, packet).await?;
                }
                WakeReason::SendAvailable(mut shared) => {
                    // We should use available bandwidth for resends before sending, to avoid
                    // starving resends
                    self.resend(&mut *shared).await?;
                    self.resend_timer = self.runtime.sleep(self.settings.resend_time).fuse();

                    self.send(&mut *shared).await?;
                }
            }

            // Don't let the connection stall.  If we are now out of unacked ranges to resend and we
            // believe the remote has no recv left, we will receive no acknowledgments to let us
            // update the remote receive window.  Keep sending a small amount of data past the
            // remote receive window, even if it is unacked, so that we are notified when the remote
            // starts processing data again.
            if self.unacked_ranges.is_empty() && self.remote_recv_available == 0 {
                self.remote_recv_available = self.settings.init_send;
            }
        }
    }

    // Send any data available to send, if we have the bandwidth for it
    async fn send(&mut self, shared: &mut Shared) -> Result<(), Error> {
        if !self.bandwidth_limiter.bytes_available() {
            return Ok(());
        }

        let send_amt = (shared.send_window.send_available())
            .min(self.remote_recv_available)
            .min(i16::MAX as u32);

        if send_amt == 0 {
            return Ok(());
        }

        let mut packet = self.packet_pool.acquire();
        let send_amt = send_amt.min((packet.capacity() - 6) as u32);

        packet.resize(6 + send_amt as usize, 0);

        let (start, end) = shared.send_window.send(&mut packet[6..]).unwrap();
        assert_eq!((end - start).0, send_amt);

        LittleEndian::write_i16(&mut packet[0..2], send_amt as i16);
        LittleEndian::write_u32(&mut packet[2..6], start.0);

        self.unacked_ranges.insert(
            start,
            UnackedRange {
                start,
                end,
                last_sent: Some(self.runtime.now()),
                retransmit: false,
            },
        );

        self.bandwidth_limiter.take_bytes(packet.len() as u32);
        future::poll_fn(|cx| self.outgoing.poll_ready(cx))
            .await
            .map_err(|_| Error::Disconnected)?;
        self.outgoing
            .start_send(packet)
            .map_err(|_| Error::Disconnected)?;

        self.remote_recv_available -= send_amt;

        Ok(())
    }

    // Resend any data whose retransmit time has been reached, if we have the bandwidth for it
    async fn resend(&mut self, shared: &mut Shared) -> Result<(), Error> {
        for unacked in self.unacked_ranges.values_mut() {
            if !self.bandwidth_limiter.bytes_available() {
                break;
            }

            let resend = if let Some(last_sent) = unacked.last_sent {
                let elapsed = self.runtime.elapsed(last_sent);
                elapsed.as_secs_f64() > self.rtt_estimate * self.settings.rtt_resend_factor
            } else {
                true
            };

            if resend {
                unacked.last_sent = Some(self.runtime.now());
                unacked.retransmit = true;

                let len = (unacked.end - unacked.start).0;

                let mut packet = self.packet_pool.acquire();
                packet.resize(6 + len as usize, 0);
                LittleEndian::write_i16(&mut packet[0..2], len as i16);
                LittleEndian::write_u32(&mut packet[2..6], unacked.start.0);

                shared
                    .send_window
                    .get_unacked(unacked.start, &mut packet[6..]);

                self.bandwidth_limiter.take_bytes(packet.len() as u32);

                let outgoing = &mut self.outgoing;
                future::poll_fn(|cx| outgoing.poll_ready(cx))
                    .await
                    .map_err(|_| Error::Disconnected)?;
                outgoing
                    .start_send(packet)
                    .map_err(|_| Error::Disconnected)?;
            }
        }

        Ok(())
    }

    // Receive the given packet and respond with an acknowledgment packet, ignoring bandwidth
    // limits.
    async fn recv_packet(&mut self, shared: &mut Shared, packet: P::Packet) -> Result<(), Error> {
        if packet.len() < 2 {
            return Err(Error::ProtocolError);
        }

        let data_len = LittleEndian::read_i16(&packet[0..2]);
        if data_len < 0 {
            if packet.len() != 10 {
                return Err(Error::ProtocolError);
            }

            let start_pos = Wrapping(LittleEndian::read_u32(&packet[2..6]));
            let end_pos = start_pos + Wrapping(-data_len as u32);
            let recv_window_end = Wrapping(LittleEndian::read_u32(&packet[6..10]));

            if stream_gt(&recv_window_end, &shared.send_window.send_pos()) {
                let old_remote_recv_available = self.remote_recv_available;
                self.remote_recv_available = self
                    .remote_recv_available
                    .max((recv_window_end - shared.send_window.send_pos()).0);

                if self.remote_recv_available != 0 && old_remote_recv_available == 0 {
                    // If we now believe the remote is newly ready to receive data, go ahead and
                    // send it.
                    self.send(shared).await?;
                }
            }

            let acked_range = match shared.send_window.ack_range(start_pos, end_pos) {
                AckResult::NotFound => None,
                AckResult::InvalidRange => {
                    return Err(Error::ProtocolError);
                }
                AckResult::Ack => {
                    let acked = self.unacked_ranges.remove(&start_pos).unwrap();
                    assert_eq!(acked.end, end_pos);
                    Some(acked)
                }
                AckResult::PartialAck(nacked_end) => {
                    let mut acked = self.unacked_ranges.remove(&start_pos).unwrap();
                    assert_eq!(acked.end, nacked_end);
                    acked.end = end_pos;
                    self.unacked_ranges.insert(
                        end_pos,
                        UnackedRange {
                            start: end_pos,
                            end: nacked_end,
                            last_sent: None,
                            retransmit: true,
                        },
                    );
                    Some(acked)
                }
            };

            if let Some(acked_range) = acked_range {
                // Only update the RTT estimation for acked ranges that did not need to be
                // retransmitted, otherwise we do not know which packet is being acked and thus
                // can't be sure of the actual RTT for this ack.
                if !acked_range.retransmit {
                    if let Some(last_sent) = acked_range.last_sent {
                        let rtt = self
                            .runtime
                            .elapsed(last_sent)
                            .min(self.settings.max_rtt)
                            .as_secs_f64();
                        self.rtt_estimate +=
                            (rtt - self.rtt_estimate) * self.settings.rtt_update_factor;
                    }
                }

                if shared.send_window.write_available() > 0 {
                    if let Some(write_ready) = shared.write_ready.take() {
                        write_ready.wake();
                    }
                }
            }
        } else {
            if packet.len() < 6 {
                return Err(Error::ProtocolError);
            }

            let start_pos = Wrapping(LittleEndian::read_u32(&packet[2..6]));
            if data_len as usize != packet.len() - 6 {
                return Err(Error::ProtocolError);
            }

            if let Some(end_pos) = shared.recv_window.recv(start_pos, &packet[6..]) {
                let mut ack_packet = self.packet_pool.acquire();
                ack_packet.resize(10, 0);
                let ack_len = (end_pos - start_pos).0 as i16;
                LittleEndian::write_i16(&mut ack_packet[0..2], -ack_len);
                LittleEndian::write_u32(&mut ack_packet[2..6], start_pos.0);
                LittleEndian::write_u32(&mut ack_packet[6..10], shared.recv_window.window_end().0);

                // We currently do not count acknowledgement packets against the outgoing bandwidth
                // at all.
                future::poll_fn(|cx| self.outgoing.poll_ready(cx))
                    .await
                    .map_err(|_| Error::Disconnected)?;
                self.outgoing
                    .start_send(ack_packet)
                    .map_err(|_| Error::Disconnected)?;

                if shared.recv_window.read_available() > 0 {
                    if let Some(read_ready) = shared.read_ready.take() {
                        read_ready.wake();
                    }
                }
            }
        }

        Ok(())
    }
}

struct BandwidthLimiter<R: Runtime> {
    runtime: R,
    bandwidth: u32,
    burst_bandwidth: u32,
    bytes_available: f64,
    last_calculation: R::Instant,
}

impl<R: Runtime> BandwidthLimiter<R> {
    fn new(
        runtime: R,
        bandwidth: u32,
        burst_bandwidth: u32,
        initial_bytes: u32,
    ) -> BandwidthLimiter<R> {
        let last_calculation = runtime.now();
        BandwidthLimiter {
            runtime,
            bandwidth,
            burst_bandwidth,
            bytes_available: initial_bytes as f64,
            last_calculation,
        }
    }

    // Delay until a time where there will be bandwidth available
    async fn delay_until_available(&self) {
        if self.bytes_available < 0. {
            self.runtime
                .sleep(Duration::from_secs_f64(
                    (-self.bytes_available) / self.bandwidth as f64,
                ))
                .await;
        }
    }

    // Actually update the amount of available bandwidth.  Additional available bytes are not added
    // until this method is called to add them.
    fn update_available(&mut self) {
        let now = self.runtime.now();
        self.bytes_available += self
            .runtime
            .duration_between(self.last_calculation, now)
            .as_secs_f64()
            * self.bandwidth as f64;
        self.bytes_available = self.bytes_available.min(self.burst_bandwidth as f64);
        self.last_calculation = now;
    }

    // The bandwidth limiter only needs to limit outgoing packets being sent at all, not their size,
    // so this returns true if a non-negative amount of bytes is available.  If a packet is sent
    // that is larger than the available bytes, the available bytes will go negative and this will
    // no longer return true.
    fn bytes_available(&self) -> bool {
        self.bytes_available >= 0.
    }

    // Record that bytes were sent, possibly going into bandwidth debt.
    fn take_bytes(&mut self, bytes: u32) {
        self.bytes_available -= bytes as f64
    }
}
