//! [`Subscription`] and [`Stop`]: a running flow, and the handle that ends it
//! (TODO §3, task 1.4).
//!
//! This is the one deliberate difference between
//! [`StreamProvider`](crate::StreamProvider) and every
//! other extension point in this tree. An `Action` runs and returns; a
//! `ModelProvider` fits and returns; a **subscription does not return** — it is
//! process-local, long-lived, and the only question anybody above it asks is
//! "is it still going, and how do I stop it?".
//!
//! [`StreamProvider::subscribe`](crate::StreamProvider::subscribe) therefore
//! **returns rather than blocks**, handing back an owned handle whose `Drop`
//! stops the flow. That is what makes "stop this stream" a `drop`, which is
//! what makes the supervisor's reload path a diff over a map rather than a
//! protocol with a provider (§6).
//!
//! ## Stopping is prompt, not polite
//!
//! Dropping the handle does two things: it closes the [`Stop`] the provider's
//! task is selecting on, and it aborts the task. The abort is there because the
//! callers are a reload, a disable and a shutdown, and none of them can wait on
//! a provider that is blocked reading a socket that will never answer. A
//! provider with a goodbye to say says it from the `Drop` of what it owns — the
//! client, the connection — which runs either way.
//!
//! ```no_run
//! # use sc_stream::Subscription;
//! let subscription = Subscription::spawn(|mut stop| async move {
//!     loop {
//!         tokio::select! {
//!             _ = stop.stopped() => break,
//!             _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => { /* poll */ }
//!         }
//!     }
//! });
//! drop(subscription); // the loop ends
//! ```

use std::future::Future;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Handed to the task a provider spawns: it completes when the
/// [`Subscription`] is dropped.
///
/// A closed channel rather than a flag, so a provider waits on it in a
/// `select!` instead of polling — a stream that checked a boolean every second
/// would take up to a second to stop, once per stream, on every reload.
#[derive(Debug)]
pub struct Stop {
    stopped: oneshot::Receiver<()>,
}

impl Stop {
    /// Completes when the subscription has been dropped.
    ///
    /// Takes `&mut self` so it can be awaited again after a `select!` branch
    /// that did not win, which is how it is always used.
    pub async fn stopped(&mut self) {
        // Either half resolves to "stop": a send (nobody sends) or the sender
        // being dropped, which is what a dropped `Subscription` does.
        let _ = (&mut self.stopped).await;
    }

    /// Whether the subscription has already been dropped, without waiting.
    ///
    /// For a provider between two pieces of work — after a poll returns, before
    /// its elements are delivered — where there is nothing to select on.
    pub fn is_stopped(&mut self) -> bool {
        matches!(
            self.stopped.try_recv(),
            Err(oneshot::error::TryRecvError::Closed) | Ok(())
        )
    }
}

/// A running subscription. **Dropping it stops the flow** (§3).
///
/// Owned by the supervisor's running stream, one per enabled stream, and by
/// nothing else: two owners would make "is this stream running" a question with
/// two answers.
#[derive(Debug)]
pub struct Subscription {
    /// Dropped — never sent on — to close the provider's [`Stop`].
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Subscription {
    /// Spawn `f` as the subscription's task, handing it a [`Stop`].
    ///
    /// The one way to build one, so every subscription in the system is stopped
    /// by the same two steps in the same order.
    #[allow(
        clippy::disallowed_methods,
        reason = "tokio::spawn: a background loop for the life of the process, at no caller's behalf, so it must not inherit the starting task's scope"
    )]
    pub fn spawn<F, Fut>(f: F) -> Subscription
    where
        F: FnOnce(Stop) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(f(Stop { stopped: rx }));
        Subscription {
            stop: Some(tx),
            task,
        }
    }

    /// Whether the provider's task has finished of its own accord.
    ///
    /// This is what the supervisor watches for (§6): a subscription that has
    /// **ended** — a broker that hung up, a poll loop that gave up — is
    /// restarted with backoff, exactly as a `subscribe` that returned an error
    /// is. A provider that means to keep running keeps its task alive.
    pub fn has_ended(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Close the channel first, so a task that is about to be aborted has
        // already been told why; then abort, because a stop cannot wait.
        drop(self.stop.take());
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    /// Give the runtime long enough for a spawned task to make progress.
    /// Milliseconds, not seconds: nothing here waits on a clock of its own.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn dropping_the_handle_stops_the_task() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let subscription = Subscription::spawn(|mut stop| async move {
            loop {
                tokio::select! {
                    _ = stop.stopped() => break,
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
        settle().await;
        assert!(!subscription.has_ended(), "still running");
        assert!(ticks.load(Ordering::SeqCst) > 0, "it ticked");

        drop(subscription);
        settle().await;
        let after = ticks.load(Ordering::SeqCst);
        settle().await;
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            after,
            "nothing ticks after a drop"
        );
    }

    #[tokio::test]
    async fn a_task_that_returns_has_ended_and_the_supervisor_can_see_it() {
        let subscription = Subscription::spawn(|_stop| async move {});
        settle().await;
        assert!(subscription.has_ended());
    }

    #[tokio::test]
    async fn a_provider_between_two_pieces_of_work_can_ask_without_waiting() {
        let seen = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&seen);
        let subscription = Subscription::spawn(|mut stop| async move {
            while !stop.is_stopped() {
                flag.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        settle().await;
        drop(subscription);
        settle().await;
        assert!(seen.load(Ordering::SeqCst) > 0);
    }
}
