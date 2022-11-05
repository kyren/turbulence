use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

pub use crossbeam_channel::{TryRecvError, TrySendError};
use futures::{task::AtomicWaker, Sink, Stream};
use thiserror::Error;

#[derive(Default)]
struct Shared {
    send_ready: AtomicWaker,
    recv_ready: AtomicWaker,
}

pub struct Receiver<T> {
    channel: crossbeam_channel::Receiver<T>,
    shared: Arc<Shared>,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.send_ready.wake();
    }
}

impl<T> Unpin for Receiver<T> {}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<T>> {
        match self.try_recv() {
            Ok(r) => Poll::Ready(Some(r)),
            Err(TryRecvError::Disconnected) => Poll::Ready(None),
            Err(TryRecvError::Empty) => {
                self.shared.recv_ready.register(cx.waker());
                match self.try_recv() {
                    Ok(r) => Poll::Ready(Some(r)),
                    Err(TryRecvError::Disconnected) => Poll::Ready(None),
                    Err(TryRecvError::Empty) => Poll::Pending,
                }
            }
        }
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let t = self.channel.try_recv()?;
        self.shared.send_ready.wake();
        Ok(t)
    }
}

#[derive(Debug, Error)]
#[error("spsc channel disconnected")]
pub struct Disconnected;

pub struct Sender<T> {
    channel: crossbeam_channel::Sender<T>,
    shared: Arc<Shared>,
    slot: Option<T>,
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.shared.recv_ready.wake()
    }
}

impl<T> Unpin for Sender<T> {}

impl<T> Sink<T> for Sender<T> {
    type Error = Disconnected;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        if let Some(t) = self.slot.take() {
            match self.try_send(t) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(TrySendError::Disconnected(_)) => Poll::Ready(Err(Disconnected)),
                Err(TrySendError::Full(t)) => {
                    self.shared.send_ready.register(cx.waker());
                    match self.try_send(t) {
                        Ok(()) => Poll::Ready(Ok(())),
                        Err(TrySendError::Disconnected(_)) => Poll::Ready(Err(Disconnected)),
                        Err(TrySendError::Full(t)) => {
                            self.slot = Some(t);
                            Poll::Pending
                        }
                    }
                }
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        if self.slot.replace(item).is_some() {
            panic!("start_send called without without being ready");
        }
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.poll_ready(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.poll_flush(cx)
    }
}

impl<T> Sender<T> {
    pub fn try_send(&mut self, t: T) -> Result<(), TrySendError<T>> {
        if let Some(prev) = self.slot.take() {
            if let Err(err) = self.channel.try_send(prev) {
                match err {
                    TrySendError::Full(inner) => {
                        self.slot = Some(inner);
                        return Err(TrySendError::Full(t));
                    }
                    TrySendError::Disconnected(inner) => {
                        self.slot = Some(inner);
                        return Err(TrySendError::Disconnected(t));
                    }
                }
            } else {
                self.shared.recv_ready.wake();
            }
        }
        self.channel.try_send(t)?;
        self.shared.recv_ready.wake();
        Ok(())
    }
}

pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = crossbeam_channel::bounded(capacity);
    let shared = Arc::new(Shared::default());

    (
        Sender {
            channel: sender,
            shared: shared.clone(),
            slot: None,
        },
        Receiver {
            channel: receiver,
            shared,
        },
    )
}
