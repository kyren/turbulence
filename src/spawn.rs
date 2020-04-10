use std::future::Future;

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
