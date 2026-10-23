//! [`SharedTx`] — one open transaction several writers take turns on (§10.3,
//! decision 6).
//!
//! A transaction handle ([`sc_db::Transaction`]) is a `&mut` thing: one owner,
//! one call at a time. That is exactly right for the two callers that had one
//! until now — a metadata mutation, a CSV import — because each of them makes
//! every statement itself, in a loop it wrote.
//!
//! A **workflow step** does not. Its writes are made by an action, the events
//! those writes raise are dispatched to other triggers, and *their* actions write
//! too — a call tree several crates deep, whose frames hold `&self` and cannot
//! pass a unique borrow down. Yet everything that tree does has to land in one
//! transaction, because that is what "each step runs in one transaction" means:
//! the step's rows and the run's advance commit **together**, or neither does.
//!
//! So the handle becomes shareable: an `Arc` over an async mutex, cloned into
//! every frame that might write, with each statement taking the lock for as long
//! as it runs and no longer. The mutex is not a concession — one connection can
//! only run one statement at a time anyway, so serialising is the truth about the
//! resource rather than a policy imposed on top of it.
//!
//! ## The caller travels with the statement, not with the transaction
//!
//! [`run_in_context`](crate::run_in_context) opens a transaction per statement,
//! so `SET LOCAL sc.role` there is unambiguous: one caller, one statement, one
//! transaction. Here a hundred statements share a transaction and they do **not**
//! share a caller — a step writes as the admin, a trigger it cascades into writes
//! as itself, and a `SET LOCAL` that outlived its statement would hand the next
//! writer the last one's identity. That is why [`SharedTx::run`] re-applies the
//! caller GUCs whenever they differ from the ones already in force, and applies
//! the **empty** value for a statement with no caller at all: both policy clauses
//! fold `''` to `NULL` (`NULLIF(current_setting(…), '')`), so "no caller" reaches
//! the policies as no access rather than as whoever went last.
//!
//! ## It begins when it is first used
//!
//! A step that calls an HTTP endpoint and writes nothing should not hold a
//! database transaction open for the length of the call — an idle transaction
//! keeps a connection and whatever locks it has taken, and a workflow's steps are
//! exactly the places where a long wait is normal. So the handle is created
//! *around* a driver and **begins on its first statement**: a step that never
//! touches the database never opens one, and one that does opens it at the moment
//! it writes rather than at the moment it started.
//!
//! ## What it will not do
//!
//! It serves the statements of **its own database** ([`SharedTx::serves`]). A
//! table on a secondary connection or one served by a module's table provider is
//! not reachable from this transaction, so a write to one takes the ordinary
//! pooled path and is *not* part of the step's atomic advance — the honest
//! reading, and one the caller can see rather than one it has to infer.
//!
//! ## What may only happen once it has committed
//!
//! A table event is raised when the statement succeeds, which inside a shared
//! transaction is **before** the commit (§10.2). A trigger wants exactly that —
//! it joins the transaction and shares its fate. Telling somebody *outside* it
//! does not: a live page shown a row the step then rolls back has been shown
//! something that never happened. So a listener can hand the transaction a
//! callback ([`SharedTx::after_commit`]) that runs only once the commit has
//! succeeded, in the order the callbacks were added, and is dropped unrun on a
//! rollback, on a commit that fails, and — through [`SharedTx::savepoint`] and
//! [`SharedTx::rollback_to_savepoint`] — when the part of the transaction that
//! added it is wound back.

use std::sync::{Arc, Mutex as SyncMutex, MutexGuard};

use sc_db::{DatabaseDriver, Row, SchemaChange};
use sc_error::{Error, Result};
use sc_query::Statement;
use tokio::sync::Mutex;

use crate::caller::CallerContext;
use crate::catalog::Catalog;
use crate::field::DbId;
use crate::rls::{clear_caller_context, map_policy_violation, set_caller_context};
use crate::table::Table;

/// Where a shared transaction is in its life.
enum State {
    /// Not begun: nothing has needed the database yet.
    Pending,
    /// Begun and usable.
    Running(Box<dyn sc_db::Transaction>),
    /// Committed or rolled back. A handle kept past that says so by name.
    Finished,
}

/// What is inside the lock: the state, and the caller whose GUCs are currently
/// in force on it.
struct Open {
    state: State,
    /// Whose GUCs are currently in force — so a run of statements by the same
    /// writer costs one `SET LOCAL` pair, not one per statement.
    applied: Applied,
}

/// Which caller the transaction's GUCs currently say.
#[derive(PartialEq)]
enum Applied {
    /// Nothing has been applied yet: the next statement must say who it is,
    /// whoever that is.
    Unknown,
    /// Explicitly nobody — [`clear_caller_context`] — in this ambient
    /// changeset (its GUC text).
    Nobody(String),
    /// This `(role, user JSON, changeset GUC text)`.
    Caller(u8, Option<String>, String),
}

/// Something to do once the transaction has committed
/// ([`SharedTx::after_commit`]).
pub type AfterCommit = Box<dyn FnOnce() + Send + 'static>;

/// The callbacks waiting for the commit, and where each open savepoint began
/// in them.
///
/// A synchronous lock rather than the transaction's own: adding a callback
/// must not wait behind a statement another writer is running, and nothing is
/// ever awaited while this one is held.
#[derive(Default)]
struct Pending {
    callbacks: Vec<AfterCommit>,
    /// `(savepoint name, callbacks.len() when it was taken)`, innermost last.
    savepoints: Vec<(String, usize)>,
    /// How the transaction finished, once it has: `Some(true)` committed.
    outcome: Option<bool>,
}

/// An open transaction that several writers share, in turn.
///
/// Cloning gives another handle on the **same** transaction; the last statement
/// wins nothing and the first commit finishes it for everyone. Finishing it is
/// the opener's job ([`commit`](SharedTx::commit) /
/// [`rollback`](SharedTx::rollback)); a clone that outlives the commit gets a
/// named error rather than a panic or a silent no-op.
#[derive(Clone)]
pub struct SharedTx {
    inner: Arc<Mutex<Open>>,
    /// What runs after the commit — see the module documentation.
    pending: Arc<SyncMutex<Pending>>,
    /// What the transaction is begun on, when something first needs it.
    driver: Arc<dyn DatabaseDriver>,
    /// The database this transaction is on: a statement for any other database
    /// does not belong here (see [`serves`](SharedTx::serves)).
    database: DbId,
    /// Whether the backend has row-level security or change-log triggers,
    /// which are the whole of what the caller GUCs are for. A backend with
    /// neither is not asked to `SET LOCAL` something it does not have.
    caller_gucs: bool,
}

impl std::fmt::Debug for SharedTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedTx")
            .field("database", &self.database.0)
            .finish_non_exhaustive()
    }
}

impl PartialEq for SharedTx {
    /// Two handles are equal when they are handles on the **same** transaction —
    /// which is what a caller comparing them is asking, and the only question
    /// about a transaction that has an answer without running a statement.
    fn eq(&self, other: &SharedTx) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl SharedTx {
    /// A transaction on `database`, to be begun when something first needs it.
    pub fn begin(catalog: &Catalog, database: &DbId) -> Result<SharedTx> {
        let driver = catalog.driver_named(database)?;
        Ok(SharedTx::begin_on(&driver, database.clone()))
    }

    /// One on the **primary** database — where the `_fd_*` tables and, for
    /// almost every deployment, the application's own tables live.
    pub fn begin_primary(catalog: &Catalog) -> Result<SharedTx> {
        SharedTx::begin(catalog, catalog.primary_db())
    }

    /// A **read of one moment** on the primary database: begun at once, read-only,
    /// and every statement reads the same snapshot (`REPEATABLE READ`), so rows
    /// of several tables agree with each other.
    pub async fn begin_snapshot(catalog: &Catalog) -> Result<SharedTx> {
        let tx = SharedTx::begin_primary(catalog)?;
        {
            let mut open = tx.inner.lock().await;
            // Both before the first statement, as `SET TRANSACTION` requires.
            let handle = open.handle(&tx.driver).await?;
            handle.set_repeatable_read().await?;
            handle.set_read_only().await?;
        }
        Ok(tx)
    }

    /// One on a driver the caller already holds.
    pub fn begin_on(driver: &Arc<dyn DatabaseDriver>, database: DbId) -> SharedTx {
        SharedTx {
            inner: Arc::new(Mutex::new(Open {
                state: State::Pending,
                applied: Applied::Unknown,
            })),
            pending: Arc::new(SyncMutex::new(Pending::default())),
            driver: driver.clone(),
            database,
            caller_gucs: crate::rls::takes_caller_gucs(driver.as_ref()),
        }
    }

    /// The database this transaction is on.
    pub fn database(&self) -> &DbId {
        &self.database
    }

    /// Whether `table`'s rows are reachable from this transaction: its own
    /// database, and not a table a module serves.
    pub fn serves(&self, table: &Table) -> bool {
        table.provider().is_none() && table.database == self.database
    }

    /// Run `stmt` as `caller`, collecting its rows.
    ///
    /// The caller's GUCs are applied first (and re-applied whenever they are not
    /// the ones already in force), so an RLS table's policies decide this
    /// statement by *this* writer — see the module documentation. A policy
    /// violation comes back as a [`NotFound`](Error::not_found), the same
    /// probe-free reading [`run_in_context`](crate::run_in_context) gives it.
    pub async fn run(&self, caller: Option<&CallerContext>, stmt: &Statement) -> Result<Vec<Row>> {
        let mut open = self.inner.lock().await;
        if self.caller_gucs {
            let wanted = match caller {
                Some(caller) => {
                    Applied::Caller(caller.role, caller.user_json(), caller.changeset_json())
                }
                None => Applied::Nobody(
                    crate::changeset::current()
                        .map(|c| c.guc_value(None))
                        .unwrap_or_default(),
                ),
            };
            if open.applied != wanted {
                let tx = open.handle(&self.driver).await?;
                // Absent is spelled out rather than left alone: an unset GUC and
                // a *stale* one are the same thing to a policy, and only one of
                // them is what this statement means.
                match caller {
                    Some(caller) => set_caller_context(tx, caller).await?,
                    None => clear_caller_context(tx).await?,
                }
                open.applied = wanted;
            }
        }
        let tx = open.handle(&self.driver).await?;
        let outcome = match tx.query(stmt).await {
            Ok(stream) => stream.try_collect().await,
            Err(e) => Err(e),
        };
        outcome.map_err(map_policy_violation).map_err(poisoned)
    }

    /// Apply a schema change inside the transaction.
    pub async fn apply_schema(&self, change: &SchemaChange) -> Result<()> {
        let mut open = self.inner.lock().await;
        open.handle(&self.driver).await?.apply_schema(change).await
    }

    /// Run a raw SQL batch inside the transaction — savepoints, and the DDL the
    /// structured [`SchemaChange`] does not model.
    pub async fn batch(&self, sql: &str) -> Result<()> {
        let mut open = self.inner.lock().await;
        open.handle(&self.driver).await?.batch(sql).await
    }

    /// Defer foreign-key checking to the commit (§13.1's import).
    pub async fn defer_constraints(&self) -> Result<()> {
        let mut open = self.inner.lock().await;
        open.handle(&self.driver).await?.defer_constraints().await
    }

    /// Run `callback` once this transaction has **committed** — never before,
    /// and never at all if it rolls back, if the commit fails, or if a
    /// savepoint taken before it was added is rolled back to.
    ///
    /// Callbacks run in the order they were added, on the task that commits,
    /// right after the commit returns. They must not block: they are the place
    /// a write is *announced* (a live page told of a row), not more work on
    /// the write's path. Added to a transaction that has already committed, it
    /// runs at once; to one already rolled back, it is dropped.
    pub fn after_commit(&self, callback: impl FnOnce() + Send + 'static) {
        let mut pending = lock(&self.pending);
        match pending.outcome {
            None => pending.callbacks.push(Box::new(callback)),
            Some(true) => {
                drop(pending);
                callback();
            }
            Some(false) => {}
        }
    }

    /// How many callbacks are waiting for the commit.
    pub fn after_commit_pending(&self) -> usize {
        lock(&self.pending).callbacks.len()
    }

    /// `SAVEPOINT name`, remembering which after-commit callbacks were added
    /// before it, so [`rollback_to_savepoint`](SharedTx::rollback_to_savepoint)
    /// drops exactly the ones added after. `name` is structural, never user
    /// data.
    pub async fn savepoint(&self, name: &str) -> Result<()> {
        self.batch(&format!("SAVEPOINT {name}")).await?;
        let mut pending = lock(&self.pending);
        let mark = pending.callbacks.len();
        pending.savepoints.push((name.to_owned(), mark));
        Ok(())
    }

    /// `RELEASE SAVEPOINT name`: what was added since it stays, now part of
    /// the enclosing transaction (or savepoint).
    pub async fn release_savepoint(&self, name: &str) -> Result<()> {
        self.batch(&format!("RELEASE SAVEPOINT {name}")).await?;
        let mut pending = lock(&self.pending);
        if let Some(at) = pending.savepoints.iter().rposition(|(n, _)| n == name) {
            // Releasing a savepoint releases every one taken after it too.
            pending.savepoints.truncate(at);
        }
        Ok(())
    }

    /// `ROLLBACK TO SAVEPOINT name`: the writes since it are undone, so the
    /// callbacks announcing them are dropped. The savepoint itself stays, as
    /// it does in SQL.
    pub async fn rollback_to_savepoint(&self, name: &str) -> Result<()> {
        self.batch(&format!("ROLLBACK TO SAVEPOINT {name}")).await?;
        // A `SET LOCAL` made since the savepoint is undone with it, so which
        // caller's GUCs are in force is no longer known.
        self.inner.lock().await.applied = Applied::Unknown;
        let mut pending = lock(&self.pending);
        if let Some(at) = pending.savepoints.iter().rposition(|(n, _)| n == name) {
            let mark = pending.savepoints[at].1;
            pending.savepoints.truncate(at + 1);
            pending.callbacks.truncate(mark);
        }
        Ok(())
    }

    /// Commit, making everything every sharer wrote durable — once, for all of
    /// them — and then run the [after-commit](SharedTx::after_commit)
    /// callbacks. A commit that fails runs none of them.
    ///
    /// A transaction nothing ever needed was never begun, and committing it is
    /// the success it describes: there is nothing to make durable.
    pub async fn commit(&self) -> Result<()> {
        let outcome = match self.take().await? {
            Some(tx) => tx.commit().await,
            None => Ok(()),
        };
        let callbacks = self.finish(outcome.is_ok());
        if outcome.is_ok() {
            for callback in callbacks {
                callback();
            }
        }
        outcome
    }

    /// Roll back, discarding everything every sharer wrote, and every
    /// after-commit callback with it.
    pub async fn rollback(&self) -> Result<()> {
        let outcome = match self.take().await? {
            Some(tx) => tx.rollback().await,
            None => Ok(()),
        };
        drop(self.finish(false));
        outcome
    }

    /// Record how the transaction ended, and take the callbacks out — to be
    /// run, or dropped, by the caller once the lock is released (a callback's
    /// captures are dropped outside it, whatever their `Drop` does).
    fn finish(&self, committed: bool) -> Vec<AfterCommit> {
        let mut pending = lock(&self.pending);
        pending.outcome = Some(committed);
        pending.savepoints.clear();
        std::mem::take(&mut pending.callbacks)
    }

    /// Whether this transaction can still be used — begun or not, but not
    /// finished.
    pub async fn is_open(&self) -> bool {
        !matches!(self.inner.lock().await.state, State::Finished)
    }

    /// Whether a statement has actually opened a transaction on the database.
    pub async fn has_begun(&self) -> bool {
        matches!(self.inner.lock().await.state, State::Running(_))
    }

    async fn take(&self) -> Result<Option<Box<dyn sc_db::Transaction>>> {
        let mut open = self.inner.lock().await;
        open.applied = Applied::Unknown;
        match std::mem::replace(&mut open.state, State::Finished) {
            State::Running(tx) => Ok(Some(tx)),
            State::Pending => Ok(None),
            State::Finished => Err(finished()),
        }
    }
}

impl Open {
    /// The handle — beginning the transaction if this is the first statement —
    /// or the error a caller that arrived after the commit gets.
    async fn handle(
        &mut self,
        driver: &Arc<dyn DatabaseDriver>,
    ) -> Result<&mut dyn sc_db::Transaction> {
        if let State::Pending = self.state {
            self.state = State::Running(driver.begin().await?);
        }
        match &mut self.state {
            State::Running(tx) => Ok(tx.as_mut()),
            _ => Err(finished()),
        }
    }
}

/// Say what "current transaction is aborted" means **here**, which is not what
/// it means to someone who wrote the statement that failed.
///
/// A statement that fails inside a shared transaction ends that transaction:
/// everything after it fails with Postgres's `25P02` until it is rolled back.
/// The writer that meets that message did nothing wrong and cannot act on it, so
/// it is answered with the sentence that is actually true — an earlier statement
/// in this unit of work failed, and the unit is over.
fn poisoned(e: Error) -> Error {
    let chain = sc_error::format_chain(&e);
    if chain.contains("25P02") || chain.contains("current transaction is aborted") {
        return Error::database(
            "an earlier statement in this transaction failed, so nothing more can be              done in it: this unit of work (a workflow step, an import) is over and              will be rolled back",
        );
    }
    e
}

/// The after-commit lock, recovered from poisoning: each change to it is one
/// push, truncate or take, so it is valid after a panic elsewhere.
fn lock(pending: &SyncMutex<Pending>) -> MutexGuard<'_, Pending> {
    match pending.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn finished() -> Error {
    Error::database("this transaction has already been committed or rolled back")
}
