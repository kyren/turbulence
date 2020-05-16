use std::{
    sync::{
        atomic::{self, AtomicBool},
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
/// You can look at this as a specialized version a bounded channel of `()` with capacity 1.
pub fn channel() -> (Sender, Receiver) {
    let state = Arc::new(State {
        waker: AtomicWaker::new(),
        signaled: AtomicBool::new(false),
    });

    let sender_state = Arc::clone(&state);

    (Sender(sender_state), Receiver(state))
}

#[derive(Clone, Debug)]
pub struct Sender(Arc<State>);

impl Sender {
    pub fn signal(&self) {
        self.0.signaled.store(true, atomic::Ordering::SeqCst);
        self.0.waker.wake()
    }
}

#[derive(Debug)]
pub struct Receiver(Arc<State>);

impl Receiver {
    pub async fn wait(&mut self) {
        future::poll_fn(|cx| {
            self.0.waker.register(cx.waker());
            if self.0.signaled.swap(false, atomic::Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

#[derive(Debug)]
struct State {
    waker: AtomicWaker,
    signaled: AtomicBool,
}
