//! Offline sync, the upload side (§14.11, `TODO-offline-sync.md` S3): a device
//! sends the inserts, updates and deletes it made, numbered in the order it
//! made them, and the server applies them one by one as its user.
//!
//! - **Through the API's own rules.** Each change is a write as the user
//!   ([`write_as`]): access rules, ownership, validation, calculated fields and
//!   triggers apply as to any REST write, so a device can do nothing its user
//!   could not.
//! - **One transaction per change**, committed before the next starts. Parents
//!   arrive before their children because the device sent them first, and a
//!   later change's ownership check sees the rows the earlier ones wrote.
//! - **A refused change does not stop the others.** It is rolled back to a
//!   savepoint and reported with its reason; the device decides what to do with
//!   it (its next pull brings the server's row).
//! - **Retrying is safe.** The change's number is claimed in
//!   `_fd_sync_clients.last_mutation` in the same transaction, so a change is
//!   handled exactly once, applied or refused, and a retried upload skips what
//!   is done. Claiming it also locks the device's row while the change runs: two
//!   uploads of one device may take turns, change by change, but each change is
//!   still handled once and in order.
//! - **Marked as the device's.** The device's id and the change's number are set
//!   for the trigger, so the change-log knows where the change came from.
//! - **Temporary keys.** In a table whose integer key the server issues, a
//!   device creates a row under a temporary key, its change number negated
//!   (`-17`). The server inserts the row without it, answers the real key, and
//!   keeps the pair in `_fd_sync_keys`: later changes naming `-17`, as their key
//!   or in a foreign key, are translated, in this upload or any later one.

use std::collections::HashMap;
use std::sync::Arc;

use sc_auth::User;
use sc_catalog::{
    CallerContext, Catalog, DataFieldKind, SYNC_KEYS_TABLE, SharedTx, Table, mark_sync_device,
};
use sc_error::{Error, Repr, Result};
use sc_expr::JsEvaluator;
use sc_query::{Statement, Value};
use sc_types::BasicType;
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use super::device::{check_device, devices_table};
use super::pull::key_text;
use crate::ownership;
use crate::rows::{self, Executor};
use crate::schema::{StructField, TypeSchema};
use crate::write_as;

/// The savepoint each change is written inside. Structural, never user data.
const CHANGE_SAVEPOINT: &str = "sc_sync_change";

/// `POST {mount}/sync/push`'s body: the device, and its changes in the order it
/// made them.
///
/// Per change: `mutation`, the device's own number for it, increasing; `table`;
/// `op`, one of `insert`, `update`, `delete`; `key`, the row's primary key
/// (for an insert it may be in `row` instead); `row`, the inserted row, or an
/// update's changed fields; `temp`, true on an insert whose `key` is temporary
/// (`-mutation`) and the server issues the real one.
pub(crate) fn push_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("client_id", TypeSchema::text()),
        StructField::new(
            "changes",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("mutation", TypeSchema::int()),
                StructField::new("table", TypeSchema::text()),
                StructField::new("op", TypeSchema::text()),
                StructField::new("key", TypeSchema::optional(TypeSchema::text())),
                StructField::new("row", TypeSchema::optional(TypeSchema::json())),
                StructField::new("temp", TypeSchema::optional(TypeSchema::bool())),
            ])),
        ),
    ])
}

/// The answer: one result per change, in order. `status` is `applied`,
/// `refused` (with `error` saying why) or `skipped`: handled by an earlier
/// upload, applied or refused — that upload's answer said which, and the next
/// pull brings the server's row either way. A `temp` insert that was applied,
/// now or by an earlier upload, has `key`: the row's real key.
pub(crate) fn push_output_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new(
        "results",
        TypeSchema::array(TypeSchema::struct_of([
            StructField::new("mutation", TypeSchema::int()),
            StructField::new("status", TypeSchema::text()),
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            StructField::new("key", TypeSchema::optional(TypeSchema::text())),
        ])),
    )])
}

/// Apply one upload of a device of `user`, to the synced tables among `exposed`.
pub(crate) async fn push(
    cat: &Catalog,
    application: Uuid,
    exposed: &[String],
    user: &User,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
    body: &Json,
) -> Result<Json> {
    let request = PushRequest::parse(body)?;
    let devices = devices_table(cat)?;
    check_device(cat, &devices, request.client_id, application, user).await?;
    let context = ownership::caller_context(Some(user));
    let device = Device {
        id: request.client_id,
        table: &devices,
        user,
        evaluator,
        context: &context,
        exposed,
    };
    let mut made = MadeKeys::new();
    let mut results = Vec::with_capacity(request.changes.len());
    for change in &request.changes {
        let outcome = device.apply(cat, change, &made).await?;
        let (mut result, real) = match outcome {
            Outcome::Applied(real) => (
                json!({ "mutation": change.mutation, "status": "applied" }),
                real,
            ),
            Outcome::Skipped(real) => (
                json!({ "mutation": change.mutation, "status": "skipped" }),
                real,
            ),
            Outcome::Refused(why) => (
                json!({ "mutation": change.mutation, "status": "refused", "error": why }),
                None,
            ),
        };
        if change.temp {
            if let Some(real) = &real {
                result["key"] = Json::String(real.clone());
            }
            let temp = change.key.clone().unwrap_or_default();
            made.insert((change.table.clone(), temp), real);
        }
        results.push(result);
    }
    Ok(json!({ "results": results }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Insert,
    Update,
    Delete,
}

/// One change, as the device sent it.
struct Change {
    mutation: i64,
    table: String,
    op: Op,
    key: Option<String>,
    row: Map<String, Json>,
    /// An insert under a temporary key (`key` is `-mutation`): the server issues
    /// the real one.
    temp: bool,
}

/// What this upload's temporary keys became, by table and temporary key: the
/// real key, or `None` when the insert was refused.
type MadeKeys = HashMap<(String, String), Option<String>>;

/// A temporary key: a negative whole number, which a key the server issues
/// never is.
fn temp_key(value: &Json) -> Option<i64> {
    let n = match value {
        Json::Number(n) => n.as_i64(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }?;
    (n < 0).then_some(n)
}

struct PushRequest {
    client_id: Uuid,
    changes: Vec<Change>,
}

impl PushRequest {
    /// A malformed upload is refused whole: it is a bug in the device, not a
    /// change the user made.
    fn parse(body: &Json) -> Result<PushRequest> {
        let client_id = body
            .get("client_id")
            .and_then(Json::as_str)
            .and_then(|s| Uuid::parse_str(s.trim()).ok())
            .ok_or_else(|| Error::invalid("`client_id` must be the id `sync/register` answered"))?;
        let items = match body.get("changes") {
            Some(Json::Array(items)) => items,
            _ => return Err(Error::invalid("`changes` must be a list")),
        };
        let mut changes = Vec::with_capacity(items.len());
        let mut last = 0;
        for (i, item) in items.iter().enumerate() {
            let at = |what: &str| Error::invalid(format!("change {}: {what}", i + 1));
            let mutation = item
                .get("mutation")
                .and_then(Json::as_i64)
                .filter(|n| *n > 0)
                .ok_or_else(|| at("`mutation` must be a positive whole number"))?;
            if mutation <= last {
                return Err(at(
                    "`mutation` numbers must increase, in the order the changes were made",
                ));
            }
            last = mutation;
            let table = item
                .get("table")
                .and_then(Json::as_str)
                .ok_or_else(|| at("`table` must name a table"))?
                .to_owned();
            let op = match item.get("op").and_then(Json::as_str) {
                Some("insert") => Op::Insert,
                Some("update") => Op::Update,
                Some("delete") => Op::Delete,
                _ => return Err(at("`op` must be `insert`, `update` or `delete`")),
            };
            let key = match item.get("key") {
                None | Some(Json::Null) => None,
                Some(key @ (Json::String(_) | Json::Number(_))) => Some(key_text(key)),
                Some(_) => return Err(at("`key` must be text or a number")),
            };
            let row = match item.get("row") {
                None | Some(Json::Null) => Map::new(),
                Some(Json::Object(row)) => row.clone(),
                Some(_) => return Err(at("`row` must be an object")),
            };
            if op != Op::Insert && key.is_none() {
                return Err(at("an update or delete needs the row's `key`"));
            }
            let temp = item.get("temp").and_then(Json::as_bool).unwrap_or(false);
            if temp && (op != Op::Insert || key != Some((-mutation).to_string())) {
                return Err(at(
                    "`temp` is for an insert whose `key` is the change's number, negated",
                ));
            }
            changes.push(Change {
                mutation,
                table,
                op,
                key,
                row,
                temp,
            });
        }
        Ok(PushRequest { client_id, changes })
    }
}

/// What became of one change. `Applied` and `Skipped` carry the real key of a
/// row created under a temporary one.
enum Outcome {
    Applied(Option<String>),
    Skipped(Option<String>),
    Refused(String),
}

/// The uploading device, and who it writes as.
struct Device<'a> {
    id: Uuid,
    table: &'a Table,
    user: &'a User,
    evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    context: &'a CallerContext,
    exposed: &'a [String],
}

impl Device<'_> {
    /// Claim `change`'s number and apply it, in a transaction of its own.
    async fn apply(&self, cat: &Catalog, change: &Change, made: &MadeKeys) -> Result<Outcome> {
        let tx = SharedTx::begin_primary(cat)?;
        if !self.claim(cat, &tx, change.mutation).await? {
            // Handled before: a row it created keeps its key for the device.
            let key = if change.temp {
                self.made_key(&tx, change.mutation, &change.table).await?
            } else {
                None
            };
            let _ = tx.rollback().await;
            return Ok(Outcome::Skipped(key));
        }
        tx.savepoint(CHANGE_SAVEPOINT).await?;
        let outcome = match self.write(cat, &tx, change, made).await {
            Ok(Ok(key)) => {
                tx.release_savepoint(CHANGE_SAVEPOINT).await?;
                Outcome::Applied(key)
            }
            Ok(Err(why)) => {
                tx.rollback_to_savepoint(CHANGE_SAVEPOINT).await?;
                Outcome::Refused(why)
            }
            Err(e) => match refusal(&e) {
                Some(why) => {
                    tx.rollback_to_savepoint(CHANGE_SAVEPOINT).await?;
                    Outcome::Refused(why)
                }
                // Not this change's fault (the database is gone, a bug): stop
                // here. What is committed stays, and the device retries the rest.
                None => {
                    let _ = tx.rollback().await;
                    return Err(e);
                }
            },
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Mark `mutation` handled for this device, unless it already is.
    async fn claim(&self, cat: &Catalog, tx: &SharedTx, mutation: i64) -> Result<bool> {
        let sql = format!(
            "UPDATE {devices} SET last_mutation = $1 \
             WHERE id = $2 AND coalesce(last_mutation, 0) < $1 RETURNING id",
            devices = cat.primary().dialect().quote_ident(&self.table.name),
        );
        let claimed = tx
            .run(
                Some(self.context),
                &Statement::raw(sql, vec![Value::Int(mutation), Value::Uuid(self.id)]),
            )
            .await?;
        Ok(!claimed.is_empty())
    }

    /// Write one change as the user, answering the real key of a row created
    /// under a temporary one. `Ok(Err(why))` is a refusal decided here; an `Err`
    /// may still be one (see [`refusal`]).
    async fn write(
        &self,
        cat: &Catalog,
        tx: &SharedTx,
        change: &Change,
        made: &MadeKeys,
    ) -> Result<std::result::Result<Option<String>, String>> {
        let Some(table) = self.synced_table(cat, &change.table)? else {
            return Ok(Err(format!(
                "`{}` is not a synced table of this application",
                change.table
            )));
        };
        let pk = rows::single_pk(&table)?;
        // For the trigger: these entries are this device's.
        mark_sync_device(tx, Some(self.context), self.id, change.mutation).await?;

        let executor = Executor::Transaction(tx.clone());
        let caller = write_as::Caller {
            user: Some(self.user),
            evaluator: self.evaluator,
            context: self.context,
            executor: &executor,
        };
        let mut row = change.row.clone();
        let mut key = change.key.clone();
        // Rows the device created offline still have temporary keys on its side.
        // Swap in the real ones: for the row being updated or deleted, and in
        // columns that point to another row.
        if change.op != Op::Insert
            && let Some(offline_key) = key.as_deref().and_then(|k| temp_key(&Json::from(k)))
            && issues_keys(&table, &pk)
        {
            match self.resolve(tx, made, &table.name, offline_key).await? {
                Ok(real) => key = Some(real),
                Err(why) => return Ok(Err(why)),
            }
        }
        for field in &table.fields {
            let DataFieldKind::Key { target_table, .. } = &field.kind else {
                continue;
            };
            let Some(offline_key) = row.get(&field.base.name).and_then(temp_key) else {
                continue;
            };
            let target = cat.require(&target_table.0)?;
            let target_pk = rows::single_pk(&target)?;
            if !issues_keys(&target, &target_pk) {
                continue;
            }
            match self.resolve(tx, made, &target.name, offline_key).await? {
                Ok(real) => {
                    let value = real.parse::<i64>().map_or(Json::String(real), Json::from);
                    row.insert(field.base.name.clone(), value);
                }
                Err(why) => return Ok(Err(why)),
            }
        }
        let denied = |d: write_as::Denied| d.message().to_owned();
        Ok(match change.op {
            Op::Insert if change.temp => {
                row.remove(&pk);
                let created =
                    match write_as::create(cat, &table, &Json::Object(row), &caller).await? {
                        Ok(created) => created,
                        Err(d) => return Ok(Err(denied(d))),
                    };
                let real = created
                    .get(&pk)
                    .filter(|v| !v.is_null())
                    .map(key_text)
                    .ok_or_else(|| {
                        Error::msg(format!(
                            "an insert into `{}` answered no `{pk}`",
                            table.name
                        ))
                    })?;
                self.save_made_key(cat, tx, change, &real).await?;
                Ok(Some(real))
            }
            Op::Insert => {
                match (&change.key, row.get(&pk)) {
                    (None, None | Some(Json::Null)) => {
                        return Ok(Err(format!(
                            "an insert from a device must carry its key, `{pk}`"
                        )));
                    }
                    (Some(key), None | Some(Json::Null)) => {
                        row.insert(pk.clone(), Json::String(key.clone()));
                    }
                    (Some(key), Some(given)) if key_text(given) != *key => {
                        return Ok(Err(format!("`key` and the row's `{pk}` differ")));
                    }
                    _ => {}
                }
                write_as::create(cat, &table, &Json::Object(row), &caller)
                    .await?
                    .map(|_| None)
                    .map_err(denied)
            }
            Op::Update => {
                let key = key.as_deref().unwrap_or_default();
                if let Some(given) = row.remove(&pk)
                    && key_text(&given) != key
                {
                    return Ok(Err(
                        "a row's primary key cannot be changed from a device".to_owned()
                    ));
                }
                if row.is_empty() {
                    return Ok(Ok(None));
                }
                write_as::update(cat, &table, key, &Json::Object(row), &caller)
                    .await?
                    .map(|_| None)
                    .map_err(denied)
            }
            Op::Delete => {
                let key = key.as_deref().unwrap_or_default();
                write_as::delete(cat, &table, key, &caller)
                    .await?
                    .map(|()| None)
                    .map_err(denied)
            }
        })
    }

    /// Look up the real key for a temporary key `temp` in `table`. It looks in this
    /// upload first, then in earlier ones. If the row was never created, the
    /// change is refused with the reason.
    async fn resolve(
        &self,
        tx: &SharedTx,
        made: &MadeKeys,
        table: &str,
        temp: i64,
    ) -> Result<std::result::Result<String, String>> {
        let refused = || {
            format!(
                "it refers to a row of `{table}` whose creation was refused (temporary key {temp})"
            )
        };
        if let Some(real) = made.get(&(table.to_owned(), temp.to_string())) {
            return Ok(real.clone().ok_or_else(refused));
        }
        Ok(self.made_key(tx, -temp, table).await?.ok_or_else(refused))
    }

    /// The key a row got that this device created in its change `mutation`.
    async fn made_key(&self, tx: &SharedTx, mutation: i64, table: &str) -> Result<Option<String>> {
        let rows = tx
            .run(
                Some(self.context),
                &Statement::raw(
                    format!(
                        "SELECT row_key FROM \"{SYNC_KEYS_TABLE}\" \
                         WHERE client_id = $1 AND mutation = $2 AND table_name = $3"
                    ),
                    vec![
                        Value::Uuid(self.id),
                        Value::Int(mutation),
                        Value::Text(table.to_owned()),
                    ],
                ),
            )
            .await?;
        Ok(rows.first().and_then(|r| match r.get("row_key") {
            Some(Value::Text(key)) => Some(key.clone()),
            _ => None,
        }))
    }

    /// Save the real key of the row `change` just created in `_fd_sync_keys`, so a
    /// retried or later upload that names its temporary key finds it
    /// ([`made_key`](Self::made_key)). Runs in the insert's transaction: no row, no pair.
    async fn save_made_key(
        &self,
        cat: &Catalog,
        tx: &SharedTx,
        change: &Change,
        real: &str,
    ) -> Result<()> {
        let sql = format!(
            "INSERT INTO {keys} (client_id, mutation, table_name, row_key) VALUES ($1, $2, $3, $4)",
            keys = cat.primary().dialect().quote_ident(SYNC_KEYS_TABLE),
        );
        tx.run(
            Some(self.context),
            &Statement::raw(
                sql,
                vec![
                    Value::Uuid(self.id),
                    Value::Int(change.mutation),
                    Value::Text(change.table.clone()),
                    Value::Text(real.to_owned()),
                ],
            ),
        )
        .await?;
        Ok(())
    }

    /// `name`, when it is one of this application's tables and synced.
    fn synced_table(&self, cat: &Catalog, name: &str) -> Result<Option<Table>> {
        if !self.exposed.iter().any(|t| t == name) {
            return Ok(None);
        }
        Ok(cat.get(name)?.filter(Table::sync_enabled))
    }
}

/// Whether the server makes `table`'s keys (an integer key with a default).
/// Only then can a device use a temporary key for a row it created.
fn issues_keys(table: &Table, pk: &str) -> bool {
    table
        .field(pk)
        .is_some_and(|f| f.has_default() && matches!(f.base.type_.as_basic(), Some(BasicType::Int)))
}

/// The reason to report when `error` is the change's own fault: a rule it broke
/// (access, validation, a constraint, a row not there) rather than a fault of the
/// server.
///
/// A configuration error (a broken formula, a query over a missing field) is not
/// the change's fault: refusing it would mark the change handled, and fixing the
/// configuration could no longer bring it back. It stops the upload instead, so
/// the device retries once the app is fixed.
fn refusal(error: &Error) -> Option<String> {
    if broke_a_rule(error) {
        return Some(error.to_string());
    }
    // Integrity (`23…`: a foreign key, not null, unique) and data (`22…`: a
    // value the column refuses) violations arrive as database errors, with
    // their SQLSTATE in brackets.
    let chain = sc_error::format_chain(error);
    (chain.contains(" [23") || chain.contains(" [22")).then(|| rows::database_message(&chain))
}

/// A row not there, an invalid value, no access, or the app's own code refusing
/// with a client status.
fn broke_a_rule(error: &Error) -> bool {
    match error.repr() {
        Repr::NotFound(_) | Repr::Invalid(_) | Repr::Auth(_) => true,
        Repr::Http(reply) => reply.status < 500,
        Repr::Context { source, .. } => source.downcast_ref::<Error>().is_some_and(broke_a_rule),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_broken_rule_refuses_a_change() {
        assert!(refusal(&Error::not_found("no row with id = 7")).is_some());
        assert!(refusal(&Error::invalid("title is required")).is_some());
        assert!(refusal(&Error::auth("insufficient privilege")).is_some());
        assert!(refusal(&Error::http(422, "the app said no", None)).is_some());
        // The app's or the server's fault: the upload stops, the change stays unclaimed.
        assert!(refusal(&Error::config("the ownership formula is broken")).is_none());
        assert!(refusal(&Error::query("no field `nope`")).is_none());
        assert!(refusal(&Error::http(500, "the app's code crashed", None)).is_none());
        assert!(refusal(&Error::database("connection reset")).is_none());
    }
}
