//! Tables a module **declares** (TODO-imap M4): the module's own tables,
//! created by this server when the module loads.
//!
//! A module that keeps state — a mail module's accounts, folders and messages —
//! needs tables, and an admin should not have to build them by hand from a
//! README, nor map a module's fields onto columns of their own. So the plugin
//! object declares them, as data:
//!
//! ```js
//! tables: [{
//!   name: "mail_folders", label: "Mail folders",
//!   fields: [
//!     { name: "account", references: "mail_accounts", required: true },
//!     { name: "path", type: "text", required: true },
//!     { name: "uidvalidity", type: "int" },
//!   ],
//!   unique: [["account", "path"]],
//!   indexes: [["uidvalidity"]],
//!   ownership: "accountⱵowner === user.id",
//!   min_role_read: 1, min_role_write: 1,
//! }]
//! ```
//!
//! and [`sync`] makes the database agree, **through the schema editor**
//! (`sc_api::schema_edit`) rather than SQL, so the result is an ordinary table:
//! triggers, live streams, REST, GraphQL, ownership and backups treat it like any
//! other. The rules:
//!
//! - **Every table gets an `id` primary key**, an `int` that numbers itself, and
//!   the declaration does not mention it. One convention for every module table
//!   means a `references` between them never has to say which column.
//! - **Missing tables are created and missing fields are added; nothing is ever
//!   dropped or changed.** Not on upgrade, not on uninstall. A field an admin
//!   added to a module table is theirs and is left alone; a declared field whose
//!   type no longer matches what the database has is an **issue** on the
//!   module's card, and stays as it is.
//! - **The settings are the creation's.** `label`, `ownership`, `min_role_read`
//!   and `min_role_write` are applied when the table is created; after that they
//!   belong to the admin, who may tighten or widen them, and a module upgrade
//!   does not undo that.
//! - **A field added by an upgrade is nullable**, whatever it declares: the
//!   table has rows already, and nothing can say what they should hold.
//! - A `file_store` names a file store. If none of that name exists, a local
//!   store is created for it in the data directory (`sc_files::suggest_local_dir`),
//!   and an admin who wants the files elsewhere creates a store of that name
//!   first, or changes the created one (TODO-imap M4.5).
//!
//! Running it twice is a no-op, which is what lets it run on **every** load:
//! install, upgrade, a configuration change and a restart are one path.

use std::collections::BTreeSet;

use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_catalog::{
    Attrs, Catalog, ConstraintKind, DataField, DataFieldKind, FILE_STORES_TABLE, FieldId,
    FileStoreId, ModuleTables, Table, TableConstraint, TableId, connect_file_store_def,
    load_file_store_by_name, save_file_store,
};
use sc_error::{Error, Result};
use serde::Deserialize;
use serde_json::Value as Json;

/// The primary key every module table is given, and that no declaration names.
pub const MODULE_TABLE_KEY: &str = "id";

/// One declared table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDecl {
    /// The table's name — global, like every table's, so a module should prefix
    /// it with what it is about (`mail_`).
    pub name: String,
    /// The human label, applied when the table is created.
    #[serde(default)]
    pub label: Option<String>,
    /// A description, applied when the table is created.
    #[serde(default)]
    pub description: Option<String>,
    /// The fields, beside the `id` key every module table has.
    #[serde(default)]
    pub fields: Vec<FieldDecl>,
    /// Jointly-unique field lists — `[["account", "path"]]`.
    #[serde(default)]
    pub unique: Vec<Vec<String>>,
    /// Indexes, each a field list — `[["thread_key"]]` — for what a client
    /// searches or groups by (TODO-imap M9.4). Added when missing, on creation
    /// and on upgrade alike, and never dropped.
    #[serde(default)]
    pub indexes: Vec<Vec<String>>,
    /// The ownership formula (§7.3), applied when the table is created.
    #[serde(default)]
    pub ownership: Option<String>,
    /// The least role that may read rows, applied when the table is created.
    #[serde(default)]
    pub min_role_read: Option<u8>,
    /// The least role that may write rows, applied when the table is created.
    #[serde(default)]
    pub min_role_write: Option<u8>,
}

/// One declared field.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDecl {
    /// The column name.
    pub name: String,
    /// A basic or rich type name (`text`, `int`, `bool`, `timestamp`, `json`,
    /// …). Omitted for a `references`, a `secret` and a `file_store` field,
    /// whose storage is decided by what they are.
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
    /// The human label.
    #[serde(default)]
    pub label: Option<String>,
    /// A description.
    #[serde(default)]
    pub description: Option<String>,
    /// `NOT NULL` — on a table being created. A field an upgrade adds is
    /// nullable whatever this says.
    #[serde(default)]
    pub required: bool,
    /// A `UNIQUE` constraint on this field alone.
    #[serde(default)]
    pub unique: bool,
    /// A key to this table's primary key — another module table, or `users`.
    #[serde(default)]
    pub references: Option<String>,
    /// A [`Secret`](DataFieldKind::Secret): stored encrypted, write-only to
    /// clients, readable by server-side code.
    #[serde(default)]
    pub secret: bool,
    /// A file field, in the file store of this name.
    #[serde(default)]
    pub file_store: Option<String>,
    /// The folder within the store a file field's files live under.
    #[serde(default)]
    pub file_folder: Option<String>,
    /// The value an insert that leaves the field out gets: a JSON value, or
    /// a JavaScript expression in a string starting with `=`. Only for a
    /// `required` field.
    #[serde(default)]
    pub default: Option<Json>,
}

impl FieldDecl {
    fn kind(&self) -> DataFieldKind {
        if let Some(target) = &self.references {
            DataFieldKind::Key {
                target_table: TableId(target.trim().to_owned()),
                target_field: FieldId(String::new()),
                summary_field: None,
            }
        } else if self.secret {
            DataFieldKind::Secret
        } else if let Some(store) = &self.file_store {
            DataFieldKind::File {
                store: FileStoreId(store.trim().to_owned()),
                folder: self
                    .file_folder
                    .as_deref()
                    .map(|f| f.trim().trim_matches('/').to_owned())
                    .filter(|f| !f.is_empty()),
                mime_allow: Vec::new(),
                on_delete: sc_catalog::FileOnDelete::default(),
                on_conflict: sc_catalog::FileOnConflict::default(),
            }
        } else {
            DataFieldKind::Plain
        }
    }

    /// The default as the JavaScript expression the schema editor stores.
    fn default_expression(&self) -> Option<String> {
        match self.default.as_ref()? {
            Json::Null => None,
            Json::String(s) if s.starts_with('=') => Some(s[1..].trim().to_owned()),
            other => Some(other.to_string()),
        }
    }

    /// The field as the schema editor creates it — on a new table when
    /// `creating`, else added to one that has rows.
    fn spec(&self, creating: bool) -> FieldSpec {
        let required = creating && self.required;
        FieldSpec {
            name: self.name.trim().to_owned(),
            type_name: self.type_name.clone().unwrap_or_default().trim().to_owned(),
            label: self.label.clone().unwrap_or_default(),
            description: self.description.clone().unwrap_or_default(),
            required,
            unique: self.unique,
            primary_key: false,
            kind: self.kind(),
            attributes: Attrs::new(),
            default_expression: required.then(|| self.default_expression()).flatten(),
        }
    }

    /// Why this declaration contradicts itself, if it does.
    fn check(&self, table: &str) -> Result<()> {
        let name = self.name.trim();
        let what = [
            self.references.is_some(),
            self.secret,
            self.file_store.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if what > 1 {
            return Err(Error::invalid(format!(
                "the field `{table}.{name}` is declared as more than one of `references`, \
                 `secret` and `file_store`"
            )));
        }
        if what == 0
            && self
                .type_name
                .as_deref()
                .is_none_or(|t| t.trim().is_empty())
        {
            return Err(Error::invalid(format!(
                "the field `{table}.{name}` needs a `type`"
            )));
        }
        if self.file_folder.is_some() && self.file_store.is_none() {
            return Err(Error::invalid(format!(
                "the field `{table}.{name}` has a `file_folder` but no `file_store`"
            )));
        }
        if self.default.as_ref().is_some_and(|d| !d.is_null()) && !self.required {
            return Err(Error::invalid(format!(
                "the field `{table}.{name}` has a `default` but is not `required`: a field \
                 that may be null is already left null by an insert that omits it"
            )));
        }
        if name == MODULE_TABLE_KEY {
            return Err(Error::invalid(format!(
                "the field `{table}.{MODULE_TABLE_KEY}` is declared, but every module table is \
                 given an `{MODULE_TABLE_KEY}` primary key by the server and must not declare one"
            )));
        }
        Ok(())
    }
}

/// Read a manifest's `tables` into declarations, each one that does not read
/// costing that table and an issue — the rest of the module still loads.
pub fn parse_declarations(raw: &[Json]) -> (Vec<TableDecl>, Vec<String>) {
    let mut decls: Vec<TableDecl> = Vec::new();
    let mut issues = Vec::new();
    for item in raw {
        let named = item.get("name").and_then(Json::as_str).unwrap_or("?");
        match serde_json::from_value::<TableDecl>(item.clone()) {
            Err(e) => issues.push(format!(
                "its table `{named}` is not declared correctly: {e}"
            )),
            Ok(decl) => match check_table(&decl, &decls) {
                Err(e) => issues.push(format!("its table `{}` is not available: {e}", decl.name)),
                Ok(()) => decls.push(decl),
            },
        }
    }
    (decls, issues)
}

/// Why a declaration cannot be applied, judged before anything is.
fn check_table(decl: &TableDecl, earlier: &[TableDecl]) -> Result<()> {
    let name = decl.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a table needs a `name`"));
    }
    if name.starts_with("_fd_") || name == sc_auth_users() {
        return Err(Error::invalid(format!(
            "`{name}` is one of this server's own tables"
        )));
    }
    if earlier.iter().any(|d| d.name.trim() == name) {
        return Err(Error::invalid(format!("`{name}` is declared twice")));
    }
    let mut seen = BTreeSet::new();
    for field in &decl.fields {
        field.check(name)?;
        if !seen.insert(field.name.trim()) {
            return Err(Error::invalid(format!(
                "the field `{name}.{}` is declared twice",
                field.name.trim()
            )));
        }
    }
    for (key, lists) in [("unique", &decl.unique), ("indexes", &decl.indexes)] {
        for fields in lists {
            if fields.is_empty() {
                return Err(Error::invalid(format!("an entry of `{key}` names no fields")));
            }
            for field in fields {
                if field != MODULE_TABLE_KEY && !seen.contains(field.trim()) {
                    return Err(Error::invalid(format!(
                        "`{key}` names `{field}`, which is not one of its fields"
                    )));
                }
            }
        }
    }
    for fields in &decl.indexes {
        if let Some(secret) = fields.iter().find(|f| {
            decl.fields
                .iter()
                .any(|d| d.name.trim() == f.trim() && d.secret)
        }) {
            return Err(Error::invalid(format!(
                "`indexes` names `{secret}`, which is a secret and cannot be indexed"
            )));
        }
    }
    Ok(())
}

/// The users table's name: `references: "users"` is how a module table refers
/// to a person, and a module must not declare a table called that.
fn sc_auth_users() -> &'static str {
    "users"
}

/// The claims every module with tables makes, for
/// [`Catalog::set_module_tables`]: which tables, and which of their fields, the
/// module's code depends on.
pub fn claims<'a>(modules: impl IntoIterator<Item = (&'a str, &'a [TableDecl])>) -> ModuleTables {
    let mut tables = ModuleTables::new();
    for (module, decls) in modules {
        for decl in decls {
            let mut fields = vec![MODULE_TABLE_KEY.to_owned()];
            fields.extend(decl.fields.iter().map(|f| f.name.trim().to_owned()));
            tables.claim(decl.name.trim(), module, fields);
        }
    }
    tables
}

/// What [`sync`] did, and what it found but left alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Tables created.
    pub tables_created: Vec<String>,
    /// Fields added to tables that were already there, as `table.field`.
    pub fields_added: Vec<String>,
    /// Unique constraints and indexes added, as `table.constraint`.
    pub constraints_added: Vec<String>,
    /// File stores created for a declared file field.
    pub stores_created: Vec<String>,
    /// What is wrong and was not changed — a declared field whose type the
    /// database disagrees with, a table another module declared, a batch the
    /// schema editor refused. Each one belongs on the module's card.
    pub issues: Vec<String>,
}

impl SyncReport {
    /// Whether nothing was changed.
    pub fn is_noop(&self) -> bool {
        self.tables_created.is_empty()
            && self.fields_added.is_empty()
            && self.constraints_added.is_empty()
            && self.stores_created.is_empty()
    }
}

/// Make the database hold `module`'s declared tables: create the ones that are
/// missing, add the fields and unique constraints that are missing, and report
/// the rest. One schema-editor batch, so a refused operation applies nothing.
///
/// `others` is what the other installed modules claim already: a table one of
/// them declared is not touched, and the clash is this module's issue.
pub async fn sync(
    catalog: &Catalog,
    module: &str,
    decls: &[TableDecl],
    others: &ModuleTables,
) -> SyncReport {
    let mut report = SyncReport::default();
    if decls.is_empty() {
        return report;
    }
    let mut operations = Vec::new();
    for decl in decls {
        let name = decl.name.trim();
        if let Some(owner) = others.module_of(name).filter(|o| *o != module) {
            report.issues.push(format!(
                "its table `{name}` is not available: the module {owner} already declares a \
                 table of that name"
            ));
            continue;
        }
        match catalog.get(name) {
            Err(e) => report
                .issues
                .push(format!("its table `{name}` could not be read: {e}")),
            Ok(None) => {
                operations.push(create_table(decl));
                report.tables_created.push(name.to_owned());
                for fields in &decl.unique {
                    operations.push(unique(name, fields));
                    report
                        .constraints_added
                        .push(format!("{name}.{}", unique_name(name, fields)));
                }
                for fields in &decl.indexes {
                    operations.push(index(name, fields));
                    report
                        .constraints_added
                        .push(format!("{name}.{}", index_name(name, fields)));
                }
            }
            Ok(Some(table)) if table.provider().is_some() || table.is_metadata() => {
                report.issues.push(format!(
                    "its table `{name}` is not available: a table of that name already exists \
                     and is not one a module can add fields to"
                ));
            }
            Ok(Some(table)) => upgrade(&table, decl, &mut operations, &mut report),
        }
    }
    if let Err(e) = create_file_stores(catalog, decls, &mut report).await {
        report.issues.push(format!(
            "the file store its tables need could not be created: {}",
            sc_error::format_chain(&e)
        ));
    }
    if operations.is_empty() {
        return report;
    }
    if let Err(e) = schema_edit::apply(catalog, &operations, &ApplyOptions::default()).await {
        report.issues.push(format!(
            "its tables could not be created or brought up to date, and nothing was changed: {}",
            sc_error::format_chain(&e)
        ));
        report.tables_created.clear();
        report.fields_added.clear();
        report.constraints_added.clear();
    }
    report
}

fn create_table(decl: &TableDecl) -> Operation {
    let mut fields = vec![FieldSpec {
        name: MODULE_TABLE_KEY.to_owned(),
        type_name: "int".to_owned(),
        primary_key: true,
        ..FieldSpec::default()
    }];
    fields.extend(decl.fields.iter().map(|f| f.spec(true)));
    Operation::CreateTable {
        name: decl.name.trim().to_owned(),
        database: String::new(),
        settings: TableSettings {
            label: decl.label.clone(),
            description: decl.description.clone(),
            min_role_read: decl.min_role_read,
            min_role_write: decl.min_role_write,
            ownership_formula: decl.ownership.clone(),
            rls_enabled: None,
            sync_enabled: None,
            history_enabled: None,
            history_min_role: None,
        },
        fields,
    }
}

fn unique_name(table: &str, fields: &[String]) -> String {
    TableConstraint::derived_name(table, &unique_kind(fields), "")
}

fn unique_kind(fields: &[String]) -> ConstraintKind {
    ConstraintKind::Unique {
        fields: fields.iter().map(|f| f.trim().to_owned()).collect(),
    }
}

fn unique(table: &str, fields: &[String]) -> Operation {
    Operation::AddConstraint {
        table: table.to_owned(),
        given_name: String::new(),
        constraint: TableConstraint::new(unique_name(table, fields), unique_kind(fields)),
    }
}

fn index_kind(fields: &[String]) -> ConstraintKind {
    ConstraintKind::Index {
        fields: fields.iter().map(|f| f.trim().to_owned()).collect(),
        expression: None,
        method: "btree".to_owned(),
    }
}

fn index_name(table: &str, fields: &[String]) -> String {
    TableConstraint::derived_name(table, &index_kind(fields), "")
}

fn index(table: &str, fields: &[String]) -> Operation {
    Operation::AddConstraint {
        table: table.to_owned(),
        given_name: String::new(),
        constraint: TableConstraint::new(index_name(table, fields), index_kind(fields)),
    }
}

/// The operations that bring an existing table up to its declaration, and the
/// mismatches that are reported instead of changed.
fn upgrade(
    table: &Table,
    decl: &TableDecl,
    operations: &mut Vec<Operation>,
    report: &mut SyncReport,
) {
    let name = table.name.as_str();
    for field in &decl.fields {
        let field_name = field.name.trim();
        match table.field(field_name) {
            None => {
                operations.push(Operation::AddField {
                    table: name.to_owned(),
                    field: field.spec(false),
                });
                report.fields_added.push(format!("{name}.{field_name}"));
            }
            Some(existing) => {
                if let Some(problem) = mismatch(existing, field) {
                    report.issues.push(format!(
                        "the field `{name}.{field_name}` is declared as {problem}; it was left \
                         as it is, and the module may not work with it"
                    ));
                }
            }
        }
    }
    for fields in &decl.unique {
        let wanted: BTreeSet<&str> = fields.iter().map(|f| f.trim()).collect();
        let present = table.constraints.iter().any(|c| match &c.kind {
            ConstraintKind::Unique { fields } => {
                fields.iter().map(String::as_str).collect::<BTreeSet<_>>() == wanted
            }
            _ => false,
        });
        // A single field's `unique` is a column property and reads back as
        // one, which this list does not hold.
        let single = wanted.len() == 1
            && wanted
                .iter()
                .all(|f| table.field(f).is_some_and(|f| f.unique));
        if !present && !single {
            operations.push(unique(name, fields));
            report
                .constraints_added
                .push(format!("{name}.{}", unique_name(name, fields)));
        }
    }
    for fields in &decl.indexes {
        let wanted: Vec<&str> = fields.iter().map(|f| f.trim()).collect();
        // Any index over the same fields in the same order serves, whoever
        // made it: one the admin added by hand is not made twice.
        let present = table.constraints.iter().any(|c| match &c.kind {
            ConstraintKind::Index { fields, .. } => {
                fields.iter().map(String::as_str).eq(wanted.iter().copied())
            }
            _ => false,
        });
        if !present {
            operations.push(index(name, fields));
            report
                .constraints_added
                .push(format!("{name}.{}", index_name(name, fields)));
        }
    }
}

/// How a declared field and the field the database has disagree, in words —
/// `None` when they agree on what the module depends on: its type and what it
/// refers to. Labels, descriptions and nullability are the admin's.
fn mismatch(existing: &DataField, decl: &FieldDecl) -> Option<String> {
    let have = describe(&existing.kind, Some(existing.base.type_.name()));
    let want = match decl.kind() {
        DataFieldKind::Plain => {
            let type_name = decl.type_name.as_deref().unwrap_or_default().trim();
            match schema_edit::resolve_field_type(type_name) {
                Ok((storage, rich)) => {
                    let wanted = rich.unwrap_or_else(|| storage.name().to_owned());
                    if matches!(existing.kind, DataFieldKind::Plain)
                        && existing.base.type_.name() == wanted
                    {
                        return None;
                    }
                    format!("`{wanted}`")
                }
                Err(e) => return Some(format!("an unknown type ({e})")),
            }
        }
        DataFieldKind::Key { target_table, .. } => match &existing.kind {
            DataFieldKind::Key {
                target_table: have, ..
            } if *have == target_table => return None,
            _ => format!("a reference to `{}`", target_table.0),
        },
        DataFieldKind::Secret if matches!(existing.kind, DataFieldKind::Secret) => return None,
        DataFieldKind::File { store, .. } => match &existing.kind {
            DataFieldKind::File { store: have, .. } if *have == store => return None,
            _ => format!("a file in the store `{}`", store.0),
        },
        other => describe(&other, None),
    };
    Some(format!("{want} but is {have}"))
}

fn describe(kind: &DataFieldKind, type_name: Option<&str>) -> String {
    match kind {
        DataFieldKind::Plain => format!("`{}`", type_name.unwrap_or("?")),
        DataFieldKind::Key { target_table, .. } => format!("a reference to `{}`", target_table.0),
        DataFieldKind::File { store, .. } => format!("a file in the store `{}`", store.0),
        DataFieldKind::Calc { .. } => "a calculated field".to_owned(),
        DataFieldKind::Secret => "a secret".to_owned(),
    }
}

/// Create a local store for every declared file field whose store does not
/// exist (TODO-imap M4.5), in the data directory under the store's name.
async fn create_file_stores(
    catalog: &Catalog,
    decls: &[TableDecl],
    report: &mut SyncReport,
) -> Result<()> {
    let wanted: BTreeSet<&str> = decls
        .iter()
        .flat_map(|d| d.fields.iter())
        .filter_map(|f| f.file_store.as_deref().map(str::trim))
        .collect();
    for name in wanted {
        if catalog.file_store(name)?.is_some() {
            continue;
        }
        if catalog.get(FILE_STORES_TABLE)?.is_none() {
            return Err(Error::config(format!(
                "there is no file store `{name}`, and this server keeps no file stores to add \
                 it to"
            )));
        }
        if load_file_store_by_name(catalog, name).await?.is_some() {
            // Defined but not connected: the store's own page says why, and
            // replacing the admin's definition would be the wrong fix.
            continue;
        }
        let def = local_store(name)?;
        save_file_store(catalog, &def).await?;
        connect_file_store_def(catalog, &def)?;
        report.stores_created.push(name.to_owned());
    }
    Ok(())
}

/// The definition of the local store a module's file field gets when there is
/// none of its name: a directory under the data directory named after it,
/// created if missing.
pub fn local_store(name: &str) -> Result<sc_files::FileStoreDef> {
    let mut def = sc_files::FileStoreDef::new(name, sc_files::LOCAL_BACKEND);
    def.description = "Created for a module's file fields".to_owned();
    def.config.insert(
        sc_files::CFG_PATH.to_owned(),
        Json::String(sc_files::suggest_local_dir(name)?.display().to_string()),
    );
    def.config
        .insert(sc_files::CFG_CREATE.to_owned(), Json::Bool(true));
    Ok(def)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(raw: Json) -> (Vec<TableDecl>, Vec<String>) {
        parse_declarations(raw.as_array().unwrap())
    }

    #[test]
    fn a_declaration_reads_and_a_broken_one_costs_only_itself() {
        let (decls, issues) = parse(json!([
            { "name": "mail_accounts", "fields": [
                { "name": "owner", "references": "users" },
                { "name": "imap_password", "secret": true },
                { "name": "enabled", "type": "bool", "required": true, "default": true },
            ], "ownership": "owner === user.id" },
            { "name": "typo", "fields": [{ "name": "x", "typ": "text" }] },
            { "name": "keyed", "fields": [{ "name": "id", "type": "int" }] },
            { "name": "both", "fields": [{ "name": "f", "secret": true, "references": "users" }] },
            { "name": "untyped", "fields": [{ "name": "f" }] },
            { "name": "_fd_mine", "fields": [] },
            { "name": "mail_accounts", "fields": [] },
            { "name": "defaulted", "fields": [{ "name": "n", "type": "int", "default": 3 }] },
            { "name": "u", "fields": [{ "name": "a", "type": "int" }], "unique": [["a", "b"]] },
            { "name": "i", "fields": [{ "name": "a", "type": "int" }], "indexes": [["c"]] },
            { "name": "s", "fields": [{ "name": "pw", "secret": true }], "indexes": [["pw"]] },
            { "name": "e", "fields": [{ "name": "a", "type": "int" }], "indexes": [[]] },
        ]));
        assert_eq!(decls.len(), 1, "{issues:?}");
        assert_eq!(decls[0].fields.len(), 3);
        let all = issues.join("\n");
        for needle in [
            "`typo` is not declared correctly",
            "every module table is given an `id`",
            "more than one of",
            "`untyped.f` needs a `type`",
            "`_fd_mine` is one of this server's own tables",
            "`mail_accounts` is declared twice",
            "has a `default` but is not `required`",
            "`unique` names `b`",
            "`indexes` names `c`",
            "`indexes` names `pw`, which is a secret",
            "an entry of `indexes` names no fields",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in:\n{all}");
        }
    }

    #[test]
    fn a_field_added_by_an_upgrade_is_nullable_and_a_default_is_an_expression() {
        let field: FieldDecl = serde_json::from_value(json!({
            "name": "folders", "type": "json", "required": true, "default": ["INBOX"]
        }))
        .unwrap();
        let created = field.spec(true);
        assert!(created.required);
        assert_eq!(created.default_expression.as_deref(), Some(r#"["INBOX"]"#));
        let added = field.spec(false);
        assert!(!added.required);
        assert_eq!(added.default_expression, None);

        let field: FieldDecl = serde_json::from_value(json!({
            "name": "n", "type": "int", "required": true, "default": "=1 + 1"
        }))
        .unwrap();
        assert_eq!(field.default_expression().as_deref(), Some("1 + 1"));
    }

    #[test]
    fn a_mismatch_names_both_types_and_a_match_is_silent() {
        use sc_types::{BasicType, TypeRef};
        let text = DataField::plain("uid", TypeRef::Basic(BasicType::Text));
        let decl: FieldDecl =
            serde_json::from_value(json!({ "name": "uid", "type": "int" })).unwrap();
        let problem = mismatch(&text, &decl).unwrap();
        assert!(problem.contains("`int` but is `text`"), "{problem}");

        let int = DataField::plain("uid", TypeRef::Basic(BasicType::Int));
        assert_eq!(mismatch(&int, &decl), None);

        let secret: FieldDecl =
            serde_json::from_value(json!({ "name": "pw", "secret": true })).unwrap();
        let problem = mismatch(&text, &secret).unwrap();
        assert!(problem.contains("a secret but is `text`"), "{problem}");
    }

    #[test]
    fn the_claims_include_the_key_and_every_declared_field() {
        let (decls, _) =
            parse(json!([{ "name": "t", "fields": [{ "name": "a", "type": "int" }] }]));
        let tables = claims([("@x/y", decls.as_slice())]);
        assert!(tables.declares_field("t", "id"));
        assert!(tables.declares_field("t", "a"));
        assert_eq!(tables.module_of("t"), Some("@x/y"));
    }
}
