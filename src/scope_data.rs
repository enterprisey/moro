use std::{
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Future, Stream};

use crate::Spawned;
pub(crate) struct ScopeData<'scope, 'env: 'scope, R: Send + 'env> {
    /// Stores the set of futures that have been spawned.
    ///
    /// This is behind a mutex so that multiple concurrent actors can access it.
    /// A `RwLock` seems better, but `FuturesUnordered is not `Sync` in the case.
    /// But in fact it doesn't matter anyway, because all spawned futures execute
    /// CONCURRENTLY and hence there will be no contention.
    futures: Mutex<Pin<Box<FuturesUnordered<BoxFuture<'scope, ()>>>>>,
    enqueued: Mutex<Vec<BoxFuture<'scope, ()>>>,
    terminated: Mutex<Option<R>>,
    phantom: PhantomData<&'scope &'env ()>,
}

fn is_sync<T: Sync>(t: T) -> T {
    t
}

impl<'scope, 'env, R: Send> ScopeData<'scope, 'env, R> {
    /// Create a scope.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(is_sync(Self {
            futures: Mutex::new(Box::pin(FuturesUnordered::new())),
            enqueued: Default::default(),
            terminated: Default::default(),
            phantom: Default::default(),
        }))
    }

    /// Polls the jobs that were spawned thus far. Returns:
    ///
    /// * `Pending` if there are jobs that cannot complete
    /// * `Ready(Ok(()))` if all jobs are completed
    /// * `Ready(Err(c))` if the scope has been canceled
    ///
    /// Should not be invoked again once `Ready(Err(c))` is returned.
    ///
    /// It is ok to invoke it again after `Ready(Ok(()))` has been returned;
    /// if any new jobs have been spawned, they will execute.
    pub(crate) fn poll_jobs(&self, cx: &mut std::task::Context<'_>) -> Poll<Option<R>> {
        let mut futures = self.futures.lock().unwrap();
        'outer: loop {
            // once we are terminated, we do no more work.
            if let Some(r) = self.terminated.lock().unwrap().take() {
                return Poll::Ready(Some(r));
            }

            futures.extend(self.enqueued.lock().unwrap().drain(..));

            while let Some(()) = ready!(futures.as_mut().poll_next(cx)) {
                // once we are terminated, we do no more work.
                if self.terminated.lock().unwrap().is_some() {
                    continue 'outer;
                }
            }

            if self.enqueued.lock().unwrap().is_empty() {
                return Poll::Ready(None);
            }
        }
    }

    /// Clear out all pending jobs. This is used when dropping the
    /// scope body to ensure that any possible references to `Scope`
    /// are removed before we drop it.
    ///
    /// # Unsafe contract
    ///
    /// Once this returns, there are no more pending tasks.
    pub(crate) fn clear(&self) {
        self.futures.lock().unwrap().clear();
        self.enqueued.lock().unwrap().clear();
    }

    /// Implementation of [`Scope`]::terminate.
    pub fn terminate<T>(&'scope self, value: R) -> impl Future<Output = T> + 'scope
    where
        T: 'scope + Send,
    {
        let mut lock = self.terminated.lock().unwrap();
        if lock.is_none() {
            *lock = Some(value.into());
        }
        std::mem::drop(lock);

        // The code below will never run
        self.spawn(async { panic!() })
    }

    /// Implementation of [`Scope`]::spawn.
    pub fn spawn<T>(
        &'scope self,
        future: impl Future<Output = T> + Send + 'scope,
    ) -> Spawned<impl Future<Output = T> + Send>
    where
        T: 'scope + Send,
    {
        // Use a channel to communicate result from the *actual* future
        // (which lives in the futures-unordered) and the caller.
        // This is kind of crappy because, ideally, the caller expressing interest
        // in the result of the future would let it run, but that would require
        // more clever coding and I'm just trying to stand something up quickly
        // here. What will happen when caller expresses an interest in result
        // now is that caller will block which should (eventually) allow the
        // futures-unordered to be polled and make progress. Good enough.

        let (tx, rx) = async_channel::bounded(1);

        self.enqueued.lock().unwrap().push(Box::pin(async move {
            let v = future.await;
            let _ = tx.send(v).await;
        }));

        Spawned::new(async move {
            match rx.recv().await {
                Ok(v) => v,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        })
    }
}
