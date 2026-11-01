//! Ending a transaction by its outcome: commit on success, roll back on
//! failure, in one place rather than at every call site.

use futures::future::BoxFuture;
use sc_error::Result;

use crate::{DatabaseDriver, Transaction};

/// Run `work` in a transaction of `driver`'s, committing it if `work`
/// succeeds and rolling it back if not.
///
/// The answer is `work`'s, or the commit's error when the commit fails.
/// `work` hands back a boxed future (`|tx| Box::pin(async move { … })`), so
/// the whole call is `Send` where its caller needs it to be. The future may
/// borrow the transaction and nothing else: move owned values into it. Where
/// that does not fit, begin the transaction and end it with [`finish`].
pub async fn with_tx<T, F>(driver: &dyn DatabaseDriver, work: F) -> Result<T>
where
    F: for<'t> FnOnce(&'t mut dyn Transaction) -> BoxFuture<'t, Result<T>>,
{
    let mut tx = driver.begin().await?;
    let outcome = work(tx.as_mut()).await;
    finish(tx, outcome).await
}

/// Commit `tx` if `outcome` is a success, roll it back if not, and answer
/// `outcome` (or the commit's error).
pub async fn finish<T>(tx: Box<dyn Transaction>, outcome: Result<T>) -> Result<T> {
    match outcome {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(e) => {
            abandon(tx).await;
            Err(e)
        }
    }
}

/// Roll `tx` back where nothing waits on the rollback: after a failure, whose
/// error is the news, or after reads with nothing to commit.
///
/// A rollback that fails is logged, not returned: the connection is dropped,
/// and the database ends the transaction with it.
pub async fn abandon(tx: Box<dyn Transaction>) {
    if let Err(e) = tx.rollback().await {
        sc_log::log_verbose!(
            "a transaction could not be rolled back, so its connection is dropped: {e}"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use sc_error::Error;
    use sc_query::{SqlDialect, Statement};

    use super::*;
    use crate::{DbCapabilities, PhysicalTable, RowStream, SchemaChange};

    type Log = Arc<Mutex<Vec<&'static str>>>;

    struct Dialect;
    impl SqlDialect for Dialect {
        fn quote_ident(&self, ident: &str) -> String {
            ident.to_owned()
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }

    /// A driver whose transactions write down how they ended.
    struct Driver {
        log: Log,
        commit_fails: bool,
    }

    struct Tx {
        log: Log,
        commit_fails: bool,
    }

    fn note(log: &Log, what: &'static str) {
        if let Ok(mut log) = log.lock() {
            log.push(what);
        }
    }

    #[async_trait]
    impl DatabaseDriver for Driver {
        async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
            Ok(Vec::new())
        }
        async fn query(&self, _stmt: &Statement) -> Result<RowStream> {
            Ok(RowStream::empty())
        }
        async fn apply_schema(&self, _change: &SchemaChange) -> Result<()> {
            Ok(())
        }
        async fn begin(&self) -> Result<Box<dyn Transaction>> {
            note(&self.log, "begin");
            Ok(Box::new(Tx {
                log: self.log.clone(),
                commit_fails: self.commit_fails,
            }))
        }
        fn capabilities(&self) -> DbCapabilities {
            DbCapabilities::none()
        }
        fn dialect(&self) -> &dyn SqlDialect {
            &Dialect
        }
    }

    #[async_trait]
    impl Transaction for Tx {
        async fn query(&mut self, _stmt: &Statement) -> Result<RowStream> {
            Ok(RowStream::empty())
        }
        async fn set_read_only(&mut self) -> Result<()> {
            note(&self.log, "work");
            Ok(())
        }
        async fn apply_schema(&mut self, _change: &SchemaChange) -> Result<()> {
            Ok(())
        }
        async fn commit(self: Box<Self>) -> Result<()> {
            note(&self.log, "commit");
            if self.commit_fails {
                return Err(Error::database("the commit failed"));
            }
            Ok(())
        }
        async fn rollback(self: Box<Self>) -> Result<()> {
            note(&self.log, "rollback");
            // A failed rollback is not the caller's news.
            Err(Error::database("the connection is gone"))
        }
    }

    fn driver(commit_fails: bool) -> (Driver, Log) {
        let log = Log::default();
        (
            Driver {
                log: log.clone(),
                commit_fails,
            },
            log,
        )
    }

    fn entries(log: &Log) -> Vec<&'static str> {
        log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    #[tokio::test]
    async fn work_that_succeeds_is_committed_and_its_answer_returned() {
        let (driver, log) = driver(false);
        let answer = with_tx(&driver, |tx| {
            Box::pin(async move {
                tx.set_read_only().await?;
                Ok(7)
            })
        })
        .await;
        assert_eq!(answer.ok(), Some(7));
        assert_eq!(entries(&log), ["begin", "work", "commit"]);
    }

    #[tokio::test]
    async fn work_that_fails_is_rolled_back_and_its_error_returned() {
        let (driver, log) = driver(false);
        let answer: Result<()> = with_tx(&driver, |_tx| {
            Box::pin(async { Err(Error::invalid("the work failed")) })
        })
        .await;
        // The work's error, not the rollback's.
        assert_eq!(
            answer.err().map(|e| e.message().to_owned()).as_deref(),
            Some("the work failed")
        );
        assert_eq!(entries(&log), ["begin", "rollback"]);
    }

    #[tokio::test]
    async fn a_commit_that_fails_is_the_answer() {
        let (driver, log) = driver(true);
        let answer = with_tx(&driver, |_tx| Box::pin(async { Ok(1) })).await;
        assert_eq!(
            answer.err().map(|e| e.message().to_owned()).as_deref(),
            Some("the commit failed")
        );
        assert_eq!(entries(&log), ["begin", "commit"]);
    }
}
