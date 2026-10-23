//! **LLM credits** (TODO-payments P6, design C3): a balance a user spends on
//! the model calls agent runs make on their behalf.
//!
//! # What it is for, and what it is not
//!
//! The ledger exists for **one** spender: an agent run's model calls (§3.8). It
//! is not a general metering facility — an application that wants to charge
//! for something else builds that from its own tables. So the debit
//! ([`debit_credits`]) is a Rust function the agent loop calls and nothing
//! else: no code body, workflow, action or module can reach it. Credits are
//! **granted** from outside: by an admin by hand, and by a module (the payments
//! module's allowances and top-ups) through `ctx.credits`.
//!
//! # Buckets, entries and the account
//!
//! - A **bucket** ([`CREDIT_BUCKETS_TABLE`]) is one grant: an amount, what is
//!   left of it, and an optional expiry. A user's balance is what is left in
//!   their unexpired buckets, less their debt. Like a role grant, a bucket that
//!   expires needs no write: the next read leaves it out.
//! - An **entry** ([`CREDIT_ENTRIES_TABLE`]) is one thing that happened: a
//!   grant, a debit (with the run, the model and the tokens) or a `low_balance`
//!   notice, with the balance after it. `(source, key)` is unique, which is
//!   what makes every operation idempotent: a replayed grant, or a debit for a
//!   step already charged, finds its entry and does nothing.
//! - The **account** ([`CREDIT_ACCOUNTS_TABLE`]) is one row per user who has
//!   had credits: the **debt** a debit left when the buckets ran dry, and the
//!   **low-balance threshold**. A model call has already happened when it is
//!   charged, so a debit may take the balance below zero; the next grant repays
//!   the debt before it fills a bucket.
//!
//! # One writer per user at a time
//!
//! Every operation runs in one transaction that first locks the user's `users`
//! row (`FOR UPDATE`), then reads and writes the buckets, the account and the
//! entries. Two debits for the same user — two agent runs at once — therefore
//! take turns, and neither can spend what the other already took. The users
//! row rather than the buckets is the lock because a user with no buckets yet
//! still needs one, and an account row created by whichever debit got there
//! first would otherwise race.
//!
//! # Ordinary tables, and their events
//!
//! The three tables have no `_fd_` prefix: an application reads them like any
//! other (a balance widget is a REST read of the buckets), each user reads
//! their own (`_read && user_id === user.id`), and only an admin may write.
//! Core writes them here, then raises each write as a table event once the
//! transaction commits, so the application's triggers fire on an entry — the
//! `low_balance` one is what an auto top-up hangs off — and a live read of the
//! buckets updates.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sc_catalog::{
    CallerContext, Catalog, DataField, DataFieldKind, FieldId, SharedTx, Table, TableConstraint,
    TableId, TableMeta, TableWrite, WriteOp, load_table_meta_by_name, save_table_meta_row,
};
use sc_db::{Row, SchemaChange};
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, OrderBy, Projection, Select, Source, Statement, UnOp,
    Update, Value,
};
use sc_types::{BasicType, TypeRef};
use serde::Serialize;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::users::{COL_EMAIL, COL_ID as COL_USER_ID_TARGET, ROLE_ADMIN, USERS_TABLE};

/// One bucket per grant.
pub const CREDIT_BUCKETS_TABLE: &str = "credit_buckets";
/// One row per thing that happened to a balance.
pub const CREDIT_ENTRIES_TABLE: &str = "credit_entries";
/// One row per user who has had credits: their debt and their threshold.
pub const CREDIT_ACCOUNTS_TABLE: &str = "credit_accounts";

/// Every row's id.
pub const COL_CREDIT_ID: &str = "id";
/// The user it belongs to (`→ users`).
pub const COL_CREDIT_USER: &str = "user_id";
/// A bucket's granted amount; an entry's change to the balance (negative for
/// a debit).
pub const COL_CREDIT_AMOUNT: &str = "amount";
/// What is left in a bucket.
pub const COL_CREDIT_REMAINING: &str = "remaining";
/// What kind of grant a bucket is (`admin`, `allowance`, `top_up`, …), or
/// what kind of entry (`grant`, `debit`, `low_balance`).
pub const COL_CREDIT_KIND: &str = "kind";
/// Who made it: `admin`, `agent`, or a module's name.
pub const COL_CREDIT_SOURCE: &str = "source";
/// The source's own key for it; unique with `source`.
pub const COL_CREDIT_KEY: &str = "key";
/// When a bucket stops counting; null for never.
pub const COL_CREDIT_EXPIRES_AT: &str = "expires_at";
/// A word for whoever reads the row.
pub const COL_CREDIT_NOTE: &str = "note";
/// When the row was written.
pub const COL_CREDIT_CREATED_AT: &str = "created_at";
/// An entry's balance after it.
pub const COL_ENTRY_BALANCE: &str = "balance";
/// The bucket a grant entry made.
pub const COL_ENTRY_BUCKET: &str = "bucket";
/// The agent run a debit was for.
pub const COL_ENTRY_RUN: &str = "run";
/// The agent whose run it was.
pub const COL_ENTRY_AGENT: &str = "agent";
/// The model that answered.
pub const COL_ENTRY_MODEL: &str = "model";
/// The call's input tokens.
pub const COL_ENTRY_INPUT_TOKENS: &str = "input_tokens";
/// The call's output tokens.
pub const COL_ENTRY_OUTPUT_TOKENS: &str = "output_tokens";
/// What the call cost at the model's prices, where known.
pub const COL_ENTRY_COST: &str = "cost";
/// An account's debt: what debits took beyond the buckets.
pub const COL_ACCOUNT_DEBT: &str = "debt";
/// An account's low-balance threshold; null for none.
pub const COL_ACCOUNT_THRESHOLD: &str = "low_balance_threshold";
/// When an account last changed.
pub const COL_ACCOUNT_UPDATED_AT: &str = "updated_at";

/// The source an admin's grant by hand carries.
pub const CREDIT_SOURCE_ADMIN: &str = "admin";
/// The source an agent run's debits carry.
pub const CREDIT_SOURCE_AGENT: &str = "agent";

/// An entry recording a grant.
pub const ENTRY_GRANT: &str = "grant";
/// An entry recording a debit.
pub const ENTRY_DEBIT: &str = "debit";
/// An entry recording what a grant took over from an earlier bucket
/// ([`CreditRollover`]): its amount is what that did to the balance — what was
/// carried, less what the earlier bucket still counted before it was emptied.
pub const ENTRY_ROLLOVER: &str = "rollover";
/// An entry saying a debit took the balance below the user's threshold — what
/// an application's auto top-up trigger listens for.
pub const ENTRY_LOW_BALANCE: &str = "low_balance";

/// The ownership formula all three tables are created with.
pub const CREDITS_OWNERSHIP: &str = "_read && user_id === user.id";

/// Credits are kept to this many decimal places: a millionth of a unit is far
/// below anything a rate per dollar makes of one model call.
pub const CREDIT_DECIMALS: u32 = 6;

/// A bucket, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreditBucket {
    /// The row's id.
    pub id: Uuid,
    /// The user it is for.
    pub user_id: Uuid,
    /// What was granted.
    pub amount: Decimal,
    /// What is left.
    pub remaining: Decimal,
    /// What kind of grant it was.
    pub kind: String,
    /// Who granted it.
    pub source: String,
    /// The source's key for it.
    pub key: String,
    /// When it stops counting, if ever.
    pub expires_at: Option<DateTime<Utc>>,
    /// The note, if any.
    pub note: Option<String>,
    /// When it was granted.
    pub created_at: DateTime<Utc>,
}

impl CreditBucket {
    /// Whether what is left of it counts at `now`.
    pub fn live_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_none_or(|end| end > now)
    }
}

/// An entry, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreditEntry {
    /// The row's id.
    pub id: Uuid,
    /// The user it is for.
    pub user_id: Uuid,
    /// `grant`, `debit` or `low_balance`.
    pub kind: String,
    /// The change to the balance: positive for a grant, negative for a debit,
    /// zero for a notice.
    pub amount: Decimal,
    /// The balance after it.
    pub balance: Decimal,
    /// Who made it.
    pub source: String,
    /// The source's key for it.
    pub key: String,
    /// The bucket a grant made.
    pub bucket: Option<Uuid>,
    /// The agent run a debit was for.
    pub run: Option<Uuid>,
    /// The agent whose run it was.
    pub agent: Option<String>,
    /// The model that answered.
    pub model: Option<String>,
    /// The call's input tokens.
    pub input_tokens: Option<i64>,
    /// The call's output tokens.
    pub output_tokens: Option<i64>,
    /// What the call cost at the model's prices, where known.
    pub cost: Option<Decimal>,
    /// The note, if any.
    pub note: Option<String>,
    /// When it was written.
    pub created_at: DateTime<Utc>,
}

/// A grant to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCreditGrant {
    /// Who receives it.
    pub user_id: Uuid,
    /// How much; more than zero.
    pub amount: Decimal,
    /// Who grants it: `admin`, or a module's name.
    pub source: String,
    /// The source's key for it, unique with the source: what makes a replayed
    /// grant do nothing.
    pub key: String,
    /// What kind of grant it is, for whoever reads the bucket (`allowance`,
    /// `top_up`); blank is `grant`.
    pub kind: String,
    /// When it stops counting; `None` is never.
    pub expires_at: Option<DateTime<Utc>>,
    /// A word for whoever reads the row.
    pub note: Option<String>,
    /// The earlier bucket of the same source whose remainder this one takes
    /// over (TODO-payments P7.1): a plan's allowance rolling over into the
    /// next period's. `None` takes over nothing.
    pub rollover: Option<CreditRollover>,
}

/// What a grant takes over from an earlier bucket of its source: what is left
/// of that bucket, up to `cap`, is carried into the new one, and the rest is
/// forfeited. The earlier bucket is emptied either way, in the grant's own
/// transaction, so it is never counted twice and a replayed grant carries
/// nothing again.
///
/// The earlier bucket's remainder counts even after it has expired: nothing
/// spends an expired bucket, so what it held at its expiry is still there —
/// and a renewal's grant usually arrives just after the period, and with it
/// the allowance, has ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditRollover {
    /// The earlier bucket's key, within the same source.
    pub from_key: String,
    /// The most carried over; `None` carries all of it, zero none.
    pub cap: Option<Decimal>,
}

/// What a grant did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreditGrant {
    /// The bucket — this grant's, or the one an earlier grant with the same key
    /// made.
    pub bucket: CreditBucket,
    /// How much of it repaid debt rather than filling the bucket.
    pub repaid: Decimal,
    /// The balance after it.
    pub balance: Decimal,
    /// Whether a grant with this key had already been made, so nothing was.
    pub replayed: bool,
    /// How much of an earlier bucket it took over ([`CreditRollover`]), on top
    /// of the amount granted.
    pub carried: Decimal,
}

/// A debit to make: one model call of an agent run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CreditDebit {
    /// Whose balance it comes from.
    pub user_id: Uuid,
    /// How much; zero or more. Zero is still recorded, so the entries list
    /// every metered call.
    pub amount: Decimal,
    /// The key, unique among debits: the run and the step.
    pub key: String,
    /// The run it was for.
    pub run: Option<Uuid>,
    /// The agent whose run it was.
    pub agent: Option<String>,
    /// The model that answered.
    pub model: Option<String>,
    /// The call's input tokens.
    pub input_tokens: Option<u64>,
    /// The call's output tokens.
    pub output_tokens: Option<u64>,
    /// What the call cost at the model's prices, where known.
    pub cost: Option<Decimal>,
    /// A word for whoever reads the row.
    pub note: Option<String>,
}

/// What a debit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CreditDebited {
    /// The balance after it; below zero when it ran into debt.
    pub balance: Decimal,
    /// Whether this step had already been charged, so nothing was.
    pub replayed: bool,
    /// Whether it took the balance below the user's threshold, so a
    /// `low_balance` entry was written.
    pub low_balance: bool,
}

/// A user's credits at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreditBalance {
    /// What can be spent: the live buckets less the debt. Below zero in debt.
    pub balance: Decimal,
    /// What is left in the live buckets.
    pub available: Decimal,
    /// What debits took beyond the buckets.
    pub debt: Decimal,
    /// The soonest a live bucket with something left expires.
    pub next_expiry: Option<DateTime<Utc>>,
    /// The low-balance threshold, if one is set.
    pub low_balance_threshold: Option<Decimal>,
}

fn decimal() -> TypeRef {
    TypeRef::Basic(BasicType::Decimal)
}

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}

fn timestamp() -> TypeRef {
    TypeRef::Basic(BasicType::Timestamp)
}

fn user_key() -> DataField {
    DataField {
        kind: DataFieldKind::Key {
            target_table: TableId(USERS_TABLE.to_owned()),
            target_field: FieldId(COL_USER_ID_TARGET.to_owned()),
            summary_field: Some(FieldId(COL_EMAIL.to_owned())),
        },
        ..DataField::plain(COL_CREDIT_USER, TypeRef::Basic(BasicType::Uuid)).required()
    }
}

fn id_field() -> DataField {
    DataField::plain(COL_CREDIT_ID, TypeRef::Basic(BasicType::Uuid))
        .required()
        .primary_key()
}

fn bucket_fields() -> Vec<DataField> {
    vec![
        id_field(),
        user_key(),
        DataField::plain(COL_CREDIT_AMOUNT, decimal()).required(),
        DataField::plain(COL_CREDIT_REMAINING, decimal()).required(),
        DataField::plain(COL_CREDIT_KIND, text()).required(),
        DataField::plain(COL_CREDIT_SOURCE, text()).required(),
        DataField::plain(COL_CREDIT_KEY, text()).required(),
        DataField::plain(COL_CREDIT_EXPIRES_AT, timestamp()),
        DataField::plain(COL_CREDIT_NOTE, text()),
        DataField::plain(COL_CREDIT_CREATED_AT, timestamp()).required(),
    ]
}

fn entry_fields() -> Vec<DataField> {
    vec![
        id_field(),
        user_key(),
        DataField::plain(COL_CREDIT_KIND, text()).required(),
        DataField::plain(COL_CREDIT_AMOUNT, decimal()).required(),
        DataField::plain(COL_ENTRY_BALANCE, decimal()).required(),
        DataField::plain(COL_CREDIT_SOURCE, text()).required(),
        DataField::plain(COL_CREDIT_KEY, text()).required(),
        DataField {
            kind: DataFieldKind::Key {
                target_table: TableId(CREDIT_BUCKETS_TABLE.to_owned()),
                target_field: FieldId(COL_CREDIT_ID.to_owned()),
                summary_field: Some(FieldId(COL_CREDIT_KIND.to_owned())),
            },
            ..DataField::plain(COL_ENTRY_BUCKET, TypeRef::Basic(BasicType::Uuid))
        },
        DataField::plain(COL_ENTRY_RUN, TypeRef::Basic(BasicType::Uuid)),
        DataField::plain(COL_ENTRY_AGENT, text()),
        DataField::plain(COL_ENTRY_MODEL, text()),
        DataField::plain(COL_ENTRY_INPUT_TOKENS, TypeRef::Basic(BasicType::Int)),
        DataField::plain(COL_ENTRY_OUTPUT_TOKENS, TypeRef::Basic(BasicType::Int)),
        DataField::plain(COL_ENTRY_COST, decimal()),
        DataField::plain(COL_CREDIT_NOTE, text()),
        DataField::plain(COL_CREDIT_CREATED_AT, timestamp()).required(),
    ]
}

fn account_fields() -> Vec<DataField> {
    vec![
        id_field(),
        DataField {
            unique: true,
            ..user_key()
        },
        DataField::plain(COL_ACCOUNT_DEBT, decimal()).required(),
        DataField::plain(COL_ACCOUNT_THRESHOLD, decimal()),
        DataField::plain(COL_ACCOUNT_UPDATED_AT, timestamp()).required(),
    ]
}

/// The jointly-unique `(source, key)`.
fn source_key() -> sc_catalog::ConstraintKind {
    sc_catalog::ConstraintKind::Unique {
        fields: vec![COL_CREDIT_SOURCE.to_owned(), COL_CREDIT_KEY.to_owned()],
    }
}

/// Ensure the three tables exist, with their keys and, when new, their access
/// settings. Idempotent; **after the users table**, which they reference.
pub async fn bootstrap_credits(catalog: &Catalog) -> Result<()> {
    sc_catalog::bootstrap_table_meta(catalog).await?;
    bootstrap_one(
        catalog,
        CREDIT_BUCKETS_TABLE,
        &bucket_fields(),
        true,
        "Credit buckets",
        "LLM credits granted to users, one bucket per grant, with what is left of it. \
         Written by Feldspar; each user reads their own.",
    )
    .await?;
    bootstrap_one(
        catalog,
        CREDIT_ENTRIES_TABLE,
        &entry_fields(),
        true,
        "Credit entries",
        "Every grant and debit of LLM credits, with the balance after it, and the \
         low-balance notices. Written by Feldspar; each user reads their own.",
    )
    .await?;
    bootstrap_one(
        catalog,
        CREDIT_ACCOUNTS_TABLE,
        &account_fields(),
        false,
        "Credit accounts",
        "Each user's LLM credit debt and low-balance threshold. Written by Feldspar; \
         each user reads their own.",
    )
    .await?;
    Ok(())
}

async fn bootstrap_one(
    catalog: &Catalog,
    name: &str,
    fields: &[DataField],
    keyed: bool,
    label: &str,
    description: &str,
) -> Result<Table> {
    let created = catalog.get(name)?.is_none();
    let mut table = catalog.bootstrap_table(name, fields).await?;
    let key = source_key();
    if keyed && !table.constraints.iter().any(|c| c.kind == key) {
        catalog
            .apply_schema_batch(&[sc_catalog::SchemaStep::Change(
                SchemaChange::AddUniqueConstraint {
                    table: name.to_owned(),
                    name: TableConstraint::derived_name(name, &key, ""),
                    columns: vec![COL_CREDIT_SOURCE.to_owned(), COL_CREDIT_KEY.to_owned()],
                },
            )])
            .await?;
        catalog.reload().await?;
        table = catalog.require(name)?;
    }
    if created && load_table_meta_by_name(catalog, name).await?.is_none() {
        let mut meta = TableMeta::new(name)
            .label(label)
            .description(description)
            .access(ROLE_ADMIN, ROLE_ADMIN);
        meta.set_ownership_formula(Some(CREDITS_OWNERSHIP));
        save_table_meta_row(catalog, &meta).await?;
        catalog.reload().await?;
        table = catalog.require(name)?;
    }
    Ok(table)
}

/// `amount` as the ledger keeps it: rounded to [`CREDIT_DECIMALS`] places.
pub fn credits(amount: Decimal) -> Decimal {
    amount.round_dp(CREDIT_DECIMALS).normalize()
}

/// Grant credits: repay any debt first, then fill a new bucket with the rest.
///
/// A grant whose `(source, key)` was already made does nothing and answers the
/// bucket that one made, with `replayed` set. A grant with a
/// [`CreditRollover`] first empties the earlier bucket it names and adds what
/// it carries to the new one, which then holds both. `caller` is who the
/// events the writes raise report.
pub async fn grant_credits(
    catalog: &Catalog,
    new: NewCreditGrant,
    caller: Option<&CallerContext>,
) -> Result<CreditGrant> {
    check_grant(catalog, &new)?;
    let amount = credits(new.amount);
    let outcome = locked(catalog, new.user_id, |tx, written| {
        let new = new.clone();
        Box::pin(async move {
            if let Some(entry) = find_entry(tx, &new.source, &new.key).await? {
                let bucket = match entry.bucket {
                    Some(id) => load_bucket(tx, id).await?,
                    None => None,
                }
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "the credit key `{}` from `{}` is already used, by something that \
                         is not a grant",
                        new.key, new.source
                    ))
                })?;
                if bucket.user_id != new.user_id {
                    return Err(Error::invalid(format!(
                        "the credit key `{}` from `{}` was already used for another user",
                        new.key, new.source
                    )));
                }
                let balance = balance_in(tx, new.user_id).await?.balance;
                return Ok(CreditGrant {
                    bucket,
                    repaid: Decimal::ZERO,
                    balance,
                    replayed: true,
                    carried: Decimal::ZERO,
                });
            }
            let now = Utc::now();
            let account = account_in(tx, new.user_id, written).await?;
            let rolled = match &new.rollover {
                Some(rollover) => roll_over(tx, &new, rollover, now, written).await?,
                None => None,
            };
            let carried = rolled.as_ref().map_or(Decimal::ZERO, |r| r.carried);
            let total = amount + carried;
            let repaid = account.debt.min(total).max(Decimal::ZERO);
            if repaid > Decimal::ZERO {
                set_debt(tx, &account, account.debt - repaid, written).await?;
            }
            let kind = match new.kind.trim() {
                "" => ENTRY_GRANT,
                kind => kind,
            };
            let bucket_row = insert_returning(
                tx,
                CREDIT_BUCKETS_TABLE,
                vec![
                    (COL_CREDIT_ID, Expr::lit(Uuid::new_v4())),
                    (COL_CREDIT_USER, Expr::lit(new.user_id)),
                    (COL_CREDIT_AMOUNT, dec(total)),
                    (COL_CREDIT_REMAINING, dec(total - repaid)),
                    (COL_CREDIT_KIND, Expr::lit(kind)),
                    (COL_CREDIT_SOURCE, Expr::lit(new.source.as_str())),
                    (COL_CREDIT_KEY, Expr::lit(new.key.as_str())),
                    (COL_CREDIT_EXPIRES_AT, optional_time(new.expires_at)),
                    (COL_CREDIT_NOTE, optional_text(new.note.as_deref())),
                    (COL_CREDIT_CREATED_AT, Expr::Lit(Value::Timestamp(now))),
                ],
            )
            .await?;
            let bucket = bucket_from_row(&bucket_row)?;
            written.insert(CREDIT_BUCKETS_TABLE, bucket_row);
            let balance = balance_in(tx, new.user_id).await?.balance;
            if let Some(rolled) = &rolled {
                // What the rollover did to the balance, before the grant's own
                // amount: the carried credits count again if the earlier
                // bucket had expired; what was forfeited counts no more.
                let change = rolled.carried - rolled.counted;
                let note = format!(
                    "{} carried over from {}{}",
                    rolled.carried.normalize(),
                    rolled.from_key,
                    match rolled.remaining - rolled.carried {
                        f if f > Decimal::ZERO => format!(", {} forfeited", f.normalize()),
                        _ => String::new(),
                    }
                );
                let entry_row = insert_entry(
                    tx,
                    EntryValues {
                        user_id: new.user_id,
                        kind: ENTRY_ROLLOVER,
                        amount: change,
                        balance: balance - amount,
                        source: &new.source,
                        key: &format!("{}:{ENTRY_ROLLOVER}", new.key),
                        bucket: Some(bucket.id),
                        note: Some(&note),
                        ..EntryValues::default()
                    },
                )
                .await?;
                written.insert(CREDIT_ENTRIES_TABLE, entry_row);
            }
            let entry_row = insert_entry(
                tx,
                EntryValues {
                    user_id: new.user_id,
                    kind: ENTRY_GRANT,
                    amount,
                    balance,
                    source: &new.source,
                    key: &new.key,
                    bucket: Some(bucket.id),
                    note: new.note.as_deref(),
                    ..EntryValues::default()
                },
            )
            .await?;
            written.insert(CREDIT_ENTRIES_TABLE, entry_row);
            Ok(CreditGrant {
                bucket,
                repaid,
                balance,
                replayed: false,
                carried,
            })
        })
    })
    .await;
    match outcome {
        Ok((grant, written)) => {
            written.emit(catalog, caller).await;
            Ok(grant)
        }
        // The same key granted at the same moment for another user, so the
        // two locks did not meet: the unique key refused the second. It is a
        // replay now; ask again, which finds it.
        Err(e) if is_unique_violation(&e) => {
            let mut again = Box::pin(grant_credits(catalog, new, caller)).await?;
            again.replayed = true;
            Ok(again)
        }
        Err(e) => Err(e),
    }
}

/// What a grant's rollover took from the earlier bucket.
struct RolledOver {
    from_key: String,
    /// What the earlier bucket held.
    remaining: Decimal,
    /// How much of it still counted toward the balance: all of it while the
    /// bucket was live, none once it had expired.
    counted: Decimal,
    /// How much of it the new bucket takes over.
    carried: Decimal,
}

/// Empty the earlier bucket `rollover` names, answering what was carried
/// from it. A bucket that is not there, or holds nothing, carries nothing.
/// Only under the lock.
async fn roll_over(
    tx: &SharedTx,
    new: &NewCreditGrant,
    rollover: &CreditRollover,
    now: DateTime<Utc>,
    written: &mut Written,
) -> Result<Option<RolledOver>> {
    let Some(old) = find_bucket(tx, &new.source, &rollover.from_key).await? else {
        return Ok(None);
    };
    if old.user_id != new.user_id {
        return Err(Error::invalid(format!(
            "the credit key `{}` from `{}` to roll over from is another user's",
            rollover.from_key, new.source
        )));
    }
    if old.remaining <= Decimal::ZERO {
        return Ok(None);
    }
    let cap = rollover.cap.map(credits).map(|c| c.max(Decimal::ZERO));
    let carried = cap.map_or(old.remaining, |c| old.remaining.min(c));
    let updated = update_returning(
        tx,
        CREDIT_BUCKETS_TABLE,
        old.id,
        vec![Assignment::new(COL_CREDIT_REMAINING, dec(Decimal::ZERO))],
    )
    .await?;
    written.update(CREDIT_BUCKETS_TABLE, updated);
    Ok(Some(RolledOver {
        from_key: rollover.from_key.clone(),
        remaining: old.remaining,
        counted: if old.live_at(now) {
            old.remaining
        } else {
            Decimal::ZERO
        },
        carried,
    }))
}

/// Take `debit.amount` from the user's live buckets, soonest-expiring first,
/// and record it. What the buckets do not cover becomes debt: the call has
/// already been made, so the debit is never refused for want of credit.
///
/// **Only the agent loop calls this** (§3.8): it is not offered to code bodies,
/// workflows or modules. A debit whose key was already charged does nothing.
pub async fn debit_credits(
    catalog: &Catalog,
    debit: CreditDebit,
    caller: Option<&CallerContext>,
) -> Result<CreditDebited> {
    if debit.key.trim().is_empty() {
        return Err(Error::invalid("a credit debit needs a key"));
    }
    if debit.amount < Decimal::ZERO {
        return Err(Error::invalid(format!(
            "a credit debit takes credits; {} is less than nothing",
            debit.amount
        )));
    }
    require_tables(catalog)?;
    let amount = credits(debit.amount);
    let (outcome, written) = locked(catalog, debit.user_id, |tx, written| {
        let debit = debit.clone();
        Box::pin(async move {
            if find_entry(tx, CREDIT_SOURCE_AGENT, &debit.key)
                .await?
                .is_some()
            {
                return Ok(CreditDebited {
                    balance: balance_in(tx, debit.user_id).await?.balance,
                    replayed: true,
                    low_balance: false,
                });
            }
            let now = Utc::now();
            let before = balance_in(tx, debit.user_id).await?;
            // Soonest-expiring first; a bucket that never expires last.
            let mut buckets = live_buckets(tx, debit.user_id, now, true).await?;
            buckets.sort_by_key(|b| (b.expires_at.is_none(), b.expires_at, b.created_at));
            let mut owed = amount;
            for bucket in buckets {
                if owed <= Decimal::ZERO {
                    break;
                }
                let take = owed.min(bucket.remaining);
                if take <= Decimal::ZERO {
                    continue;
                }
                owed -= take;
                let updated = update_returning(
                    tx,
                    CREDIT_BUCKETS_TABLE,
                    bucket.id,
                    vec![Assignment::new(
                        COL_CREDIT_REMAINING,
                        dec(bucket.remaining - take),
                    )],
                )
                .await?;
                written.update(CREDIT_BUCKETS_TABLE, updated);
            }
            let account = account_in(tx, debit.user_id, written).await?;
            if owed > Decimal::ZERO {
                set_debt(tx, &account, account.debt + owed, written).await?;
            }
            let balance = before.balance - amount;
            let entry_row = insert_entry(
                tx,
                EntryValues {
                    user_id: debit.user_id,
                    kind: ENTRY_DEBIT,
                    amount: -amount,
                    balance,
                    source: CREDIT_SOURCE_AGENT,
                    key: &debit.key,
                    run: debit.run,
                    agent: debit.agent.as_deref(),
                    model: debit.model.as_deref(),
                    input_tokens: debit.input_tokens,
                    output_tokens: debit.output_tokens,
                    cost: debit.cost,
                    note: debit.note.as_deref(),
                    ..EntryValues::default()
                },
            )
            .await?;
            written.insert(CREDIT_ENTRIES_TABLE, entry_row);
            // Crossing the threshold, not being under it: one notice per fall,
            // so an auto top-up fires once rather than on every later call.
            let low_balance = account
                .threshold
                .is_some_and(|t| crosses_below(before.balance, balance, t));
            if low_balance {
                let notice = insert_entry(
                    tx,
                    EntryValues {
                        user_id: debit.user_id,
                        kind: ENTRY_LOW_BALANCE,
                        amount: Decimal::ZERO,
                        balance,
                        source: CREDIT_SOURCE_AGENT,
                        key: &format!("{}:{ENTRY_LOW_BALANCE}", debit.key),
                        run: debit.run,
                        agent: debit.agent.as_deref(),
                        ..EntryValues::default()
                    },
                )
                .await?;
                written.insert(CREDIT_ENTRIES_TABLE, notice);
            }
            Ok(CreditDebited {
                balance,
                replayed: false,
                low_balance,
            })
        })
    })
    .await?;
    written.emit(catalog, caller).await;
    Ok(outcome)
}

/// Whether a fall from `before` to `after` goes below `threshold`.
fn crosses_below(before: Decimal, after: Decimal, threshold: Decimal) -> bool {
    before >= threshold && after < threshold
}

/// A user's balance now. A user who has never had credits has none.
pub async fn credit_balance(catalog: &Catalog, user: Uuid) -> Result<CreditBalance> {
    if catalog.get(CREDIT_BUCKETS_TABLE)?.is_none() {
        return Ok(CreditBalance::empty());
    }
    let now = Utc::now();
    let buckets = select_buckets(catalog, user, Some(now)).await?;
    let account = select_account(catalog, user).await?;
    Ok(summarise(&buckets, account.as_ref(), now))
}

impl CreditBalance {
    fn empty() -> CreditBalance {
        CreditBalance {
            balance: Decimal::ZERO,
            available: Decimal::ZERO,
            debt: Decimal::ZERO,
            next_expiry: None,
            low_balance_threshold: None,
        }
    }
}

/// Set or clear a user's low-balance threshold: a debit that takes the balance
/// from at or above it to below it writes a `low_balance` entry.
pub async fn set_low_balance_threshold(
    catalog: &Catalog,
    user: Uuid,
    threshold: Option<Decimal>,
    caller: Option<&CallerContext>,
) -> Result<CreditBalance> {
    require_tables(catalog)?;
    if threshold.is_some_and(|t| t < Decimal::ZERO) {
        return Err(Error::invalid(
            "a low-balance threshold is an amount of credits, zero or more",
        ));
    }
    let threshold = threshold.map(credits);
    let (balance, written) = locked(catalog, user, |tx, written| {
        Box::pin(async move {
            let account = account_in(tx, user, written).await?;
            let updated = update_returning(
                tx,
                CREDIT_ACCOUNTS_TABLE,
                account.id,
                vec![
                    Assignment::new(
                        COL_ACCOUNT_THRESHOLD,
                        threshold.map_or(Expr::Lit(Value::Null), dec),
                    ),
                    Assignment::new(
                        COL_ACCOUNT_UPDATED_AT,
                        Expr::Lit(Value::Timestamp(Utc::now())),
                    ),
                ],
            )
            .await?;
            written.update(CREDIT_ACCOUNTS_TABLE, updated);
            balance_in(tx, user).await
        })
    })
    .await?;
    written.emit(catalog, caller).await;
    Ok(balance)
}

/// Every bucket a user has had, newest first: live, spent and expired.
pub async fn list_credit_buckets(catalog: &Catalog, user: Uuid) -> Result<Vec<CreditBucket>> {
    if catalog.get(CREDIT_BUCKETS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    let mut buckets = select_buckets(catalog, user, None).await?;
    buckets.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    Ok(buckets)
}

/// A user's most recent entries, newest first, at most `limit`.
pub async fn list_credit_entries(
    catalog: &Catalog,
    user: Uuid,
    limit: u64,
) -> Result<Vec<CreditEntry>> {
    if catalog.get(CREDIT_ENTRIES_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    let mut select = Select::from(Source::table(CREDIT_ENTRIES_TABLE))
        .filter(Expr::col(COL_CREDIT_USER).eq(Expr::lit(user)))
        .limit(limit);
    select.order = vec![OrderBy::desc(Expr::col(COL_CREDIT_CREATED_AT))];
    rows(catalog, Statement::from(select))
        .await?
        .iter()
        .map(entry_from_row)
        .collect()
}

/// Delete every credit row a user has — what deleting the user does first,
/// since the rows reference them.
pub(crate) async fn delete_credits_for_user(catalog: &Catalog, user: Uuid) -> Result<()> {
    // Entries first: a grant's entry references its bucket.
    for table in [
        CREDIT_ENTRIES_TABLE,
        CREDIT_BUCKETS_TABLE,
        CREDIT_ACCOUNTS_TABLE,
    ] {
        if catalog.get(table)?.is_none() {
            continue;
        }
        let delete = Delete::from(table).filter(Expr::col(COL_CREDIT_USER).eq(Expr::lit(user)));
        rows(catalog, Statement::from(delete)).await?;
    }
    Ok(())
}

fn require_tables(catalog: &Catalog) -> Result<()> {
    for table in [
        CREDIT_BUCKETS_TABLE,
        CREDIT_ENTRIES_TABLE,
        CREDIT_ACCOUNTS_TABLE,
    ] {
        if catalog.get(table)?.is_none() {
            return Err(Error::config(format!(
                "this installation has no `{table}` table; restart the server to create it"
            )));
        }
    }
    Ok(())
}

fn check_grant(catalog: &Catalog, new: &NewCreditGrant) -> Result<()> {
    require_tables(catalog)?;
    if new.amount <= Decimal::ZERO {
        return Err(Error::invalid(format!(
            "a credit grant gives credits; {} is not more than nothing",
            new.amount
        )));
    }
    if credits(new.amount).is_zero() {
        return Err(Error::invalid(format!(
            "{} credits is less than the ledger keeps ({CREDIT_DECIMALS} decimal places)",
            new.amount
        )));
    }
    if new.source.trim().is_empty() || new.key.trim().is_empty() {
        return Err(Error::invalid(
            "a credit grant needs a source and a key, which together name it, so that \
             granting it again does nothing",
        ));
    }
    if let Some(rollover) = &new.rollover {
        if rollover.from_key.trim().is_empty() || rollover.from_key == new.key {
            return Err(Error::invalid(
                "a credit grant rolls over from an earlier grant's key, which is not its own",
            ));
        }
        if rollover.cap.is_some_and(|c| c < Decimal::ZERO) {
            return Err(Error::invalid(
                "a rollover's cap is the most carried over: zero or more",
            ));
        }
    }
    if new.source == CREDIT_SOURCE_AGENT {
        return Err(Error::invalid(
            "`agent` is the source of agent runs' debits; a grant needs another",
        ));
    }
    if let Some(end) = new.expires_at
        && end <= Utc::now()
    {
        return Err(Error::invalid(format!(
            "a credit grant that expires at {end} has already expired"
        )));
    }
    Ok(())
}

// ---- the locked transaction -------------------------------------------------

/// What a transaction wrote, to raise as table events once it commits.
#[derive(Default)]
struct Written {
    writes: Vec<(&'static str, WriteOp, Row, Option<Row>)>,
}

impl Written {
    fn insert(&mut self, table: &'static str, row: Row) {
        self.writes.push((table, WriteOp::Insert, row, None));
    }

    /// An update, as `(new, old)`.
    fn update(&mut self, table: &'static str, (new, old): (Row, Row)) {
        self.writes.push((table, WriteOp::Update, new, Some(old)));
    }

    /// Hand each write to the table events — the application's triggers and
    /// the live hub. A dispatch that fails is said in the log, never the
    /// operation's failure: the credits are already where they belong.
    async fn emit(self, catalog: &Catalog, caller: Option<&CallerContext>) {
        for (name, op, row, old) in self.writes {
            if !catalog.observes_writes(name, op) {
                continue;
            }
            let Ok(Some(table)) = catalog.get(name) else {
                continue;
            };
            let write = TableWrite {
                table: &table,
                op,
                row: row_json(&row),
                old_row: old.as_ref().map(row_json),
                caller,
                tx: None,
            };
            if let Err(e) = catalog.emit_write(write).await {
                sc_log::log_warn!(
                    "a write to `{name}` could not be raised as an event: {}",
                    sc_error::format_chain(&e)
                );
            }
        }
    }
}

fn row_json(row: &Row) -> Json {
    Json::Object(
        row.columns()
            .iter()
            .zip(row.values())
            .map(|(c, v)| (c.clone(), sc_expr::value_to_json(v)))
            .collect(),
    )
}

type Work<'t, T> = std::pin::Pin<Box<dyn Future<Output = Result<T>> + Send + 't>>;

/// Run `work` in a transaction holding `user`'s `users` row, and commit.
async fn locked<T, F>(catalog: &Catalog, user: Uuid, work: F) -> Result<(T, Written)>
where
    F: for<'t> FnOnce(&'t SharedTx, &'t mut Written) -> Work<'t, T>,
{
    let tx = SharedTx::begin_primary(catalog)?;
    let mut written = Written::default();
    let outcome = async {
        let lock = Select::from(Source::table(USERS_TABLE))
            .columns(vec![Projection::expr(Expr::col(COL_USER_ID_TARGET))])
            .filter(Expr::col(COL_USER_ID_TARGET).eq(Expr::lit(user)))
            .for_update();
        if tx.run(None, &Statement::from(lock)).await?.is_empty() {
            return Err(Error::not_found(format!("no user with id {user}")));
        }
        work(&tx, &mut written).await
    }
    .await;
    match outcome {
        Ok(value) => {
            tx.commit().await?;
            Ok((value, written))
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// An account, as the locked transaction reads it.
struct Account {
    id: Uuid,
    debt: Decimal,
    threshold: Option<Decimal>,
}

/// The user's account, made if they have none. Only under the lock.
async fn account_in(tx: &SharedTx, user: Uuid, written: &mut Written) -> Result<Account> {
    let select = Select::from(Source::table(CREDIT_ACCOUNTS_TABLE))
        .filter(Expr::col(COL_CREDIT_USER).eq(Expr::lit(user)))
        .for_update();
    if let Some(row) = tx.run(None, &Statement::from(select)).await?.first() {
        return account_from_row(row);
    }
    let row = insert_returning(
        tx,
        CREDIT_ACCOUNTS_TABLE,
        vec![
            (COL_CREDIT_ID, Expr::lit(Uuid::new_v4())),
            (COL_CREDIT_USER, Expr::lit(user)),
            (COL_ACCOUNT_DEBT, dec(Decimal::ZERO)),
            (COL_ACCOUNT_THRESHOLD, Expr::Lit(Value::Null)),
            (
                COL_ACCOUNT_UPDATED_AT,
                Expr::Lit(Value::Timestamp(Utc::now())),
            ),
        ],
    )
    .await?;
    let account = account_from_row(&row)?;
    written.insert(CREDIT_ACCOUNTS_TABLE, row);
    Ok(account)
}

async fn set_debt(
    tx: &SharedTx,
    account: &Account,
    debt: Decimal,
    written: &mut Written,
) -> Result<()> {
    let updated = update_returning(
        tx,
        CREDIT_ACCOUNTS_TABLE,
        account.id,
        vec![
            Assignment::new(COL_ACCOUNT_DEBT, dec(debt.max(Decimal::ZERO))),
            Assignment::new(
                COL_ACCOUNT_UPDATED_AT,
                Expr::Lit(Value::Timestamp(Utc::now())),
            ),
        ],
    )
    .await?;
    written.update(CREDIT_ACCOUNTS_TABLE, updated);
    Ok(())
}

async fn balance_in(tx: &SharedTx, user: Uuid) -> Result<CreditBalance> {
    let now = Utc::now();
    let buckets = live_buckets(tx, user, now, false).await?;
    let select = Select::from(Source::table(CREDIT_ACCOUNTS_TABLE))
        .filter(Expr::col(COL_CREDIT_USER).eq(Expr::lit(user)));
    let account = tx
        .run(None, &Statement::from(select))
        .await?
        .first()
        .map(account_from_row)
        .transpose()?;
    Ok(summarise(&buckets, account.as_ref(), now))
}

fn summarise(
    buckets: &[CreditBucket],
    account: Option<&Account>,
    now: DateTime<Utc>,
) -> CreditBalance {
    let live = || {
        buckets
            .iter()
            .filter(move |b| b.live_at(now) && b.remaining > Decimal::ZERO)
    };
    let available: Decimal = live().map(|b| b.remaining).sum();
    let debt = account.map_or(Decimal::ZERO, |a| a.debt);
    CreditBalance {
        balance: available - debt,
        available,
        debt,
        next_expiry: live().filter_map(|b| b.expires_at).min(),
        low_balance_threshold: account.and_then(|a| a.threshold),
    }
}

/// The user's buckets with something left that have not expired.
async fn live_buckets(
    tx: &SharedTx,
    user: Uuid,
    now: DateTime<Utc>,
    lock: bool,
) -> Result<Vec<CreditBucket>> {
    let mut select =
        Select::from(Source::table(CREDIT_BUCKETS_TABLE)).filter(live_filter(user, now));
    if lock {
        select = select.for_update();
    }
    tx.run(None, &Statement::from(select))
        .await?
        .iter()
        .map(bucket_from_row)
        .collect()
}

fn live_filter(user: Uuid, now: DateTime<Utc>) -> Expr {
    Expr::col(COL_CREDIT_USER)
        .eq(Expr::lit(user))
        .and(Expr::binary(
            BinOp::Gt,
            Expr::col(COL_CREDIT_REMAINING),
            dec(Decimal::ZERO),
        ))
        .and(
            Expr::unary(UnOp::IsNull, Expr::col(COL_CREDIT_EXPIRES_AT)).or(Expr::binary(
                BinOp::Gt,
                Expr::col(COL_CREDIT_EXPIRES_AT),
                Expr::Lit(Value::Timestamp(now)),
            )),
        )
}

async fn select_buckets(
    catalog: &Catalog,
    user: Uuid,
    live_at: Option<DateTime<Utc>>,
) -> Result<Vec<CreditBucket>> {
    let filter = match live_at {
        Some(now) => live_filter(user, now),
        None => Expr::col(COL_CREDIT_USER).eq(Expr::lit(user)),
    };
    let select = Select::from(Source::table(CREDIT_BUCKETS_TABLE)).filter(filter);
    rows(catalog, Statement::from(select))
        .await?
        .iter()
        .map(bucket_from_row)
        .collect()
}

async fn select_account(catalog: &Catalog, user: Uuid) -> Result<Option<Account>> {
    let select = Select::from(Source::table(CREDIT_ACCOUNTS_TABLE))
        .filter(Expr::col(COL_CREDIT_USER).eq(Expr::lit(user)));
    rows(catalog, Statement::from(select))
        .await?
        .first()
        .map(account_from_row)
        .transpose()
}

async fn find_entry(tx: &SharedTx, source: &str, key: &str) -> Result<Option<CreditEntry>> {
    let select = Select::from(Source::table(CREDIT_ENTRIES_TABLE))
        .filter(
            Expr::col(COL_CREDIT_SOURCE)
                .eq(Expr::lit(source))
                .and(Expr::col(COL_CREDIT_KEY).eq(Expr::lit(key))),
        )
        .limit(1);
    tx.run(None, &Statement::from(select))
        .await?
        .first()
        .map(entry_from_row)
        .transpose()
}

async fn find_bucket(tx: &SharedTx, source: &str, key: &str) -> Result<Option<CreditBucket>> {
    let select = Select::from(Source::table(CREDIT_BUCKETS_TABLE))
        .filter(
            Expr::col(COL_CREDIT_SOURCE)
                .eq(Expr::lit(source))
                .and(Expr::col(COL_CREDIT_KEY).eq(Expr::lit(key))),
        )
        .limit(1)
        .for_update();
    tx.run(None, &Statement::from(select))
        .await?
        .first()
        .map(bucket_from_row)
        .transpose()
}

async fn load_bucket(tx: &SharedTx, id: Uuid) -> Result<Option<CreditBucket>> {
    let select = Select::from(Source::table(CREDIT_BUCKETS_TABLE))
        .filter(Expr::col(COL_CREDIT_ID).eq(Expr::lit(id)))
        .limit(1);
    tx.run(None, &Statement::from(select))
        .await?
        .first()
        .map(bucket_from_row)
        .transpose()
}

/// An entry's values; what a kind does not use is left at its default.
#[derive(Default)]
struct EntryValues<'a> {
    user_id: Uuid,
    kind: &'a str,
    amount: Decimal,
    balance: Decimal,
    source: &'a str,
    key: &'a str,
    bucket: Option<Uuid>,
    run: Option<Uuid>,
    agent: Option<&'a str>,
    model: Option<&'a str>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost: Option<Decimal>,
    note: Option<&'a str>,
}

async fn insert_entry(tx: &SharedTx, entry: EntryValues<'_>) -> Result<Row> {
    let uuid = |u: Option<Uuid>| u.map_or(Expr::Lit(Value::Null), Expr::lit);
    let tokens = |t: Option<u64>| {
        t.map_or(Expr::Lit(Value::Null), |t| {
            Expr::lit(i64::try_from(t).unwrap_or(i64::MAX))
        })
    };
    insert_returning(
        tx,
        CREDIT_ENTRIES_TABLE,
        vec![
            (COL_CREDIT_ID, Expr::lit(Uuid::new_v4())),
            (COL_CREDIT_USER, Expr::lit(entry.user_id)),
            (COL_CREDIT_KIND, Expr::lit(entry.kind)),
            (COL_CREDIT_AMOUNT, dec(entry.amount)),
            (COL_ENTRY_BALANCE, dec(entry.balance)),
            (COL_CREDIT_SOURCE, Expr::lit(entry.source)),
            (COL_CREDIT_KEY, Expr::lit(entry.key)),
            (COL_ENTRY_BUCKET, uuid(entry.bucket)),
            (COL_ENTRY_RUN, uuid(entry.run)),
            (COL_ENTRY_AGENT, optional_text(entry.agent)),
            (COL_ENTRY_MODEL, optional_text(entry.model)),
            (COL_ENTRY_INPUT_TOKENS, tokens(entry.input_tokens)),
            (COL_ENTRY_OUTPUT_TOKENS, tokens(entry.output_tokens)),
            (
                COL_ENTRY_COST,
                entry.cost.map_or(Expr::Lit(Value::Null), dec),
            ),
            (COL_CREDIT_NOTE, optional_text(entry.note)),
            (
                COL_CREDIT_CREATED_AT,
                Expr::Lit(Value::Timestamp(Utc::now())),
            ),
        ],
    )
    .await
}

async fn insert_returning(
    tx: &SharedTx,
    table: &str,
    values: Vec<(&'static str, Expr)>,
) -> Result<Row> {
    let (columns, exprs): (Vec<String>, Vec<Expr>) =
        values.into_iter().map(|(c, e)| (c.to_owned(), e)).unzip();
    let insert = Insert::row(table, columns, exprs).returning(vec![Projection::all()]);
    tx.run(None, &Statement::from(insert))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::database(format!("inserting into `{table}` returned no row")))
}

/// Update one row by id, answering `(new, old)`.
async fn update_returning(
    tx: &SharedTx,
    table: &str,
    id: Uuid,
    assignments: Vec<Assignment>,
) -> Result<(Row, Row)> {
    let by_id = || Expr::col(COL_CREDIT_ID).eq(Expr::lit(id));
    let old = tx
        .run(
            None,
            &Statement::from(Select::from(Source::table(table)).filter(by_id())),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found(format!("no row {id} in `{table}`")))?;
    let update = Update {
        table: table.to_owned(),
        assignments,
        filter: Some(by_id()),
        returning: vec![Projection::all()],
    };
    let new = tx
        .run(None, &Statement::from(update))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found(format!("no row {id} in `{table}`")))?;
    Ok((new, old))
}

/// Whether `e` is the database refusing a duplicate key (SQLSTATE 23505, as
/// the Postgres driver writes it into the message).
fn is_unique_violation(e: &Error) -> bool {
    sc_error::format_chain(e).contains("[23505]")
}

fn dec(d: Decimal) -> Expr {
    Expr::Lit(Value::Decimal(d))
}

fn optional_time(t: Option<DateTime<Utc>>) -> Expr {
    Expr::Lit(t.map_or(Value::Null, Value::Timestamp))
}

fn optional_text(t: Option<&str>) -> Expr {
    match t.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => Expr::lit(t),
        None => Expr::Lit(Value::Null),
    }
}

// ---- reading rows back --------------------------------------------------------

struct Fields<'r> {
    row: &'r Row,
    table: &'static str,
}

impl Fields<'_> {
    fn bad(&self, column: &str) -> Error {
        Error::invalid(format!("{}.{column} is malformed", self.table))
    }

    fn uuid(&self, column: &str) -> Result<Uuid> {
        self.opt_uuid(column)?.ok_or_else(|| self.bad(column))
    }

    fn opt_uuid(&self, column: &str) -> Result<Option<Uuid>> {
        match self.row.get(column) {
            Some(Value::Uuid(u)) => Ok(Some(*u)),
            Some(Value::Null) | None => Ok(None),
            _ => Err(self.bad(column)),
        }
    }

    fn text(&self, column: &str) -> Result<String> {
        self.opt_text(column).ok_or_else(|| self.bad(column))
    }

    fn opt_text(&self, column: &str) -> Option<String> {
        match self.row.get(column) {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        }
    }

    fn decimal(&self, column: &str) -> Result<Decimal> {
        self.opt_decimal(column)?.ok_or_else(|| self.bad(column))
    }

    fn opt_decimal(&self, column: &str) -> Result<Option<Decimal>> {
        match self.row.get(column) {
            Some(Value::Decimal(d)) => Ok(Some(d.normalize())),
            Some(Value::Int(i)) => Ok(Some(Decimal::from(*i))),
            Some(Value::Null) | None => Ok(None),
            _ => Err(self.bad(column)),
        }
    }

    fn time(&self, column: &str) -> Result<DateTime<Utc>> {
        self.opt_time(column)?.ok_or_else(|| self.bad(column))
    }

    fn opt_time(&self, column: &str) -> Result<Option<DateTime<Utc>>> {
        match self.row.get(column) {
            Some(Value::Timestamp(t)) => Ok(Some(*t)),
            Some(Value::Null) | None => Ok(None),
            _ => Err(self.bad(column)),
        }
    }

    fn opt_int(&self, column: &str) -> Option<i64> {
        match self.row.get(column) {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        }
    }
}

fn bucket_from_row(row: &Row) -> Result<CreditBucket> {
    let f = Fields {
        row,
        table: CREDIT_BUCKETS_TABLE,
    };
    Ok(CreditBucket {
        id: f.uuid(COL_CREDIT_ID)?,
        user_id: f.uuid(COL_CREDIT_USER)?,
        amount: f.decimal(COL_CREDIT_AMOUNT)?,
        remaining: f.decimal(COL_CREDIT_REMAINING)?,
        kind: f.text(COL_CREDIT_KIND)?,
        source: f.text(COL_CREDIT_SOURCE)?,
        key: f.text(COL_CREDIT_KEY)?,
        expires_at: f.opt_time(COL_CREDIT_EXPIRES_AT)?,
        note: f.opt_text(COL_CREDIT_NOTE),
        created_at: f.time(COL_CREDIT_CREATED_AT)?,
    })
}

fn entry_from_row(row: &Row) -> Result<CreditEntry> {
    let f = Fields {
        row,
        table: CREDIT_ENTRIES_TABLE,
    };
    Ok(CreditEntry {
        id: f.uuid(COL_CREDIT_ID)?,
        user_id: f.uuid(COL_CREDIT_USER)?,
        kind: f.text(COL_CREDIT_KIND)?,
        amount: f.decimal(COL_CREDIT_AMOUNT)?,
        balance: f.decimal(COL_ENTRY_BALANCE)?,
        source: f.text(COL_CREDIT_SOURCE)?,
        key: f.text(COL_CREDIT_KEY)?,
        bucket: f.opt_uuid(COL_ENTRY_BUCKET)?,
        run: f.opt_uuid(COL_ENTRY_RUN)?,
        agent: f.opt_text(COL_ENTRY_AGENT),
        model: f.opt_text(COL_ENTRY_MODEL),
        input_tokens: f.opt_int(COL_ENTRY_INPUT_TOKENS),
        output_tokens: f.opt_int(COL_ENTRY_OUTPUT_TOKENS),
        cost: f.opt_decimal(COL_ENTRY_COST)?,
        note: f.opt_text(COL_CREDIT_NOTE),
        created_at: f.time(COL_CREDIT_CREATED_AT)?,
    })
}

fn account_from_row(row: &Row) -> Result<Account> {
    let f = Fields {
        row,
        table: CREDIT_ACCOUNTS_TABLE,
    };
    Ok(Account {
        id: f.uuid(COL_CREDIT_ID)?,
        debt: f.decimal(COL_ACCOUNT_DEBT)?,
        threshold: f.opt_decimal(COL_ACCOUNT_THRESHOLD)?,
    })
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn bucket(remaining: i64, expires_in: Option<i64>) -> CreditBucket {
        let now = Utc::now();
        CreditBucket {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            amount: Decimal::from(remaining),
            remaining: Decimal::from(remaining),
            kind: "grant".into(),
            source: "test".into(),
            key: Uuid::new_v4().to_string(),
            expires_at: expires_in.map(|s| now + Duration::seconds(s)),
            note: None,
            created_at: now,
        }
    }

    #[test]
    fn the_balance_is_the_live_buckets_less_the_debt() {
        let now = Utc::now();
        let soon = bucket(5, Some(60));
        let buckets = [bucket(10, None), soon.clone(), bucket(7, Some(-1))];
        let account = Account {
            id: Uuid::new_v4(),
            debt: Decimal::from(3),
            threshold: Some(Decimal::from(4)),
        };
        let b = summarise(&buckets, Some(&account), now);
        // The expired bucket of 7 does not count.
        assert_eq!(b.available, Decimal::from(15));
        assert_eq!(b.balance, Decimal::from(12));
        assert_eq!(b.next_expiry, soon.expires_at);
        assert_eq!(b.low_balance_threshold, Some(Decimal::from(4)));
        assert_eq!(summarise(&[], None, now), CreditBalance::empty());
    }

    #[test]
    fn a_notice_is_for_crossing_the_threshold_not_for_being_under_it() {
        let d = Decimal::from;
        assert!(crosses_below(d(10), d(4), d(5)));
        assert!(crosses_below(d(5), d(4), d(5)));
        assert!(!crosses_below(d(4), d(3), d(5)), "already under");
        assert!(!crosses_below(d(10), d(5), d(5)), "at it is not below it");
    }

    #[test]
    fn credits_are_kept_to_six_places() {
        let third = Decimal::ONE / Decimal::from(3);
        assert_eq!(credits(third).to_string(), "0.333333");
        assert_eq!(credits(Decimal::new(1500, 3)).to_string(), "1.5");
    }
}
