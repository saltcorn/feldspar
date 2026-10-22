//! Reading row history, with its access rule (§14.12 "Reading", TODO-history
//! H2): the one place the admin API, the MCP tools and later the code API ask.
//!
//! - A table's history is the **admin's** unless `history.min_role` opens it
//!   to a wider role.
//! - Then each row is checked too: a caller below the admin sees the history of
//!   a row they may read now (§7.3's read rule, [`row_readable_as`]), and of a
//!   deleted row when the rule grants its deleted image ([`RowImage`], reified
//!   as for an `only_if`).
//! - A secret is always its sentinel, before and after.
//! - Changesets span tables, so listing and reading them is the admin's alone.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_auth::{ROLE_ADMIN, User};
use sc_catalog::history::{
    self, AsOf, ChangesetFilter, ChangesetRecord, ChangesetSummary, HistoryEntry,
};
use sc_catalog::{Catalog, Table};
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_query::{Statement, Value};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::ownership::{RowImage, row_readable_as};

/// Who is reading history: their role and user, and the evaluator a reified
/// read rule needs.
#[derive(Clone, Copy)]
pub struct HistoryReader<'a> {
    /// The catalog.
    pub catalog: &'a Catalog,
    /// The caller's role (public when nobody is signed in).
    pub role: u8,
    /// The caller, when signed in.
    pub user: Option<&'a User>,
    /// For a read rule that needs JavaScript.
    pub evaluator: Option<&'a Arc<dyn JsEvaluator>>,
}

/// The most entries of a list one call answers.
pub const HISTORY_PAGE_CAP: u64 = 500;

impl<'a> HistoryReader<'a> {
    /// The reader for `user`, at their own role.
    pub fn new(
        catalog: &'a Catalog,
        user: Option<&'a User>,
        evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    ) -> HistoryReader<'a> {
        HistoryReader {
            catalog,
            role: crate::ownership::caller_role(user),
            user,
            evaluator,
        }
    }

    pub(crate) fn is_admin(&self) -> bool {
        self.role <= ROLE_ADMIN
    }

    /// `name`, when its history is on and open to this caller.
    pub fn table(&self, name: &str) -> Result<Table> {
        let table = self.catalog.require(name)?;
        if !table.history_enabled() {
            return Err(Error::invalid(format!(
                "table `{name}` does not keep row history; switch it on in the table's settings"
            )));
        }
        let min_role = history::history_min_role(&table.attributes);
        if self.role > min_role.max(ROLE_ADMIN) {
            return Err(Error::auth(format!(
                "you may not read the history of `{name}`"
            )));
        }
        Ok(table)
    }

    /// Whether this caller may see the history of row `key`: always for the
    /// admin; otherwise when they may read the row now or, if it is gone, its
    /// deleted image.
    pub(crate) async fn may_see_row(&self, table: &Table, key: &str) -> Result<bool> {
        if self.is_admin() {
            return Ok(true);
        }
        if let Some(image) = history::deleted_image(self.catalog, table, key).await? {
            return self.may_see_image(table, &image).await;
        }
        row_readable_as(
            self.catalog,
            table,
            key,
            self.role,
            self.user,
            self.evaluator,
        )
        .await
    }

    /// Whether the read rule grants this caller a deleted row's image. A rule
    /// that could not be decided denies.
    async fn may_see_image(&self, table: &Table, image: &Map<String, Json>) -> Result<bool> {
        if self.is_admin() {
            return Ok(true);
        }
        let mut image = RowImage::new(table, &Json::Object(image.clone()));
        Ok(image
            .readable(self.catalog, self.role, self.user, self.evaluator)
            .await
            .unwrap_or(false))
    }

    /// `GET {table}/{key}/history`: the row's entries, newest first. A row the
    /// caller may not read has none they can see, which is "not found".
    pub async fn row_history(&self, table: &str, key: &str) -> Result<Json> {
        let table = self.table(table)?;
        if !self.may_see_row(&table, key).await? {
            return Err(not_found(&table, key));
        }
        let entries = history::row_history(self.catalog, &table, key).await?;
        let emails = self.emails(entries.iter().map(|e| &e.changeset)).await?;
        Ok(Json::Array(
            entries
                .iter()
                .map(|e| entry_json(&table, e, &emails))
                .collect(),
        ))
    }

    /// `GET {table}/deleted`: rows deleted and not back, newest first, each
    /// with its deleted image.
    pub async fn deleted_rows(&self, table: &str, limit: Option<u64>, offset: u64) -> Result<Json> {
        let table = self.table(table)?;
        let limit = limit.unwrap_or(HISTORY_PAGE_CAP).min(HISTORY_PAGE_CAP);
        // A bound counted over rows the caller may not see would answer
        // "10 rows" with three, so below the admin it waits for the check.
        let found = if self.is_admin() {
            history::deleted_rows(self.catalog, &table, Some(limit), offset).await?
        } else {
            let mut granted = Vec::new();
            for entry in history::deleted_rows(self.catalog, &table, None, 0).await? {
                let image = entry.old_values.clone().unwrap_or_default();
                if self.may_see_image(&table, &image).await? {
                    granted.push(entry);
                }
            }
            granted
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize)
                .collect()
        };
        let emails = self.emails(found.iter().map(|e| &e.changeset)).await?;
        Ok(Json::Array(
            found
                .iter()
                .map(|entry| {
                    let (row, removed) = split_removed(
                        &table,
                        redacted(&table, entry.old_values.clone().unwrap_or_default()),
                    );
                    json!({
                        "key": entry.key,
                        "seq": entry.seq,
                        "at": entry.at,
                        "row": row,
                        "removed": removed,
                        "changeset": changeset_json(&entry.changeset, &emails),
                    })
                })
                .collect(),
        ))
    }

    /// `GET {table}/{key}?as_of=`: the row as it was at `as_of`. `row` is null
    /// when it did not exist then; `removed` holds values of columns dropped
    /// since.
    pub async fn row_as_of(&self, table: &str, key: &str, as_of: &str) -> Result<Json> {
        let point = AsOf::parse(as_of)?;
        let table = self.table(table)?;
        if !self.may_see_row(&table, key).await? {
            return Err(not_found(&table, key));
        }
        let rebuilt = history::row_as_of(self.catalog, &table, key, point).await?;
        Ok(json!({
            "row": rebuilt.row.map(|row| Json::Object(redacted(&table, row))),
            "removed": Json::Object(redacted(&table, rebuilt.removed)),
        }))
    }

    /// `GET /changesets`: what each action did, newest first. The admin's.
    pub async fn list_changesets(&self, filter: ChangesetFilter) -> Result<Json> {
        self.require_admin()?;
        let filter = ChangesetFilter {
            limit: if filter.limit == 0 {
                HISTORY_PAGE_CAP
            } else {
                filter.limit.min(HISTORY_PAGE_CAP)
            },
            ..filter
        };
        let found = history::list_changesets(self.catalog, &filter).await?;
        let emails = self.emails(found.iter().map(|s| &s.changeset)).await?;
        Ok(Json::Array(
            found
                .iter()
                .map(
                    |ChangesetSummary {
                         changeset,
                         entries,
                         tables,
                     }| {
                        let mut out = changeset_json(changeset, &emails);
                        out["entries"] = json!(entries);
                        out["tables"] = json!(tables);
                        out
                    },
                )
                .collect(),
        ))
    }

    /// `GET /changesets/{id}`: one changeset and its entries. The admin's.
    pub async fn changeset(&self, id: &str) -> Result<Json> {
        self.require_admin()?;
        let parsed: Uuid = id
            .parse()
            .map_err(|_| Error::not_found(format!("there is no changeset `{id}`")))?;
        let Some((changeset, entries)) = history::changeset_entries(self.catalog, parsed).await?
        else {
            return Err(Error::not_found(format!("there is no changeset `{id}`")));
        };
        let emails = self.emails(std::iter::once(&changeset)).await?;
        let mut out = changeset_json(&changeset, &emails);
        let mut rendered = Vec::new();
        for entry in &entries {
            // A table dropped since has no secrets to know of; its entries are
            // deleted with it (H5), so this is a moment's race at most.
            match self.catalog.get(&entry.table)? {
                Some(table) => rendered.push(entry_json(&table, entry, &emails)),
                None => continue,
            }
        }
        out["entries"] = Json::Array(rendered);
        Ok(out)
    }

    fn require_admin(&self) -> Result<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(Error::auth(
                "only an administrator may read changesets, which span tables",
            ))
        }
    }

    /// The email of each user among `changesets`, for the admin. Below the
    /// admin a reader is shown who by id only: opening a table's history is
    /// not opening the user list.
    async fn emails<'c>(
        &self,
        changesets: impl Iterator<Item = &'c ChangesetRecord>,
    ) -> Result<BTreeMap<Uuid, String>> {
        let ids: Vec<Uuid> = changesets.filter_map(|c| c.user_id).collect();
        if ids.is_empty() || !self.is_admin() {
            return Ok(BTreeMap::new());
        }
        let Some(users) = self.catalog.get(sc_auth::USERS_TABLE)? else {
            return Ok(BTreeMap::new());
        };
        let dialect = self.catalog.primary().dialect();
        let sql = format!(
            "SELECT {id}, {email} FROM {users} \
             WHERE {id}::text IN (SELECT jsonb_array_elements_text($1::jsonb))",
            id = dialect.quote_ident(sc_auth::COL_ID),
            email = dialect.quote_ident(sc_auth::COL_EMAIL),
            users = dialect.quote_ident(&users.name),
        );
        let ids = Json::Array(ids.iter().map(|id| Json::String(id.to_string())).collect());
        let rows = self
            .catalog
            .primary()
            .query(&Statement::raw(sql, vec![Value::Json(ids)]))
            .await?
            .try_collect()
            .await?;
        Ok(rows
            .iter()
            .filter_map(
                |row| match (row.get(sc_auth::COL_ID), row.get(sc_auth::COL_EMAIL)) {
                    (Some(Value::Uuid(id)), Some(Value::Text(email))) => Some((*id, email.clone())),
                    _ => None,
                },
            )
            .collect())
    }
}

pub(crate) fn not_found(table: &Table, key: &str) -> Error {
    Error::not_found(format!(
        "there is no history of row `{key}` of `{}` that you may read",
        table.name
    ))
}

/// `values` with each secret of `table` as its sentinel.
pub(crate) fn redacted(table: &Table, values: Map<String, Json>) -> Map<String, Json> {
    let mut row = Json::Object(values);
    sc_catalog::secrets::redact_json(table, &mut row);
    match row {
        Json::Object(map) => map,
        _ => Map::new(),
    }
}

/// A row image split into the columns `table` still has and those dropped since.
fn split_removed(table: &Table, values: Map<String, Json>) -> (Json, Json) {
    let (mut row, mut removed) = (Map::new(), Map::new());
    for (column, value) in values {
        if table.field(&column).is_some() {
            row.insert(column, value);
        } else {
            removed.insert(column, value);
        }
    }
    (Json::Object(row), Json::Object(removed))
}

fn changeset_json(changeset: &ChangesetRecord, emails: &BTreeMap<Uuid, String>) -> Json {
    json!({
        "id": changeset.id,
        "at": changeset.at,
        "user_id": changeset.user_id,
        "user_email": changeset.user_id.and_then(|id| emails.get(&id)),
        "source": changeset.source,
        "application": changeset.application,
        "description": changeset.description,
        "cause": changeset.cause,
        "undo_table": changeset.undo_table,
        "client_id": changeset.client_id,
        "mutation": changeset.mutation,
    })
}

/// One entry, with its field-level changes: each column it recorded, before
/// and after, in the table's field order, and the columns dropped since last,
/// marked `removed`.
fn entry_json(table: &Table, entry: &HistoryEntry, emails: &BTreeMap<Uuid, String>) -> Json {
    let old = redacted(table, entry.old_values.clone().unwrap_or_default());
    let new = redacted(table, entry.new_values.clone().unwrap_or_default());
    let mut columns: Vec<&String> = old.keys().chain(new.keys()).collect();
    let position = |c: &str| {
        table
            .fields
            .iter()
            .position(|f| f.base.name == c)
            .unwrap_or(usize::MAX)
    };
    columns.sort_by(|a, b| position(a).cmp(&position(b)).then(a.cmp(b)));
    columns.dedup();
    let changes: Vec<Json> = columns
        .iter()
        .map(|c| {
            json!({
                "field": c,
                "old": if entry.op == "i" { Json::Null } else { old.get(*c).cloned().unwrap_or(Json::Null) },
                "new": if entry.op == "d" { Json::Null } else { new.get(*c).cloned().unwrap_or(Json::Null) },
                "removed": table.field(c).is_none(),
            })
        })
        .collect();
    json!({
        "seq": entry.seq,
        "version": entry.version,
        "table": entry.table,
        "key": entry.key,
        "op": entry.op,
        "at": entry.at,
        "changes": changes,
        "changeset": changeset_json(&entry.changeset, emails),
    })
}

/// A changeset filter from the query string: `user`, `table`, `source`,
/// `since`, `limit` and `offset`, each optional.
pub fn changeset_filter(query: &[(String, String)]) -> Result<ChangesetFilter> {
    let get = |name: &str| {
        query
            .iter()
            .find(|(k, v)| k == name && !v.is_empty())
            .map(|(_, v)| v.as_str())
    };
    let number = |name: &str| -> Result<u64> {
        get(name).map_or(Ok(0), |v| {
            v.parse()
                .map_err(|_| Error::invalid(format!("`{name}` must be a whole number")))
        })
    };
    let user = get("user")
        .map(|u| {
            u.parse::<Uuid>()
                .map_err(|_| Error::invalid("`user` must be a user's id"))
        })
        .transpose()?;
    let since = get("since")
        .map(|s| match AsOf::parse(s)? {
            AsOf::Time(at) => Ok(at),
            AsOf::Seq(_) => Err(Error::invalid("`since` must be a time")),
        })
        .transpose()?;
    Ok(ChangesetFilter {
        user,
        table: get("table").map(str::to_owned),
        source: get("source").map(str::to_owned),
        since,
        limit: number("limit")?,
        offset: number("offset")?,
    })
}
