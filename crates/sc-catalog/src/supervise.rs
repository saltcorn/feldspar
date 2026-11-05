//! A background loop that outlives its panics (security review 2026-11 F-5,
//! TODO-type-hardening H5.2).
//!
//! The server's clocks — the trigger scheduler, the backup scheduler, the
//! stream supervisor, the sweeps — are each a `loop { sleep; tick }` on a task
//! of its own, with the handle dropped. A panic in one tick ended that task,
//! and with it every periodic trigger or backup for the life of the process,
//! with nothing but a line on stderr to say so. [`supervise`] runs the loop
//! instead: a panic is logged by name and the loop is started again, after a
//! pause that doubles while it keeps failing.
//!
//! A loop that **returns** has finished on purpose and is not restarted. Abort
//! the handle to stop one: the loop runs on the handle's own task (it is not
//! spawned again inside it), so aborting it drops the loop too.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// How long [`supervise`] waits before starting a loop again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Backoff {
    /// The pause after the first panic.
    pub(crate) first: Duration,
    /// The longest pause: the doubling stops here.
    pub(crate) max: Duration,
    /// A run that lasted this long before it panicked starts the doubling
    /// over: that was a new failure, not the same one again.
    pub(crate) reset_after: Duration,
}

/// The pauses the server's loops restart with: a second, doubling to five
/// minutes.
const BACKOFF: Backoff = Backoff {
    first: Duration::from_secs(1),
    max: Duration::from_secs(5 * 60),
    reset_after: Duration::from_secs(10 * 60),
};

/// Run the background loop `make` builds, on a task of its own, for as long
/// as it runs — starting it again (with a new `make()`) whenever it panics.
///
/// `name` says which loop it is in the log line a panic produces ("the
/// trigger scheduler"). The task has no [`TaskScope`](crate::TaskScope): a
/// background loop runs at no caller's behalf.
pub fn supervise<F, Fut>(name: &'static str, make: F) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    supervise_with(name, BACKOFF, make)
}

/// [`supervise`], with the pauses given: a test's are milliseconds.
#[allow(
    clippy::disallowed_methods,
    reason = "tokio::spawn: a background loop for the life of the process, at no caller's behalf, so it must not inherit the starting task's scope"
)]
pub(crate) fn supervise_with<F, Fut>(
    name: &'static str,
    backoff: Backoff,
    mut make: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut pause = backoff.first;
        loop {
            let started = Instant::now();
            let Err(panic) = CatchUnwind(Box::pin(make())).await else {
                return;
            };
            if started.elapsed() >= backoff.reset_after {
                pause = backoff.first;
            }
            sc_log::log_error!(
                "feldspar: {name} stopped with a bug ({}); starting it again in {}",
                sc_log::panic_message(panic.as_ref()),
                sc_log::human_duration(pause)
            );
            tokio::time::sleep(pause).await;
            pause = pause.saturating_mul(2).min(backoff.max);
        }
    })
}

/// A future, with a panic while it is polled caught and handed back as the
/// payload: `FutureExt::catch_unwind` without the dependency.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future<Output = ()>> Future for CatchUnwind<F> {
    type Output = Result<(), Box<dyn std::any::Any + Send>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Unwind safety: a loop that panicked is dropped, not polled again, so
        // nothing observes the state it left half-changed; what it shares
        // with its next run is behind `Arc`s and locks that recover from
        // poisoning.
        let inner = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const QUICK: Backoff = Backoff {
        first: Duration::from_millis(5),
        max: Duration::from_millis(20),
        reset_after: Duration::from_secs(60),
    };

    #[tokio::test]
    async fn a_loop_that_panics_is_started_again_until_it_finishes() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let handle = supervise_with("a test loop", QUICK, move || {
            let runs = Arc::clone(&counted);
            async move {
                let run = runs.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                if run < 3 {
                    std::panic::panic_any(format!("tick {run} fell over"));
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the loop finishes once it stops panicking")
            .expect("the supervisor itself never panics");
        assert_eq!(runs.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn aborting_the_handle_stops_the_loop() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let handle = supervise_with("a test loop", QUICK, move || {
            let runs = Arc::clone(&counted);
            async move {
                loop {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        handle.abort();
        let _ = handle.await;
        let after = runs.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(runs.load(Ordering::SeqCst), after);
    }
}
