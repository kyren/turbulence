use std::{
    future::Future,
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

use crate::{
    packet_multiplexer::{MuxPacket, MuxPacketPool},
    spawn::Spawn,
    timer::Timer,
};

struct TimeState {
    time: u64,
    queue: Vec<(u64, Waker)>,
}

pub struct TestTimer(Arc<Mutex<TimeState>>);

impl TestTimer {
    pub fn new() -> TestTimer {
        TestTimer(Arc::new(Mutex::new(TimeState {
            time: 0,
            queue: Vec::new(),
        })))
    }

    pub fn handle(&self) -> TestTimerHandle {
        TestTimerHandle(Arc::clone(&self.0))
    }

    pub fn advance(&mut self, millis: u64) {
        let mut state = self.0.lock().unwrap();
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
}

#[derive(Clone)]
pub struct TestTimerHandle(Arc<Mutex<TimeState>>);

async fn do_delay(state: Arc<Mutex<TimeState>>, duration: Duration) -> u64 {
    // Our timer requires manual advancing, so delays should never spawn arrived so we don't starve
    // the code that manually advances the time.
    let arrival = state.lock().unwrap().time + (duration.as_millis() as u64).max(1);
    future::poll_fn(move |cx| -> Poll<u64> {
        let mut state = state.lock().unwrap();
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

impl Timer for TestTimerHandle {
    type Instant = u64;
    type Delay = Pin<Box<dyn Future<Output = ()> + Send>>;
    type Interval = Pin<Box<dyn Stream<Item = u64> + Send>>;

    fn now(&self) -> Self::Instant {
        self.0.lock().unwrap().time
    }

    fn elapsed(&self, instant: Self::Instant) -> Duration {
        Duration::from_millis(self.0.lock().unwrap().time - instant)
    }

    fn duration_between(&self, earlier: Self::Instant, later: Self::Instant) -> Duration {
        Duration::from_millis(later - earlier)
    }

    fn delay(&self, duration: Duration) -> Self::Delay {
        let state = Arc::clone(&self.0);
        Box::pin(async move {
            do_delay(state, duration).await;
        })
    }

    fn interval(&self, duration: Duration) -> Self::Interval {
        Box::pin(stream::unfold(Arc::clone(&self.0), move |state| {
            async move {
                let time = do_delay(Arc::clone(&state), duration).await;
                Some((time, state))
            }
        }))
    }
}

#[derive(Clone, Copy)]
pub struct LinkCondition {
    pub loss: f64,
    pub duplicate: f64,
    pub delay: Duration,
    pub jitter: Duration,
}

pub fn condition_link(
    condition: LinkCondition,
    spawn: impl Spawn + Clone + Send + 'static,
    timer: impl Timer,
    pool: MuxPacketPool,
    mut rng: impl Rng + Send + 'static,
    mut incoming: mpsc::Receiver<MuxPacket>,
    outgoing: mpsc::Sender<MuxPacket>,
) {
    spawn.spawn({
        let spawn = spawn.clone();
        async move {
            loop {
                match incoming.next().await {
                    Some(packet) => {
                        if rng.gen::<f64>() > condition.loss {
                            if rng.gen::<f64>() <= condition.duplicate {
                                spawn.spawn({
                                    let timer = timer.clone();
                                    let mut outgoing = outgoing.clone();
                                    let delay = Duration::from_secs_f64(
                                        condition.delay.as_secs_f64()
                                            + rng.gen::<f64>() * condition.jitter.as_secs_f64(),
                                    );
                                    let mut dup_packet = pool.acquire();
                                    dup_packet.extend(&packet[..]);
                                    async move {
                                        timer.delay(delay).await;
                                        let _ = outgoing.send(dup_packet).await;
                                    }
                                });
                            }

                            spawn.spawn({
                                let timer = timer.clone();
                                let mut outgoing = outgoing.clone();
                                let delay = Duration::from_secs_f64(
                                    condition.delay.as_secs_f64()
                                        + rng.gen::<f64>() * condition.jitter.as_secs_f64(),
                                );
                                async move {
                                    timer.delay(delay).await;
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

type Incoming = Arc<Mutex<Vec<BoxFuture<'static, ()>>>>;

#[derive(Default)]
pub struct SimpleExecutor {
    pool: FuturesUnordered<BoxFuture<'static, ()>>,
    incoming: Incoming,
}

/// Simple single-threaded scheduler with a `Send` spawner.
#[derive(Clone)]
pub struct SimpleSpawner(Incoming);

impl SimpleExecutor {
    pub fn spawner(&self) -> SimpleSpawner {
        SimpleSpawner(Arc::clone(&self.incoming))
    }

    /// Returns true when all spawned futures are complete.
    pub fn run_until_stalled(&mut self) -> bool {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        loop {
            {
                let mut incoming = self.incoming.lock().unwrap();
                for task in incoming.drain(..) {
                    self.pool.push(task);
                }
            }

            let next = self.pool.poll_next_unpin(&mut cx);

            if self.incoming.lock().unwrap().is_empty() {
                match next {
                    Poll::Pending => return false,
                    Poll::Ready(None) => return true,
                    Poll::Ready(Some(())) => {}
                }
            }
        }
    }
}

impl Spawn for SimpleExecutor {
    fn spawn<F: Future<Output = ()> + Send + 'static>(&self, f: F) {
        self.incoming.lock().unwrap().push(Box::pin(f))
    }
}

impl Spawn for SimpleSpawner {
    fn spawn<F: Future<Output = ()> + Send + 'static>(&self, f: F) {
        self.0.lock().unwrap().push(Box::pin(f))
    }
}
