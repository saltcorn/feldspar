//! Restore and undo (§14.12 "Restore and undo", TODO-history H3): putting a
//! deleted row back, putting a row back as it was at one of its versions, and
//! taking back what one changeset did.
//!
//! All three are **ordinary writes through the row layer, as the caller**
//! ([`write_as`]): write rules, ownership, validation and triggers apply, so
//! none of them grants anything the caller could not do by hand. Each runs in
//! **one transaction** under a changeset of its own (`source` `restore` or
//! `undo`, `cause` what it reverses), which the change log records like any
//! other, so an undo can itself be undone.
//!
//! - Columns that no longer exist are dropped, and the answer names them.
//! - A write that would refer to a row that is gone is refused with the reason
//!   before the database refuses it in words nobody can act on.
//! - An undo first checks that every entry's columns still hold the values the
//!   changeset left (the same question sync's field-wise merge asks). If they
//!   do not, it is refused with the conflicts listed, unless an admin passes
//!   `force`.
//!
//! Effects outside the database (an email a trigger sent) are not undone.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use sc_auth::{ROLE_ADMIN, User};
use sc_catalog::history::{self, AsOf, HistoryEntry};
use sc_catalog::{
    CallerContext, Catalog, Changeset, ChangesetSource, DataFieldKind, SharedTx, Table, changeset,
};
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_query::{Statement, Value};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::history::{HistoryReader, not_found, redacted};
use crate::ownership::{caller_context_at, caller_role};
use crate::rows::{self, Executor};
use crate::write_as::{self, Caller, Denied};

/// Who is restoring or undoing: their role and user, and the evaluator a
/// write rule may need.
#[derive(Clone, Copy)]
pub struct Reverser<'a> {
    /// The catalog.
    pub catalog: &'a Catalog,
    /// The caller's role (public when nobody is signed in).
    pub role: u8,
    /// The caller, when signed in.
    pub user: Option<&'a User>,
    /// For a write rule that needs JavaScript.
    pub evaluator: Option<&'a Arc<dyn JsEvaluator>>,
}

/// A restored or reverted row.
#[derive(Debug, Clone, PartialEq)]
pub struct Restored {
    /// The `restore` changeset it was written in.
    pub changeset: Uuid,
    /// The row as written, secrets as the sentinel.
    pub row: Json,
    /// Columns the old row had that the table no longer has, left out.
    pub dropped: Vec<String>,
}

impl Restored {
    /// The wire shape.
    pub fn to_json(&self) -> Json {
        json!({ "changeset": self.changeset, "row": self.row, "dropped": self.dropped })
    }
}

/// Why one entry cannot be undone cleanly.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    /// The entry's table.
    pub table: String,
    /// The row's key.
    pub key: String,
    /// The column changed since, or `None` when the row itself is gone (or is
    /// back).
    pub field: Option<String>,
    /// What the changeset left in it (the sentinel for a secret).
    pub expected: Json,
    /// What it holds now.
    pub actual: Json,
    /// The conflict in a sentence.
    pub reason: String,
}

impl Conflict {
    /// The wire shape.
    pub fn to_json(&self) -> Json {
        json!({
            "table": self.table,
            "key": self.key,
            "field": self.field,
            "expected": self.expected,
            "actual": self.actual,
            "reason": self.reason,
        })
    }
}

/// What an undo did.
#[derive(Debug, Clone, PartialEq)]
pub enum UndoOutcome {
    /// The entries were reversed in the `undo` changeset `changeset`.
    Undone {
        /// The undo's own changeset, which can be undone in turn.
        changeset: Uuid,
        /// How many entries were reversed.
        entries: usize,
        /// `table.column` of each value left out because the column is gone.
        dropped: Vec<String>,
    },
    /// Nothing was written: these rows were changed since.
    Refused(Vec<Conflict>),
}

impl UndoOutcome {
    /// The wire shape: `undone`, and the changeset and counts or the
    /// conflicts.
    pub fn to_json(&self) -> Json {
        match self {
            UndoOutcome::Undone {
                changeset,
                entries,
                dropped,
            } => json!({
                "undone": true,
                "changeset": changeset,
                "entries": entries,
                "dropped": dropped,
                "conflicts": [],
            }),
            UndoOutcome::Refused(conflicts) => json!({
                "undone": false,
                "changeset": null,
                "entries": 0,
                "dropped": [],
                "conflicts": conflicts.iter().map(Conflict::to_json).collect::<Vec<_>>(),
            }),
        }
    }

    /// The refusal in one sentence, for a caller that cannot show a list (the
    /// Undo action). `None` when it was undone.
    pub fn refusal(&self) -> Option<String> {
        let UndoOutcome::Refused(conflicts) = self else {
            return None;
        };
        let reasons: Vec<&str> = conflicts.iter().map(|c| c.reason.as_str()).collect();
        Some(format!(
            "this cannot be undone, because it has been changed since: {}",
            reasons.join("; ")
        ))
    }
}

/// One write of an undo, in the order [`plan_order`] puts them.
#[derive(Debug)]
enum Step {
    /// Put a deleted row back.
    Insert(Map<String, Json>),
    /// Set these columns back.
    Update(Map<String, Json>),
    /// Remove an inserted row.
    Delete,
}

/// An undo's step with what it applies to.
struct Planned<'e> {
    entry: &'e HistoryEntry,
    table: &'e Table,
    step: Step,
}

impl<'a> Reverser<'a> {
    /// The reverser for `user`, at their own role.
    pub fn new(
        catalog: &'a Catalog,
        user: Option<&'a User>,
        evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    ) -> Reverser<'a> {
        Reverser {
            catalog,
            role: caller_role(user),
            user,
            evaluator,
        }
    }

    fn is_admin(&self) -> bool {
        self.role <= ROLE_ADMIN
    }

    fn reader(&self) -> HistoryReader<'a> {
        HistoryReader {
            catalog: self.catalog,
            role: self.role,
            user: self.user,
            evaluator: self.evaluator,
        }
    }

    /// `name` when its history is on and this caller may read row `key`'s:
    /// restoring a row needs knowing what it was.
    async fn readable(&self, name: &str, key: &str) -> Result<Table> {
        let reader = self.reader();
        let table = reader.table(name)?;
        if !reader.may_see_row(&table, key).await? {
            return Err(not_found(&table, key));
        }
        Ok(table)
    }

    /// A new changeset reversing `cause`, in the caller's session and
    /// application: the Undo action steps back through a session, and a
    /// restore made in it is one of its steps.
    fn changeset(
        &self,
        source: ChangesetSource,
        cause: Uuid,
        undo_table: Option<String>,
    ) -> Arc<Changeset> {
        let ambient = changeset::current();
        Arc::new(
            Changeset::new(source)
                .session(ambient.as_ref().and_then(|c| c.session.clone()))
                .in_application(ambient.as_ref().and_then(|c| c.application()))
                .cause(Some(cause))
                .undoing_table(undo_table),
        )
    }

    /// The caller, as the writes carry it. Built inside the changeset's scope,
    /// which is where it picks the changeset up.
    fn context(&self) -> CallerContext {
        caller_context_at(self.role, self.user)
    }

    /// For the reads the checks make: whether a row is there, and what it
    /// holds. At the admin's role so a policy cannot make a row look gone; the
    /// write that follows is the caller's and is checked as theirs.
    fn checking(&self) -> CallerContext {
        caller_context_at(ROLE_ADMIN, self.user)
    }

    /// `POST {table}/deleted/{key}/restore`: insert the deleted row's image
    /// again, in a `restore` changeset caused by the delete.
    pub async fn restore_row(&self, table: &str, key: &str) -> Result<Restored> {
        let table = self.readable(table, key).await?;
        let latest = history::row_history(self.catalog, &table, key)
            .await?
            .into_iter()
            .next();
        let Some(image) = history::deleted_image(self.catalog, &table, key).await? else {
            return Err(match latest {
                Some(_) => Error::invalid(format!(
                    "row `{key}` of `{}` is not deleted, so there is nothing to restore",
                    table.name
                )),
                None => not_found(&table, key),
            });
        };
        let cause = latest.map(|e| e.changeset.id).unwrap_or_default();
        let changeset = self.changeset(ChangesetSource::Restore, cause, None);
        let id = changeset.id;
        changeset::scope(changeset, async {
            let tx = SharedTx::begin_primary(self.catalog)?;
            let written = async {
                let (values, dropped) = writable(&table, image, true);
                let row = self.insert(&tx, &table, values).await?;
                Ok((row, dropped))
            }
            .await;
            let (row, dropped) = finish(&tx, written).await?;
            Ok(Restored {
                changeset: id,
                row: redacted_json(&table, row),
                dropped,
            })
        })
        .await
    }

    /// `POST {table}/rows/{key}/revert`: set the row back to how it was just
    /// after its version `version`, as one update (or an insert, when the row
    /// is gone now), in a `restore` changeset caused by the row's latest change.
    pub async fn revert_row(&self, table: &str, key: &str, version: i64) -> Result<Restored> {
        let table = self.readable(table, key).await?;
        let entries = history::row_history(self.catalog, &table, key).await?;
        let Some(target) = entries.iter().find(|e| e.version == version) else {
            return Err(Error::not_found(format!(
                "row `{key}` of `{}` has no version {version}",
                table.name
            )));
        };
        let then = history::row_as_of(self.catalog, &table, key, AsOf::Seq(target.seq)).await?;
        let Some(then_row) = then.row else {
            return Err(Error::invalid(format!(
                "version {version} of row `{key}` is its deletion; delete the row instead"
            )));
        };
        let mut dropped: Vec<String> = then.removed.keys().cloned().collect();
        let cause = entries[0].changeset.id;
        let changeset = self.changeset(ChangesetSource::Restore, cause, None);
        let id = changeset.id;
        changeset::scope(changeset, async {
            let tx = SharedTx::begin_primary(self.catalog)?;
            let written = async {
                let now = current_row(self.catalog, &tx, &self.checking(), &table, key).await?;
                match now {
                    Some(now) => {
                        let (values, skipped) = writable(&table, then_row, false);
                        dropped.extend(skipped);
                        let changes: Map<String, Json> = values
                            .into_iter()
                            .filter(|(c, v)| now.get(c).unwrap_or(&Json::Null) != v)
                            .collect();
                        if changes.is_empty() {
                            return Err(Error::invalid(format!(
                                "row `{key}` of `{}` is already as it was at version {version}",
                                table.name
                            )));
                        }
                        self.update(&tx, &table, key, changes).await
                    }
                    None => {
                        let (values, skipped) = writable(&table, then_row, true);
                        dropped.extend(skipped);
                        self.insert(&tx, &table, values).await
                    }
                }
            }
            .await;
            let row = finish(&tx, written).await?;
            dropped.sort();
            dropped.dedup();
            Ok(Restored {
                changeset: id,
                row: redacted_json(&table, row),
                dropped,
            })
        })
        .await
    }

    /// `POST /changesets/{id}/undo`: apply the inverse of each of the
    /// changeset's entries not undone yet (only `only_table`'s, when given),
    /// in one transaction and in an `undo` changeset caused by it.
    ///
    /// Below the admin, only the caller's own changesets, and never `force`.
    pub async fn undo_changeset(
        &self,
        id: Uuid,
        only_table: Option<&str>,
        force: bool,
    ) -> Result<UndoOutcome> {
        if force && !self.is_admin() {
            return Err(Error::auth("only an administrator may force an undo"));
        }
        let Some((record, entries)) = history::changeset_entries(self.catalog, id).await? else {
            return Err(Error::not_found(format!("there is no changeset `{id}`")));
        };
        if !self.is_admin() && (self.user.map(|u| u.id) != record.user_id || self.user.is_none()) {
            return Err(Error::auth("you may undo only your own changes"));
        }
        if let Some(name) = only_table
            && !self.catalog.require(name)?.history_enabled()
        {
            return Err(Error::invalid(format!(
                "table `{name}` does not keep row history, so it has nothing to undo"
            )));
        }
        let undone = history::undone(self.catalog, id).await?;
        let pending: Vec<&HistoryEntry> = entries
            .iter()
            .filter(|e| only_table.is_none_or(|t| t == e.table))
            .filter(|e| !undone.covers(&e.table))
            .collect();
        if pending.is_empty() {
            let any = entries
                .iter()
                .any(|e| only_table.is_none_or(|t| t == e.table));
            return Err(Error::invalid(match (any, only_table) {
                (true, _) => "this has been undone already".to_owned(),
                (false, Some(t)) => format!("this changed nothing in `{t}`"),
                (false, None) => "this changed nothing that keeps history".to_owned(),
            }));
        }
        let mut tables = BTreeMap::new();
        for entry in &pending {
            if !tables.contains_key(&entry.table) {
                tables.insert(entry.table.clone(), self.catalog.require(&entry.table)?);
            }
        }
        let depths = fk_depths(self.catalog, tables.values());

        let changeset = self.changeset(ChangesetSource::Undo, id, only_table.map(str::to_owned));
        let undo_id = changeset.id;
        changeset::scope(changeset, async {
            let tx = SharedTx::begin_primary(self.catalog)?;
            let outcome = async {
                let mut conflicts = Vec::new();
                let mut plan = Vec::new();
                let mut dropped = BTreeSet::new();
                for entry in &pending {
                    let table = &tables[&entry.table];
                    let now =
                        current_row(self.catalog, &tx, &self.checking(), table, &entry.key).await?;
                    let step = self.inverse(table, entry, now, force, &mut conflicts, &mut dropped);
                    if let Some(step) = step {
                        plan.push(Planned { entry, table, step });
                    }
                }
                if !conflicts.is_empty() && !force {
                    return Ok(UndoOutcome::Refused(conflicts));
                }
                plan.sort_by_key(|p| plan_order(p, &depths));
                let entries = plan.len();
                for Planned { entry, table, step } in plan {
                    match step {
                        Step::Insert(values) => {
                            self.insert(&tx, table, values).await?;
                        }
                        Step::Update(values) => {
                            self.update(&tx, table, &entry.key, values).await?;
                        }
                        Step::Delete => self.delete(&tx, table, &entry.key).await?,
                    }
                }
                Ok(UndoOutcome::Undone {
                    changeset: undo_id,
                    entries,
                    dropped: dropped.into_iter().collect(),
                })
            }
            .await;
            match outcome {
                // Nothing was written; nothing to keep.
                Ok(refused @ UndoOutcome::Refused(_)) => {
                    let _ = tx.rollback().await;
                    Ok(refused)
                }
                other => finish(&tx, other).await,
            }
        })
        .await
    }

    /// The write that takes `entry` back, given the row as it is `now`, or
    /// `None` when there is nothing left to do. Conflicts are added to
    /// `conflicts`; with `force` they are overridden where that can be done
    /// (a row that is gone cannot be updated, so it is passed over).
    fn inverse(
        &self,
        table: &Table,
        entry: &HistoryEntry,
        now: Option<Map<String, Json>>,
        force: bool,
        conflicts: &mut Vec<Conflict>,
        dropped: &mut BTreeSet<String>,
    ) -> Option<Step> {
        let mut note_dropped = |columns: Vec<String>| {
            dropped.extend(columns.into_iter().map(|c| format!("{}.{c}", table.name)));
        };
        let gone = |reason: &str| Conflict {
            table: table.name.clone(),
            key: entry.key.clone(),
            field: None,
            expected: Json::Null,
            actual: Json::Null,
            reason: format!("row `{}` of `{}` {reason}", entry.key, table.name),
        };
        match entry.op.as_str() {
            "d" => {
                let image = entry.old_values.clone().unwrap_or_default();
                match now {
                    None => {
                        let (values, skipped) = writable(table, image, true);
                        note_dropped(skipped);
                        Some(Step::Insert(values))
                    }
                    Some(_) => {
                        conflicts.push(gone("has been created again since"));
                        let (values, skipped) = writable(table, image, false);
                        note_dropped(skipped);
                        (force && !values.is_empty()).then_some(Step::Update(values))
                    }
                }
            }
            op @ ("i" | "u") => {
                let Some(now) = now else {
                    conflicts.push(gone("has been deleted since"));
                    return None;
                };
                let left = entry.new_values.clone().unwrap_or_default();
                for (column, expected) in &left {
                    if table.field(column).is_none() {
                        continue;
                    }
                    let actual = now.get(column).cloned().unwrap_or(Json::Null);
                    if actual != *expected {
                        let shown = |v: &Json| shown_value(table, column, v);
                        conflicts.push(Conflict {
                            table: table.name.clone(),
                            key: entry.key.clone(),
                            field: Some(column.clone()),
                            expected: shown(expected),
                            actual: shown(&actual),
                            reason: format!(
                                "`{column}` of row `{}` of `{}` has been changed since",
                                entry.key, table.name
                            ),
                        });
                    }
                }
                if op == "i" {
                    return Some(Step::Delete);
                }
                let (values, skipped) =
                    writable(table, entry.old_values.clone().unwrap_or_default(), false);
                note_dropped(skipped);
                (!values.is_empty()).then_some(Step::Update(values))
            }
            _ => None,
        }
    }

    /// Insert `values` into `table` as the caller, after checking the rows it
    /// refers to are there.
    async fn insert(
        &self,
        tx: &SharedTx,
        table: &Table,
        values: Map<String, Json>,
    ) -> Result<Json> {
        self.check_references(tx, table, &values).await?;
        let context = self.context();
        let executor = Executor::Transaction(tx.clone());
        let caller = self.caller(&context, &executor);
        write_as::create(self.catalog, table, &Json::Object(values), &caller)
            .await?
            .map_err(refused)
    }

    /// Update row `key` of `table` with `values` as the caller.
    async fn update(
        &self,
        tx: &SharedTx,
        table: &Table,
        key: &str,
        values: Map<String, Json>,
    ) -> Result<Json> {
        self.check_references(tx, table, &values).await?;
        let context = self.context();
        let executor = Executor::Transaction(tx.clone());
        let caller = self.caller(&context, &executor);
        write_as::update(self.catalog, table, key, &Json::Object(values), &caller)
            .await?
            .map_err(refused)
    }

    /// Delete row `key` of `table` as the caller.
    async fn delete(&self, tx: &SharedTx, table: &Table, key: &str) -> Result<()> {
        let context = self.context();
        let executor = Executor::Transaction(tx.clone());
        let caller = self.caller(&context, &executor);
        write_as::delete(self.catalog, table, key, &caller)
            .await?
            .map_err(refused)
    }

    fn caller<'c>(&self, context: &'c CallerContext, executor: &'c Executor) -> Caller<'c>
    where
        'a: 'c,
    {
        Caller {
            user: self.user,
            evaluator: self.evaluator,
            context,
            executor,
        }
    }

    /// Refuse a write whose keys refer to a row that is not there (in this
    /// transaction, so a parent put back earlier in the same undo counts).
    async fn check_references(
        &self,
        tx: &SharedTx,
        table: &Table,
        values: &Map<String, Json>,
    ) -> Result<()> {
        let quote = |ident: &str| self.catalog.primary().dialect().quote_ident(ident);
        for field in &table.fields {
            let DataFieldKind::Key {
                target_table,
                target_field,
                ..
            } = &field.kind
            else {
                continue;
            };
            let name = &field.base.name;
            let Some(value) = values.get(name).filter(|v| !v.is_null()) else {
                continue;
            };
            let text = json_text(value);
            let found = match self.catalog.get(&target_table.0)? {
                Some(target) => {
                    let sql = format!(
                        "SELECT 1 AS found FROM {target} t WHERE t.{column}::text = $1 LIMIT 1",
                        target = quote(&target.name),
                        column = quote(&target_field.0),
                    );
                    let stmt = Statement::raw(sql, vec![Value::Text(text.clone())]);
                    !tx.run(Some(&self.checking()), &stmt).await?.is_empty()
                }
                None => false,
            };
            if !found {
                return Err(Error::invalid(format!(
                    "`{}.{name}` refers to row {text} of `{}`, which no longer exists; \
                     restore that row first",
                    table.name, target_table.0
                )));
            }
        }
        Ok(())
    }
}

/// Commit `outcome`'s transaction when it succeeded, roll it back when not.
async fn finish<T>(tx: &SharedTx, outcome: Result<T>) -> Result<T> {
    match outcome {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

fn refused(denied: Denied) -> Error {
    Error::auth(denied.message())
}

/// A key or value as the log's text compares it.
fn json_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The row `key` of `table` as it is in `tx`, every stored column as the log
/// records them; `None` when there is none.
async fn current_row(
    catalog: &Catalog,
    tx: &SharedTx,
    context: &CallerContext,
    table: &Table,
    key: &str,
) -> Result<Option<Map<String, Json>>> {
    let quote = |ident: &str| catalog.primary().dialect().quote_ident(ident);
    let sql = format!(
        "SELECT to_jsonb(t) AS row FROM {table} t WHERE t.{pk}::text = $1",
        table = quote(&table.name),
        pk = quote(&rows::single_pk(table)?),
    );
    let found = tx
        .run(
            Some(context),
            &Statement::raw(sql, vec![Value::Text(key.to_owned())]),
        )
        .await?;
    Ok(found.first().and_then(|row| match row.get("row") {
        Some(Value::Json(Json::Object(map))) => Some(map.clone()),
        Some(Value::Text(text)) => serde_json::from_str(text).ok(),
        _ => None,
    }))
}

/// The columns of a logged row `values` that can be written back to `table`,
/// and the names of those that are gone. A calculated or document column is
/// not written by a row write, so it is left out (and not reported: it is
/// still there). The key is kept only for an insert (`with_key`).
fn writable(
    table: &Table,
    values: Map<String, Json>,
    with_key: bool,
) -> (Map<String, Json>, Vec<String>) {
    let pk = table.primary_key.first().map(String::as_str);
    let (mut kept, mut dropped) = (Map::new(), Vec::new());
    for (column, value) in values {
        match table.field(&column) {
            None => dropped.push(column),
            Some(field) if field.is_calc() || field.is_collab_doc() => {}
            Some(_) if !with_key && Some(column.as_str()) == pk => {}
            Some(_) => {
                kept.insert(column, value);
            }
        }
    }
    (kept, dropped)
}

/// `value` of `column` as a conflict may show it: a secret as the sentinel.
fn shown_value(table: &Table, column: &str, value: &Json) -> Json {
    let mut one = Map::new();
    one.insert(column.to_owned(), value.clone());
    redacted(table, one).remove(column).unwrap_or(Json::Null)
}

fn redacted_json(table: &Table, mut row: Json) -> Json {
    sc_catalog::secrets::redact_json(table, &mut row);
    row
}

/// Each table's foreign-key depth: 0 for a table that refers to no other, and
/// one more than the deepest table it refers to otherwise. A self-reference
/// and a cycle count as no reference.
fn fk_depths<'t>(
    catalog: &Catalog,
    tables: impl Iterator<Item = &'t Table>,
) -> BTreeMap<String, usize> {
    fn depth(
        catalog: &Catalog,
        name: &str,
        visiting: &mut BTreeSet<String>,
        known: &mut BTreeMap<String, usize>,
    ) -> usize {
        if let Some(d) = known.get(name) {
            return *d;
        }
        if !visiting.insert(name.to_owned()) {
            return 0;
        }
        let parents: Vec<String> = catalog
            .get(name)
            .ok()
            .flatten()
            .map(|t| {
                t.fields
                    .iter()
                    .filter_map(|f| match &f.kind {
                        DataFieldKind::Key { target_table, .. } if target_table.0 != name => {
                            Some(target_table.0.clone())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let d = parents
            .iter()
            .map(|p| depth(catalog, p, visiting, known) + 1)
            .max()
            .unwrap_or(0);
        visiting.remove(name);
        known.insert(name.to_owned(), d);
        d
    }
    let mut known = BTreeMap::new();
    for table in tables {
        depth(catalog, &table.name, &mut BTreeSet::new(), &mut known);
    }
    known
}

/// Where a step goes in an undo: rows put back first, parents before
/// children; then updates; then rows removed, children before parents. Within
/// each, the latest entry first, as an editor's undo goes.
fn plan_order(planned: &Planned<'_>, depths: &BTreeMap<String, usize>) -> (u8, i64, i64) {
    let depth = depths.get(&planned.table.name).copied().unwrap_or(0) as i64;
    match planned.step {
        Step::Insert(_) => (0, depth, -planned.entry.seq),
        Step::Update(_) => (1, 0, -planned.entry.seq),
        Step::Delete => (2, -depth, -planned.entry.seq),
    }
}
