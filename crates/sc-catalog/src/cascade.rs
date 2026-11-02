//! Depth refusals, carried back to whoever is waiting (§10.2, TODO T1).
//!
//! A trigger fired by a **write** reports its failure rather than returning it:
//! the write has happened, and whoever made it is not failed for a trigger
//! further down (decision 1). That rule is right for a trigger that failed at
//! its job. It is wrong for one the cascade bound refused. A refusal is not a
//! failure of the trigger. It means the **structure** that led there is too
//! deep, so the work the caller started was cut short where nobody could see
//! it. User report: an agent tool ran the app's `case_action` trigger, whose
//! outbox insert fired `mail_deliver` one level past the limit, and the tool
//! said "ok" while no mail was sent.
//!
//! So a **synchronous entry point** (running a trigger by name, running a code
//! body, an agent's tool call) runs inside [`surfacing_refusals`], and the
//! dispatcher [`record_refusal`]s every trigger the bound refuses. If the work
//! comes back `Ok` but something under it was refused, the entry point returns
//! the refusal instead. Each entry point opens its own scope, so a refusal goes
//! to the **nearest** caller. A body that runs `trigger("x")` and catches the
//! error has handled it, and the call that ran the body is not failed again.
//!
//! Only the bound's refusals are collected. Every other trigger failure keeps
//! the reported-not-returned rule. A task-local reaches what this task awaits,
//! including a code body's host calls, which are served on the task that runs
//! the body. Work handed to `spawn_scoped` is not reached: it may outlive the
//! call, and a detached task must not fail a call that has nothing to do with it.

use std::future::Future;
use std::sync::{Arc, Mutex};

use sc_error::{Error, Result};

tokio::task_local! {
    static REFUSALS: Arc<Mutex<Vec<String>>>;
}

/// Note that the cascade bound refused a trigger, with the refusal's message.
/// Does nothing outside [`surfacing_refusals`].
pub fn record_refusal(message: &str) {
    let _ = REFUSALS.try_with(|refusals| {
        if let Ok(mut refusals) = refusals.lock() {
            refusals.push(message.to_owned());
        }
    });
}

/// Run `fut` as an entry point the caller is waiting on. A trigger the cascade
/// bound refused anywhere under it turns an `Ok` into that refusal (the first
/// one, which names the whole chain). An `Err` is returned as it is.
pub async fn surfacing_refusals<T, F>(fut: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let refusals = Arc::new(Mutex::new(Vec::new()));
    let result = REFUSALS.scope(Arc::clone(&refusals), fut).await?;
    let refused = refusals.lock().map(|r| r.clone()).unwrap_or_default();
    match refused.split_first() {
        None => Ok(result),
        Some((first, rest)) => {
            let more = match rest.len() {
                0 => String::new(),
                1 => " (and 1 more trigger was refused the same way)".to_owned(),
                n => format!(" (and {n} more triggers were refused the same way)"),
            };
            Err(Error::invalid(format!(
                "the work finished, but part of it was not run: {first}{more}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_refusal_under_the_scope_fails_the_call() {
        let ok: Result<i32> = surfacing_refusals(async {
            record_refusal("trigger `x` was not run");
            Ok(1)
        })
        .await;
        let msg = ok.unwrap_err().to_string();
        assert!(msg.contains("trigger `x` was not run"), "{msg}");
    }

    #[tokio::test]
    async fn the_nearest_scope_gets_the_refusal() {
        let outer: Result<i32> = surfacing_refusals(async {
            // The inner call fails, and the body handles it.
            let inner: Result<i32> = surfacing_refusals(async {
                record_refusal("deep");
                Ok(2)
            })
            .await;
            assert!(inner.is_err());
            Ok(1)
        })
        .await;
        assert_eq!(outer.unwrap(), 1);
    }

    #[tokio::test]
    async fn nothing_is_recorded_outside_a_scope_and_errors_pass_through() {
        record_refusal("nobody is listening");
        let err: Result<i32> = surfacing_refusals(async {
            record_refusal("refused");
            Err(Error::invalid("its own failure"))
        })
        .await;
        assert_eq!(err.unwrap_err().message(), "its own failure");
        let clean: Result<i32> = surfacing_refusals(async { Ok(3) }).await;
        assert_eq!(clean.unwrap(), 3);
    }
}
