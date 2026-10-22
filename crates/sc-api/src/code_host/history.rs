//! `history` — a table's row history, read and restored from a code body
//! (§14.12, TODO-history H6.1).
//!
//! A code body reaches history through the `db` host, as `op: "history"`
//! requests, for the reason models are: it is the same seam in both languages,
//! on the same call budget, and the JavaScript and Python surfaces (the v1
//! `Table` history methods, `db.<table>.history(…)` and friends) are prelude
//! over it.
//!
//! ```json
//! { "op": "history", "what": "history", "table": "todo", "key": 7 }
//! { "op": "history", "what": "versions", "table": "todo", "key": 7 }
//! { "op": "history", "what": "deleted", "table": "todo", "limit": 20, "offset": 0 }
//! { "op": "history", "what": "as_of", "table": "todo", "key": 7, "as_of": "2026-10-01" }
//! { "op": "history", "what": "restore", "table": "todo", "key": 7 }
//! { "op": "history", "what": "revert", "table": "todo", "key": 7, "version": 3 }
//! { "op": "history", "what": "restore_version", "table": "todo", "key": 7, "version": 3 }
//! { "op": "history", "what": "undo_row", "table": "todo", "key": 7, "authority": "user" }
//! { "op": "history", "what": "redo_row", "table": "todo", "key": 7 }
//! ```
//!
//! Every request goes through H2's [`HistoryReader`] and H3's [`Reverser`], at
//! the plan's authority: the admin's by default (a code body is server-side
//! configuration), the caller's or a named user's when it says so. So the
//! access rule, the secret sentinel, the `restore` changeset and the write
//! rules are the same ones the admin API and the MCP tools have.
//!
//! `versions`, `restore_version`, `undo_row` and `redo_row` are Saltcorn 1's
//! shapes. v1 kept a full copy of the row per version, and stepped back and
//! forth through them with `_restore_of_version`. Here a version is rebuilt from
//! the log, and which version a revert went back to is found by looking for the
//! earlier version it equals ([`step_target`]).

use serde_json::{Map, Value as Json, json};

use sc_auth::ROLE_ADMIN;
use sc_catalog::Table;
use sc_catalog::history::{self, AsOf, HistoryEntry};
use sc_error::{Error, Result};

use super::plan::Authority;
use super::{Actor, TableHost};
use crate::history::{HistoryReader, redacted};
use crate::rows::Executor;
use crate::undo::Reverser;

/// One `history` request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryPlan {
    /// Always `history`; read before the plan is.
    #[allow(dead_code)]
    op: String,
    /// What to do: see the module documentation.
    what: String,
    /// The table.
    table: String,
    /// The row's key, for everything but `deleted`.
    #[serde(default)]
    key: Option<Json>,
    /// `revert` and `restore_version`: the version, as the row's history numbers it.
    #[serde(default)]
    version: Option<i64>,
    /// `as_of`: a time, a date, or an entry's `seq`.
    #[serde(default)]
    as_of: Option<Json>,
    /// `deleted`: at most this many.
    #[serde(default)]
    limit: Option<u64>,
    /// `deleted`: skipping this many.
    #[serde(default)]
    offset: Option<u64>,
    /// Whose authority it runs under.
    #[serde(default)]
    authority: Authority,
}

const WHATS: [&str; 9] = [
    "history",
    "versions",
    "deleted",
    "as_of",
    "restore",
    "revert",
    "restore_version",
    "undo_row",
    "redo_row",
];

/// Answer one `history` request of `host`'s run.
pub(super) async fn answer(host: &TableHost<'_>, request: Json) -> Result<Json> {
    let plan: HistoryPlan = serde_json::from_value(request).map_err(|e| {
        Error::invalid(format!(
            "this history request is not one the server understands: {e}"
        ))
    })?;
    if !WHATS.contains(&plan.what.as_str()) {
        return Err(Error::invalid(format!(
            "row history has no `{}`; it has {}",
            plan.what,
            WHATS.join(", ")
        )));
    }
    let actor = host.actor(&plan.authority).await?;
    let (role, user) = match &actor {
        // The admin's authority, in the event's user's name.
        Actor::Admin(_) => (
            ROLE_ADMIN,
            sc_auth::User::from_json(ROLE_ADMIN, host.user.as_ref())?,
        ),
        Actor::Caller { role, user } => (*role, user.clone()),
    };
    let evaluator = host.evaluator.as_ref();
    let reader = HistoryReader {
        catalog: host.catalog,
        role,
        user: user.as_ref(),
        evaluator,
    };
    let reverser = Reverser {
        catalog: host.catalog,
        role,
        user: user.as_ref(),
        evaluator,
    };
    let table = plan.table.as_str();
    let key = || -> Result<String> {
        match &plan.key {
            Some(Json::String(text)) if !text.is_empty() => Ok(text.clone()),
            Some(Json::Number(n)) => Ok(n.to_string()),
            _ => Err(Error::invalid(format!(
                "history `{}` of `{table}` needs the row's key",
                plan.what
            ))),
        }
    };
    let version = || {
        plan.version.ok_or_else(|| {
            Error::invalid(format!(
                "history `{}` of `{table}` needs the version to go back to",
                plan.what
            ))
        })
    };
    let writes = matches!(
        plan.what.as_str(),
        "restore" | "revert" | "restore_version" | "undo_row" | "redo_row"
    );
    if writes && matches!(host.executor, Executor::Transaction(_)) {
        // A restore writes in a transaction of its own, under a changeset of its
        // own. Inside a step's transaction it would wait on the step's own locks.
        return Err(Error::invalid(
            "a row cannot be restored from inside a workflow step's transaction; run the \
             restore from a trigger instead",
        ));
    }
    match plan.what.as_str() {
        "history" => reader.row_history(table, &key()?).await,
        "deleted" => {
            reader
                .deleted_rows(table, plan.limit, plan.offset.unwrap_or(0))
                .await
        }
        "as_of" => {
            let when = match &plan.as_of {
                Some(Json::String(text)) => text.clone(),
                Some(Json::Number(n)) => n.to_string(),
                _ => {
                    return Err(Error::invalid(format!(
                        "history `as_of` of `{table}` needs a time, a date or an entry's number"
                    )));
                }
            };
            reader.row_as_of(table, &key()?, &when).await
        }
        "versions" => {
            let (table, versions) = versions(&reader, table, &key()?).await?;
            Ok(Json::Array(
                versions.iter().map(|v| v.to_v1_json(&table)).collect(),
            ))
        }
        "restore" => Ok(reverser.restore_row(table, &key()?).await?.to_json()),
        "revert" => Ok(reverser
            .revert_row(table, &key()?, version()?)
            .await?
            .to_json()),
        "restore_version" => Ok(reverser
            .revert_row_if_changed(table, &key()?, version()?)
            .await?
            .map_or(Json::Null, |r| r.to_json())),
        step => {
            let key = key()?;
            let (_, versions) = versions(&reader, table, &key).await?;
            let target = step_target(&versions, step == "redo_row");
            match target {
                Some(target) => Ok(reverser
                    .revert_row_if_changed(table, &key, target)
                    .await?
                    .map_or(Json::Null, |r| r.to_json())),
                None => Ok(Json::Null),
            }
        }
    }
}

/// One version of a row: the entry that made it, and the row just after it
/// (`None` after its deletion), in the columns the table still has.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Version {
    pub(crate) entry: HistoryEntry,
    pub(crate) row: Option<Map<String, Json>>,
}

impl Version {
    /// Whether this version was written by putting the row back: a restore, a
    /// revert or an undo.
    fn is_restore(&self) -> bool {
        matches!(self.entry.changeset.source.as_str(), "restore" | "undo")
    }

    /// Saltcorn 1's history row: the row's columns, with `_version`, `_time`
    /// and `_userid`, and what this server adds: `_deleted`, `_op`,
    /// `_changeset` and `_source`.
    fn to_v1_json(&self, table: &Table) -> Json {
        let deleted = self.row.is_none();
        let mut out = match &self.row {
            Some(row) => redacted(table, row.clone()),
            None => Map::new(),
        };
        out.insert(
            table.primary_key.first().cloned().unwrap_or_default(),
            match self
                .row
                .as_ref()
                .and_then(|r| table.primary_key.first().and_then(|pk| r.get(pk)).cloned())
            {
                Some(key) => key,
                None => Json::String(self.entry.key.clone()),
            },
        );
        out.insert("_version".into(), json!(self.entry.version));
        out.insert("_time".into(), json!(self.entry.at));
        out.insert("_userid".into(), json!(self.entry.changeset.user_id));
        out.insert("_deleted".into(), json!(deleted));
        out.insert("_op".into(), json!(self.entry.op));
        out.insert("_changeset".into(), json!(self.entry.changeset.id));
        out.insert("_source".into(), json!(self.entry.changeset.source));
        Json::Object(out)
    }
}

/// Every version of row `key` of `table`, oldest first, after the reader's
/// access check.
async fn versions(
    reader: &HistoryReader<'_>,
    table: &str,
    key: &str,
) -> Result<(Table, Vec<Version>)> {
    let table = reader.table(table)?;
    if !reader.may_see_row(&table, key).await? {
        return Err(crate::history::not_found(&table, key));
    }
    let entries = history::row_history(reader.catalog, &table, key).await?;
    let now = history::row_as_of(reader.catalog, &table, key, AsOf::Seq(i64::MAX)).await?;
    let current = now.row.map(|mut row| {
        row.extend(now.removed);
        row
    });
    let versions = rebuild(&|c| table.field(c).is_some(), current, entries);
    Ok((table, versions))
}

/// The row after each of `entries` (newest first, as [`history::row_history`]
/// answers them), starting from the row as it is now and undoing them one by
/// one. Answered oldest first, each row in the columns `has_column` says the
/// table still has.
pub(crate) fn rebuild(
    has_column: &dyn Fn(&str) -> bool,
    current: Option<Map<String, Json>>,
    entries: Vec<HistoryEntry>,
) -> Vec<Version> {
    let mut state = current;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let row = state.as_ref().map(|row| {
            row.iter()
                .filter(|(column, _)| has_column(column))
                .map(|(c, v)| (c.clone(), v.clone()))
                .collect()
        });
        match entry.op.as_str() {
            "u" => {
                if let (Some(row), Some(old)) = (state.as_mut(), entry.old_values.clone()) {
                    row.extend(old);
                }
            }
            "d" => state = entry.old_values.clone(),
            _ => state = None,
        }
        out.push(Version { entry, row });
    }
    out.reverse();
    out
}

/// Which version Saltcorn 1's `undo_row_changes` (or, with `redo`,
/// `redo_row_changes`) goes to, if any.
///
/// v1 remembered which version a restore restored (`_restore_of_version`):
/// undo goes to the version before the one the row is at, skipping back over
/// its own restores, and redo to the next one that is not a restore. Here the
/// version a restore went back to is the latest earlier version, not itself a
/// restore, whose row it equals.
pub(crate) fn step_target(versions: &[Version], redo: bool) -> Option<i64> {
    let latest = versions.last()?;
    // The version the row is "at": its latest, or the one its latest restore
    // put back.
    let at = if latest.is_restore() {
        versions
            .iter()
            .rev()
            .skip(1)
            .find(|v| !v.is_restore() && v.row == latest.row)
            .map(|v| v.entry.version)
    } else {
        Some(latest.entry.version)
    };
    if redo {
        // Redo only follows an undo: the row must be at a restore.
        if !latest.is_restore() {
            return None;
        }
        let at = at?;
        return versions
            .iter()
            .find(|v| {
                v.entry.version > at && v.entry.version < latest.entry.version && !v.is_restore()
            })
            .map(|v| v.entry.version);
    }
    let at = at?;
    versions
        .iter()
        .rev()
        .find(|v| v.entry.version < at && !v.is_restore() && v.row.is_some())
        .map(|v| v.entry.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sc_catalog::history::ChangesetRecord;
    use uuid::Uuid;

    fn has_column(column: &str) -> bool {
        column == "id" || column == "title"
    }

    fn entry(version: i64, op: &str, source: &str, old: Json, new: Json) -> HistoryEntry {
        let object = |j: Json| match j {
            Json::Object(m) => Some(m),
            _ => None,
        };
        HistoryEntry {
            seq: version * 10,
            version,
            table: "todo".into(),
            key: "1".into(),
            op: op.into(),
            old_values: object(old),
            new_values: object(new),
            at: Utc::now(),
            changeset: ChangesetRecord {
                id: Uuid::new_v4(),
                at: Utc::now(),
                user_id: None,
                source: source.into(),
                application: None,
                description: None,
                cause: None,
                undo_table: None,
                client_id: None,
                mutation: None,
            },
        }
    }

    fn row(title: &str) -> Option<Map<String, Json>> {
        match json!({ "id": 1, "title": title }) {
            Json::Object(m) => Some(m),
            _ => None,
        }
    }

    fn titles(versions: &[Version]) -> Vec<Option<String>> {
        versions
            .iter()
            .map(|v| {
                v.row
                    .as_ref()
                    .map(|r| r["title"].as_str().unwrap_or("").to_owned())
            })
            .collect()
    }

    /// a, b, c written; undo goes back to b (a restore), undo again to a,
    /// redo to b.
    fn walked(extra: &[(&str, &str, &str)]) -> Vec<Version> {
        let mut newest_first = vec![
            entry(
                3,
                "u",
                "request",
                json!({"title": "b"}),
                json!({"title": "c"}),
            ),
            entry(
                2,
                "u",
                "request",
                json!({"title": "a"}),
                json!({"title": "b"}),
            ),
            entry(
                1,
                "i",
                "request",
                Json::Null,
                json!({"id": 1, "title": "a"}),
            ),
        ];
        let mut now = "c".to_owned();
        for (n, (from, to, source)) in extra.iter().enumerate() {
            newest_first.insert(
                0,
                entry(
                    4 + n as i64,
                    "u",
                    source,
                    json!({ "title": from }),
                    json!({ "title": to }),
                ),
            );
            now = (*to).to_owned();
        }
        rebuild(&has_column, row(&now), newest_first)
    }

    #[test]
    fn versions_are_rebuilt_from_the_row_now_oldest_first() {
        let versions = walked(&[]);
        assert_eq!(
            titles(&versions),
            vec![Some("a".into()), Some("b".into()), Some("c".into())]
        );
        let deleted = rebuild(
            &has_column,
            None,
            vec![
                entry(
                    2,
                    "d",
                    "request",
                    json!({"id": 1, "title": "a"}),
                    Json::Null,
                ),
                entry(
                    1,
                    "i",
                    "request",
                    Json::Null,
                    json!({"id": 1, "title": "a"}),
                ),
            ],
        );
        assert_eq!(titles(&deleted), vec![Some("a".into()), None]);
    }

    #[test]
    fn undo_steps_back_over_its_own_restores_and_redo_steps_forward() {
        assert_eq!(step_target(&walked(&[]), false), Some(2));
        assert_eq!(step_target(&walked(&[]), true), None);
        // Undone once: at b by a restore of version 2.
        let once = walked(&[("c", "b", "restore")]);
        assert_eq!(step_target(&once, false), Some(1));
        assert_eq!(step_target(&once, true), Some(3));
        // Undone twice: at a, the first version, so nothing is before it.
        let twice = walked(&[("c", "b", "restore"), ("b", "a", "restore")]);
        assert_eq!(step_target(&twice, false), None);
        assert_eq!(step_target(&twice, true), Some(2));
        // Undone twice and redone once: at b again.
        let redone = walked(&[
            ("c", "b", "restore"),
            ("b", "a", "restore"),
            ("a", "b", "restore"),
        ]);
        assert_eq!(step_target(&redone, false), Some(1));
        assert_eq!(step_target(&redone, true), Some(3));
    }
}
