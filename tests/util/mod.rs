#![allow(unused)]

use std::{
    future::Future,
    ops::Deref,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Duration,
};

use futures::{
    channel::mpsc,
    future::{self, BoxFuture},
    stream::{self, FuturesUnordered},
    task::noop_waker,
    SinkExt, Stream, StreamExt,
};
use rand::Rng;

use turbulence::{
    buffer::BufferPool,
    packet::{Packet, PacketPool},
    packet_multiplexer::{MuxPacket, MuxPacketPool},
    runtime::Runtime,
};

#[derive(Debug, Copy, Clone)]
pub struct SimpleBufferPool(pub usize);

impl BufferPool for SimpleBufferPool {
    type Buffer = Box<[u8]>;

    fn acquire(&self) -> Self::Buffer {
        vec![0; self.0].into_boxed_slice()
    }
}

struct TimeState {
    time: u64,
    queue: Vec<(u64, Waker)>,
}

type IncomingTasks = Mutex<Vec<BoxFuture<'static, ()>>>;

struct HandleInner {
    time_state: Mutex<TimeState>,
    incoming_tasks: IncomingTasks,
}

pub struct SimpleRuntime {
    pool: FuturesUnordered<BoxFuture<'static, ()>>,
    handle: SimpleRuntimeHandle,
}

#[derive(Clone)]
pub struct SimpleRuntimeHandle(Arc<HandleInner>);

impl SimpleRuntime {
    pub fn new() -> Self {
        SimpleRuntime {
            pool: FuturesUnordered::new(),
            handle: SimpleRuntimeHandle(Arc::new(HandleInner {
                time_state: Mutex::new(TimeState {
                    time: 0,
                    queue: Vec::new(),
                }),
                incoming_tasks: IncomingTasks::default(),
            })),
        }
    }

    pub fn handle(&self) -> SimpleRuntimeHandle {
        self.handle.clone()
    }

    pub fn advance_time(&mut self, millis: u64) {
        let mut state = self.handle.0.time_state.lock().unwrap();
        state.time += millis;

        let mut arrived = 0;
        for i in 0..state.queue.len() {
            if state.time >= state.queue[i].0 {
                arrived = i + 1;
            } else {
                break;
            }
        }

        for (_, waker) in state.queue.drain(0..arrived) {
            waker.wake();
        }
    }

    pub fn run_until_stalled(&mut self) -> bool {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        loop {
            {
                let mut incoming = self.handle.0.incoming_tasks.lock().unwrap();
                for task in incoming.drain(..) {
                    self.pool.push(task);
                }
            }

            let next = self.pool.poll_next_unpin(&mut cx);

            if self.handle.0.incoming_tasks.lock().unwrap().is_empty() {
                match next {
                    Poll::Pending => return false,
                    Poll::Ready(None) => return true,
                    Poll::Ready(Some(())) => {}
                }
            }
        }
    }
}

impl Deref for SimpleRuntime {
    type Target = SimpleRuntimeHandle;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

async fn do_delay(state: Arc<HandleInner>, duration: Duration) -> u64 {
    // Our timer requires manual advancing, so delays should never spawn arrived so we don't starve
    // the code that manually advances the time.
    let arrival = state.time_state.lock().unwrap().time + (duration.as_millis() as u64).max(1);
    future::poll_fn(move |cx| -> Poll<u64> {
        let mut state = state.time_state.lock().unwrap();
        if state.time >= arrival {
            Poll::Ready(state.time)
        } else {
            let i = match state.queue.binary_search_by_key(&arrival, |(t, _)| *t) {
                Ok(i) => i,
                Err(i) => i,
            };
            state.queue.insert(i, (arrival, cx.waker().clone()));
            Poll::Pending
        }
    })
    .await
}

impl Runtime for SimpleRuntimeHandle {
    type Instant = u64;
    type Sleep = Pin<Box<dyn Future<Output = ()> + Send>>;

    fn spawn<F: Future<Output = ()> + Send + 'static>(&self, f: F) {
        self.0.incoming_tasks.lock().unwrap().push(Box::pin(f))
    }

    fn now(&self) -> Self::Instant {
        self.0.time_state.lock().unwrap().time
    }

    fn elapsed(&self, instant: Self::Instant) -> Duration {
        Duration::from_millis(self.0.time_state.lock().unwrap().time - instant)
    }

    fn duration_between(&self, earlier: Self::Instant, later: Self::Instant) -> Duration {
        Duration::from_millis(later - earlier)
    }

    fn sleep(&self, duration: Duration) -> Self::Sleep {
        let state = Arc::clone(&self.0);
        Box::pin(async move {
            do_delay(state, duration).await;
        })
    }
}

#[derive(Clone, Copy)]
pub struct LinkCondition {
    pub loss: f64,
    pub duplicate: f64,
    pub delay: Duration,
    pub jitter: Duration,
}

pub fn condition_link<P>(
    condition: LinkCondition,
    runtime: impl Runtime + Clone + Send + 'static,
    pool: P,
    mut rng: impl Rng + Send + 'static,
    mut incoming: mpsc::Receiver<P::Packet>,
    outgoing: mpsc::Sender<P::Packet>,
) where
    P: PacketPool + Send + 'static,
    P::Packet: Send,
{
    runtime.spawn({
        let runtime = runtime.clone();
        async move {
            loop {
                match incoming.next().await {
                    Some(packet) => {
                        if rng.gen::<f64>() > condition.loss {
                            if rng.gen::<f64>() <= condition.duplicate {
                                runtime.spawn({
                                    let runtime = runtime.clone();
                                    let mut outgoing = outgoing.clone();
                                    let delay = Duration::from_secs_f64(
                                        condition.delay.as_secs_f64()
                                            + rng.gen::<f64>() * condition.jitter.as_secs_f64(),
                                    );
                                    let mut dup_packet = pool.acquire();
                                    dup_packet.extend(&packet[..]);
                                    async move {
                                        runtime.sleep(delay).await;
                                        let _ = outgoing.send(dup_packet).await;
                                    }
                                });
                            }

                            runtime.spawn({
                                let runtime = runtime.clone();
                                let mut outgoing = outgoing.clone();
                                let delay = Duration::from_secs_f64(
                                    condition.delay.as_secs_f64()
                                        + rng.gen::<f64>() * condition.jitter.as_secs_f64(),
                                );
                                async move {
                                    runtime.sleep(delay).await;
                                    let _ = outgoing.send(packet).await;
                                }
                            });
                        }
                    }
                    None => break,
                }
            }
        }
    });
}
