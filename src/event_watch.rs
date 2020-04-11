use std::{
    sync::{
        atomic::{self, AtomicU64},
        Arc,
    },
    task::Poll,
};

use futures::{
    future::{self},
    task::AtomicWaker,
};

/// Creates a multi-producer single-consumer stream of events with certain beneficial properties.
///
/// This is sort of similar to `futures::channel::mspc::unbounded::<()>` or
/// `tokio::sync::watch::channel::<()>` but slightly different.
///
/// If a receiver is waiting on a signaled event, calling `Sender::signal` will wakeup the receiver
/// as normal.  However, if the receiver is *not* waiting on a signaled event and `Sender::signal`
/// has been called since the last time the `Receiver::wait` was called, then calling
/// `Receiver::wait` again will immediately resolve.  In this way, the receiver is prevented from
/// possibly missing events.
///
/// In other words, calling `Sender::signal` will always do one of two things:
///   1) Wake up a currently waiting receiver
///   2) Make the next call to `Receiver::wait` resolve immediately
///
/// Multiple calls to `Sender::signal` events will however *not* cause *multiple* calls to
/// `Receiver::wait` to resolve immediately, only the very next call to `Receiver::wait`.
///
/// You can look at this as a specialized version of `futures::channel::mpsc::unbounded::<()>`.  It
/// works as if `Sender::siganl` always sends a `()` message over the channel and `Receiver::wait`
/// waits until at least one `()` message is available on the channel, then consumes all available
/// messages before returning.
pub fn channel() -> (Sender, Receiver) {
    let state = Arc::new(State {
        waker: AtomicWaker::new(),
        idx: AtomicU64::new(0),
    });

    let sender_state = Arc::clone(&state);

    (Sender(sender_state), Receiver { state, last_idx: 0 })
}

#[derive(Clone, Debug)]
pub struct Sender(Arc<State>);

impl Sender {
    pub fn signal(&self) {
        self.0.idx.fetch_add(1, atomic::Ordering::SeqCst);
        self.0.waker.wake()
    }
}

#[derive(Debug)]
pub struct Receiver {
    state: Arc<State>,
    last_idx: u64,
}

impl Receiver {
    pub async fn wait(&mut self) {
        future::poll_fn(|cx| {
            let idx = self.state.idx.load(atomic::Ordering::SeqCst);
            if self.last_idx < idx {
                self.last_idx = idx;
                return Poll::Ready(());
            } else {
                self.state.waker.register(cx.waker());
                return Poll::Pending;
            }
        })
        .await
    }
}

#[derive(Debug)]
struct State {
    waker: AtomicWaker,
    idx: AtomicU64,
}
