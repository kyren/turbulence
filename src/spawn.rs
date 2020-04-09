use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use futures::{channel::oneshot, FutureExt};
use pin_project::pin_project;
use thiserror::Error;

/// A trait for spawning tasks on an executor.
///
/// This is similar to `futures::task::Spawn`, but it is generic in the spawned future, which is
/// better for backends like tokio.
pub trait Spawn {
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static;
}

impl<'a, S: Spawn> Spawn for &'a S {
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        S::spawn(*self, future)
    }
}

#[derive(Debug, Error)]
#[error("future for the given remote handle has been dropped")]
pub struct RemoteDropped;

/// A version of `futures::future::Remote` that has a handle that supports receiving from outside of
/// a task.
#[pin_project]
pub struct RemoteFuture<F: Future> {
    #[pin]
    future: F,
    sender: Option<oneshot::Sender<F::Output>>,
    keep_running: Arc<AtomicBool>,
}

impl<F: Future> RemoteFuture<F> {
    /// Create a future that wraps this future and outputs `()` and delivers its real output to a
    /// remote handle.
    ///
    /// Useful for spawning a future on a separate task and receiving its output.
    ///
    /// Unless `RemoteHandle::forget` is called, the future will be canceled as soon as the reote
    /// handle is dropped.
    pub fn new(future: F) -> (RemoteFuture<F>, RemoteHandle<F::Output>) {
        let (sender, receiver) = oneshot::channel();
        let keep_running = Arc::new(AtomicBool::new(false));
        let remote_future = RemoteFuture {
            future,
            sender: Some(sender),
            keep_running: Arc::clone(&keep_running),
        };
        let remote_handle = RemoteHandle {
            receiver,
            keep_running,
        };
        (remote_future, remote_handle)
    }
}

impl<F: Future> Future for RemoteFuture<F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.project();

        if !this.keep_running.load(Ordering::SeqCst) {
            match this.sender.as_mut().unwrap().poll_canceled(cx) {
                Poll::Ready(()) => return Poll::Ready(()),
                Poll::Pending => {}
            }
        }

        match this.future.poll(cx) {
            Poll::Ready(r) => {
                let _ = this.sender.take().unwrap().send(r);
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A version of `futures::future::RemoteHandle` that supports receiving outside of a task.
pub struct RemoteHandle<T> {
    receiver: oneshot::Receiver<T>,
    keep_running: Arc<AtomicBool>,
}

impl<T> RemoteHandle<T> {
    /// Normally, dropping a `RemoteHandle` cancels the inner future.  This can be called to drop
    /// the `RemoteHandle` without cancelling the inner future.
    pub fn forget(self) {
        self.keep_running.store(true, Ordering::SeqCst);
    }

    /// Receive the output of a `RemoteFuture` outside of the context of a task.
    ///
    /// # Panics
    /// Panics if the `RemoteFuture` has been dropped, usually due to it panicking.
    pub fn recv(&mut self) -> Option<T> {
        self.try_recv().unwrap()
    }

    /// Try and receive the output of the remote future outside the context of a task.
    pub fn try_recv(&mut self) -> Result<Option<T>, RemoteDropped> {
        self.receiver.try_recv().map_err(|_| RemoteDropped)
    }
}

impl<T: Send + 'static> Future for RemoteHandle<T> {
    type Output = Result<T, RemoteDropped>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T, RemoteDropped>> {
        match self.receiver.poll_unpin(cx) {
            Poll::Ready(Ok(output)) => Poll::Ready(Ok(output)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(RemoteDropped)),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub trait SpawnExt {
    /// Spawn the future and return a handle that can receive the output of the future.  The
    /// receiver will only error with `RemoteDropped` in the case that either the provided future
    /// panics or the executor has been shut down.
    fn spawn_with_handle<F>(&self, future: F) -> RemoteHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send;
}

impl<S: Spawn> SpawnExt for S {
    fn spawn_with_handle<F>(&self, future: F) -> RemoteHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        let (remote, remote_handle) = RemoteFuture::new(future);
        self.spawn(remote);
        remote_handle
    }
}

/// A trait for spawning tasks on an executor that executes on this thread.
///
/// Unlike `Spawn`, spawned futures do not have to implement `Send`.
pub trait SpawnLocal: Spawn {
    fn spawn_local<F>(&self, future: F)
    where
        F: Future<Output = ()> + 'static;
}

impl<'a, S: SpawnLocal> SpawnLocal for &'a S {
    fn spawn_local<F>(&self, future: F)
    where
        F: Future<Output = ()> + 'static,
    {
        S::spawn_local(*self, future)
    }
}

pub trait SpawnLocalExt {
    fn spawn_local_with_handle<F>(&self, future: F) -> RemoteHandle<F::Output>
    where
        F: Future + 'static;
}

impl<S: SpawnLocal + ?Sized> SpawnLocalExt for S {
    fn spawn_local_with_handle<F>(&self, future: F) -> RemoteHandle<F::Output>
    where
        F: Future + 'static,
    {
        let (remote, remote_handle) = RemoteFuture::new(future);
        self.spawn_local(remote);
        remote_handle
    }
}
