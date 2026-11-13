//! An account's **authentication epoch** (security review 2026-11, §2 and B4;
//! TODO-type-hardening H2.3).
//!
//! A number per account that goes up whenever what signs the account in
//! changes: a new password (set by an admin, or through a reset link), the
//! second factor turned on or off, the account disabled, its role made to
//! require a second factor it lacks. Every session row records the epoch it was
//! started at ([`crate::SessionStore`]), and so does a sign-in that is waiting
//! for its second factor (the challenge row). A session or a challenge from an
//! earlier epoch is nobody: whatever the old credentials handed out ends with
//! them, in one place, however the change was made — the admin UI, the CLI, an
//! MCP tool, a password link.
//!
//! The epochs live in [`_fd_auth_epochs`](AUTH_EPOCHS_TABLE), one row per
//! account that has ever changed a credential; an account with no row is at
//! epoch 0, which is also what every session row made before epochs existed
//! records (its column is `NULL`). So an upgrade signs nobody out, and the
//! first credential change after it ends the sessions from before it.
//!
//! # `CredentialsChanged`
//!
//! The functions that change a credential raise the epoch in the database
//! themselves, so no caller can forget the part that matters on every node. They
//! return a [`CredentialsChanged`], which is `#[must_use]` and which only
//! [`SessionStore::bump_epoch`](crate::SessionStore::bump_epoch) consumes: that
//! ends the account's session rows (but the one making the change, when it
//! keeps going), drops this node's cached copies, and tells the live sockets
//! holding one.

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement};
use sc_query::{Update, Value};
use sc_types::{BasicType, TypeRef};
use uuid::Uuid;

/// The table holding each account's epoch (module docs).
pub const AUTH_EPOCHS_TABLE: &str = "_fd_auth_epochs";
/// The account (`users.id`), the primary key.
const COL_USER: &str = "user_id";
/// The account's epoch.
const COL_EPOCH: &str = "epoch";

/// Which generation of an account's credentials something was issued under
/// (module docs). Compared, never computed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct AuthEpoch(i64);

impl AuthEpoch {
    /// The epoch of an account that has never changed a credential, and of a
    /// row written before epochs existed.
    pub const INITIAL: AuthEpoch = AuthEpoch(0);

    /// The number, as a session or challenge row stores it.
    pub fn value(self) -> i64 {
        self.0
    }

    /// The epoch a stored column holds: `NULL` (a row from before epochs) is
    /// [`INITIAL`](AuthEpoch::INITIAL).
    pub(crate) fn of_column(value: Option<&Value>) -> Result<AuthEpoch> {
        match value {
            None | Some(Value::Null) => Ok(AuthEpoch::INITIAL),
            Some(Value::Int(n)) => Ok(AuthEpoch(*n)),
            Some(other) => Err(Error::invalid(format!(
                "an auth epoch should be an integer, got {}",
                other.kind()
            ))),
        }
    }

    /// The epoch one bump after this one.
    pub(crate) fn next(self) -> AuthEpoch {
        AuthEpoch(self.0.saturating_add(1))
    }
}

/// A credential of `user` changed, and the epoch it is now at (module docs).
///
/// Consumed by [`SessionStore::bump_epoch`](crate::SessionStore::bump_epoch),
/// which ends what the old credentials handed out on this node. Dropping one
/// leaves the database right (the epoch is already raised, so every node
/// refuses an older session at its next read) but this node's cache and its
/// live sockets a minute behind, which is why it is `#[must_use]`.
#[must_use = "a credential change must be passed to SessionStore::bump_epoch, which ends the \
              account's other sessions on this node"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialsChanged {
    user: Uuid,
    epoch: AuthEpoch,
}

impl CredentialsChanged {
    /// The account whose credentials changed.
    pub fn user(&self) -> Uuid {
        self.user
    }

    /// The epoch the account is now at.
    pub fn epoch(&self) -> AuthEpoch {
        self.epoch
    }

    /// A change made up for a unit test of the session store.
    #[cfg(test)]
    pub(crate) fn new(user: Uuid, epoch: AuthEpoch) -> CredentialsChanged {
        CredentialsChanged { user, epoch }
    }

    /// Take it apart: what `bump_epoch` acts on.
    pub(crate) fn into_parts(self) -> (Uuid, AuthEpoch) {
        (self.user, self.epoch)
    }
}

fn epoch_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_USER, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain(COL_EPOCH, TypeRef::Basic(BasicType::Int)).required(),
    ]
}

/// Ensure the epochs table exists (additive: a new table, no migration).
pub(crate) async fn bootstrap_auth_epochs(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(AUTH_EPOCHS_TABLE, &epoch_fields())
        .await
}

/// The epoch `user` is at now. An installation whose epochs table is not there
/// yet (a database bootstrapped by an older release and not since) has every
/// account at the initial epoch.
pub async fn current_epoch(catalog: &Catalog, user: Uuid) -> Result<AuthEpoch> {
    if catalog.get(AUTH_EPOCHS_TABLE)?.is_none() {
        return Ok(AuthEpoch::INITIAL);
    }
    let select = Select::from(Source::table(AUTH_EPOCHS_TABLE))
        .columns(vec![Projection::expr(Expr::col(COL_EPOCH))])
        .filter(Expr::col(COL_USER).eq(Expr::lit(user)))
        .limit(1);
    let found = rows(catalog, Statement::from(select)).await?;
    AuthEpoch::of_column(found.first().and_then(|row| row.get(COL_EPOCH)))
}

/// Raise `user`'s epoch by one (module docs), returning the change to hand to
/// [`SessionStore::bump_epoch`](crate::SessionStore::bump_epoch).
///
/// An `UPDATE … RETURNING`, or the first row at 1 for an account that has none.
/// Two first bumps racing both try the insert; the loser's primary-key conflict
/// sends it back to the update, so neither bump is lost.
pub(crate) async fn bump_auth_epoch(catalog: &Catalog, user: Uuid) -> Result<CredentialsChanged> {
    if catalog.get(AUTH_EPOCHS_TABLE)?.is_none() {
        bootstrap_auth_epochs(catalog).await?;
    }
    let mut last = None;
    for _ in 0..3 {
        let update = Update {
            table: AUTH_EPOCHS_TABLE.to_owned(),
            assignments: vec![Assignment::new(
                COL_EPOCH,
                Expr::binary(BinOp::Add, Expr::col(COL_EPOCH), Expr::lit(1i64)),
            )],
            filter: Some(Expr::col(COL_USER).eq(Expr::lit(user))),
            returning: vec![Projection::expr(Expr::col(COL_EPOCH))],
        };
        let bumped = rows(catalog, Statement::from(update)).await?;
        if let Some(row) = bumped.first() {
            let epoch = AuthEpoch::of_column(row.get(COL_EPOCH))?;
            return Ok(CredentialsChanged { user, epoch });
        }
        let insert = Insert::row(
            AUTH_EPOCHS_TABLE,
            vec![COL_USER.to_owned(), COL_EPOCH.to_owned()],
            vec![Expr::lit(user), Expr::lit(1i64)],
        );
        match rows(catalog, Statement::from(insert)).await {
            Ok(_) => {
                return Ok(CredentialsChanged {
                    user,
                    epoch: AuthEpoch(1),
                });
            }
            // Another bump inserted the row first: go round and raise it.
            Err(e) => last = Some(e),
        }
    }
    Err(last
        .unwrap_or_else(|| Error::msg(format!("could not raise the auth epoch of user {user}"))))
}

/// Forget `user`'s epoch: the account is being deleted, and its id is never
/// reused.
pub(crate) async fn delete_auth_epoch(catalog: &Catalog, user: Uuid) -> Result<()> {
    if catalog.get(AUTH_EPOCHS_TABLE)?.is_none() {
        return Ok(());
    }
    let delete = Delete::from(AUTH_EPOCHS_TABLE).filter(Expr::col(COL_USER).eq(Expr::lit(user)));
    rows(catalog, Statement::from(delete)).await?;
    Ok(())
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await
}
