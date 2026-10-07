//! Restoring a backup: a zip and a selection in, an installation changed.
//!
//! Three rules run through everything below.
//!
//! **A restore adds; it does not destroy.** Nothing here drops a table, deletes a
//! row, replaces an account or overwrites a file-store definition that already
//! exists. An admin who restores a backup onto a server that already has things
//! in it gets what was missing, and is *told* what was left alone. The alternative
//! — a restore that makes the server match the file — would mean an admin one
//! click away from deleting the installation they are standing in, and the
//! destructive verbs (drop a table, delete a user) already exist elsewhere in the
//! admin UI where they are individually deliberate.
//!
//! **Every item is restored on its own.** A restore is dozens of independent
//! acts, and one of them failing — an application naming a table that is not in
//! the file, an agent whose LLM provider does not exist here — must not lose the
//! other twenty. Each failure becomes a line in [`RestoreReport::warnings`]
//! naming what was skipped and why; only a file that is not a backup at all is an
//! error.
//!
//! **The parsers are the admin API's.** Every record goes back in through the
//! same `*_from_body` the endpoint that creates one uses, which is what makes the
//! validation identical: an agent restored from a backup is refused for exactly
//! the reasons an agent typed into the form would be.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::sync::Arc;

use sc_api::{rows, schema_edit};
use sc_catalog::{
    Catalog, ConstraintKind, DataFieldKind, Table, TableConstraint, load_file_store_by_name,
};
use sc_db::{ColumnGenerator, SchemaChange};
use sc_error::{Error, Result};
use sc_query::{Expr, Insert, Statement};
use serde_json::{Map, Value as Json};

use super::{Available, MANIFEST_FILE, SSL_SECTION, Selection};
use crate::apps::AppMounts;
use crate::handlers::{
    agent_from_body, agents_of, application_from_body, backup_file_meta_from_json,
    create_backend_resources, dataset_shape, db_connection_from_body, field_spec_from_body,
    file_store_from_body, library_item_from_body, llm_model_from_body, llm_provider_from_body,
    model_from_body, models_of, module_from_backup, modules_of, page_from_body, stream_from_body,
    streams_of, table_settings_from_body, trigger_from_body, trigger_table, triggers_of,
    view_from_body, workflow_from_document,
};

/// What a restore did, and what it declined to do.
#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    /// One line per thing that was restored, in the order it happened.
    pub restored: Vec<String>,
    /// One line per thing that was skipped, each saying why.
    pub warnings: Vec<String>,
}

impl RestoreReport {
    fn did(&mut self, line: impl Into<String>) {
        self.restored.push(line.into());
    }

    fn skipped(&mut self, line: impl Into<String>) {
        self.warnings.push(line.into());
    }

    /// Record the outcome of one item: a line either way, never silence.
    fn outcome(&mut self, what: &str, result: Result<String>) {
        match result {
            Ok(detail) if detail.is_empty() => self.did(what.to_owned()),
            Ok(detail) => self.did(format!("{what}: {detail}")),
            Err(e) => self.skipped(format!("{what}: {}", e.causes())),
        }
    }
}

/// The entries of a backup zip, read whole.
///
/// The archive is expanded into memory rather than read entry by entry as the
/// restore proceeds, because the restore is asynchronous and a zip reader's
/// borrow of the archive is not: holding one across an `await` is not expressible.
/// The cost is that a restore holds the uncompressed backup in memory, which is
/// the same trade the writer makes and for a rarer operation.
pub(super) type Entries = BTreeMap<String, Vec<u8>>;

/// Read a backup's manifest without restoring anything: what the dialog is built
/// from after the file is uploaded.
pub fn inspect(archive: &[u8]) -> Result<(Available, Json)> {
    let entries = read_backup(archive)?;
    let manifest = manifest_of(&entries)?;
    let contents = manifest
        .get("contents")
        .ok_or_else(|| Error::invalid("this backup's manifest does not say what is in it"))?;
    Ok((Available::from_json(contents)?, Json::Object(manifest)))
}

/// Restore the parts of `archive` that `selection` asks for.
pub async fn restore_backup(
    catalog: &Catalog,
    apps: &AppMounts,
    archive: &[u8],
    selection: &Selection,
) -> Result<RestoreReport> {
    let entries = read_backup(archive)?;
    let manifest = manifest_of(&entries)?;
    let contents = Available::from_json(
        manifest
            .get("contents")
            .ok_or_else(|| Error::invalid("this backup's manifest does not say what is in it"))?,
    )?;
    // What the file holds, narrowed by what was asked for: a selection naming
    // something the backup does not carry is quietly dropped rather than
    // reported, because it is the dialog's own list that was edited.
    let selection = selection.intersect(&contents);
    let mut report = RestoreReport::default();
    // What the archive could not offer in the first place — everything a
    // Saltcorn 1 import left behind (`super::v1`), and nothing at all for a backup
    // this system wrote. Reported before the restore rather than after, because
    // they are facts about the *file*, not about what happened to it.
    for note in super::v1::notes_of(&manifest) {
        report.skipped(note);
    }

    // --- roles and users, before anything points at them --------------------
    //
    // An account that is already here keeps its own id, so the rows that pointed
    // at the backup's id for it are pointed at this one as they go in.
    let mut user_ids = BTreeMap::new();
    if selection.users {
        user_ids = restore_users(catalog, &entries, &mut report).await;
    }

    // --- modules, before anything that might be one of theirs ----------------
    //
    // A module can supply a table provider, a framework, an action, a stream
    // provider or a model provider — so it is installed before every one of the
    // things below that could name what it supplies.
    if selection.modules {
        restore_modules(apps, catalog, &entries, &mut report).await;
    }

    // --- connections to other databases, before the tables -------------------
    //
    // So a table that lives on one is found there rather than created here. A
    // SQLite connection is a file in a file store, and saving one needs the
    // store; those wait for the file stores below.
    if selection.db_connections {
        restore_db_connections(catalog, &entries, false, &mut report).await;
    }

    // --- tables: every table, then every column, then the rows --------------
    //
    // In that order because a `Key` field can only resolve against a table that
    // exists, and *all* of them exist once the first pass is done — which is what
    // lets a backup with two tables referencing each other be restored at all.
    let mut restored_tables = Vec::new();
    let mut deferred_access = Vec::new();
    for name in &selection.tables {
        match restore_table(catalog, &entries, name).await {
            Ok((line, access)) => {
                report.did(line);
                restored_tables.push(name.clone());
                deferred_access.extend(access.map(|access| (name.clone(), access)));
            }
            Err(e) => report.skipped(format!("table `{name}`: {}", e.causes())),
        }
    }
    // Columns in three passes over every table: the plain columns, then the
    // references, then the calculated fields. A `Key` takes its storage type from
    // the column it points at (the schema editor resolves it), and that column may
    // be in a table further down the list — or in this same table, when a row
    // points at its own kind. A calculated field's expression is checked against
    // the columns it reads, and a join (`authorⱵname`) reads through a `Key` —
    // so the calculated fields wait for every reference. One pass per table would
    // make a backup's column order decide whether half of them survived.
    for pass in [FieldPass::Plain, FieldPass::Reference] {
        for name in &restored_tables {
            restore_fields(catalog, &entries, name, pass, &mut report).await;
        }
    }
    restore_calc_fields(catalog, &entries, &restored_tables, &mut report).await;

    // Rows in dependency order, so a row holding a foreign key is inserted after
    // the row it points at.
    for name in order_by_references(catalog, &restored_tables) {
        if !selection.includes_data(&name) {
            continue;
        }
        match table_rows(&entries, &format!("tables/{name}/rows.json")) {
            Ok(rows) => {
                let table = match catalog.require(&name) {
                    Ok(table) => table,
                    Err(e) => {
                        report.skipped(format!("rows of `{name}`: {}", e.causes()));
                        continue;
                    }
                };
                let rows = repoint_users(&table, rows, &user_ids);
                let (inserted, problems) = insert_rows(catalog, &table, &rows).await;
                report.did(format!("{inserted} rows into `{name}`"));
                for problem in problems {
                    report.skipped(format!("a row of `{name}`: {problem}"));
                }
                rewind_identities(catalog, &table, &mut report).await;
            }
            Err(e) => report.skipped(format!("rows of `{name}`: {}", e.causes())),
        }
    }

    // The ownership formula and row-level security, now that every column is
    // here and the rows are in. The formula is checked against the columns it
    // reads (`therapist`, or `assignmentⱵassigned_by` through a `Key`), none of
    // which exist when the table is created; and a forced RLS policy switched on
    // before the rows would judge each insert against them.
    for (name, access) in deferred_access {
        let result = schema_edit::apply(
            catalog,
            &[schema_edit::Operation::AlterTable {
                table: name.clone(),
                settings: access,
            }],
            &schema_edit::ApplyOptions::default(),
        )
        .await
        .map(|_| String::new());
        report.outcome(&format!("the ownership rule of `{name}`"), result);
    }

    // Constraints last, **after** the rows, exactly as `pg_dump` orders them: a
    // unique constraint created over the restored data checks it in one pass
    // rather than once per insert, and a row constraint created first would have
    // judged every row as it arrived — against a table whose other rows were not
    // in yet (§5.1).
    for name in &restored_tables {
        restore_constraints(catalog, &entries, name, &mut report).await;
    }

    // --- file stores: the definition, then the bytes, then the rules ---------
    for name in &selection.file_stores {
        restore_file_store(catalog, &entries, name, &mut report).await;
    }
    if selection.db_connections {
        restore_db_connections(catalog, &entries, true, &mut report).await;
    }

    // --- analytics: datasets over the tables, models over the datasets -------
    if selection.analytics {
        restore_analytics(catalog, apps, &entries, &selection, &mut report).await;
    }

    // --- streams, before the applications that expose them and the triggers
    // that listen to them ------------------------------------------------------
    if selection.streams {
        restore_streams(catalog, apps, &entries, &mut report).await;
    }

    // --- applications, LLM providers, agents, triggers ----------------------
    //
    // Applications after the file stores, deliberately: building one is a bundler
    // run over its source tree, and that tree is what the stores just restored.
    // LLM providers before agents and triggers, because an agent is refused
    // unless the provider (and model) it names exists.
    for subdomain in &selection.applications {
        restore_application(catalog, apps, &entries, subdomain, &selection, &mut report).await;
    }
    if selection.llm_providers {
        restore_llm_providers(catalog, &entries, &mut report).await;
    }
    if selection.agents {
        restore_agents(catalog, apps, &entries, &mut report).await;
    }
    if selection.triggers {
        restore_triggers(catalog, apps, &entries, &selection, &mut report).await;
    }

    // --- the SSL settings ---------------------------------------------------
    if selection.ssl {
        let result = restore_ssl(catalog, &entries).await;
        report.outcome("the SSL settings", result);
    }
    if selection.settings {
        for section in sc_config::config_sections()
            .iter()
            .filter(|section| section.name != SSL_SECTION)
        {
            let path = format!("settings/{}.json", section.name);
            if !entries.contains_key(&path) {
                continue;
            }
            let result = restore_settings_section(catalog, &entries, section.name).await;
            report.outcome(&format!("the {} settings", section.name), result);
        }
    }

    Ok(report)
}

// --- the parts -----------------------------------------------------------------

/// Roles, then the columns an admin added to the users table, then accounts.
///
/// **An account that is already here is left exactly as it is**, matched by id or
/// by email address. That rule is what makes the restore safe to run on a server
/// somebody is signed in to: the alternative would let a backup overwrite the
/// password of the admin running it, and lock them out of the screen they are
/// standing on.
///
/// Returns, for each account kept that has a different id here than in the
/// backup (the same email, a different server), the backup's id mapped to this
/// one — what [`repoint_users`] rewrites the rows' references with.
async fn restore_users(
    catalog: &Catalog,
    entries: &Entries,
    report: &mut RestoreReport,
) -> BTreeMap<String, String> {
    let mut repointed = BTreeMap::new();
    let document = match json_entry(entries, "users.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("users: {}", e.causes()));
            return repointed;
        }
    };

    for value in array_field(&document, "roles") {
        let result = restore_role(catalog, &value).await;
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("a role")
            .to_owned();
        report.outcome(&format!("role `{name}`"), result);
    }

    let users = array_field(&document, "users");
    if catalog.require(sc_auth::USERS_TABLE).is_err() {
        report.skipped("users: this server has no users table");
        return repointed;
    }

    // The columns an admin added to the users table where the backup was taken —
    // and, for a Saltcorn 1 import, the `legacy_id` its integer keys land in.
    // Before the rows, because a row carrying a value for a column that is not
    // there loses it: `insert_row` writes the columns the table has.
    for value in array_field(&document, "fields") {
        let Some(field) = value.as_object() else {
            continue;
        };
        let Some(name) = field.get("name").and_then(Json::as_str) else {
            continue;
        };
        let name = name.to_owned();
        match catalog.require(sc_auth::USERS_TABLE) {
            // Already here — an admin-added column of the same name, or a second
            // restore. Left exactly as it is, like every other existing column.
            Ok(live) if live.field(&name).is_some() => continue,
            Ok(_) => {}
            Err(e) => {
                report.skipped(format!("columns of `users`: {}", e.causes()));
                break;
            }
        }
        let result = add_field(catalog, sc_auth::USERS_TABLE, field).await;
        report.outcome(&format!("column `users.{name}`"), result);
    }

    let Ok(table) = catalog.require(sc_auth::USERS_TABLE) else {
        report.skipped("users: this server has no users table");
        return repointed;
    };
    let existing = existing_users(catalog).await.unwrap_or_default();
    let mut restored = 0;
    for user in &users {
        let email = user
            .get(sc_auth::COL_EMAIL)
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let id = user
            .get(sc_auth::COL_ID)
            .and_then(Json::as_str)
            .unwrap_or("");
        if existing.ids.contains(id) {
            report.skipped(format!(
                "user `{email}` is already on this server; kept as it is"
            ));
            continue;
        }
        if let Some(here) = existing.by_email.get(&email) {
            report.skipped(format!(
                "user `{email}` is already on this server; kept as it is, and the restored \
                 rows that referred to it refer to it here"
            ));
            repointed.insert(id.to_owned(), here.clone());
            continue;
        }
        let (inserted, problems) = insert_rows(catalog, &table, std::slice::from_ref(user)).await;
        restored += inserted;
        for problem in problems {
            report.skipped(format!("user `{email}`: {problem}"));
        }
    }
    if restored > 0 {
        report.did(format!("{restored} users"));
    }
    repointed
}

/// The accounts already here, by id and by email: either colliding means "this
/// account is here".
#[derive(Default)]
struct ExistingUsers {
    ids: BTreeSet<String>,
    /// Email → the id it has here.
    by_email: BTreeMap<String, String>,
}

async fn existing_users(catalog: &Catalog) -> Result<ExistingUsers> {
    let table = catalog.require(sc_auth::USERS_TABLE)?;
    let mut out = ExistingUsers::default();
    for row in rows_of(catalog, &table).await? {
        let id = row.get(sc_auth::COL_ID).and_then(Json::as_str);
        if let Some(id) = id {
            out.ids.insert(id.to_owned());
        }
        if let (Some(id), Some(email)) = (id, row.get(sc_auth::COL_EMAIL).and_then(Json::as_str)) {
            out.by_email.insert(email.to_owned(), id.to_owned());
        }
    }
    Ok(out)
}

/// `rows` with every reference to the users table that names a kept account by
/// its backup id ([`restore_users`]) naming it by its id here instead.
fn repoint_users(
    table: &Table,
    mut rows: Vec<Json>,
    user_ids: &BTreeMap<String, String>,
) -> Vec<Json> {
    if user_ids.is_empty() {
        return rows;
    }
    let columns: Vec<&str> = table
        .fields
        .iter()
        .filter(|field| {
            matches!(&field.kind, DataFieldKind::Key { target_table, .. }
                if target_table.0 == sc_auth::USERS_TABLE)
        })
        .map(|field| field.base.name.as_str())
        .collect();
    for row in &mut rows {
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        for column in &columns {
            let here = obj
                .get(*column)
                .and_then(Json::as_str)
                .and_then(|id| user_ids.get(id));
            if let Some(here) = here {
                obj.insert((*column).to_owned(), Json::String(here.clone()));
            }
        }
    }
    rows
}

async fn restore_role(catalog: &Catalog, value: &Json) -> Result<String> {
    let obj = value
        .as_object()
        .ok_or_else(|| Error::invalid("a role must be an object"))?;
    let number = obj
        .get("role")
        .and_then(Json::as_i64)
        .and_then(|n| u8::try_from(n).ok())
        .filter(|r| sc_auth::role_in_range(*r))
        .ok_or_else(|| Error::invalid("a role must have a number between 1 and 100"))?;
    let name = obj
        .get("name")
        .and_then(Json::as_str)
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| Error::invalid("a role must have a name"))?;
    let mut role = sc_auth::Role::new(number, name.trim());
    role.description = obj
        .get("description")
        .and_then(Json::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    sc_auth::save_role(catalog, &role).await?;
    Ok(String::new())
}

/// Create the table if it is not here, or apply the backup's settings to it if it
/// is. Columns are a separate pass — see [`restore_backup`].
///
/// The ownership formula and row-level security are held back and returned, to
/// be applied once the columns they read exist — `None` when there is nothing to
/// apply (a new table with neither).
async fn restore_table(
    catalog: &Catalog,
    entries: &Entries,
    name: &str,
) -> Result<(String, Option<schema_edit::TableSettings>)> {
    let document = json_entry(entries, &format!("tables/{name}/table.json"))?;
    let table = document
        .get("table")
        .and_then(Json::as_object)
        .ok_or_else(|| Error::invalid("a table entry must carry a `table` object"))?;
    let mut settings = table_settings_from_body(table)?;
    let access = schema_edit::TableSettings {
        ownership_formula: settings.ownership_formula.take(),
        rls_enabled: settings.rls_enabled.take(),
        ..Default::default()
    };
    let exists = catalog.get(name)?.is_some();
    let operation = if exists {
        schema_edit::Operation::AlterTable {
            table: name.to_owned(),
            settings,
        }
    } else {
        schema_edit::Operation::CreateTable {
            name: name.to_owned(),
            // A backup carries Saltcorn's own schema; a table that lived on a
            // connection is that connection's to restore, not this archive's.
            database: String::new(),
            settings,
            // Deliberately none: the columns go in one at a time in the next
            // pass, so one column the schema editor refuses does not take the
            // whole table with it.
            fields: Vec::new(),
        }
    };
    schema_edit::apply(catalog, &[operation], &schema_edit::ApplyOptions::default()).await?;
    let line = if exists {
        format!("table `{name}` (settings; it was already here)")
    } else {
        format!("table `{name}`")
    };
    let nothing_to_apply = !exists
        && access
            .ownership_formula
            .as_deref()
            .unwrap_or_default()
            .is_empty()
        && access.rls_enabled != Some(true);
    Ok((line, (!nothing_to_apply).then_some(access)))
}

/// Which of the column passes in [`restore_backup`] a field belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldPass {
    Plain,
    Reference,
    Calc,
}

impl FieldPass {
    fn of(field: &Map<String, Json>) -> FieldPass {
        match field
            .get("kind")
            .and_then(|kind| kind.get("type"))
            .and_then(Json::as_str)
        {
            Some("key") => FieldPass::Reference,
            Some("calc") => FieldPass::Calc,
            _ => FieldPass::Plain,
        }
    }
}

/// The fields of one pass that the backup describes for `name` and the live
/// table has not got, with their names.
fn missing_fields(
    catalog: &Catalog,
    entries: &Entries,
    name: &str,
    pass: FieldPass,
) -> Result<Vec<(String, Map<String, Json>)>> {
    let Ok(document) = json_entry(entries, &format!("tables/{name}/table.json")) else {
        return Ok(Vec::new());
    };
    let live = catalog.require(name)?;
    Ok(array_field(&document, "fields")
        .iter()
        .filter_map(Json::as_object)
        .filter(|field| FieldPass::of(field) == pass)
        .filter_map(|field| {
            let field_name = field.get("name").and_then(Json::as_str).unwrap_or_default();
            // Unnamed, or a column an existing table already has: not news.
            (!field_name.is_empty() && live.field(field_name).is_none())
                .then(|| (field_name.to_owned(), field.clone()))
        })
        .collect())
}

/// Add the columns of one pass that the backup describes and this table has not
/// got.
///
/// A column that is already here is left alone rather than altered: its type is
/// the database's answer, and a restore that rewrote a live column's type would
/// be a migration nobody asked for.
async fn restore_fields(
    catalog: &Catalog,
    entries: &Entries,
    name: &str,
    pass: FieldPass,
    report: &mut RestoreReport,
) {
    let missing = match missing_fields(catalog, entries, name, pass) {
        Ok(missing) => missing,
        Err(e) => {
            report.skipped(format!("columns of `{name}`: {}", e.causes()));
            return;
        }
    };
    for (field_name, field) in missing {
        // The primary key travels as what it is — a field that says it is one —
        // so a restored table has the key the backup had, composite or not.
        // Nothing invents a key here or anywhere else (GOALS).
        let result = add_field(catalog, name, &field).await;
        report.outcome(&format!("column `{name}.{field_name}`"), result);
    }
}

/// The calculated fields of every restored table, after all the other columns.
///
/// One calculated field may read another — in its own table or, through a join,
/// in a table further down the list — so a field the schema editor refuses is
/// tried again once others have gone in, until a round adds nothing. Only what
/// is still refused then is reported, with the reason the last attempt gave.
async fn restore_calc_fields(
    catalog: &Catalog,
    entries: &Entries,
    tables: &[String],
    report: &mut RestoreReport,
) {
    let mut pending = Vec::new();
    for name in tables {
        match missing_fields(catalog, entries, name, FieldPass::Calc) {
            Ok(missing) => pending.extend(
                missing
                    .into_iter()
                    .map(|(field_name, field)| (name.clone(), field_name, field)),
            ),
            Err(e) => report.skipped(format!("columns of `{name}`: {}", e.causes())),
        }
    }
    loop {
        let mut refused = Vec::new();
        let before = pending.len();
        for (name, field_name, field) in pending {
            match add_field(catalog, &name, &field).await {
                Ok(line) => report.outcome(&format!("column `{name}.{field_name}`"), Ok(line)),
                Err(e) => refused.push((name, field_name, field, e)),
            }
        }
        if refused.is_empty() || refused.len() == before {
            for (name, field_name, _, e) in refused {
                report.outcome(&format!("column `{name}.{field_name}`"), Err(e));
            }
            return;
        }
        pending = refused
            .into_iter()
            .map(|(name, field_name, field, _)| (name, field_name, field))
            .collect();
    }
}

/// Add the constraints the backup describes and this table has not got.
///
/// Without this a restore would hand back a table that **accepts what the
/// original refused** — the columns and the rows, with none of the rules that
/// were the point of half of them — and would say nothing about it. Constraints
/// are not stored in an `_fd_*` table (§5.1), so they travel in the backup as
/// what the database reported, which is also how a constraint somebody added by
/// hand comes back.
///
/// Two things are deliberately not attempted. A constraint the table already has
/// is left alone rather than replaced, like a column that is already there. And
/// an index over an **expression** that Saltcorn did not create is reported as
/// skipped rather than approximated: what could be recreated from it is an index
/// over no columns, which is not the index the backup described.
async fn restore_constraints(
    catalog: &Catalog,
    entries: &Entries,
    name: &str,
    report: &mut RestoreReport,
) {
    let Ok(document) = json_entry(entries, &format!("tables/{name}/table.json")) else {
        return;
    };
    for value in array_field(&document, "constraints") {
        let Some(obj) = value.as_object() else {
            continue;
        };
        let constraint_name = obj
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned();
        if constraint_name.is_empty() {
            continue;
        }
        let live = match catalog.require(name) {
            Ok(table) => table,
            Err(e) => {
                report.skipped(format!("constraints of `{name}`: {}", e.causes()));
                return;
            }
        };
        if live.constraints.iter().any(|c| c.name == constraint_name) {
            continue;
        }
        let what = format!("constraint `{constraint_name}` on `{name}`");
        let constraint = match constraint_from_backup(obj) {
            Ok(constraint) => constraint,
            Err(e) => {
                report.skipped(format!("{what}: {}", e.causes()));
                continue;
            }
        };
        let result = schema_edit::apply(
            catalog,
            &[schema_edit::Operation::AddConstraint {
                table: name.to_owned(),
                given_name: String::new(),
                constraint,
            }],
            &schema_edit::ApplyOptions::default(),
        )
        .await
        .map(|_| what.clone());
        report.outcome(&what, result);
    }
}

/// One constraint out of a backup's `table.json`, keeping the name it had — a
/// restored constraint reports itself by the same name a violation would have
/// named before the backup was taken.
fn constraint_from_backup(obj: &Map<String, Json>) -> Result<TableConstraint> {
    let text = |key: &str| {
        obj.get(key)
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let fields: Vec<String> = obj
        .get("fields")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let type_name = obj
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid("a constraint entry needs a `type`"))?;
    let kind = match type_name {
        "unique" => ConstraintKind::Unique { fields },
        "index" if fields.is_empty() => {
            return Err(Error::invalid(
                "an index over an expression is not one this restore can recreate; \
                 add it by hand",
            ));
        }
        "index" => ConstraintKind::Index {
            fields,
            expression: None,
            method: text("method").unwrap_or_else(|| "btree".to_owned()),
        },
        "full_text_search" => ConstraintKind::FullTextSearch {
            language: text("language").unwrap_or_else(|| "english".to_owned()),
        },
        "formula" => ConstraintKind::Formula {
            formula: text("formula")
                .ok_or_else(|| Error::invalid("a row constraint entry needs a `formula`"))?,
        },
        other => {
            return Err(Error::invalid(format!(
                "`{other}` is not a constraint type"
            )));
        }
    };
    let name = obj
        .get("name")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut constraint = TableConstraint::new(name, kind);
    constraint.error_message = text("error_message");
    Ok(constraint)
}

async fn add_field(catalog: &Catalog, table: &str, field: &Map<String, Json>) -> Result<String> {
    let mut spec = field_spec_from_body(field)?;
    // A reference's storage type is its target's, and the schema editor is what
    // knows that (it fills the type in from the table pointed at). The backup
    // carries the *introspected* type, which is the same answer for as long as
    // both ends agree — and is the wrong thing to insist on when they might not.
    if matches!(spec.kind, DataFieldKind::Key { .. }) {
        spec.type_name = String::new();
    }
    schema_edit::apply(
        catalog,
        &[schema_edit::Operation::AddField {
            table: table.to_owned(),
            field: spec,
        }],
        &schema_edit::ApplyOptions::default(),
    )
    .await?;
    Ok(String::new())
}

/// The restored tables ordered so that a table comes after everything its `Key`
/// fields point at.
///
/// A cycle (two tables referencing each other) cannot be ordered and is left in
/// place at the end: its rows are attempted, and whichever ones the database
/// refuses are reported row by row. A restore that silently dropped them would be
/// the worse answer.
fn order_by_references(catalog: &Catalog, tables: &[String]) -> Vec<String> {
    let mut remaining: Vec<String> = tables.to_vec();
    let mut done: Vec<String> = Vec::new();
    // At most one pass per table: each pass either places something or the rest
    // is a cycle.
    while !remaining.is_empty() {
        let mut placed = false;
        let mut next = Vec::new();
        for name in remaining {
            let ready = match catalog.get(&name) {
                Ok(Some(table)) => table.fields.iter().all(|field| match &field.kind {
                    DataFieldKind::Key { target_table, .. } => {
                        target_table.0 == name
                            || !tables.contains(&target_table.0)
                            || done.contains(&target_table.0)
                    }
                    _ => true,
                }),
                _ => true,
            };
            if ready {
                done.push(name);
                placed = true;
            } else {
                next.push(name);
            }
        }
        if !placed {
            done.extend(next);
            break;
        }
        remaining = next;
    }
    done
}

/// Insert rows as they were backed up — primary keys included, so a foreign key
/// pointing at one still points at it.
///
/// Not through [`rows::create_row`], and that is the substance of this function:
/// Wind every identity column of `table` past the keys the restore just wrote.
///
/// A backup carries its rows' keys and they are inserted as given — the identity
/// is `BY DEFAULT`, so an explicit value is accepted, which is the whole reason
/// it is not `ALWAYS`. But the sequence behind it never saw those inserts and is
/// still sitting at 1, so the first row written *after* a restore would be handed
/// a key some restored row already has. Re-applying the generator is what winds
/// it, and it is the same change the schema editor emits when a key is switched
/// on over rows that are already there — the two situations are the same
/// situation.
///
/// Reported rather than raised: a restore that got the rows in is worth having
/// even if one sequence could not be wound, and the report is where a partial
/// restore says what to fix by hand.
async fn rewind_identities(catalog: &Catalog, table: &Table, report: &mut RestoreReport) {
    for field in &table.fields {
        if field.generated != Some(ColumnGenerator::Identity) {
            continue;
        }
        let change = SchemaChange::SetColumnGenerator {
            table: table.name.clone(),
            column: field.base.name.clone(),
            generator: Some(ColumnGenerator::Identity),
        };
        if let Err(e) = catalog.primary().apply_schema(&change).await {
            report.skipped(format!(
                "the numbering of `{}.{}` could not be wound past the restored rows: {}",
                table.name,
                field.base.name,
                e.causes()
            ));
        }
    }
}

/// that path raises the table's insert **triggers** (§10.2), which during a
/// restore would fire an installation's automation for every historical row it
/// ever had. A restore puts data back; it does not replay it.
///
/// Returns how many rows went in, and one message per row that did not.
async fn insert_rows(catalog: &Catalog, table: &Table, values: &[Json]) -> (usize, Vec<String>) {
    let mut inserted = 0;
    let mut problems = Vec::new();
    for value in values {
        match insert_row(catalog, table, value).await {
            Ok(()) => inserted += 1,
            Err(e) => problems.push(e.causes()),
        }
    }
    (inserted, problems)
}

async fn insert_row(catalog: &Catalog, table: &Table, value: &Json) -> Result<()> {
    let obj = value
        .as_object()
        .ok_or_else(|| Error::invalid("a row must be an object"))?;
    let mut columns = Vec::with_capacity(obj.len());
    let mut literals = Vec::with_capacity(obj.len());
    for (column, json) in obj {
        match table.field(column) {
            // A calculated field has no column to write (§3.4); it is computed
            // on read, and the backup carries the value it had.
            Some(field) if field.is_calc() => continue,
            // A column this server has not got: the backup is from an
            // installation whose table had more in it. Skipped rather than
            // failing the row, because the row is still worth having.
            None => continue,
            Some(_) => {}
        }
        columns.push(column.clone());
        literals.push(Expr::lit(rows::column_value(table, column, json)?));
    }
    if columns.is_empty() {
        return Err(Error::invalid(
            "no column of this row matches the table's columns",
        ));
    }
    let insert = Insert::row(table.name.clone(), columns, literals);
    catalog
        .primary()
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// A store's definition (only when there is not one already), then its files.
///
/// **An existing store keeps its definition.** A backup's store points at a path
/// on the machine it was taken from, and a restore onto a different machine must
/// not repoint a working store at a directory that is not there. The files are
/// restored into whatever the store here already is, which is what an admin who
/// set the store up before restoring meant.
///
/// **A new store is placed where this server can reach it.** Its definition's
/// directory is kept only when that absolute path exists here; otherwise it is
/// moved to the location this installation recommends for a store of that
/// backend ([`sc_files::relocate_for_restore`]), and the report says so.
///
/// **A new git store is brought up, not just defined.** A git store connects
/// only over a checkout, so a definition alone would leave its files nowhere to
/// go. The backup normally *is* the checkout — a store's files are everything in
/// its directory, `.git` included — and then the checkout is written back as it
/// was: its branch, its unpushed commits, its uncommitted edits, with no network
/// and no deploy key needed (a pull brings it up to date). Only a backup with no
/// `.git` in it is cloned from the store's URL, as creating the store would.
///
/// **A `.git` is never written into a repository that is already there.** That
/// repository is the one the admin meant, and the backup's refs and index laid
/// over it would corrupt it rather than restore it.
async fn restore_file_store(
    catalog: &Catalog,
    entries: &Entries,
    name: &str,
    report: &mut RestoreReport,
) {
    let document = match json_entry(entries, &format!("file-stores/{name}/store.json")) {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("file store `{name}`: {}", e.causes()));
            return;
        }
    };
    let prefix = format!("file-stores/{name}/files/");
    let files: Vec<(&str, &Vec<u8>)> = entries
        .iter()
        .filter_map(|(entry, bytes)| Some((entry.strip_prefix(&prefix)?, bytes)))
        .filter(|(path, _)| !path.is_empty())
        .collect();
    // A backup written before `node_modules` was left out still carries it, and
    // a copy of it does not run here ([`super::is_installed_dependency`]).
    let installed = files
        .iter()
        .filter(|(path, _)| super::is_installed_dependency(path))
        .count();
    let files: Vec<(&str, &Vec<u8>)> = files
        .into_iter()
        .filter(|(path, _)| !super::is_installed_dependency(path))
        .collect();
    if installed > 0 {
        report.skipped(format!(
            "{} in `node_modules` of `{name}`: dependencies are installed by the build, \
             not restored",
            counted(installed, "file", "files")
        ));
    }
    let carries_checkout = files.iter().any(|(path, _)| is_git_path(path));

    // Set when this restore defined a git store whose checkout is in the backup:
    // the store is connected once that checkout has been written.
    let mut checkout_to_write = None;
    match load_file_store_by_name(catalog, name).await {
        Ok(Some(_)) => report.skipped(format!(
            "file store `{name}` is already defined here; its definition was kept and the \
             backup's files were written into it"
        )),
        Ok(None) => {
            match define_store(catalog, document.get("definition"), carries_checkout).await {
                Ok((def, detail, write_checkout)) => {
                    if write_checkout {
                        checkout_to_write = Some(def);
                    }
                    report.outcome(&format!("file store `{name}`"), Ok(detail));
                }
                Err(e) => report.outcome(&format!("file store `{name}`"), Err(e)),
            }
        }
        Err(e) => {
            report.skipped(format!("file store `{name}`: {}", e.causes()));
            return;
        }
    }

    let store: Arc<dyn sc_files::FileStore> = if let Some(def) = &checkout_to_write {
        match checkout_store(def) {
            Ok(store) => store,
            Err(e) => {
                report.skipped(format!("files of `{name}`: {}", e.causes()));
                return;
            }
        }
    } else {
        match catalog.require_file_store(name) {
            Ok(store) => store,
            Err(_) => {
                report.skipped(format!(
                    "files of `{name}`: the store is not connected on this server"
                ));
                return;
            }
        }
    };
    // Checked once, before anything is written: whether there is a repository
    // here that is not the one this restore is writing.
    let keep_existing_git = checkout_to_write.is_none() && store.is_git_repo();

    let mut written = 0;
    let mut metadata: BTreeMap<String, Json> = BTreeMap::new();
    for value in array_field(&document, "files") {
        if let Some(path) = value.get("path").and_then(Json::as_str) {
            metadata.insert(path.to_owned(), value.clone());
        }
    }
    let mut kept_git = false;
    for (path, bytes) in files {
        // The store itself refuses a path that escapes its root, and this refuses
        // it before the bytes are read: a zip is a file somebody can hand us, and
        // `..` in an entry name is the oldest trick there is.
        if path.split('/').any(|part| part == ".." || part == ".") {
            report.skipped(format!(
                "file `{path}` of `{name}`: the path is not relative"
            ));
            continue;
        }
        if keep_existing_git && is_git_path(path) {
            kept_git = true;
            continue;
        }
        let mode = metadata
            .get(path)
            .and_then(|value| value.get("mode"))
            .and_then(Json::as_u64)
            .and_then(|mode| u32::try_from(mode).ok());
        if mode.is_some() {
            make_writable(store.as_ref(), path);
        }
        match store.write(path, bytes.clone().into()).await {
            Ok(()) => written += 1,
            Err(e) => {
                report.skipped(format!("file `{path}` of `{name}`: {}", e.causes()));
                continue;
            }
        }
        if let Some(mode) = mode
            && let Err(e) = set_mode(store.as_ref(), path, mode)
        {
            report.skipped(format!(
                "permissions of `{path}` in `{name}`: {}",
                e.causes()
            ));
        }
        // The rules the file carried, after the bytes it carried them for.
        if let Some(value) = metadata.get(path)
            && let Ok((_, meta)) = backup_file_meta_from_json(value)
            && (meta.min_role.is_some() || !meta.attributes.is_empty())
            && let Err(e) = store.set_meta(path, &meta).await
        {
            report.skipped(format!("metadata of `{path}` in `{name}`: {}", e.causes()));
        }
    }
    if written > 0 {
        report.did(format!("{written} files into `{name}`"));
    }
    if kept_git {
        report.skipped(format!(
            "the `.git` of `{name}` in the backup was not written: the store already has a \
             repository, which was left as it is"
        ));
    }
    if let Some(def) = checkout_to_write {
        match sc_catalog::connect_file_store_def(catalog, &def) {
            Ok(()) => report.did(format!(
                "the git working copy of `{name}`, restored from the backup"
            )),
            Err(e) => report.skipped(format!(
                "file store `{name}` was restored but is not connected: {}",
                e.causes()
            )),
        }
    }
}

/// Give a file the permission bits the backup recorded for it — what keeps an
/// executable script executable.
///
/// Only the `rwx` bits, whatever the file says: setuid, setgid and sticky are not
/// something a zip somebody handed us gets to set. A store with no directory on
/// disk, or a platform without Unix modes, has nothing to set.
#[cfg(unix)]
fn set_mode(store: &dyn sc_files::FileStore, path: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let Some(local) = store.local_path(path)? else {
        return Ok(());
    };
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o777))
        .map_err(|e| Error::msg(format!("setting the mode of {}: {e}", local.display())))
}

#[cfg(not(unix))]
fn set_mode(_store: &dyn sc_files::FileStore, _path: &str, _mode: u32) -> Result<()> {
    Ok(())
}

/// Let the owner write a file that is already there, so the backup's copy can
/// replace it.
///
/// A file restored with its recorded mode may be read-only — every object in a
/// `.git` is — and restoring the same backup again would otherwise fail on each
/// one. The backup's own mode is put back straight after the write. Best-effort:
/// a file this cannot change is reported by the write that follows.
#[cfg(unix)]
fn make_writable(store: &dyn sc_files::FileStore, path: &str) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(Some(local)) = store.local_path(path) else {
        return;
    };
    if let Ok(meta) = std::fs::metadata(&local)
        && meta.is_file()
        && meta.permissions().mode() & 0o200 == 0
    {
        let _ = std::fs::set_permissions(
            &local,
            std::fs::Permissions::from_mode(meta.permissions().mode() | 0o200),
        );
    }
}

#[cfg(not(unix))]
fn make_writable(_store: &dyn sc_files::FileStore, _path: &str) {}

/// Whether a store-relative path is in (or is) a repository's `.git` — a
/// directory in an ordinary checkout, a file in a worktree.
fn is_git_path(path: &str) -> bool {
    path == ".git" || path.starts_with(".git/")
}

/// A plain directory store over where a git store's checkout belongs, for writing
/// that checkout before the git store itself can connect to it.
fn checkout_store(def: &sc_files::FileStoreDef) -> Result<Arc<dyn sc_files::FileStore>> {
    let root = sc_files::clone_path(def)?;
    std::fs::create_dir_all(&root)
        .map_err(|e| Error::msg(format!("creating {}: {e}", root.display())))?;
    Ok(Arc::new(sc_files::LocalFileStore::new(&def.name, &root)?))
}

/// Define a store from a backup's definition, returning what was saved, a note
/// for the report, and whether the caller is to write the backup's checkout and
/// then connect the store ([`restore_file_store`]).
///
/// A git store whose backup does **not** carry its checkout is cloned here, as
/// creating it in the admin UI would. One whose directory already holds a
/// checkout (a restore onto the machine the backup came from) is simply
/// connected to it.
async fn define_store(
    catalog: &Catalog,
    definition: Option<&Json>,
    carries_checkout: bool,
) -> Result<(sc_files::FileStoreDef, String, bool)> {
    let definition =
        definition.ok_or_else(|| Error::invalid("the entry carries no store definition"))?;
    let mut def = file_store_from_body(sc_files::FileStoreDefId::new(), definition)?;
    // The backup's directory is the one the store had on the machine it was taken
    // from. Unless that path exists here too (a shared drive, or the same machine),
    // the store goes where this installation puts its stores.
    let moved = sc_files::relocate_for_restore(&mut def)?;
    sc_catalog::check_file_store_saveable(catalog, &def).await?;
    let mut notes = Vec::new();
    if let Some((from, to)) = moved {
        notes.push(format!(
            "{from} does not exist on this server, so the store was placed in {}",
            to.display()
        ));
    }
    let mut write_checkout = false;
    if def.backend == sc_files::GIT_BACKEND {
        write_checkout = carries_checkout && !sc_files::GitRepo::from_def(&def)?.is_cloned();
        // Where the checkout goes is recorded either way, so a later rename of
        // the store cannot orphan it — exactly what the clone operation records.
        let root = sc_files::clone_path(&def)?;
        sc_files::record_clone_path(&mut def, &root);
        if !carries_checkout && !write_checkout {
            // A clone that fails still leaves a saved store: the definition is
            // what the admin repairs (a deploy key to install, a URL to fix), and
            // the Clone button retries.
            if let Err(e) = create_backend_resources(&mut def).await {
                notes.push(format!("defined, but not cloned: {}", e.causes()));
            }
        }
    }
    sc_catalog::save_file_store(catalog, &def).await?;
    // Connected straight away, as `createFileStore` does, so the files have
    // somewhere to go — and so an unreachable directory is reported now. A git
    // store whose checkout is still to be written cannot connect yet, and is
    // connected once it has been.
    if !write_checkout && let Err(e) = sc_catalog::connect_file_store_def(catalog, &def) {
        notes.push(format!("defined, but not connected: {}", e.causes()));
    }
    Ok((def, notes.join("; "), write_checkout))
}

/// One application, under the id it had, so what references it still does.
/// One application, under the id it had, **built and mounted**.
///
/// The build is the point of doing it here rather than leaving it to the
/// Applications screen: a restored installation that does not serve its
/// applications is not a restored installation, and the admin has no way to know
/// which of them needed a button pressed. So each one goes through the same
/// `build_and_mount` the Build button does — the bundler over the source tree the
/// file stores restored a moment ago (which is why applications come *after* them),
/// then a live remount, so the app answers on its subdomain as soon as the restore
/// finishes. A first build installs the project's dependencies too, so a source
/// tree that arrived without its `node_modules` is not a special case.
///
/// A build failure is a **warning against a saved application**, not a lost one:
/// the definition is already stored, the reason is the bundler's own message, and
/// pressing Build after fixing it is exactly the repair.
/// One application, then its views and pages, then its build.
///
/// `key` is the subdomain the file keys the application by. It is the one it is
/// served on, except for a Saltcorn 1 import whose subdomain was taken
/// ([`import_application`]).
async fn restore_application(
    catalog: &Catalog,
    apps: &AppMounts,
    entries: &Entries,
    key: &str,
    selection: &Selection,
    report: &mut RestoreReport,
) {
    let saved = async {
        let document = json_entry(entries, &format!("applications/{key}.json"))?;
        match document
            .get("id")
            .and_then(Json::as_str)
            .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
        {
            Some(id) => {
                let app = application_from_body(sc_app::AppId(id), &document)?;
                Ok((
                    sc_app::save_application(catalog, &app).await?,
                    String::new(),
                ))
            }
            None => import_application(catalog, &document).await,
        }
    }
    .await;
    let app = match saved {
        Ok((app, detail)) => {
            report.outcome(&format!("application `{}`", app.subdomain), Ok(detail));
            app
        }
        Err(e) => {
            report.skipped(format!("application `{key}`: {}", e.causes()));
            return;
        }
    };
    let subdomain = app.subdomain.clone();

    // Before the build: mounting a Saltcorn UI application is reading its views.
    restore_views_and_pages(catalog, entries, key, &app, selection, report).await;
    restore_translations(catalog, entries, key, &app, report).await;

    // A framework constructed from a factory (Saltcorn UI) has no build and no
    // Build button, so "build it once its source is in place" would send the
    // admin looking for both.
    let builds = sc_app::framework_factory(&app.framework.name).is_none();
    match crate::apps::build_and_mount(apps, app).await {
        Ok(_) if !builds => report.did(format!("application `{subdomain}` serving")),
        Ok(built) => report.did(format!(
            "application `{subdomain}` built and serving{}",
            if built.installed {
                " (its dependencies were installed)"
            } else {
                ""
            }
        )),
        Err(e) if !builds => report.skipped(format!(
            "application `{subdomain}` is restored but could not be mounted, so it is not \
             serving: {}",
            e.causes()
        )),
        Err(e) => report.skipped(format!(
            "application `{subdomain}` is restored but did not build, so it is not serving \
             yet — build it from the Applications screen once its source is in place: {}",
            e.causes()
        )),
    }
}

/// An application a **Saltcorn 1 import** describes (§13) — the one kind of
/// application document with no `id`, and so matched by **name** among the
/// Saltcorn UI applications already here, since the name is what a second import
/// of the same backup has in common with the first (8.6).
///
/// Found, it keeps its row as it is — its subdomain, its settings, its CSP, all of
/// which an admin may have changed since the first import — and gains only the
/// tables, file stores and triggers this import lists that it does not, so the
/// views about to be restored can name them. Not found, it is created on the
/// subdomain the import derived, or on the first free one after it.
///
/// Returns the application and what the report should add about it.
async fn import_application(
    catalog: &Catalog,
    document: &Json,
) -> Result<(sc_app::Application, String)> {
    let incoming = application_from_body(sc_app::AppId::new(), document)?;
    let existing = sc_app::list_applications(catalog)
        .await?
        .into_iter()
        .find(|a| a.name == incoming.name && a.framework.name == incoming.framework.name);
    if let Some(mut app) = existing {
        for table in incoming.tables {
            if !app.tables.contains(&table) {
                app.tables.push(table);
            }
        }
        for store in incoming.file_stores {
            if !app.file_stores.contains(&store) {
                app.file_stores.push(store);
            }
        }
        for trigger in incoming.triggers {
            if !app.triggers.contains(&trigger) {
                app.triggers.push(trigger);
            }
        }
        let app = sc_app::save_application(catalog, &app).await?;
        let detail = format!(
            "already here as `{}`, so its settings are kept and the imported views and \
             pages replace its own",
            app.name
        );
        return Ok((app, detail));
    }
    let mut app = incoming;
    let wanted = app.subdomain.clone();
    app.subdomain = free_subdomain(catalog, &wanted).await?;
    let detail = if app.subdomain == wanted {
        String::new()
    } else {
        format!("`{wanted}` is another application's subdomain")
    };
    Ok((sc_app::save_application(catalog, &app).await?, detail))
}

/// `wanted`, or `wanted-2`, `wanted-3`, … — the first no application is served on.
async fn free_subdomain(catalog: &Catalog, wanted: &str) -> Result<String> {
    for n in 1..=1000 {
        let candidate = if n == 1 {
            wanted.to_owned()
        } else {
            let suffix = format!("-{n}");
            // Still one DNS label.
            let stem: String = wanted.chars().take(63 - suffix.len()).collect();
            format!("{}{suffix}", stem.trim_end_matches('-'))
        };
        if sc_app::load_application_by_subdomain(catalog, &candidate)
            .await?
            .is_none()
        {
            return Ok(candidate);
        }
    }
    Err(Error::invalid(format!(
        "every subdomain from `{wanted}` to `{wanted}-1000` is taken"
    )))
}

/// An application's library, views and pages (TODO "Saltcorn UI" 8.4–8.6, "The
/// builder" 4.4).
///
/// **They replace the application's own.** This is the one place a restore
/// deletes anything, and what it deletes is the content of the application whose
/// row was just written from the same file, not something else on the server:
/// re-importing a backup must leave seven views, not fourteen. A kind is replaced
/// only when it was chosen and the file carries it for this application. The
/// library travels under the views' choice, and is restored **first**, so the
/// views and pages that place its items find them.
///
/// Each item, view and page is saved on its own and a refusal is a line naming
/// it — a pattern this server has not got, a table that did not come, anything
/// `save_view` refuses. Then each link from what was saved to a view the
/// application does not have is a line too, and so is each placement of a library
/// item it does not have: the view is imported, and the admin is told its link
/// leads nowhere before somebody clicks it.
async fn restore_views_and_pages(
    catalog: &Catalog,
    entries: &Entries,
    key: &str,
    app: &sc_app::Application,
    selection: &Selection,
    report: &mut RestoreReport,
) {
    let wanted = |kind: &str, chosen: bool| {
        let path = format!("applications/{key}/{kind}.json");
        (chosen && entries.contains_key(&path)).then_some(path)
    };
    let mut library = Vec::new();
    if let Some(path) = wanted("library", selection.views) {
        let replaced = async {
            let document = json_entry(entries, &path)?;
            for item in sc_viewpattern::list_library(catalog, app.id).await? {
                sc_viewpattern::delete_library_item(catalog, app.id, item.id).await?;
            }
            Ok::<_, Error>(document)
        }
        .await;
        match replaced {
            Ok(document) => {
                for value in array_list(&document) {
                    let name = value
                        .get("name")
                        .and_then(Json::as_str)
                        .unwrap_or("a library item")
                        .to_owned();
                    let result = async {
                        let item = library_item_from_body(
                            record_id(&value).map_or_else(
                                sc_viewpattern::LibraryItemId::new,
                                sc_viewpattern::LibraryItemId,
                            ),
                            app.id,
                            &value,
                        )?;
                        sc_viewpattern::save_library_item(catalog, &item).await
                    }
                    .await;
                    match result {
                        Ok(item) => library.push(item),
                        Err(e) => report.skipped(format!(
                            "library item `{name}` was not imported: {}",
                            e.causes()
                        )),
                    }
                }
                report.did(format!(
                    "{} into application `{}`",
                    counted(library.len(), "library item", "library items"),
                    app.subdomain
                ));
            }
            Err(e) => report.skipped(format!(
                "the library of application `{}`: {}",
                app.subdomain,
                e.causes()
            )),
        }
    }

    let mut views = Vec::new();
    if let Some(path) = wanted("views", selection.views) {
        let replaced = async {
            let document = json_entry(entries, &path)?;
            for view in sc_viewpattern::list_views(catalog, app.id).await? {
                sc_viewpattern::delete_view(catalog, app.id, &view.name).await?;
            }
            Ok::<_, Error>(document)
        }
        .await;
        match replaced {
            Ok(document) => {
                let patterns = sc_viewpattern::registered_patterns();
                for value in array_list(&document) {
                    let name = value
                        .get("name")
                        .and_then(Json::as_str)
                        .unwrap_or("a view")
                        .to_owned();
                    match restore_view(catalog, app, &patterns, &value).await {
                        Ok(view) => views.push(view),
                        Err(e) => {
                            report
                                .skipped(format!("view `{name}` was not imported: {}", e.causes()));
                        }
                    }
                }
                report.did(format!(
                    "{} into application `{}`",
                    counted(views.len(), "view", "views"),
                    app.subdomain
                ));
            }
            Err(e) => report.skipped(format!(
                "the views of application `{}`: {}",
                app.subdomain,
                e.causes()
            )),
        }
    }

    let mut pages = Vec::new();
    if let Some(path) = wanted("pages", selection.pages) {
        let replaced = async {
            let document = json_entry(entries, &path)?;
            for page in sc_viewpattern::list_pages(catalog, app.id).await? {
                sc_viewpattern::delete_page(catalog, app.id, &page.name).await?;
            }
            Ok::<_, Error>(document)
        }
        .await;
        match replaced {
            Ok(document) => {
                for value in array_list(&document) {
                    let name = value
                        .get("name")
                        .and_then(Json::as_str)
                        .unwrap_or("a page")
                        .to_owned();
                    let result = async {
                        let page = page_from_body(
                            record_id(&value)
                                .map_or_else(sc_viewpattern::PageId::new, sc_viewpattern::PageId),
                            app.id,
                            &value,
                        )?;
                        sc_viewpattern::save_page(catalog, &page).await
                    }
                    .await;
                    match result {
                        Ok(page) => pages.push(page),
                        Err(e) => {
                            report
                                .skipped(format!("page `{name}` was not imported: {}", e.causes()));
                        }
                    }
                }
                report.did(format!(
                    "{} into application `{}`",
                    counted(pages.len(), "page", "pages"),
                    app.subdomain
                ));
            }
            Err(e) => report.skipped(format!(
                "the pages of application `{}`: {}",
                app.subdomain,
                e.causes()
            )),
        }
    }

    if views.is_empty() && pages.is_empty() && library.is_empty() {
        return;
    }
    // Against every view the application has now, not only the ones just
    // restored: pages restored with the views left alone may name the old ones.
    let known: BTreeSet<String> = match sc_viewpattern::list_views(catalog, app.id).await {
        Ok(all) => all.into_iter().map(|v| v.name).collect(),
        Err(e) => {
            report.skipped(format!(
                "the links between application `{}`'s views were not checked: {}",
                app.subdomain,
                e.causes()
            ));
            return;
        }
    };
    for view in &views {
        for target in sc_viewpattern::referenced_views(&Json::Object(view.configuration.clone())) {
            if !known.contains(&target) {
                report.skipped(format!(
                    "view `{}` links to the view `{target}`, which was not imported",
                    view.name
                ));
            }
        }
    }
    for page in &pages {
        for target in sc_viewpattern::referenced_views(&page.layout) {
            if !known.contains(&target) {
                report.skipped(format!(
                    "page `{}` shows the view `{target}`, which was not imported",
                    page.name
                ));
            }
        }
    }

    // Placements, against every item the application has now. An unknown id
    // renders blank (v1's `resolveSegment`), which is why it is a line and not a
    // refusal — and why the line is worth having.
    let items: BTreeSet<String> = match sc_viewpattern::list_library(catalog, app.id).await {
        Ok(all) => all.into_iter().map(|i| i.id.0.to_string()).collect(),
        Err(e) => {
            report.skipped(format!(
                "the library placements of application `{}` were not checked: {}",
                app.subdomain,
                e.causes()
            ));
            return;
        }
    };
    let placements = library
        .iter()
        .map(|i| (format!("library item `{}`", i.name), i.layout.clone()))
        .chain(views.iter().map(|v| {
            (
                format!("view `{}`", v.name),
                Json::Object(v.configuration.clone()),
            )
        }))
        .chain(
            pages
                .iter()
                .map(|p| (format!("page `{}`", p.name), p.layout.clone())),
        );
    for (what, layout) in placements {
        for id in sc_viewpattern::placed_library_ids(&layout) {
            if !items.contains(&id) {
                report.skipped(format!(
                    "{what} places the library item `{id}`, which was not imported, so it \
                     renders blank"
                ));
            }
        }
    }
}

/// One view, refused in the import's own words where the import knows why.
///
/// The two refusals checked here first are ones `save_view` would also make, but
/// in words for somebody building an application ("add it to the application
/// first"). Here the pattern is missing from this *server*, and the table from
/// this *restore*.
async fn restore_view(
    catalog: &Catalog,
    app: &sc_app::Application,
    patterns: &[sc_viewpattern::PatternInfo],
    value: &Json,
) -> Result<sc_viewpattern::View> {
    let id = record_id(value).map_or_else(sc_viewpattern::ViewId::new, sc_viewpattern::ViewId);
    let view = view_from_body(id, app.id, value)?;
    if sc_viewpattern::find_pattern(patterns, &view.viewpattern).is_none() {
        return Err(Error::invalid(format!(
            "it uses the view pattern `{}`, which this server does not have (Room and \
             WorkflowRoom are realtime and not supported, and a pattern a Saltcorn 1 plugin \
             supplied needs that plugin installed as a module)",
            view.viewpattern
        )));
    }
    if let Some(table) = &view.table_name
        && catalog.get(table)?.is_none()
    {
        return Err(Error::invalid(format!(
            "it is over the table `{table}`, which was not imported"
        )));
    }
    sc_viewpattern::save_view(catalog, &view).await
}

/// A record's `id`, when it carries one — a Feldspar backup's views and pages
/// do, a Saltcorn 1 import's do not.
fn record_id(value: &Json) -> Option<uuid::Uuid> {
    value
        .get("id")
        .and_then(Json::as_str)
        .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
}

/// `1 view` / `7 views`.
fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// LLM providers, each followed by its models.
///
/// **A provider already defined here — by name or by id — is kept as it is**: its
/// key and endpoint are the ones this server is using, and a restore does not
/// overwrite them. Models of the backup's that it lacks are still added to it,
/// never as its default, so an agent naming one of them can be restored; a model
/// it already has is left alone.
async fn restore_llm_providers(catalog: &Catalog, entries: &Entries, report: &mut RestoreReport) {
    let document = match json_entry(entries, "llm-providers.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("LLM providers: {}", e.causes()));
            return;
        }
    };
    for value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("an LLM provider")
            .to_owned();
        let what = format!("LLM provider `{name}`");
        let (provider, existed) = match restore_llm_provider(catalog, &value).await {
            Ok(found) => found,
            Err(e) => {
                report.outcome(&what, Err(e));
                continue;
            }
        };
        if existed {
            report.skipped(format!(
                "{what} is already defined here; its settings and key were kept"
            ));
        }
        let mut added = 0;
        for model in array_field(&value, "models") {
            let model_name = model
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or("a model")
                .to_owned();
            match restore_llm_model(catalog, &provider, &model, existed).await {
                Ok(true) => added += 1,
                Ok(false) => {}
                Err(e) => report.skipped(format!("model `{model_name}` of {what}: {}", e.causes())),
            }
        }
        if existed {
            if added > 0 {
                report.did(format!(
                    "{} added to {what}",
                    counted(added, "model", "models")
                ));
            }
        } else {
            report.did(format!("{what}: {}", counted(added, "model", "models")));
        }
    }
}

/// The provider `value` describes, created unless one by its name or id is
/// already here — and whether it was.
async fn restore_llm_provider(
    catalog: &Catalog,
    value: &Json,
) -> Result<(sc_llm::LlmProviderDef, bool)> {
    let backed_up = value
        .get("id")
        .and_then(Json::as_str)
        .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
        .map(sc_llm::LlmProviderDefId);
    let def = llm_provider_from_body(backed_up.unwrap_or_default(), value)?;
    if let Some(existing) = sc_llm::load_llm_provider_by_name(catalog, def.name.trim()).await? {
        return Ok((existing, true));
    }
    // The backup's id where it is free, so a restore into the installation it
    // came from gives the provider back its identity; a fresh one where some
    // other provider holds it, rather than overwriting that provider.
    let mut def = def;
    if sc_llm::load_llm_provider(catalog, def.id).await?.is_some() {
        def.id = sc_llm::LlmProviderDefId::new();
    }
    sc_llm::save_llm_provider(catalog, &def).await?;
    Ok((def, false))
}

/// One model of `provider`, added unless the provider has one by that name.
/// `Ok(false)` is "already there".
async fn restore_llm_model(
    catalog: &Catalog,
    provider: &sc_llm::LlmProviderDef,
    value: &Json,
    provider_existed: bool,
) -> Result<bool> {
    let backed_up = value
        .get("id")
        .and_then(Json::as_str)
        .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
        .map(sc_llm::LlmModelDefId);
    let mut model = llm_model_from_body(backed_up.unwrap_or_default(), provider.id, value)?;
    if sc_llm::load_llm_model_by_name(catalog, provider, &model.name)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    if sc_llm::load_llm_model(catalog, model.id).await?.is_some() {
        model.id = sc_llm::LlmModelDefId::new();
    }
    // Saving a default clears the flag on the provider's other models, and a
    // provider that was already here keeps the default it had.
    if provider_existed {
        model.is_default = false;
    }
    sc_llm::save_llm_model(catalog, &model).await?;
    Ok(true)
}

async fn restore_agents(
    catalog: &Catalog,
    apps: &AppMounts,
    entries: &Entries,
    report: &mut RestoreReport,
) {
    let document = match json_entry(entries, "agents.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("agents: {}", e.causes()));
            return;
        }
    };
    let services = match agents_of(apps) {
        Ok(services) => services,
        Err(e) => {
            report.skipped(format!("agents: {}", e.causes()));
            return;
        }
    };
    let registry = services.registry();
    for value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("an agent")
            .to_owned();
        let result = async {
            let id = value
                .get("id")
                .and_then(Json::as_str)
                .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
                .map(sc_agent::AgentId)
                .unwrap_or_else(sc_agent::AgentId::new);
            let agent = agent_from_body(id, &value)?;
            sc_agent::save_agent(catalog, registry.as_ref(), &agent).await?;
            Ok(String::new())
        }
        .await;
        report.outcome(&format!("agent `{name}`"), result);
    }
}

async fn restore_triggers(
    catalog: &Catalog,
    apps: &AppMounts,
    entries: &Entries,
    selection: &Selection,
    report: &mut RestoreReport,
) {
    let document = match json_entry(entries, "triggers.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("triggers: {}", e.causes()));
            return;
        }
    };
    let dispatcher = match triggers_of(apps) {
        Ok(dispatcher) => dispatcher,
        Err(e) => {
            report.skipped(format!("triggers: {}", e.causes()));
            return;
        }
    };
    let mut any = false;
    for value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("a trigger")
            .to_owned();
        let result = async {
            let id = value
                .get("id")
                .and_then(Json::as_str)
                .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
                .map(sc_action::TriggerId)
                .unwrap_or_else(sc_action::TriggerId::new);
            let trigger = trigger_from_body(id, &value)?;
            // The same rule the writer applied, applied again on the way in: a
            // trigger on a table this restore is not bringing has nothing to fire
            // on. The file may have been taken with the table included and
            // restored without it.
            if !selection.includes_trigger(trigger_table(&trigger)) {
                return Err(Error::invalid(
                    "it fires on a table this restore is not bringing",
                ));
            }
            sc_action::save_trigger(catalog, &dispatcher.registry(), &trigger).await?;
            Ok(trigger)
        }
        .await;
        any |= result.is_ok();
        let result = match result {
            Ok(trigger) => match value.get("workflow").filter(|w| !w.is_null()) {
                Some(steps) => restore_workflow(catalog, &dispatcher, &trigger, steps).await,
                None => Ok(String::new()),
            },
            Err(e) => Err(e),
        };
        report.outcome(&format!("trigger `{name}`"), result);
    }
    // One reload for the batch: the live set is what will fire, and it must match
    // the rows now rather than at the next restart.
    if any && let Err(e) = dispatcher.reload(catalog).await {
        report.skipped(format!(
            "the restored triggers are saved but not live yet: {}",
            e.causes()
        ));
    }
}

/// The SSL settings, checked against their declarations on the way in like any
/// other save. Not applied to the running listener: a certificate takes effect
/// when the server restarts, which is what the settings screen says too.
async fn restore_ssl(catalog: &Catalog, entries: &Entries) -> Result<String> {
    match restore_settings_section(catalog, entries, SSL_SECTION).await? {
        detail if detail.is_empty() => Ok("they take effect when the server restarts".to_owned()),
        detail => Ok(detail),
    }
}

/// One settings section's stored values, checked against their declarations on
/// the way in like any other save. Empty on success; a detail when there was
/// nothing to restore, or when keys this host pins were kept.
async fn restore_settings_section(
    catalog: &Catalog,
    entries: &Entries,
    section: &str,
) -> Result<String> {
    let document = json_entry(entries, &format!("settings/{section}.json"))?;
    let values = document
        .as_object()
        .ok_or_else(|| Error::invalid(format!("the {section} settings must be an object")))?;
    // Only the section's own keys. The entry is written by this system, but a
    // zip is a file somebody can edit: without the check, `settings/ssl.json` would
    // be a way to write *any* declared configuration value — including the ones no
    // settings form shows — under a heading that says certificates.
    let keys: Vec<&str> = sc_config::config_sections()
        .iter()
        .filter(|s| s.name == section)
        .flat_map(|s| s.fields.iter().map(|def| def.key()))
        .collect();
    // And not a key this host pins in its `feldspar.toml`: the file wins over
    // the table, so restoring one would either be refused (and take the rest of
    // the section with it) or store a value nothing reads.
    let pinned = sc_config::host_config_keys(catalog);
    let mut attrs = sc_types::Attrs::new();
    let mut kept = Vec::new();
    for (key, value) in values {
        if !keys.contains(&key.as_str()) {
            continue;
        }
        if pinned.contains(key) {
            kept.push(format!("`{key}`"));
            continue;
        }
        attrs.insert(key.clone(), value.clone());
    }
    let kept = match kept.as_slice() {
        [] => String::new(),
        keys => format!("{} kept from this host's feldspar.toml", keys.join(", ")),
    };
    if attrs.is_empty() {
        return Ok(if kept.is_empty() {
            "nothing to restore".to_owned()
        } else {
            kept
        });
    }
    sc_config::set_config_many(catalog, &attrs).await?;
    Ok(kept)
}

/// A workflow trigger's steps, as its first version here — unless it already
/// has steps, which are kept: the trigger was here before the restore, and its
/// current version is the one its runs and its admin are using.
///
/// Checked as a save from the editor is, so a workflow naming a table or an
/// action this server has not got is reported rather than stored broken.
async fn restore_workflow(
    catalog: &Catalog,
    dispatcher: &sc_action::TriggerDispatcher,
    trigger: &sc_action::Trigger,
    steps: &Json,
) -> Result<String> {
    if sc_workflow::current_workflow(catalog, trigger.id)
        .await?
        .is_some()
    {
        return Ok("its workflow was already here and was kept".to_owned());
    }
    let workflow = workflow_from_document(trigger.id, steps)?;
    sc_workflow::validate_workflow(
        catalog,
        &dispatcher.registry(),
        &workflow,
        trigger.channel.as_deref(),
    )
    .await
    .map_err(|e| {
        Error::invalid(format!(
            "its workflow could not be restored: {}",
            e.causes()
        ))
    })?;
    sc_workflow::save_workflow(catalog, &workflow, "restored from a backup", None).await?;
    Ok(format!(
        "with its workflow ({})",
        counted(workflow.steps.len(), "step", "steps")
    ))
}

/// Modules, each reinstalled from where its row says it came, then given back
/// the configuration and permissions it had. **One already installed here — by
/// package name — is kept as it is.** One reload for the batch, so what they
/// supply is live before anything below names it.
async fn restore_modules(
    apps: &AppMounts,
    catalog: &Catalog,
    entries: &Entries,
    report: &mut RestoreReport,
) {
    let document = match json_entry(entries, "modules.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("modules: {}", e.causes()));
            return;
        }
    };
    let services = match modules_of(apps) {
        Ok(services) => services,
        Err(e) => {
            report.skipped(format!("modules: {}", e.causes()));
            return;
        }
    };
    let mut any = false;
    for value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("a module")
            .to_owned();
        let result: Result<_> = async {
            let mut module = module_from_backup(&value)?;
            if sc_module::load_module_by_name(catalog, &module.name)
                .await?
                .is_some()
            {
                return Ok(None);
            }
            let package = services
                .install_package(module.language, module.source, &module.location)
                .await?;
            if sc_module::load_module(catalog, module.id).await?.is_some() {
                module.id = sc_module::ModuleId::new();
            }
            module.name = package.name;
            module.version = Some(package.version.clone());
            sc_module::save_module(catalog, &module).await?;
            Ok(Some(package.version))
        }
        .await;
        match result {
            Ok(None) => report.skipped(format!(
                "module `{name}` is already installed here; its settings and permissions were kept"
            )),
            Ok(Some(version)) => {
                any = true;
                report.did(format!("module `{name}` {version}, installed"));
            }
            Err(e) => report.skipped(format!("module `{name}`: {}", e.causes())),
        }
    }
    if any && let Err(e) = services.reload().await {
        report.skipped(format!(
            "the restored modules are installed but not loaded yet: {}",
            e.causes()
        ));
    }
}

/// Connections to other databases — the SQLite ones (`sqlite == true`), whose
/// file is in a file store, or every other kind — each saved and dialled. **One
/// already defined here by name is kept.** A connection that is saved but does
/// not answer is reported with the reason, as one the Connections screen can
/// repair.
async fn restore_db_connections(
    catalog: &Catalog,
    entries: &Entries,
    sqlite: bool,
    report: &mut RestoreReport,
) {
    let document = match json_entry(entries, "db-connections.json") {
        Ok(document) => document,
        Err(e) => {
            // Said once, by the first pass.
            if !sqlite {
                report.skipped(format!("database connections: {}", e.causes()));
            }
            return;
        }
    };
    let mut any = false;
    for value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("a database connection")
            .to_owned();
        let what = format!("database connection `{name}`");
        let id = value
            .get("id")
            .and_then(Json::as_str)
            .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
            .map(sc_catalog::DbConnectionId)
            .unwrap_or_default();
        // No stored row to merge a sentinel against: the password is the
        // backup's, as it was stored.
        let mut def = match db_connection_from_body(id, &value, None) {
            Ok(def) => def,
            Err(e) => {
                if !sqlite {
                    report.skipped(format!("{what}: {}", e.causes()));
                }
                continue;
            }
        };
        if def.is_sqlite() != sqlite {
            continue;
        }
        let result: Result<bool> = async {
            if sc_catalog::load_db_connection_by_name(catalog, def.name.trim())
                .await?
                .is_some()
            {
                return Ok(false);
            }
            if sc_catalog::load_db_connection(catalog, def.id)
                .await?
                .is_some()
            {
                def.id = sc_catalog::DbConnectionId::new();
            }
            sc_catalog::check_db_connection_saveable(catalog, &def).await?;
            sc_catalog::save_db_connection(catalog, &def).await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(false) => report.skipped(format!(
                "{what} is already defined here; its settings and password were kept"
            )),
            Ok(true) => {
                any = true;
                match sc_catalog::connect_db_connection(catalog, &def).await {
                    Ok(()) => report.did(what),
                    Err(e) => report.skipped(format!(
                        "{what} is restored but not connected: {}",
                        e.causes()
                    )),
                }
            }
            Err(e) => report.skipped(format!("{what}: {}", e.causes())),
        }
    }
    if any && let Err(e) = catalog.reload().await {
        report.skipped(format!(
            "the restored database connections' tables are not listed yet: {}",
            e.causes()
        ));
    }
}

/// Streams, each saved against the providers this server has — so one whose
/// provider came with a module is restored after the module is. **One already
/// defined here by name is kept.** One reload for the batch.
async fn restore_streams(
    catalog: &Catalog,
    apps: &AppMounts,
    entries: &Entries,
    report: &mut RestoreReport,
) {
    let document = match json_entry(entries, "streams.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("streams: {}", e.causes()));
            return;
        }
    };
    let services = match streams_of(apps) {
        Ok(services) => services,
        Err(e) => {
            report.skipped(format!("streams: {}", e.causes()));
            return;
        }
    };
    let registry = services.registry();
    let mut any = false;
    for value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("a stream")
            .to_owned();
        let result: Result<_> = async {
            let obj = value
                .as_object()
                .ok_or_else(|| Error::invalid("a stream must be an object"))?;
            let mut stream = stream_from_body(obj)?;
            if sc_stream::load_stream_by_name(catalog, stream.name.trim())
                .await?
                .is_some()
            {
                return Ok(false);
            }
            if sc_stream::load_stream(catalog, stream.id).await?.is_some() {
                stream.id = sc_stream::StreamId::new();
            }
            sc_stream::save_stream(catalog, &registry, &stream).await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(false) => report.skipped(format!(
                "stream `{name}` is already defined here; its settings were kept"
            )),
            Ok(true) => {
                any = true;
                report.did(format!("stream `{name}`"));
            }
            Err(e) => report.skipped(format!("stream `{name}`: {}", e.causes())),
        }
    }
    if any && let Err(e) = services.reload(catalog).await {
        report.skipped(format!(
            "the restored streams are saved but not running yet: {}",
            e.causes()
        ));
    }
}

/// A Saltcorn UI application's translations, a locale at a time. **A locale
/// this application already has a catalogue for keeps it**: those are the
/// translations its visitors are reading.
async fn restore_translations(
    catalog: &Catalog,
    entries: &Entries,
    key: &str,
    app: &sc_app::Application,
    report: &mut RestoreReport,
) {
    let path = format!("applications/{key}/translations.json");
    if !entries.contains_key(&path) || sc_app::app_source_from_config(&app.framework).is_ok() {
        return;
    }
    let result = async {
        let document = json_entry(entries, &path)?;
        let catalogues = document
            .as_object()
            .ok_or_else(|| Error::invalid("translations must be an object of locales"))?;
        use sc_app::CatalogStore;
        let store = sc_app::RowCatalogStore::new(app.id);
        let (mut restored, mut kept) = (Vec::new(), Vec::new());
        for (tag, messages) in catalogues {
            let locale = sc_i18n::Locale::parse(tag)?;
            if store.load(catalog, &locale).await?.is_some() {
                kept.push(tag.clone());
                continue;
            }
            store
                .save(catalog, &sc_i18n::Catalog::from_json(locale, messages)?)
                .await?;
            restored.push(tag.clone());
        }
        Ok(match (restored.is_empty(), kept.is_empty()) {
            (_, true) => restored.join(", "),
            (true, false) => format!("{} already here and kept", kept.join(", ")),
            (false, false) => format!(
                "{}; {} already here and kept",
                restored.join(", "),
                kept.join(", ")
            ),
        })
    }
    .await;
    report.outcome(&format!("translations of `{}`", app.subdomain), result);
}

/// Datasets, then models, then — when chosen — their fits, then workspaces.
///
/// **Ids are kept where they are free**, because a model names its datasets by
/// id and a dataset its base. A dataset already here *by name* is kept as it is,
/// and everything that named the backup's id is pointed at the one here
/// instead; a model already here by name is kept, and the backup's fits of it
/// are not added to it.
async fn restore_analytics(
    catalog: &Catalog,
    apps: &AppMounts,
    entries: &Entries,
    selection: &Selection,
    report: &mut RestoreReport,
) {
    // The backup's dataset ids, as the ids they have here.
    let mut datasets: BTreeMap<String, String> = BTreeMap::new();
    if entries.contains_key("analytics/datasets.json") {
        restore_datasets(catalog, entries, &mut datasets, report).await;
    }
    // The backup's model ids, for the models this restore created.
    let mut models: BTreeMap<String, uuid::Uuid> = BTreeMap::new();
    if entries.contains_key("analytics/models.json") {
        restore_models(catalog, apps, entries, &datasets, &mut models, report).await;
    }
    if selection.fits && entries.contains_key("analytics/fits.json") {
        restore_fits(catalog, entries, &models, report).await;
    }
    if entries.contains_key("analytics/workspaces.json") {
        restore_workspaces(catalog, entries, report).await;
    }
}

async fn restore_datasets(
    catalog: &Catalog,
    entries: &Entries,
    ids: &mut BTreeMap<String, String>,
    report: &mut RestoreReport,
) {
    let document = match json_entry(entries, "analytics/datasets.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("datasets: {}", e.causes()));
            return;
        }
    };
    // Bases first is the order the writer put them in, so one pass will do.
    for mut value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("a dataset")
            .to_owned();
        let backed_up = value
            .get("id")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned();
        let result: Result<_> = async {
            if let Some(existing) = sc_dataset::load_dataset_by_name(catalog, name.trim()).await? {
                ids.insert(backed_up.clone(), existing.id.to_string());
                return Ok(false);
            }
            if let Some(base) = value.pointer_mut("/base/dataset")
                && let Some(here) = base.as_str().and_then(|b| ids.get(b))
            {
                *base = Json::String(here.clone());
            }
            let mut id: sc_dataset::DatasetId = backed_up
                .parse()
                .unwrap_or_else(|_| sc_dataset::DatasetId::new());
            if sc_dataset::load_dataset(catalog, id).await?.is_some() {
                id = sc_dataset::DatasetId::new();
            }
            let def = crate::analytics::def_from_input(&value, id)?;
            sc_dataset::save_dataset(catalog, &def).await?;
            ids.insert(backed_up.clone(), id.to_string());
            Ok(true)
        }
        .await;
        match result {
            Ok(false) => report.skipped(format!(
                "dataset `{name}` is already defined here; its operations were kept"
            )),
            Ok(true) => report.did(format!("dataset `{name}`")),
            Err(e) => report.skipped(format!("dataset `{name}`: {}", e.causes())),
        }
    }
}

async fn restore_models(
    catalog: &Catalog,
    apps: &AppMounts,
    entries: &Entries,
    datasets: &BTreeMap<String, String>,
    created: &mut BTreeMap<String, uuid::Uuid>,
    report: &mut RestoreReport,
) {
    let document = match json_entry(entries, "analytics/models.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("models: {}", e.causes()));
            return;
        }
    };
    let services = match models_of(apps) {
        Ok(services) => services,
        Err(e) => {
            report.skipped(format!("models: {}", e.causes()));
            return;
        }
    };
    let registry = services.registry();
    for mut value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("a model")
            .to_owned();
        let backed_up = value
            .get("id")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned();
        let result: Result<_> = async {
            if sc_model::load_model_by_name(catalog, name.trim())
                .await?
                .is_some()
            {
                return Ok(false);
            }
            // Every dataset reference pointed at the dataset it is here.
            let remap = |slot: Option<&mut Json>| {
                if let Some(slot) = slot
                    && let Some(here) = slot.as_str().and_then(|d| datasets.get(d))
                {
                    *slot = Json::String(here.clone());
                }
            };
            remap(value.pointer_mut("/dataset/dataset_id"));
            if let Some(Json::Array(related)) = value.get_mut("related") {
                for item in related {
                    remap(item.get_mut("dataset_id"));
                }
            }
            let id = match uuid::Uuid::parse_str(&backed_up) {
                Ok(id)
                    if sc_model::load_model(catalog, sc_model::ModelId(id))
                        .await?
                        .is_none() =>
                {
                    id
                }
                _ => uuid::Uuid::new_v4(),
            };
            if let Json::Object(map) = &mut value {
                map.insert("id".to_owned(), Json::String(id.to_string()));
            }
            let obj = value
                .as_object()
                .ok_or_else(|| Error::invalid("a model must be an object"))?;
            let model = model_from_body(catalog, obj).await?;
            let shape = dataset_shape(&services, &model.dataset).await;
            sc_model::save_model(catalog, &registry, &model, shape.as_ref()).await?;
            if let Some(Json::Object(view_state)) = value.get("view_state")
                && !view_state.is_empty()
            {
                sc_model::patch_model_view_state(catalog, model.id, view_state).await?;
            }
            created.insert(backed_up.clone(), id);
            Ok(true)
        }
        .await;
        match result {
            Ok(false) => report.skipped(format!(
                "model `{name}` is already defined here; it and its fits were kept"
            )),
            Ok(true) => report.did(format!("model `{name}`")),
            Err(e) => report.skipped(format!("model `{name}`: {}", e.causes())),
        }
    }
}

/// The fits of the models this restore created, as the rows they were — each
/// with its output frames and its draws. A fit is all or nothing: its rows go
/// in one after another and a failure is reported against the fit.
async fn restore_fits(
    catalog: &Catalog,
    entries: &Entries,
    models: &BTreeMap<String, uuid::Uuid>,
    report: &mut RestoreReport,
) {
    let result = async {
        let document = json_entry(entries, "analytics/fits.json")?;
        let instances = catalog.require(sc_model::INSTANCES_TABLE)?;
        let outputs = catalog.require(sc_model::OUTPUTS_TABLE)?;
        let draws = catalog.require(sc_model::DRAWS_TABLE)?;
        let all_outputs = array_field(&document, "outputs");
        let mut restored = 0;
        for mut fit in array_field(&document, "instances") {
            let id = fit
                .get("id")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned();
            let Some(model) = fit
                .get("model")
                .and_then(Json::as_str)
                .and_then(|m| models.get(m))
            else {
                // A fit of a model this restore did not create: the model here
                // keeps the fits it has.
                continue;
            };
            if let Json::Object(map) = &mut fit {
                map.insert("model".to_owned(), Json::String(model.to_string()));
            }
            let fit_result = async {
                insert_row(catalog, &instances, &fit).await?;
                for output in all_outputs
                    .iter()
                    .filter(|o| o.get("instance").and_then(Json::as_str) == Some(id.as_str()))
                {
                    insert_row(catalog, &outputs, output).await?;
                }
                let path = format!("analytics/draws/{id}.json");
                if entries.contains_key(&path) {
                    let rows = array_list(&json_entry(entries, &path)?);
                    insert_rows_batched(catalog, &draws, &rows).await?;
                }
                Ok::<(), Error>(())
            }
            .await;
            match fit_result {
                Ok(()) => restored += 1,
                Err(e) => report.skipped(format!("fit {id}: {}", e.causes())),
            }
        }
        Ok(counted(restored, "fit", "fits"))
    }
    .await;
    report.outcome("model fits", result);
}

/// Rows into a table a batch at a time — a posterior's draws, which one
/// `INSERT` a row would take minutes over. Every row must carry the same
/// columns, which rows written by [`super::write`] do.
async fn insert_rows_batched(catalog: &Catalog, table: &Table, values: &[Json]) -> Result<()> {
    const BATCH: usize = 250;
    for chunk in values.chunks(BATCH) {
        let Some(first) = chunk.first().and_then(Json::as_object) else {
            continue;
        };
        let columns: Vec<String> = first
            .keys()
            .filter(|c| table.field(c).is_some_and(|f| !f.is_calc()))
            .cloned()
            .collect();
        let mut rows = Vec::with_capacity(chunk.len());
        for value in chunk {
            let obj = value
                .as_object()
                .ok_or_else(|| Error::invalid("a row must be an object"))?;
            let mut row = Vec::with_capacity(columns.len());
            for column in &columns {
                let json = obj.get(column).unwrap_or(&Json::Null);
                row.push(Expr::lit(rows::column_value(table, column, json)?));
            }
            rows.push(row);
        }
        let insert = Insert {
            rows,
            ..Insert::row(table.name.clone(), columns, Vec::new())
        };
        catalog
            .primary()
            .query(&Statement::from(insert))
            .await?
            .try_collect()
            .await?;
    }
    Ok(())
}

/// Analytics workspaces. One already here — by id — is kept: workspace names
/// are not unique, so the id is the only thing that says it is the same one.
async fn restore_workspaces(catalog: &Catalog, entries: &Entries, report: &mut RestoreReport) {
    let document = match json_entry(entries, "analytics/workspaces.json") {
        Ok(document) => document,
        Err(e) => {
            report.skipped(format!("workspaces: {}", e.causes()));
            return;
        }
    };
    for value in array_list(&document) {
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("a workspace")
            .to_owned();
        let result: Result<_> = async {
            let kind = sc_analytics::WorkspaceKind::parse(
                value.get("kind").and_then(Json::as_str).unwrap_or_default(),
            )?;
            let mut workspace = sc_analytics::Workspace::new(
                name.clone(),
                kind,
                value
                    .get("created_by")
                    .and_then(Json::as_str)
                    .and_then(|raw| uuid::Uuid::parse_str(raw).ok()),
            );
            if let Some(id) = value
                .get("id")
                .and_then(Json::as_str)
                .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
            {
                if sc_analytics::load_workspace(catalog, sc_analytics::WorkspaceId(id))
                    .await?
                    .is_some()
                {
                    return Ok(false);
                }
                workspace.id = sc_analytics::WorkspaceId(id);
            }
            workspace.state = value.get("state").cloned().unwrap_or(Json::Null);
            if workspace.state.is_null() {
                workspace.state = Json::Object(Map::new());
            }
            sc_analytics::create_workspace(catalog, &workspace).await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(false) => report.skipped(format!("workspace `{name}` is already here and was kept")),
            Ok(true) => report.did(format!("workspace `{name}`")),
            Err(e) => report.skipped(format!("workspace `{name}`: {}", e.causes())),
        }
    }
}

// --- reading the file ----------------------------------------------------------

/// The entries to restore from: the zip's own, or — for a **Saltcorn 1** backup —
/// what [`super::v1::convert`] makes of them.
///
/// The one place the two kinds of file meet. Everything downstream reads this
/// system's layout and cannot tell which it was handed, which is the point: a v1
/// import is inspected, chosen from, restored and reported by the same code as
/// any other backup.
fn read_backup(archive: &[u8]) -> Result<Entries> {
    let entries = read_zip(archive)?;
    if !entries.contains_key(MANIFEST_FILE) && super::v1::is_v1_backup(&entries) {
        return super::v1::convert(&entries);
    }
    Ok(entries)
}

/// Expand a zip into path → bytes, refusing anything that is not a zip.
fn read_zip(archive: &[u8]) -> Result<Entries> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|e| {
        Error::invalid(format!(
            "this file is not a zip archive, so it is not a Saltcorn backup: {e}"
        ))
    })?;
    let mut out = Entries::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| {
            Error::invalid(format!("the backup's entry {index} cannot be read: {e}"))
        })?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let mut bytes = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| Error::invalid(format!("`{name}` cannot be read from the backup: {e}")))?;
        out.insert(name, bytes);
    }
    Ok(out)
}

/// The manifest, with the format and version it declares checked.
///
/// Refused by name rather than by the confusing absence of everything else: a zip
/// of holiday photographs and a backup from a future version are different
/// mistakes, and both deserve to be told apart from "nothing was restored".
fn manifest_of(entries: &Entries) -> Result<Map<String, Json>> {
    let manifest = json_entry(entries, MANIFEST_FILE).map_err(|_| {
        Error::invalid(format!(
            "this zip has no `{MANIFEST_FILE}` and is not a Saltcorn 1 backup either,              so it is not a backup this server can read"
        ))
    })?;
    let obj = manifest
        .as_object()
        .ok_or_else(|| Error::invalid(format!("`{MANIFEST_FILE}` is not an object")))?;
    match obj.get("format").and_then(Json::as_str) {
        Some(format) if format == super::FORMAT => {}
        Some(other) => {
            return Err(Error::invalid(format!(
                "this is a `{other}` archive, not a Feldspar backup"
            )));
        }
        None => return Err(Error::invalid("this backup does not say what format it is")),
    }
    match obj.get("version").and_then(Json::as_i64) {
        Some(version) if version == super::FORMAT_VERSION => {}
        Some(other) => {
            return Err(Error::invalid(format!(
                "this backup is in format version {other}; this server reads version {}",
                super::FORMAT_VERSION
            )));
        }
        None => {
            return Err(Error::invalid(
                "this backup does not say what version it is",
            ));
        }
    }
    Ok(obj.clone())
}

/// One entry, parsed as JSON.
fn json_entry(entries: &Entries, path: &str) -> Result<Json> {
    let bytes = entries
        .get(path)
        .ok_or_else(|| Error::invalid(format!("the backup has no `{path}`")))?;
    serde_json::from_slice(bytes)
        .map_err(|e| Error::invalid(format!("`{path}` in the backup is not valid JSON: {e}")))
}

/// A table's rows entry — absent means "no rows were backed up", which is not an
/// error: a table can be backed up with its metadata alone.
fn table_rows(entries: &Entries, path: &str) -> Result<Vec<Json>> {
    if !entries.contains_key(path) {
        return Ok(Vec::new());
    }
    match json_entry(entries, path)? {
        Json::Array(values) => Ok(values),
        _ => Err(Error::invalid(format!("`{path}` must be an array of rows"))),
    }
}

/// A named array of objects in a document, or nothing.
fn array_field(document: &Json, key: &str) -> Vec<Json> {
    document
        .get(key)
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A document that is itself an array.
fn array_list(document: &Json) -> Vec<Json> {
    document.as_array().cloned().unwrap_or_default()
}

/// Every row of a table as JSON — the restore's own read, for the accounts that
/// are already here.
async fn rows_of(catalog: &Catalog, table: &Table) -> Result<Vec<Json>> {
    let json = rows::list_rows(catalog, table).await?;
    Ok(match json {
        Json::Array(values) => values,
        _ => Vec::new(),
    })
}
