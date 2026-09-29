//! The rule for **changing the schema**: creating and altering tables, adding,
//! altering and dropping fields, dropping tables, and writing a table's access
//! rules (technical design §3.3, §7.3, §13.1; TODO Phase 7).
//!
//! ## Why it is here and not in a handler
//!
//! Creating a table and creating a field used to exist only as closures inside
//! `sc-server`'s admin handlers: the `type` → storage-type resolution, how a
//! primary-key field is stored, the DDL-then-overlay sequence, the
//! calculated-field check, the ownership validation and the RLS sync. That was fine while an
//! HTTP request was the only way to change a schema. It stopped being fine the
//! moment an **agent** could (§11.3's `admin_copilot`): `sc-core-traits` is
//! layer 9 and cannot name `sc-server`, so a trait that re-implemented any of it
//! would be a second answer to "what does creating a field mean", and the two
//! would drift within a release.
//!
//! So the rule lives here, beside [`rows`](crate::rows) and for the same reason:
//! the admin API and an agent are two callers of one implementation, not two
//! implementations. The handlers are thin callers of this module exactly as the
//! REST provider is a thin caller of `rows`.
//!
//! ## A batch is one transaction and one reload
//!
//! [`apply`] takes an **ordered list** of [`Operation`]s, not one operation. A
//! schema is a set of *connected* tables — `matters` carries a key to `clients`,
//! `time_entries` a key to `matters` — so one-at-a-time would turn a twelve-table
//! schema into forty round trips, each able to fail halfway with no way back.
//! Instead:
//!
//! - **Every operation is resolved against a projected schema**
//!   ([`SchemaProjection`]), so a key pointing at a table created three
//!   operations earlier and a formula naming a field added two operations earlier
//!   both validate. The batch validates against the schema it *ends* with.
//! - **All the DDL goes through one [`Transaction`](sc_db::Transaction)**, so a
//!   refused operation rolls the whole batch back and a half-built schema is
//!   never a state anybody has to clean up by hand. The RLS policies join that
//!   transaction too (they are raw SQL through
//!   [`SchemaStep::Sql`](sc_catalog::SchemaStep)), emitted after the column
//!   changes they may reference.
//! - **The catalog reloads once**, at the end, rather than once per operation.
//! - **The `_fd_tables`/`_fd_fields` overlay rows are written after the commit**
//!   and cannot join that transaction — they go through the row layer, not the
//!   driver handle. So the partial-failure message `createField` always carried —
//!   the column exists, its settings did not save, edit or drop it and retry —
//!   becomes the batch's, naming the operation that half-applied.
//!
//! ## Grants
//!
//! [`Grants`] bounds what a batch may contain, checked **before** anything is
//! applied and refusing the batch **whole**. The admin API passes
//! [`Grants::all`]; an agent passes what its trait's four checkboxes say. They
//! are one struct rather than four entry points because the operations share a
//! batch: creating `matters` with a key to an existing `clients` is a create
//! *and* an edit, and a batch that half-applied for want of a grant is the state
//! the transaction exists to avoid.

use std::collections::{BTreeMap, BTreeSet};

use sc_catalog::{
    ATTR_OWNERSHIP_FORMULA, Attrs, Catalog, ConstraintKind, DataField, DataFieldKind, DbId,
    FIELD_META_TABLE, FieldId, FieldMeta, ReprojectedApp, SchemaChanged, SchemaProjection,
    SchemaStep, Table, TableConstraint, TableId, TableMeta, create_constraint_steps,
    disable_rls_sql, drop_constraint_steps, enable_rls_sql, formula_fields,
    load_field_meta_by_field, load_table_meta_by_name, save_field_meta_row, save_table_meta_row,
    validate_formula,
};
use sc_db::{ColumnGenerator, ColumnRef, SchemaChange};
use sc_error::{Error, Result};
use sc_query::{Expr, UnOp};
use sc_types::{BasicType, RichTypeRef, TypeRef};

/// The column default a `uuid` primary-key column is given, so a row can be
/// written without one being typed while an explicit UUID is still accepted.
pub const UUID_PK_DEFAULT: &str = "gen_random_uuid()";

/// How a **primary-key** field of this type fills itself in, or `None` for a key
/// the writer supplies.
///
/// Not a default primary key — there is none, and a table is created with
/// exactly the key its fields declare (GOALS). This is what "an integer that is
/// the key" *is*: a key nobody can supply a value for is a table no form can
/// insert into, so an `int` key numbers itself and a `uuid` key generates
/// itself. Every other type is a key somebody types, because there is no
/// sensible value to invent for a `text` or `date` key.
///
/// A [`Key`](DataFieldKind::Key) is excluded on purpose — its value is the row it
/// points at, and a generated one would point nowhere — as is a rich type, whose
/// storage is its own business.
///
/// The one place this is decided, because it is asked in two: when the field is
/// created with the box already ticked, and when the box is ticked afterwards on
/// a column that already exists.
pub fn key_generator(storage: &TypeRef, kind: &DataFieldKind) -> Option<ColumnGenerator> {
    match (kind, storage.as_basic()) {
        (DataFieldKind::Plain, Some(BasicType::Int)) => Some(ColumnGenerator::Identity),
        (DataFieldKind::Plain, Some(BasicType::Uuid)) => {
            Some(ColumnGenerator::Default(UUID_PK_DEFAULT.to_owned()))
        }
        _ => None,
    }
}

/// The users table, which is described and may gain a field but is never dropped
/// and never loses a built-in column.
const USERS_TABLE: &str = "users";
/// The role table, protected for the same reason.
const ROLES_TABLE: &str = "_fd_roles";

/// The columns of `users` and `_fd_roles` that the rest of the system requires
/// to exist. Dropping one is refused regardless of grant: authentication reads
/// them by name, so "it dropped and now nobody can log in" is not a state any
/// checkbox should be able to produce.
const PROTECTED_COLUMNS: &[(&str, &[&str])] = &[
    (USERS_TABLE, &["id", "email", "password_hash", "role"]),
    (ROLES_TABLE, &["id", "role", "name"]),
];

// --- what a batch may do ------------------------------------------------------

/// The four grants a batch is checked against (§11.3).
///
/// Four rather than one for the reason `insert_row`/`update_rows`/`delete_rows`
/// are three traits: each is a different thing to trust a caller with. Dropping
/// and access changes default **off**, and access changes sit above dropping —
/// a drop announces itself, a widened role floor does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grants {
    /// May create tables and their fields.
    pub create: bool,
    /// May alter a table's label/description and add or alter its fields.
    pub edit: bool,
    /// May drop tables and fields.
    pub drop: bool,
    /// May write `min_role_read`, `min_role_write`, the ownership formula and
    /// the RLS flag.
    pub access_changes: bool,
}

/// The configuration key each grant is stored under, so the refusal can name the
/// checkbox that would allow the operation rather than describe it.
pub const GRANT_CREATE: &str = "allow_create";
/// See [`GRANT_CREATE`].
pub const GRANT_EDIT: &str = "allow_edit";
/// See [`GRANT_CREATE`].
pub const GRANT_DROP: &str = "allow_drop";
/// See [`GRANT_CREATE`].
pub const GRANT_ACCESS_CHANGES: &str = "allow_access_changes";

impl Grants {
    /// Everything permitted — what the admin API passes, because an admin
    /// reaching these endpoints has already been checked by
    /// [`AuthRequirement::admin`](crate::AuthRequirement).
    pub fn all() -> Grants {
        Grants {
            create: true,
            edit: true,
            drop: true,
            access_changes: true,
        }
    }

    /// Nothing permitted — the base an agent's checkboxes turn on.
    pub fn none() -> Grants {
        Grants {
            create: false,
            edit: false,
            drop: false,
            access_changes: false,
        }
    }

    fn check(&self, granted: bool, what: &str, key: &str) -> Result<()> {
        if granted {
            return Ok(());
        }
        Err(Error::invalid(format!(
            "not permitted to {what}; the whole batch was refused and nothing was applied. \
             Turn on `{key}` to allow it."
        )))
    }
}

/// How a batch is applied.
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// What the caller may do.
    pub grants: Grants,
    /// Validate the whole batch and apply **none** of it. The same refusals, the
    /// same messages, no DDL and no overlay writes.
    pub dry_run: bool,
}

impl Default for ApplyOptions {
    fn default() -> ApplyOptions {
        ApplyOptions {
            grants: Grants::all(),
            dry_run: false,
        }
    }
}

// --- the operations -----------------------------------------------------------

/// A field to create, as a caller describes it.
///
/// `type_name` may be **empty for a `Key` field**, in which case the storage type
/// is taken from the target's primary key — so the pair cannot disagree, which
/// is the mistake a caller asked for both would eventually make.
#[derive(Debug, Clone)]
pub struct FieldSpec {
    /// The column name.
    pub name: String,
    /// A basic type name, a registered rich type's name, or empty for a `Key`.
    pub type_name: String,
    /// The human label; empty for none.
    pub label: String,
    /// The description; empty for none.
    pub description: String,
    /// `NOT NULL`.
    pub required: bool,
    /// A `UNIQUE` constraint.
    pub unique: bool,
    /// Part of the table's **primary key**.
    ///
    /// A field like any other, which is the whole point (GOALS: "do not create
    /// primary key fields when table is created — user must create primary key
    /// fields like other fields"). More than one field may say yes, and then the
    /// key is composite in the order the fields are declared. A key field is
    /// `NOT NULL` whether or not [`required`](Self::required) says so, because a
    /// primary key column cannot be null.
    pub primary_key: bool,
    /// What the field references, if anything. A `Key` whose `target_field` is
    /// empty resolves to the target's primary key.
    pub kind: DataFieldKind,
    /// Rich-type attributes.
    pub attributes: Attrs,
}

impl Default for FieldSpec {
    fn default() -> FieldSpec {
        FieldSpec {
            name: String::new(),
            type_name: String::new(),
            label: String::new(),
            description: String::new(),
            required: false,
            unique: false,
            primary_key: false,
            kind: DataFieldKind::Plain,
            attributes: Attrs::new(),
        }
    }
}

/// Settings on a table, each `None` meaning **leave it as it is**.
///
/// A deliberate divergence from `updateTable`'s whole-object contract (§13.1),
/// recorded as one: that endpoint takes every setting on purpose, because the
/// admin UI edits a table it has loaded and an omitted role could otherwise mean
/// either "leave it" or "reset it". A caller that has *not* loaded the table —
/// an agent — has no such object, and under the whole-object contract
/// `{op: alter_table, table: "clients", min_role_read: 40}` would blank the
/// ownership formula and turn RLS off. So here, omitted means unchanged, and the
/// handler that wants the whole-object contract passes every field as `Some`.
#[derive(Debug, Clone, Default)]
pub struct TableSettings {
    /// The human label.
    pub label: Option<String>,
    /// The description.
    pub description: Option<String>,
    /// Least-privileged role that may read rows.
    pub min_role_read: Option<u8>,
    /// Least-privileged role that may write rows.
    pub min_role_write: Option<u8>,
    /// The ownership formula source; `Some("")` clears it.
    pub ownership_formula: Option<String>,
    /// Whether the database enforces the formula with policies.
    pub rls_enabled: Option<bool>,
}

impl TableSettings {
    /// Whether anything here touches the access rules — the four settings behind
    /// [`Grants::access_changes`].
    pub fn touches_access(&self) -> bool {
        self.min_role_read.is_some()
            || self.min_role_write.is_some()
            || self.ownership_formula.is_some()
            || self.rls_enabled.is_some()
    }

    fn is_empty(&self) -> bool {
        self.label.is_none() && self.description.is_none() && !self.touches_access()
    }
}

/// Overlay-only settings on an existing field, each `None` meaning leave it.
///
/// There is no `required`, `unique` or storage type here for the reason
/// `updateField` has none (§3.3): retyping or re-constraining a column is a
/// migration, and a migration framework is out of scope.
#[derive(Debug, Clone, Default)]
pub struct FieldSettings {
    /// The human label.
    pub label: Option<String>,
    /// The description.
    pub description: Option<String>,
    /// The rich type's name; `Some("")` clears it back to the basic column type.
    pub type_name: Option<String>,
    /// What the field references.
    ///
    /// Not overlay-only for a `Key`: the database's foreign key is what the
    /// merge reads a key's target from, so a changed target (or a field that
    /// becomes or stops being a key) also replaces the column's foreign key —
    /// see `Plan::repoint_reference`.
    pub kind: Option<DataFieldKind>,
    /// Rich-type attributes.
    pub attributes: Option<Attrs>,
    /// Whether the field is part of the primary key.
    ///
    /// The **one** column property this may change, and the exception is
    /// deliberate: no table is created with a key it did not declare (GOALS), so
    /// a table that has none — one imported from a CSV with no key column, one
    /// whose key field was dropped — could otherwise only get one by being
    /// recreated. Setting it emits `SET PRIMARY KEY` over the key's columns plus
    /// this one; clearing it takes this column out, and clearing the last leaves
    /// the table with no key at all. The `NOT NULL` a key column is given is
    /// **not** taken away again by this; `required: Some(false)` does that,
    /// once the column is out of the key.
    pub primary_key: Option<bool>,
    /// Whether the column rejects nulls: `SET NOT NULL` or `DROP NOT NULL`.
    ///
    /// Making a field required is refused, by name and before any DDL, while a
    /// row still holds a null in it — nothing invents a value for those rows.
    /// A key column is `NOT NULL` whatever this says, so asking for it to be
    /// optional is refused; a calculated field has no column to constrain.
    pub required: Option<bool>,
}

/// One schema operation.
#[derive(Debug, Clone)]
pub enum Operation {
    /// Create a table with an identity primary key and these fields.
    CreateTable {
        /// The new table's name.
        name: String,
        /// Which database to create it in: `primary` (or empty, which means the
        /// same) for Saltcorn's own, otherwise the name of a connected database
        /// connection (§5.0).
        ///
        /// Named rather than inferred, because a table that does not exist yet
        /// has nothing to infer it from. Every later operation on the table —
        /// add a field, drop a column, drop it — reads the answer back off the
        /// table this created, so this is the only place the question is asked.
        database: String,
        /// Its settings.
        settings: TableSettings,
        /// Its fields, beside the primary key it is given unasked.
        fields: Vec<FieldSpec>,
    },
    /// Change a table's settings, leaving what is not named.
    AlterTable {
        /// The table.
        table: String,
        /// What to change.
        settings: TableSettings,
    },
    /// Add a field to an existing table.
    AddField {
        /// The table.
        table: String,
        /// The field.
        field: FieldSpec,
    },
    /// Change an existing field's overlay settings.
    AlterField {
        /// The table.
        table: String,
        /// The field.
        field: String,
        /// What to change.
        settings: FieldSettings,
    },
    /// Drop a field and its overlay row.
    DropField {
        /// The table.
        table: String,
        /// The field.
        field: String,
    },
    /// Drop a table, its columns, its rows and its overlay rows.
    DropTable {
        /// The table.
        table: String,
    },
    /// Add a constraint — a jointly-unique key, an index, a full-text index or a
    /// row constraint — to a table.
    ///
    /// The constraint's `name` may be empty, in which case it is derived from
    /// what the constraint *is* ([`TableConstraint::derived_name`]); a row
    /// constraint needs `given_name`, because a formula's identity cannot be
    /// derived from its fields.
    AddConstraint {
        /// The table.
        table: String,
        /// The short name an admin gave a row constraint; ignored by the other
        /// kinds, which name themselves.
        given_name: String,
        /// The constraint to create.
        constraint: TableConstraint,
    },
    /// Drop a constraint by the name it is listed under.
    DropConstraint {
        /// The table.
        table: String,
        /// The constraint, index or trigger name.
        name: String,
    },
}

impl Operation {
    /// The `op` name this operation is spelled with on the wire, for messages
    /// that have to name the operation the caller wrote.
    pub fn op_name(&self) -> &'static str {
        match self {
            Operation::CreateTable { .. } => "create_table",
            Operation::AlterTable { .. } => "alter_table",
            Operation::AddField { .. } => "add_field",
            Operation::AlterField { .. } => "alter_field",
            Operation::DropField { .. } => "drop_field",
            Operation::DropTable { .. } => "drop_table",
            Operation::AddConstraint { .. } => "add_constraint",
            Operation::DropConstraint { .. } => "drop_constraint",
        }
    }

    /// The table this operation is about.
    pub fn table(&self) -> &str {
        match self {
            Operation::CreateTable { name, .. } => name,
            Operation::AlterTable { table, .. }
            | Operation::AddField { table, .. }
            | Operation::AlterField { table, .. }
            | Operation::DropField { table, .. }
            | Operation::AddConstraint { table, .. }
            | Operation::DropConstraint { table, .. }
            | Operation::DropTable { table } => table,
        }
    }
}

/// What a batch did (or, for a dry run, would have done).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Whether nothing was actually written.
    pub dry_run: bool,
    /// Tables created, in order.
    pub tables_created: Vec<String>,
    /// Tables whose settings changed.
    pub tables_altered: Vec<String>,
    /// Tables dropped.
    pub tables_dropped: Vec<String>,
    /// Fields added, as `table.field`.
    pub fields_added: Vec<String>,
    /// Fields whose settings changed, as `table.field`.
    pub fields_altered: Vec<String>,
    /// Fields dropped, as `table.field`.
    pub fields_dropped: Vec<String>,
    /// Constraints added, as `table.constraint`.
    pub constraints_added: Vec<String>,
    /// Constraints dropped, as `table.constraint`.
    pub constraints_dropped: Vec<String>,
    /// Things the caller has to be told in words, because they are not visible
    /// in the schema afterwards — chiefly that a table stopped enforcing RLS.
    pub notes: Vec<String>,
    /// The mounted applications this batch re-projected, each named once
    /// however many of its tables the batch touched (§13.6).
    ///
    /// Re-projection runs no bundler, deliberately — but a schema change
    /// rewrites an application's generated TypeScript client, and a code
    /// framework serves a *built* bundle. So an application here whose
    /// [`wants_build`](ReprojectedApp::wants_build) is set is one whose bundle
    /// is now behind its client, and a caller that does not say so leaves it
    /// serving a stale one. That is the half-finished state this module spends
    /// its transaction avoiding, one layer up.
    pub applications: Vec<ReprojectedApp>,
}

// --- applying -----------------------------------------------------------------

/// Apply an ordered batch of schema operations.
///
/// Either every operation is applied or none is. See the module docs for the
/// shape; the short version is: validate the whole batch against the schema it
/// would leave behind, then one transaction for the DDL, then the overlay rows,
/// then one reload, then one notification per changed table.
pub async fn apply(
    catalog: &Catalog,
    operations: &[Operation],
    options: &ApplyOptions,
) -> Result<Applied> {
    if operations.is_empty() {
        return Err(Error::invalid("a schema edit needs at least one operation"));
    }
    let mut plan = Plan::new(catalog).await?;
    for (index, op) in operations.iter().enumerate() {
        refuse_on_provided(catalog, op).map_err(|e| at(index, op, e))?;
        plan.push(catalog, index, op, &options.grants)
            .await
            .map_err(|e| at(index, op, e))?;
    }
    // Deferred to here on purpose: an ownership formula naming a field the batch
    // adds, and a calculated field reading one, must both validate against the
    // schema the batch *ends* with (§7.3, Phase 8).
    let model_notes = plan.validate_deferred(catalog).await?;
    plan.applied.notes.extend(model_notes);
    let steps = plan.steps(catalog)?;

    if options.dry_run {
        let mut applied = plan.applied;
        applied.dry_run = true;
        applied
            .notes
            .push("dry run: nothing was applied".to_owned());
        return Ok(applied);
    }

    // The batch is one transaction against the one database it was pinned to
    // (`Plan::target`). `None` cannot happen for a non-empty batch — every
    // operation pins or is refused — but a plan that somehow named nothing is
    // Saltcorn's own database, which is where a batch went before databases were
    // a thing anyone could choose.
    let database = plan.database.clone().unwrap_or_else(DbId::primary);
    catalog.apply_schema_batch_in(&database, &steps).await?;

    // Past the commit. The columns are there; the overlay rows are not yet, and
    // cannot join the transaction that made them — so a failure here is reported
    // as exactly what it is rather than as a failure of the batch.
    for write in &plan.metas {
        write.run(catalog).await.map_err(|e| {
            Error::invalid(format!(
                "the schema changes were applied, but saving the settings for \
                 operation {} ({}) failed: {e}. The columns exist as plain columns; \
                 edit or drop them and retry.",
                write.index, write.op_name
            ))
        })?;
    }

    catalog.reload().await?;

    let mut applied = plan.applied;
    for change in &plan.changes {
        match catalog.notify_schema_changed(change) {
            // One application, however many of its tables this batch touched:
            // the caller is being told what to rebuild, and a name repeated
            // three times is not three things to rebuild.
            Ok(reprojected) => {
                for app in reprojected {
                    if !applied.applications.contains(&app) {
                        applied.applications.push(app);
                    }
                }
            }
            Err(e) => applied.notes.push(format!(
                "the schema changed, but re-projecting `{}` for the running \
                 applications failed: {e}",
                change.table()
            )),
        }
    }
    Ok(applied)
}

/// Refuse a DDL operation on a **provided** table (§8.3), naming the provider
/// that decides its columns.
///
/// A provided table has no columns in any database: what it has is a module that
/// answers `fields(cfg)`, and there is nothing for `ALTER TABLE` to alter. So
/// every operation that would emit DDL is refused *before* the batch is planned,
/// and refused with the sentence that says where the columns actually come from
/// — an admin who wants a different column edits the provider's settings, or the
/// module.
///
/// Three are **not** refused:
///
/// - `alter_table`, because a label, a description, access rules and an
///   ownership formula are the overlay's, and a provided table has an overlay
///   row like any other table (it *is* that row).
/// - `create_table`, which cannot name a provided table: a name already in the
///   catalog is refused by the planner as a name already in the catalog.
/// - `drop_table`, which is handled by the caller: dropping a provided table is
///   deleting its row, and `provided_tables::forget` is what does it. Reaching
///   here with one is a caller that did not check, so it is refused rather than
///   allowed to issue a `DROP TABLE` for a table no database has.
fn refuse_on_provided(catalog: &Catalog, op: &Operation) -> Result<()> {
    if matches!(
        op,
        Operation::AlterTable { .. } | Operation::CreateTable { .. }
    ) {
        return Ok(());
    }
    let Some(table) = catalog.get(op.table())? else {
        return Ok(());
    };
    let Some((module, provider)) = table.provider() else {
        return Ok(());
    };
    Err(Error::invalid(format!(
        "`{}` is served by the table provider `{provider}` of `{module}`, so its columns are the \
         module's and not the database's: there is no column here to {}. Change what it presents \
         in the table provider's own settings.",
        table.name,
        match op {
            Operation::DropTable { .. } =>
                "drop — delete the table instead, which forgets its definition",
            Operation::AddField { .. } => "add",
            Operation::AlterField { .. } => "alter",
            Operation::DropField { .. } => "drop",
            _ => "constrain",
        }
    )))
}

/// Forget a table's stored settings: delete its `_fd_tables` row, drop the
/// row-level-security policies if the row was what turned them on, and notify
/// the observers.
///
/// **Not an [`Operation`]**, because it is not a change to the schema: the table,
/// its columns and every row in it are untouched, and this is also how an
/// *orphan* row — one whose table is gone (§1.1) — is cleaned up, which means it
/// must work when there is no table to resolve. It lives here rather than in the
/// handler for the half that *is* this module's: "was RLS on? then drop the
/// policies", which is the same rule `alter_table` applies when it turns the flag
/// off. A `FORCE`'d table whose policies outlived the settings that generated
/// them would deny everyone.
pub async fn forget_table_settings(catalog: &Catalog, table: &str) -> Result<bool> {
    let was_rls = catalog.get(table)?.is_some_and(|t| t.rls_enabled);
    let Some(meta) = load_table_meta_by_name(catalog, table).await? else {
        return Ok(false);
    };
    // A **provided** table's row is not settings, it is the table (§8.3), so
    // "forget the settings" would delete the table — which is a different verb
    // with a different confirmation, and is `provided_tables::forget`. Refused
    // by name rather than obeyed.
    if let Some(def) = meta.provider() {
        return Err(Error::invalid(format!(
            "`{table}` is served by the table provider `{}` of `{}`, and its stored row is the \
             table's whole definition rather than settings added to it: forgetting it would \
             delete the table. Delete the table instead if that is what you meant.",
            def.provider, def.module
        )));
    }
    // A **metadata** table's row is what puts it in the tables list, so
    // forgetting it is removing the table from the list — the other verb again.
    if meta.is_metadata_table() {
        return Err(Error::invalid(format!(
            "`{table}` is one of Saltcorn's metadata tables, and its stored row is what puts it \
             in the tables list: forgetting it would remove it from the list. Remove the table \
             from the list instead if that is what you meant."
        )));
    }
    if !sc_catalog::delete_table_meta(catalog, meta.id).await? {
        return Ok(false);
    }
    if was_rls {
        sc_catalog::disable_rls(catalog, table).await?;
    }
    // Forgetting reverts the table to the admin-only default, and a mounted app
    // exposing it must pick that up now rather than at the next restart.
    catalog.notify_schema_changed(&SchemaChanged::TableChanged(table.to_owned()))?;
    Ok(true)
}

/// The database an operation named, read as a [`DbId`].
///
/// Empty means the primary, so a caller that does not care about databases —
/// every caller that existed before connections did — writes nothing and gets
/// Saltcorn's own database, which is what it always got.
fn parse_database(name: &str) -> DbId {
    match name.trim() {
        "" => DbId::primary(),
        other => DbId(other.to_owned()),
    }
}

/// Prefix an operation's error with which operation it was, by index and by the
/// `op` name the caller wrote. A model that is told "table `clients` not found"
/// with a twelve-operation batch cannot act; told "operation 4 (add_field on
/// clients)", it can.
fn at(index: usize, op: &Operation, e: Error) -> Error {
    let message = format!(
        "operation {index} ({} on `{}`): {e}",
        op.op_name(),
        op.table()
    );
    match e.repr() {
        sc_error::Repr::NotFound(_) => Error::not_found(message),
        _ => Error::invalid(message),
    }
}

/// One pending overlay write, with the operation that asked for it.
struct MetaWrite {
    index: usize,
    op_name: &'static str,
    what: MetaOp,
}

/// Whether a planned constraint is being created or dropped.
enum ConstraintOp {
    Add(Box<TableConstraint>),
    Drop(Box<TableConstraint>),
}

enum MetaOp {
    SaveTable(Box<TableMeta>),
    ForgetTable(String),
    SaveField(Box<FieldMeta>),
    ForgetField(String, String),
}

impl MetaWrite {
    async fn run(&self, catalog: &Catalog) -> Result<()> {
        match &self.what {
            MetaOp::SaveTable(meta) => save_table_meta_row(catalog, meta).await,
            MetaOp::ForgetTable(name) => catalog.forget_table_meta(name).await,
            MetaOp::SaveField(meta) => save_field_meta_row(catalog, meta).await,
            MetaOp::ForgetField(table, field) => catalog.forget_field_meta(table, field).await,
        }
    }
}

/// The batch under construction: the schema as it will be, the DDL that gets it
/// there, the overlay rows that follow, and what to report.
struct Plan {
    projection: SchemaProjection,
    ddl: Vec<SchemaChange>,
    metas: Vec<MetaWrite>,
    /// The pending `_fd_tables` row per table, so two `alter_table`s on one table
    /// update one row rather than racing to create two.
    table_metas: BTreeMap<String, TableMeta>,
    /// Whether each table was enforcing RLS *before* the batch — what decides,
    /// after it, whether policies are created, recreated or dropped.
    was_rls: BTreeMap<String, bool>,
    /// Tables whose policies must be re-emitted or dropped at the end.
    rls_dirty: BTreeSet<String>,
    /// Tables dropped by this batch, so nothing is emitted for them afterwards.
    dropped: BTreeSet<String>,
    /// Calculated-field expressions to validate against the final schema, as
    /// `(table, field, expression)`.
    deferred_calc: Vec<(String, String, String)>,
    /// Row-constraint formulae to validate against the final schema, as
    /// `(table, formula)` — deferred beside the calculated fields, because a
    /// constraint on a table this batch is building names its fields.
    deferred_constraints: Vec<(String, String)>,
    /// Constraints to create or drop, in the order they were asked for. The DDL
    /// is generated in `steps` rather than here: a formula's expression must be
    /// built against the schema the batch ends with.
    constraints: Vec<(String, ConstraintOp)>,
    changes: Vec<SchemaChanged>,
    /// The database every operation in this batch touches.
    ///
    /// `None` until the first operation names one. A batch is **one
    /// transaction**, and a transaction cannot span two Postgres servers, so a
    /// batch that reaches into two databases is refused rather than silently
    /// split into two that can half-fail (see [`Plan::target`]).
    database: Option<DbId>,
    applied: Applied,
}

impl Plan {
    async fn new(catalog: &Catalog) -> Result<Plan> {
        let projection = SchemaProjection::live(catalog)?;
        let was_rls = projection
            .tables()
            .iter()
            .map(|t| (t.name.clone(), t.rls_enabled))
            .collect();
        Ok(Plan {
            projection,
            was_rls,
            ddl: Vec::new(),
            metas: Vec::new(),
            table_metas: BTreeMap::new(),
            rls_dirty: BTreeSet::new(),
            dropped: BTreeSet::new(),
            deferred_calc: Vec::new(),
            deferred_constraints: Vec::new(),
            constraints: Vec::new(),
            changes: Vec::new(),
            database: None,
            applied: Applied::default(),
        })
    }

    /// Pin this batch to `database`, or refuse it for reaching into a second
    /// one.
    ///
    /// The refusal is the honest answer, not a limitation to route around: the
    /// DDL runs in one transaction, two databases mean two transactions, and two
    /// transactions mean a batch that can leave the first database changed and
    /// the second not. A caller with work in two databases sends two batches and
    /// knows that is what it did.
    fn target(&mut self, database: &DbId, table: &str) -> Result<()> {
        match &self.database {
            Some(pinned) if pinned != database => Err(Error::invalid(format!(
                "this batch already changes database `{}`, and `{table}` is in `{}`; \
                 a batch of schema changes is one transaction, so it cannot span two databases",
                pinned.0, database.0
            ))),
            Some(_) => Ok(()),
            None => {
                self.database = Some(database.clone());
                Ok(())
            }
        }
    }

    async fn push(
        &mut self,
        catalog: &Catalog,
        index: usize,
        op: &Operation,
        grants: &Grants,
    ) -> Result<()> {
        // Which database this operation lands in, pinned for the whole batch. A
        // create says so itself; everything else reads it off the table it names,
        // and an operation naming a table that is not there is left to its own
        // handler, which reports it far better than this could.
        match op {
            Operation::CreateTable { database, .. } => {
                self.target(&parse_database(database), op.table())?
            }
            other => {
                if let Some(table) = self.projection.get(other.table()) {
                    let database = table.database.clone();
                    self.target(&database, other.table())?;
                }
            }
        }
        match op {
            Operation::CreateTable {
                name,
                database,
                settings,
                fields,
            } => {
                grants.check(grants.create, "create a table", GRANT_CREATE)?;
                self.create_table(catalog, index, op, name, database, settings, fields)
                    .await
            }
            Operation::AlterTable { table, settings } => {
                grants.check(grants.edit, "change a table's settings", GRANT_EDIT)?;
                if settings.touches_access() {
                    grants.check(
                        grants.access_changes,
                        "change a table's access rules",
                        GRANT_ACCESS_CHANGES,
                    )?;
                }
                self.alter_table(catalog, index, op, table, settings).await
            }
            Operation::AddField { table, field } => {
                grants.check(grants.edit, "add a field", GRANT_EDIT)?;
                self.add_field(catalog, index, op, table, field).await
            }
            Operation::AlterField {
                table,
                field,
                settings,
            } => {
                grants.check(grants.edit, "change a field's settings", GRANT_EDIT)?;
                self.alter_field(catalog, index, op, table, field, settings)
                    .await
            }
            Operation::DropField { table, field } => {
                grants.check(grants.drop, "drop a field", GRANT_DROP)?;
                self.drop_field(index, op, table, field)
            }
            Operation::DropTable { table } => {
                grants.check(grants.drop, "drop a table", GRANT_DROP)?;
                self.drop_table(index, op, table)
            }
            Operation::AddConstraint {
                table,
                given_name,
                constraint,
            } => {
                grants.check(grants.edit, "add a constraint", GRANT_EDIT)?;
                self.add_constraint(table, given_name, constraint)
            }
            Operation::DropConstraint { table, name } => {
                // Dropping, not editing: a constraint is a rule the data has
                // been kept to, and taking it away is the operation whose damage
                // is invisible afterwards — the same reason `drop` is its own
                // grant for a column.
                grants.check(grants.drop, "drop a constraint", GRANT_DROP)?;
                self.drop_constraint(table, name)
            }
        }
    }

    // --- create ---------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn create_table(
        &mut self,
        catalog: &Catalog,
        index: usize,
        op: &Operation,
        name: &str,
        database: &str,
        settings: &TableSettings,
        fields: &[FieldSpec],
    ) -> Result<()> {
        let name = name.trim();
        check_identifier(name, "table")?;
        if name.starts_with("_fd_") {
            return Err(Error::invalid(format!(
                "`{name}` is a system table name; the `_fd_` prefix is reserved"
            )));
        }
        if self.projection.get(name).is_some() {
            return Err(Error::invalid(format!("table `{name}` already exists")));
        }

        // **No invented `id`.** The table starts with no columns and no key, and
        // gets exactly the key its fields declare (GOALS §5) — which may be
        // none. A table without one is a real state: the admin adds the key
        // field afterwards like any other field, and the field list says in red
        // that it is missing until they do.
        // The database is checked here rather than at the DDL, so "there is no
        // connection called `reporting`" is reported while the whole batch is
        // still a plan and nothing has been applied.
        let database = parse_database(database);
        catalog.driver_named(&database)?;
        let mut table = Table::projected(database, name, Vec::new(), Vec::new());
        // In the projection before the fields resolve, so a self-referencing key
        // (`employees.manager` → `employees`) finds its own primary key — which
        // is why the key columns go in as they are declared, below, rather than
        // being collected at the end.
        self.projection.insert(table.clone());

        let mut columns: Vec<DataField> = Vec::new();
        let mut pending_meta: Vec<FieldMeta> = Vec::new();
        for spec in fields {
            let resolved = self.resolve_field(name, spec)?;
            if table.field(&resolved.field.base.name).is_some() {
                return Err(Error::invalid(format!(
                    "table `{name}` is declared with two fields called `{}`",
                    resolved.field.base.name
                )));
            }
            if resolved.field.primary_key {
                table.primary_key.push(resolved.field.base.name.clone());
            }
            if !resolved.field.is_calc() {
                columns.push(resolved.field.clone());
            }
            if let Some(expression) = resolved.field.calc_expression() {
                self.deferred_calc.push((
                    name.to_owned(),
                    resolved.field.base.name.clone(),
                    expression.to_owned(),
                ));
            }
            table.fields.push(resolved.field);
            if let Some(meta) = resolved.meta {
                pending_meta.push(meta);
            }
            self.projection.insert(table.clone());
        }

        self.ddl.push(SchemaChange::CreateTable {
            name: name.to_owned(),
            columns: columns.iter().map(DataField::to_column_def).collect(),
            primary_key: table.primary_key.clone(),
            unlogged: false,
        });
        for meta in pending_meta {
            self.metas.push(MetaWrite {
                index,
                op_name: op.op_name(),
                what: MetaOp::SaveField(Box::new(meta)),
            });
        }
        // A brand-new table has no stored overlay to read, so the settings start
        // from a fresh row rather than being loaded.
        if !settings.is_empty() {
            self.table_metas
                .insert(name.to_owned(), TableMeta::new(name));
            self.write_settings(catalog, index, op, name, settings)
                .await?;
        } else {
            self.projection.insert(table);
        }
        self.applied.tables_created.push(name.to_owned());
        self.changes
            .push(SchemaChanged::TableCreated(name.to_owned()));
        Ok(())
    }

    // --- alter table ----------------------------------------------------------

    async fn alter_table(
        &mut self,
        catalog: &Catalog,
        index: usize,
        op: &Operation,
        table: &str,
        settings: &TableSettings,
    ) -> Result<()> {
        self.require_configurable(table)?;
        if settings.is_empty() {
            return Err(Error::invalid(
                "nothing to change; name at least one setting",
            ));
        }
        self.load_table_meta(catalog, table).await?;
        self.write_settings(catalog, index, op, table, settings)
            .await?;
        if !self.applied.tables_altered.iter().any(|t| t == table) {
            self.applied.tables_altered.push(table.to_owned());
        }
        self.note_changed(table);
        Ok(())
    }

    /// Seed the pending overlay row for `table` from the stored one, so an edit
    /// updates the existing row and an omitted setting keeps the value that row
    /// already carries.
    async fn load_table_meta(&mut self, catalog: &Catalog, table: &str) -> Result<()> {
        if self.table_metas.contains_key(table) {
            return Ok(());
        }
        let stored = load_table_meta_by_name(catalog, table).await?;
        self.table_metas.insert(
            table.to_owned(),
            stored.unwrap_or_else(|| TableMeta::new(table)),
        );
        Ok(())
    }

    async fn write_settings(
        &mut self,
        _catalog: &Catalog,
        index: usize,
        op: &Operation,
        table: &str,
        settings: &TableSettings,
    ) -> Result<()> {
        let meta = self
            .table_metas
            .get_mut(table)
            .ok_or_else(|| Error::msg("settings row missing from the plan"))?;
        if let Some(label) = &settings.label {
            meta.label = label.trim().to_owned();
        }
        if let Some(description) = &settings.description {
            meta.description = description.trim().to_owned();
        }
        let mut access = meta.access.clone();
        if let Some(role) = settings.min_role_read {
            access.min_role_read = check_role(role, "min_role_read")?;
        }
        if let Some(role) = settings.min_role_write {
            access.min_role_write = check_role(role, "min_role_write")?;
        }
        let access_moved = access != meta.access;
        meta.access = access;
        if let Some(formula) = &settings.ownership_formula {
            meta.set_ownership_formula(Some(formula.trim()));
        }
        if let Some(enabled) = settings.rls_enabled {
            // Row-level security is DDL — `CREATE POLICY` against the primary
            // driver — so it is not available on a table a *connection*
            // contributed. Refused here rather than left to fail as a puzzling
            // Postgres error against a table of the same name in the wrong
            // database. The rest of a foreign table's settings (its label, its
            // roles) are the overlay's and work as they do anywhere.
            if enabled
                && let Some(projected) = self.projection.get(table)
                && projected.database != sc_catalog::DbId::primary()
            {
                return Err(Error::invalid(format!(
                    "table `{table}` lives in database connection `{}`; \
                     row-level security applies to Saltcorn's own database only",
                    projected.database.0
                )));
            }
            // And not on a **provided** table (§8.3), for the sharper version of
            // the same reason: there is no table in any database for
            // `CREATE POLICY` to name. Its ownership *formula* still works —
            // that is a filter this system applies, and it applies to a
            // provider's rows like any other — so only the database's own
            // enforcement is refused.
            if enabled
                && let Some(projected) = self.projection.get(table)
                && let Some((module, provider)) = projected.provider()
            {
                return Err(Error::invalid(format!(
                    "table `{table}` is served by the table provider `{provider}` of \
                     `{module}`; row-level security is enforced by the database, and this \
                     table's rows are not in one. Its ownership formula still applies."
                )));
            }
            // Nor on a **metadata** table: the policies are `FORCE`d on every
            // reader, and Saltcorn is one of this table's readers.
            if enabled && self.projection.get(table).is_some_and(|t| t.is_metadata()) {
                return Err(Error::invalid(format!(
                    "table `{table}` is one of Saltcorn's metadata tables: row-level security \
                     is forced on every reader, Saltcorn included, so enabling it would lock \
                     the server out of its own metadata. Its ownership formula still applies."
                )));
            }
            meta.set_rls_enabled(enabled);
        }
        let meta = meta.clone();

        // Mirror the row onto the projection exactly as `Catalog::reload` will,
        // so the rest of the batch sees the table as it will be.
        let mut projected =
            self.projection.get(table).cloned().ok_or_else(|| {
                Error::not_found(format!("table `{table}` is not in the catalog"))
            })?;
        projected.apply_overlay(&meta);
        self.projection.insert(projected);

        if settings.touches_access() || access_moved {
            self.rls_dirty.insert(table.to_owned());
        }
        self.metas.push(MetaWrite {
            index,
            op_name: op.op_name(),
            what: MetaOp::SaveTable(Box::new(meta)),
        });
        Ok(())
    }

    // --- fields ---------------------------------------------------------------

    async fn add_field(
        &mut self,
        catalog: &Catalog,
        index: usize,
        op: &Operation,
        table: &str,
        spec: &FieldSpec,
    ) -> Result<()> {
        self.require_editable(table)?;
        let resolved = self.resolve_field(table, spec)?;
        let name = resolved.field.base.name.clone();
        let projected = self
            .projection
            .get(table)
            .ok_or_else(|| Error::not_found(format!("table `{table}` is not in the catalog")))?;
        if projected.field(&name).is_some() {
            return Err(Error::invalid(format!(
                "table `{table}` already has a field `{name}`"
            )));
        }
        if let Some(expression) = resolved.field.calc_expression() {
            self.deferred_calc
                .push((table.to_owned(), name.clone(), expression.to_owned()));
        } else {
            self.ddl.push(SchemaChange::AddColumn {
                table: table.to_owned(),
                column: resolved.field.to_column_def(),
            });
        }
        let mut projected = projected.clone();
        let becomes_key = resolved.field.primary_key;
        projected.fields.push(resolved.field);
        // The key a table did not have: this is the ordinary way a table gets
        // one, since no table is created with a key it did not declare. A table
        // that *has* a key gains a column to it, in declaration order — which is
        // how a composite key is built one field at a time.
        if becomes_key {
            projected.primary_key.push(name.clone());
            self.ddl.push(SchemaChange::SetPrimaryKey {
                table: table.to_owned(),
                columns: projected.primary_key.clone(),
            });
        }
        self.projection.insert(projected);
        // A new text column belongs in the table's full-text index, or the
        // index quietly stops covering the table it says it covers.
        self.refresh_full_text_index(table);

        if let Some(mut meta) = resolved.meta {
            // Reuse the stored row when one is somehow already there (a column
            // dropped and re-added inside one batch), so the save updates rather
            // than colliding with the one-row-per-field rule.
            if let Some(existing) = stored_field_meta(catalog, table, &name).await? {
                meta = meta.id(existing.id);
            }
            self.metas.push(MetaWrite {
                index,
                op_name: op.op_name(),
                what: MetaOp::SaveField(Box::new(meta)),
            });
        }
        self.applied.fields_added.push(format!("{table}.{name}"));
        self.note_changed(table);
        Ok(())
    }

    async fn alter_field(
        &mut self,
        catalog: &Catalog,
        index: usize,
        op: &Operation,
        table: &str,
        field: &str,
        settings: &FieldSettings,
    ) -> Result<()> {
        self.require_editable(table)?;
        let projected = self
            .projection
            .get(table)
            .ok_or_else(|| Error::not_found(format!("table `{table}` is not in the catalog")))?
            .clone();
        let existing = projected
            .field(field)
            .ok_or_else(|| {
                Error::not_found(format!(
                    "table `{table}` has no field `{field}`; it has {}",
                    field_list(&projected)
                ))
            })?
            .clone();

        let stored = stored_field_meta(catalog, table, field).await?;
        let mut meta = FieldMeta::new(table, field);
        if let Some(row) = &stored {
            meta = meta.id(row.id);
            meta.label.clone_from(&row.label);
            meta.description.clone_from(&row.description);
            meta.type_name.clone_from(&row.type_name);
            meta.kind = row.kind.clone();
            meta.attributes = row.attributes.clone();
        } else {
            meta.kind = existing.kind.clone();
        }
        if let Some(label) = &settings.label {
            meta.label = label.trim().to_owned();
        }
        if let Some(description) = &settings.description {
            meta.description = description.trim().to_owned();
        }
        if let Some(type_name) = &settings.type_name {
            let type_name = type_name.trim();
            meta.type_name = if type_name.is_empty() {
                None
            } else {
                // An unknown type name is refused here; whether a known rich type
                // fits the column is the merge's to report (§3.2).
                //
                // Only a *rich* name is recorded, which is what `resolve_field_type`
                // returns and what `resolve_field` stores when the field is created.
                // A caller that names the column's basic type — `text` for a field
                // that reads back as `text`, which is what an editor round-tripping a
                // field sends — is saying "no rich type", not "the rich type `text`":
                // recording the latter would leave an overlay naming a rich type that
                // is not registered, and the merge would report the field as broken.
                resolve_field_type(type_name)?.1
            };
        }
        if let Some(kind) = &settings.kind {
            meta.kind = self.resolve_kind(table, field, kind)?;
            if !existing.is_calc() {
                self.repoint_reference(table, &existing, &meta.kind)?;
            }
        }
        if let Some(attributes) = &settings.attributes {
            meta.attributes = attributes.clone();
        }
        if let DataFieldKind::Calc { expression } = &meta.kind {
            self.deferred_calc
                .push((table.to_owned(), field.to_owned(), expression.clone()));
        }

        // Mirror onto the projection **in place**, so a later operation reads the
        // field as it will be — and reads the table's fields in the order
        // introspection will hand them back, which is the order every message in
        // this module lists them in.
        let mut projected = projected;
        if let Some(slot) = projected.fields.iter_mut().find(|f| f.base.name == field) {
            slot.kind = meta.kind.clone();
            if !meta.label.is_empty() {
                slot.base.label.clone_from(&meta.label);
            }
        }
        if let Some(wanted) = settings.primary_key
            && wanted != existing.primary_key
        {
            // The built-in columns are the system's, and `users.id` in
            // particular is what every session lookup addresses: un-keying it
            // would break authentication for a tick box.
            if let Some((_, columns)) = PROTECTED_COLUMNS.iter().find(|(t, _)| *t == table)
                && columns.contains(&field)
            {
                return Err(Error::invalid(format!(
                    "`{table}.{field}` is a built-in column of `{table}`; its key is not \
                     the admin's to change"
                )));
            }
            if existing.is_calc() {
                return Err(Error::invalid(format!(
                    "`{table}.{field}` is calculated and has no column to key on"
                )));
            }
            if wanted {
                projected.primary_key.push(field.to_owned());
            } else {
                projected.primary_key.retain(|c| c != field);
            }
            // A key switched on afterwards has to fill itself in exactly as one
            // declared at creation does, or the checkbox would quietly produce
            // two different kinds of key depending on when it was ticked — and
            // the later kind is the one nothing can insert into without typing a
            // number. Only when the column has no generator already: a column
            // that carries a default of its own is not this module's to
            // overwrite.
            //
            // Switching the key **off** leaves the generator alone, for the
            // reason the `NOT NULL` is left alone: taking it away would rewrite
            // the column, and a former key that still numbers itself is a
            // perfectly ordinary column.
            let generator = match wanted && existing.generated.is_none() {
                true => key_generator(&existing.base.type_, &existing.kind),
                false => None,
            };
            for slot in &mut projected.fields {
                if slot.base.name == field {
                    slot.primary_key = wanted;
                    slot.required = slot.required || wanted;
                    if generator.is_some() {
                        slot.generated.clone_from(&generator);
                    }
                }
            }
            // The key first: `SET PRIMARY KEY` is what makes the column
            // `NOT NULL`, and Postgres will not make a nullable column an
            // identity.
            self.ddl.push(SchemaChange::SetPrimaryKey {
                table: table.to_owned(),
                columns: projected.primary_key.clone(),
            });
            if generator.is_some() {
                self.ddl.push(SchemaChange::SetColumnGenerator {
                    table: table.to_owned(),
                    column: field.to_owned(),
                    generator,
                });
            }
        }
        // After the key, which it reads: a field keyed by this very operation is
        // `NOT NULL` already, and one un-keyed by it may now be made optional.
        if let Some(required) = settings.required {
            self.set_required(catalog, &mut projected, &existing, required)
                .await?;
        }
        self.projection.insert(projected);

        self.metas.push(MetaWrite {
            index,
            op_name: op.op_name(),
            what: MetaOp::SaveField(Box::new(meta)),
        });
        self.applied.fields_altered.push(format!("{table}.{field}"));
        self.note_changed(table);
        Ok(())
    }

    /// Bring the column's foreign key into line with the kind an edit gives it —
    /// the half of changing a `Key`'s target that the overlay cannot carry.
    ///
    /// Where a foreign key stands behind a column, the merge takes the target
    /// from the database and never from the overlay (§3.2), so an edit that
    /// only rewrote the overlay would be saved and then read back pointing where
    /// it always did. The reference is therefore changed where it lives: the
    /// old key dropped and the new one added, in the same transaction as the
    /// overlay row. A field that stops being a `Key` loses its foreign key, and
    /// a plain column that becomes one gains the key a field created that way
    /// would have had.
    ///
    /// The column keeps its storage type — retyping one is a migration (§3.3) —
    /// so a target stored as something else is refused by name rather than left
    /// for the database to reject with a type error.
    fn repoint_reference(
        &mut self,
        table: &str,
        existing: &DataField,
        kind: &DataFieldKind,
    ) -> Result<()> {
        fn target(kind: &DataFieldKind) -> Option<(&str, &str)> {
            match kind {
                DataFieldKind::Key {
                    target_table,
                    target_field,
                    ..
                } => Some((target_table.0.as_str(), target_field.0.as_str())),
                _ => None,
            }
        }
        let wanted = target(kind);
        if target(&existing.kind) == wanted {
            return Ok(());
        }
        let field = &existing.base.name;
        let references = match wanted {
            Some((target_table, target_field)) => {
                let storage = self.key_storage_type(
                    &TableId(target_table.to_owned()),
                    &FieldId(target_field.to_owned()),
                )?;
                let column = TypeRef::from_sql_type(existing.base.type_.sql_type());
                if storage.sql_type() != column.sql_type() {
                    return Err(Error::invalid(format!(
                        "field `{table}.{field}` is stored as `{}` and cannot point at \
                         `{target_table}.{target_field}`, which is stored as `{}`; \
                         changing a column's type is not supported, so drop the field \
                         and add it again as a key onto `{target_table}`",
                        column.sql_type(),
                        storage.sql_type()
                    )));
                }
                Some(ColumnRef {
                    table: target_table.to_owned(),
                    column: target_field.to_owned(),
                })
            }
            None => None,
        };
        self.ddl.push(SchemaChange::SetColumnReference {
            table: table.to_owned(),
            column: field.clone(),
            references,
        });
        Ok(())
    }

    /// Make an existing column reject nulls, or accept them — the `NOT NULL`
    /// half of [`alter_field`](Self::alter_field).
    ///
    /// `projected` is the table as the operation has left it so far, so a key
    /// switched on or off by the same operation is already reflected in it.
    async fn set_required(
        &mut self,
        catalog: &Catalog,
        projected: &mut Table,
        existing: &DataField,
        required: bool,
    ) -> Result<()> {
        let table = projected.name.clone();
        let field = existing.base.name.clone();
        let Some(slot) = projected.fields.iter_mut().find(|f| f.base.name == field) else {
            return Ok(());
        };
        if slot.required == required {
            return Ok(());
        }
        if existing.is_calc() {
            return Err(Error::invalid(format!(
                "`{table}.{field}` is calculated and has no column to make {}",
                if required { "required" } else { "optional" }
            )));
        }
        if slot.primary_key {
            // Only reachable asking for `false`: a key column is `NOT NULL`
            // already, so asking for `true` on one is the no-op above.
            return Err(Error::invalid(format!(
                "`{table}.{field}` is part of the primary key, which never accepts \
                 nulls; take it out of the key first"
            )));
        }
        if let Some((_, columns)) = PROTECTED_COLUMNS.iter().find(|(t, _)| *t == table)
            && columns.contains(&field.as_str())
        {
            return Err(Error::invalid(format!(
                "`{table}.{field}` is a built-in column of `{table}`; whether it \
                 accepts nulls is not the admin's to change"
            )));
        }
        if required {
            // The database would refuse too, but with a message about a
            // constraint rather than about the rows the admin has to fix. Only a
            // column that exists already can hold a null: one added earlier in
            // this batch is in a table with no rows yet, or has a null in every
            // row — and the database says so.
            if let Some(live) = catalog.get(&table)?
                && live.field(&field).is_some_and(|f| !f.is_calc())
            {
                let nulls = crate::rows::count_rows_where(
                    catalog,
                    &live,
                    Some(Expr::unary(UnOp::IsNull, Expr::col(field.as_str()))),
                    None,
                )
                .await?;
                if nulls > 0 {
                    return Err(Error::invalid(format!(
                        "`{table}.{field}` cannot be made required: {nulls} row{} \
                         {} no value in it. Fill {} in first.",
                        if nulls == 1 { "" } else { "s" },
                        if nulls == 1 { "has" } else { "have" },
                        if nulls == 1 { "it" } else { "them" },
                    )));
                }
            }
        }
        slot.required = required;
        self.ddl.push(SchemaChange::SetColumnNullable {
            table,
            column: field,
            nullable: !required,
        });
        Ok(())
    }

    fn drop_field(&mut self, index: usize, op: &Operation, table: &str, field: &str) -> Result<()> {
        self.require_editable(table)?;
        let projected = self
            .projection
            .get(table)
            .ok_or_else(|| Error::not_found(format!("table `{table}` is not in the catalog")))?
            .clone();
        let existing = projected.field(field).ok_or_else(|| {
            Error::not_found(format!(
                "table `{table}` has no field `{field}`; it has {}",
                field_list(&projected)
            ))
        })?;

        // Everything the database would otherwise refuse with an error nobody can
        // act on, refused here by name and before any DDL.
        if projected.primary_key.iter().any(|k| k == field) {
            return Err(Error::invalid(format!(
                "`{table}.{field}` is the primary key; a table without one cannot be \
                 addressed, so it cannot be dropped. Drop the table instead."
            )));
        }
        if let Some((_, columns)) = PROTECTED_COLUMNS.iter().find(|(t, _)| *t == table)
            && columns.contains(&field)
        {
            return Err(Error::invalid(format!(
                "`{table}.{field}` is a built-in column of `{table}` and is never dropped"
            )));
        }
        let readers = calc_readers(&projected, field);
        if !readers.is_empty() {
            return Err(Error::invalid(format!(
                "`{table}.{field}` is read by the calculated field(s) {}; \
                 drop or change them first",
                quoted(&readers)
            )));
        }
        let referencing = keys_targeting(&self.projection, table, field);
        if !referencing.is_empty() {
            return Err(Error::invalid(format!(
                "`{table}.{field}` is referenced by {}; drop those fields first",
                quoted(&referencing)
            )));
        }
        // Postgres would drop the unique constraint and the index along with the
        // column, silently — a rule the data has been kept to for a year would
        // disappear with one field. Named here instead, so dropping it is a
        // decision somebody makes twice. A row constraint is included: its
        // trigger would survive the drop and fail at the next write with a
        // `plpgsql` error naming a column that is no longer there.
        let constraining: Vec<String> = projected
            .constraints
            .iter()
            .filter(|c| match &c.kind {
                ConstraintKind::Formula { formula } => {
                    formula_fields(&self.projection, table, formula).contains(field)
                }
                kind => kind.fields().iter().any(|f| f == field),
            })
            .map(|c| c.name.clone())
            .collect();
        if !constraining.is_empty() {
            return Err(Error::invalid(format!(
                "`{table}.{field}` is constrained by {}; drop those constraints first",
                quoted(&constraining)
            )));
        }
        if projected
            .ownership
            .as_ref()
            .is_some_and(|f| f.source().contains(field))
        {
            // Not a parse of the formula — a name check, because the formula is
            // re-validated at the end of the batch anyway and this message is the
            // one that says which table's rule is in the way.
            self.rls_dirty.insert(table.to_owned());
        }

        if !existing.is_calc() {
            self.ddl.push(SchemaChange::DropColumn {
                table: table.to_owned(),
                column: field.to_owned(),
                if_exists: false,
            });
        }
        let mut projected = projected;
        projected.fields.retain(|f| f.base.name != field);
        self.projection.insert(projected);
        self.refresh_full_text_index(table);

        self.metas.push(MetaWrite {
            index,
            op_name: op.op_name(),
            what: MetaOp::ForgetField(table.to_owned(), field.to_owned()),
        });
        self.applied.fields_dropped.push(format!("{table}.{field}"));
        self.note_changed(table);
        Ok(())
    }

    fn drop_table(&mut self, index: usize, op: &Operation, table: &str) -> Result<()> {
        self.require_editable(table)?;
        if table == USERS_TABLE || table == ROLES_TABLE {
            return Err(Error::invalid(format!(
                "`{table}` is a built-in table and is never dropped"
            )));
        }
        let referencing = self.projection.referencing_fields(table);
        if !referencing.is_empty() {
            let names: Vec<String> = referencing
                .iter()
                .map(|(t, f)| format!("{t}.{f}"))
                .collect();
            return Err(Error::invalid(format!(
                "table `{table}` is referenced by {}; drop those fields (or their \
                 tables) first",
                quoted(&names)
            )));
        }
        self.ddl.push(SchemaChange::DropTable {
            name: table.to_owned(),
            if_exists: false,
        });
        self.projection.remove(table);
        self.dropped.insert(table.to_owned());
        self.rls_dirty.remove(table);
        self.table_metas.remove(table);
        self.metas.push(MetaWrite {
            index,
            op_name: op.op_name(),
            what: MetaOp::ForgetTable(table.to_owned()),
        });
        self.applied.tables_dropped.push(table.to_owned());
        self.changes
            .push(SchemaChanged::TableDropped(table.to_owned()));
        Ok(())
    }

    // --- constraints ------------------------------------------------------------

    /// Add a constraint to a table: check what can be checked now, put it on the
    /// projected table, and leave the DDL to [`steps`](Plan::steps).
    ///
    /// The DDL is deferred for the reason the policies' is: a row constraint's
    /// formula may name a field an earlier operation in this batch added, and
    /// the expression has to be generated against the schema the batch *ends*
    /// with, not the one this operation sees.
    fn add_constraint(
        &mut self,
        table: &str,
        given_name: &str,
        constraint: &TableConstraint,
    ) -> Result<()> {
        self.require_editable(table)?;
        let projected = self
            .projection
            .get(table)
            .ok_or_else(|| Error::not_found(format!("table `{table}` is not in the catalog")))?
            .clone();

        let mut constraint = constraint.clone();
        constraint.error_message = constraint
            .error_message
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty());

        // Every field a constraint names must be a real, stored column: an index
        // on a calculated field is an index on nothing, and Postgres's error for
        // it names a column the admin never created.
        let mut named = constraint.kind.fields();
        if let ConstraintKind::Unique { fields } = &constraint.kind {
            if fields.is_empty() {
                return Err(Error::invalid(
                    "a jointly-unique constraint needs at least one field",
                ));
            }
            let mut seen = BTreeSet::new();
            if let Some(dup) = fields.iter().find(|f| !seen.insert((*f).clone())) {
                return Err(Error::invalid(format!(
                    "field `{dup}` is named twice in the same unique constraint"
                )));
            }
        }
        if let ConstraintKind::Index { fields, .. } = &constraint.kind
            && fields.is_empty()
        {
            return Err(Error::invalid("an index needs a field to index"));
        }
        named.sort();
        named.dedup();
        for field in &named {
            match projected.field(field) {
                Some(f) if f.is_calc() => {
                    return Err(Error::invalid(format!(
                        "`{table}.{field}` is a calculated field: it has no column to \
                         constrain or index"
                    )));
                }
                Some(_) => {}
                None => {
                    return Err(Error::not_found(format!(
                        "table `{table}` has no field `{field}`; it has {}",
                        field_list(&projected)
                    )));
                }
            }
        }

        if let ConstraintKind::Formula { formula } = &constraint.kind {
            let given = given_name.trim();
            check_identifier(given, "constraint")?;
            if formula.trim().is_empty() {
                return Err(Error::invalid("a row constraint needs a formula"));
            }
            // Validated against the schema the batch ends with, beside the
            // calculated fields and for the same reason.
            self.deferred_constraints
                .push((table.to_owned(), formula.clone()));
        }

        if constraint.name.trim().is_empty() {
            constraint.name =
                TableConstraint::derived_name(table, &constraint.kind, given_name.trim());
        }
        if let Some(existing) = projected
            .constraints
            .iter()
            .find(|c| c.name == constraint.name)
        {
            // Named by what it is, so this is "you already have this rule" and
            // not merely a name clash — and the message says which rule.
            return Err(Error::invalid(format!(
                "table `{table}` already has the constraint `{}` ({})",
                existing.name,
                existing.kind.type_name()
            )));
        }

        let mut projected = projected;
        projected.constraints.push(constraint.clone());
        self.projection.insert(projected);
        self.constraints
            .push((table.to_owned(), ConstraintOp::Add(Box::new(constraint))));
        self.applied
            .constraints_added
            .push(format!("{table}.{}", self.last_constraint_name()));
        self.note_changed(table);
        Ok(())
    }

    /// The name of the constraint most recently pushed — for the report, which
    /// says what happened rather than what was asked for (the name may have been
    /// derived).
    fn last_constraint_name(&self) -> String {
        match self.constraints.last() {
            Some((_, ConstraintOp::Add(c))) => c.name.clone(),
            Some((_, ConstraintOp::Drop(c))) => c.name.clone(),
            None => String::new(),
        }
    }

    /// Rebuild a table's full-text index when its text fields change.
    ///
    /// A full-text index is over **every** text field of the table (decision 8),
    /// so a field added to or dropped from it changes what the index should be.
    /// Postgres would drop the index along with a column it names — silently,
    /// leaving a table that says it has a full-text index and does not — and a
    /// *new* text column would simply never be searchable. Both are the same
    /// fix: drop what is there and create it again from the fields the table now
    /// has. A table with no text field left keeps the drop and says so, because
    /// there is nothing to index.
    fn refresh_full_text_index(&mut self, table: &str) {
        let Some(projected) = self.projection.get(table) else {
            return;
        };
        let Some(fts) = projected
            .constraints
            .iter()
            .find(|c| matches!(c.kind, ConstraintKind::FullTextSearch { .. }))
            .cloned()
        else {
            return;
        };
        // Already being rebuilt by another operation in this batch — once is
        // enough, and twice would be a create over a create.
        if self
            .constraints
            .iter()
            .any(|(t, op)| t == table && matches!(op, ConstraintOp::Drop(c) if c.name == fts.name))
        {
            return;
        }
        self.constraints
            .push((table.to_owned(), ConstraintOp::Drop(Box::new(fts.clone()))));
        let has_text = projected
            .fields
            .iter()
            .any(|f| !f.is_calc() && f.base.type_.as_basic() == Some(&sc_types::BasicType::Text));
        if has_text {
            self.constraints
                .push((table.to_owned(), ConstraintOp::Add(Box::new(fts))));
        } else {
            self.applied.notes.push(format!(
                "`{table}` has no text fields left, so its full-text search index \
                 (`{}`) was dropped rather than rebuilt",
                fts.name
            ));
        }
    }

    fn drop_constraint(&mut self, table: &str, name: &str) -> Result<()> {
        self.require_editable(table)?;
        let projected = self
            .projection
            .get(table)
            .ok_or_else(|| Error::not_found(format!("table `{table}` is not in the catalog")))?
            .clone();
        let constraint = projected
            .constraints
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .ok_or_else(|| {
                Error::not_found(format!("table `{table}` has no constraint `{name}`"))
            })?;

        let mut projected = projected;
        projected.constraints.retain(|c| c.name != name);
        self.projection.insert(projected);
        self.constraints
            .push((table.to_owned(), ConstraintOp::Drop(Box::new(constraint))));
        self.applied
            .constraints_dropped
            .push(format!("{table}.{name}"));
        self.note_changed(table);
        Ok(())
    }

    // --- shared checks --------------------------------------------------------

    /// What `alter_table` needs: [`require_editable`](Self::require_editable),
    /// except that a system table an admin has added as a **metadata table** may
    /// have its settings changed — its settings are the admin's, its schema is
    /// not.
    fn require_configurable(&self, table: &str) -> Result<()> {
        if self.projection.get(table).is_some_and(|t| t.is_metadata()) {
            return Ok(());
        }
        self.require_editable(table)
    }

    /// The table exists, is not a system table, and is not one this batch has
    /// already dropped.
    fn require_editable(&self, table: &str) -> Result<()> {
        if table.starts_with("_fd_") {
            return Err(Error::invalid(format!(
                "`{table}` is a system table; its schema is not editable"
            )));
        }
        if self.dropped.contains(table) {
            return Err(Error::invalid(format!(
                "table `{table}` was dropped earlier in this batch"
            )));
        }
        if self.projection.get(table).is_none() {
            return Err(Error::not_found(format!(
                "table `{table}` is not in the catalog"
            )));
        }
        Ok(())
    }

    fn note_changed(&mut self, table: &str) {
        let change = SchemaChanged::TableChanged(table.to_owned());
        if !self.changes.contains(&change) {
            self.changes.push(change);
        }
    }

    /// Resolve a caller's field description into the column to create and the
    /// overlay row that describes it, against the projected schema.
    fn resolve_field(&self, table: &str, spec: &FieldSpec) -> Result<ResolvedField> {
        let name = spec.name.trim();
        check_identifier(name, "field")?;
        let kind = self.resolve_kind(table, name, &spec.kind)?;

        // `sql_type` is derived from `type`, never asked for — a rich type sits on
        // its own storage type, and a Key sits on its target's.
        let type_name = spec.type_name.trim();
        let (storage, rich_name) = if type_name.is_empty() {
            match &kind {
                DataFieldKind::Key {
                    target_table,
                    target_field,
                    ..
                } => (self.key_storage_type(target_table, target_field)?, None),
                DataFieldKind::Calc { .. } => (TypeRef::Basic(BasicType::Text), None),
                _ => {
                    return Err(Error::invalid(format!(
                        "field `{name}` needs a type; it is only optional for a \
                         reference, whose type comes from the table it points at"
                    )));
                }
            }
        } else {
            resolve_field_type(type_name)?
        };

        if spec.primary_key && matches!(kind, DataFieldKind::Calc { .. }) {
            return Err(Error::invalid(format!(
                "field `{name}` cannot be both calculated and part of the primary key: \
                 a calculated field has no column to key on"
            )));
        }

        let mut field = DataField::plain(name, storage);
        // A primary-key field is `NOT NULL` whether or not the caller said so —
        // a key column cannot be null — and, where the type allows, fills itself
        // in (see [`key_generator`]).
        field.required = spec.required || spec.primary_key;
        field.unique = spec.unique;
        field.primary_key = spec.primary_key;
        field.generated = spec
            .primary_key
            .then(|| key_generator(&field.base.type_, &kind))
            .flatten();
        field.kind = kind.clone();
        if !spec.label.trim().is_empty() {
            field.base.label = spec.label.trim().to_owned();
        }
        field.base.attributes = spec.attributes.clone();

        let label = spec.label.trim().to_owned();
        let description = spec.description.trim().to_owned();
        let needs_overlay = rich_name.is_some()
            || !matches!(kind, DataFieldKind::Plain)
            || !label.is_empty()
            || !description.is_empty()
            || !spec.attributes.is_empty();
        let meta = needs_overlay.then(|| {
            let mut meta = FieldMeta::new(table, name)
                .label(&label)
                .description(&description)
                .kind(kind);
            meta.type_name = rich_name;
            meta.attributes = spec.attributes.clone();
            meta
        });
        Ok(ResolvedField { field, meta })
    }

    /// Fill in a `Key`'s target field when the caller named only a table, and
    /// check that the target exists — against the *projected* schema, so a key
    /// onto a table created earlier in the same batch resolves.
    fn resolve_kind(
        &self,
        table: &str,
        field: &str,
        kind: &DataFieldKind,
    ) -> Result<DataFieldKind> {
        let DataFieldKind::Key {
            target_table,
            target_field,
            summary_field,
        } = kind
        else {
            return Ok(kind.clone());
        };
        let target = self.projection.get(&target_table.0).ok_or_else(|| {
            Error::invalid(format!(
                "field `{table}.{field}` references table `{}`, which does not exist; \
                 the tables are {}",
                target_table.0,
                quoted(&self.table_names())
            ))
        })?;
        let target_field = if target_field.0.trim().is_empty() {
            match target.primary_key.as_slice() {
                [pk] => FieldId(pk.clone()),
                _ => {
                    return Err(Error::invalid(format!(
                        "field `{table}.{field}` references `{}`, which has no single-column \
                         primary key to point at",
                        target_table.0
                    )));
                }
            }
        } else {
            if target.field(&target_field.0).is_none() {
                return Err(Error::invalid(format!(
                    "field `{table}.{field}` references `{}.{}`, which does not exist",
                    target_table.0, target_field.0
                )));
            }
            target_field.clone()
        };
        if let Some(summary) = summary_field
            && target.field(&summary.0).is_none()
        {
            return Err(Error::invalid(format!(
                "field `{table}.{field}` names summary field `{}`, which `{}` does not have",
                summary.0, target_table.0
            )));
        }
        Ok(DataFieldKind::Key {
            target_table: target_table.clone(),
            target_field,
            summary_field: summary_field.clone(),
        })
    }

    /// The storage type a foreign key takes: the target column's, never the
    /// caller's — a key whose type disagrees with what it points at is a column
    /// Postgres refuses, and the caller has no reason to know the answer.
    fn key_storage_type(&self, target_table: &TableId, target_field: &FieldId) -> Result<TypeRef> {
        let target = self
            .projection
            .get(&target_table.0)
            .ok_or_else(|| Error::invalid(format!("table `{}` does not exist", target_table.0)))?;
        let field = target.field(&target_field.0).ok_or_else(|| {
            Error::invalid(format!(
                "`{}.{}` does not exist",
                target_table.0, target_field.0
            ))
        })?;
        // The *storage* type, so a rich-typed key is referenced by the column
        // underneath it. Whether the target numbers itself is the target's
        // business and never the referrer's — a foreign key holds the value that
        // is there, and generating one would point it at a row that is not.
        Ok(TypeRef::from_sql_type(field.base.type_.sql_type()))
    }

    fn table_names(&self) -> Vec<String> {
        self.projection
            .tables()
            .iter()
            .filter(|t| !t.is_hidden())
            .map(|t| t.name.clone())
            .collect()
    }

    // --- end of batch ---------------------------------------------------------

    /// Everything that can only be checked once every operation has been read:
    /// the calculated-field expressions and the ownership formulas, both against
    /// the schema the batch ends with.
    ///
    /// Answers the notices the batch should carry: a calculated field that
    /// predicts with a model that has no active fit yet is saved, and told
    /// that its table's reads fail until one is.
    async fn validate_deferred(&self, catalog: &Catalog) -> Result<Vec<String>> {
        let shape = self.projection.shape();
        let mut notes = Vec::new();
        for (table, field, expression) in &self.deferred_calc {
            let analysis = validate_calc_expression(&shape, table, field, expression)?;
            let declared = self
                .projection
                .get(table)
                .and_then(|t| t.field(field))
                .map(|f| f.base.type_.clone());
            notes.extend(
                check_calc_predictions(catalog, table, field, expression, &analysis, declared)
                    .await?,
            );
        }
        for (table, formula) in &self.deferred_constraints {
            validate_formula(&self.projection, table, formula)?;
        }
        for name in &self.rls_dirty {
            let Some(table) = self.projection.get(name) else {
                continue;
            };
            validate_ownership(catalog, &self.projection, table)?;
        }
        Ok(notes)
    }

    /// The whole batch as steps of one transaction: the structured DDL in the
    /// order the caller wrote it, then the policy DDL, which is emitted last
    /// because it may reference columns the DDL above it creates.
    fn steps(&mut self, catalog: &Catalog) -> Result<Vec<SchemaStep>> {
        let mut steps: Vec<SchemaStep> = self.ddl.iter().cloned().map(SchemaStep::Change).collect();
        let dialect = catalog.primary().dialect();
        // After the columns (a constraint may name one this batch added) and
        // before the policies (which are the last thing that can reference
        // anything).
        for (table_name, op) in &self.constraints {
            let Some(table) = self.projection.get(table_name) else {
                continue;
            };
            match op {
                ConstraintOp::Add(constraint) => steps.extend(create_constraint_steps(
                    dialect,
                    &self.projection,
                    table,
                    constraint,
                )?),
                ConstraintOp::Drop(constraint) => {
                    steps.extend(drop_constraint_steps(dialect, table_name, constraint));
                }
            }
        }
        for name in &self.rls_dirty {
            let Some(table) = self.projection.get(name) else {
                continue;
            };
            let was = self.was_rls.get(name).copied().unwrap_or(false);
            if table.rls_enabled {
                steps.push(SchemaStep::Sql(enable_rls_sql(
                    dialect,
                    &self.projection,
                    table,
                )?));
            } else if was {
                steps.push(SchemaStep::Sql(disable_rls_sql(dialect, name)));
                self.applied.notes.push(format!(
                    "row-level security is no longer enforced on `{name}`: its policies \
                     were dropped, and the database now returns every row of it to any \
                     caller the role floors let through"
                ));
            }
        }
        Ok(steps)
    }
}

/// The stored `_fd_fields` row for a field, or `None` — including on a database
/// that has no overlay table at all, which is how a catalog behaves before
/// bootstrap and how §9 says a legacy database must go on behaving.
async fn stored_field_meta(
    catalog: &Catalog,
    table: &str,
    field: &str,
) -> Result<Option<FieldMeta>> {
    if catalog.get(FIELD_META_TABLE)?.is_none() {
        return Ok(None);
    }
    load_field_meta_by_field(catalog, table, field).await
}

/// A field spec resolved into the column to create and the overlay row that
/// describes it (`None` when the column alone records everything).
struct ResolvedField {
    field: DataField,
    meta: Option<FieldMeta>,
}

// --- validation shared with the handlers --------------------------------------

/// Validate a table's ownership settings against a projected schema (§7.3).
///
/// The formula must parse and validate; enabling RLS additionally requires a
/// backend that can enforce it and a formula the symbolic translator can turn
/// into policies for **all four** operations under the GUC environment. Enabling
/// and disabling are not symmetric on purpose: enabling is refused when it cannot
/// be honoured, because a flag that silently does nothing is worse than a
/// refusal, while disabling always succeeds and is *reported* — it is the one
/// operation here whose damage is invisible in the schema afterwards.
fn validate_ownership(
    catalog: &Catalog,
    projection: &SchemaProjection,
    table: &Table,
) -> Result<()> {
    let source = table
        .ownership
        .as_ref()
        .map(|f| f.source().to_owned())
        .or_else(|| {
            table
                .attributes
                .get(ATTR_OWNERSHIP_FORMULA)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if source.trim().is_empty() {
        if table.rls_enabled {
            return Err(Error::invalid(format!(
                "table `{}`: row-level security needs an ownership formula to enforce; \
                 set a formula or leave RLS off",
                table.name
            )));
        }
        return Ok(());
    }
    let formula = sc_expr::Formula::parse(&source)
        .map_err(|e| Error::invalid(format!("table `{}`: {e}", table.name)))?;
    let shape = projection.shape();
    let analysis = formula
        .validate(&shape, &table.name)
        .map_err(|e| Error::invalid(format!("table `{}`: {e}", table.name)))?;
    // An **ownership** formula may not call a module function at all, hoistable
    // or not (§4b). `JsEvaluator`'s contract is that `Err` is deny, so a rule
    // calling `geocode_lat` turns a Nominatim outage into "nobody may read
    // anything" — and every row read would wait on a third party first. Every
    // other formula use takes the hoist; the one that decides authorization
    // does not.
    if let Some(call) = analysis.first_module_call() {
        return Err(Error::invalid(format!(
            "table `{}`: an ownership formula may not call the module function `{}`. A rule \
             that decides who may read a row must fail closed, so a module that is slow or \
             down would deny every read of this table — and every read would wait on it. Use \
             a calculated field or a trigger instead",
            table.name, call.function
        )));
    }
    // And for the same reason no `predict("…")` (milestone 31 §4): a provider
    // that is slow, or a model with no active fit, would deny every read.
    if let Some(call) = analysis.first_model_call() {
        return Err(Error::invalid(format!(
            "table `{}`: an ownership formula may not call `{}`. A rule that decides who may \
             read a row must fail closed, so a model that is slow or cannot answer would deny \
             every read of this table — and every read would wait on it. Use a calculated \
             field or a trigger instead",
            table.name, call.key
        )));
    }
    if !table.rls_enabled {
        return Ok(());
    }
    if !catalog.primary().capabilities().row_level_security {
        return Err(Error::invalid(
            "the connected database does not support row-level security",
        ));
    }
    let env = sc_expr::UserEnv::Guc {
        field_types: projection.user_field_types(),
    };
    let calc = table.calc_formulas();
    for op in [
        sc_expr::Operation::Read,
        sc_expr::Operation::Insert,
        sc_expr::Operation::Update,
        sc_expr::Operation::Delete,
    ] {
        if let Err(e) = sc_expr::translate(
            &formula,
            op,
            &sc_expr::Env::new(&env).with_calc(&calc),
            &shape,
            &table.name,
        ) {
            return Err(Error::invalid(format!(
                "table `{}`: cannot enable row-level security: {e}",
                table.name
            )));
        }
    }
    Ok(())
}

/// Validate a calculated field's expression against a projected schema, so a
/// broken formula is refused by name rather than becoming an overlay row the
/// merge silently drops. Validated over the calc scope: **no `user` and no
/// operation flags** — a calc field has no caller.
fn validate_calc_expression(
    shape: &sc_expr::SchemaShape,
    table: &str,
    field: &str,
    expression: &str,
) -> Result<sc_expr::Analysis> {
    let formula = sc_expr::Formula::parse(expression)
        .map_err(|e| Error::invalid(format!("calculated field `{table}.{field}`: {e}")))?;
    let analysis = formula
        .validate(shape, table)
        .map_err(|e| Error::invalid(format!("calculated field `{table}.{field}`: {e}")))?;
    if analysis.uses(sc_expr::Ambient::User) || !analysis.flags.is_empty() {
        return Err(Error::invalid(format!(
            "calculated field `{table}.{field}`: a calculated field cannot use `user` \
             or the operation flags"
        )));
    }
    Ok(analysis)
}

/// The save check for a calculated field that calls `predict("…")`
/// (milestone 31 §4): [`check_model_calls`](sc_catalog::check_model_calls)'s
/// three (the model exists, is a model of this table, and predicts), and —
/// when the expression **is** the call, so the field's value is the
/// prediction — that the field's declared type can hold what the model
/// produces. That is the check `predict_row` made against its target field.
///
/// Answers a notice per model with no active fit: the field is saved, and
/// every read of its table fails until a fit is activated, which the admin is
/// told now rather than on the next read.
async fn check_calc_predictions(
    catalog: &Catalog,
    table: &str,
    field: &str,
    expression: &str,
    analysis: &sc_expr::Analysis,
    declared: Option<TypeRef>,
) -> Result<Vec<String>> {
    let named = |e: Error| Error::invalid(format!("calculated field `{table}.{field}`: {e}"));
    let summaries = sc_catalog::check_model_calls(catalog, table, analysis)
        .await
        .map_err(named)?;
    let whole = sc_expr::Formula::parse(expression)
        .ok()
        .and_then(|f| sc_expr::hoisted_call_key(f.ast()));
    let mut notes = Vec::new();
    for (call, summary) in analysis.model_calls.iter().zip(&summaries) {
        if whole.as_deref() == Some(call.key.as_str())
            && let Some(basic) = declared.as_ref().and_then(TypeRef::as_basic)
            && !summary
                .prediction_types
                .iter()
                .any(|produced| holds_prediction(basic, produced))
        {
            return Err(named(Error::invalid(format!(
                "the field is {} and `{}` predicts {}; declare the field as {}",
                basic.name(),
                summary.name,
                summary
                    .prediction_types
                    .iter()
                    .map(|p| p.name().to_owned())
                    .collect::<Vec<_>>()
                    .join(" or "),
                summary
                    .prediction_types
                    .iter()
                    .map(|p| p.name().to_owned())
                    .collect::<Vec<_>>()
                    .join(" or "),
            ))));
        }
        if summary.active_fit.is_none() {
            notes.push(format!(
                "calculated field `{table}.{field}`: `{}` has no active fit yet, so every read \
                 of `{table}` fails until one is. Fit the model and make a fit active",
                summary.name
            ));
        }
    }
    Ok(notes)
}

/// Whether a field of type `field` can hold a `produced` prediction.
///
/// Deliberately narrow, as `predict_row`'s was: a number is numeric, a class
/// **name** is text, a cluster number is any number, and a vector is JSON and
/// nothing else, because a vector rendered as text is unreadable by anything
/// that wanted to use it.
fn holds_prediction(field: &BasicType, produced: &BasicType) -> bool {
    match produced {
        BasicType::Float => matches!(field, BasicType::Float | BasicType::Decimal),
        BasicType::Int => matches!(
            field,
            BasicType::Int | BasicType::Float | BasicType::Decimal
        ),
        BasicType::Text => matches!(field, BasicType::Text),
        BasicType::Json => matches!(field, BasicType::Json),
        _ => false,
    }
}

// --- the type vocabulary ------------------------------------------------------

/// The basic types offered in a field-type picker — the fixed scalar families
/// (the `Other` catch-all is not a thing anyone picks).
pub fn basic_field_types() -> Vec<BasicType> {
    use BasicType::{
        Bool, Bytes, Date, Decimal, Float, Int, Json as JsonT, Text, Time, Timestamp, Uuid,
    };
    vec![
        Text, Int, Float, Decimal, Bool, Uuid, Date, Time, Timestamp, JsonT, Bytes,
    ]
}

/// Every type name a field may be created with: the basic types then the
/// registered rich types, in that order.
///
/// The one list a picker and a tool's JSON-Schema `enum` are both built from
/// (§11.3), so a model cannot invent `varchar(255)` and an admin cannot be
/// offered a type the server would refuse.
pub fn field_type_names() -> Vec<String> {
    let mut names: Vec<String> = basic_field_types()
        .into_iter()
        .map(|b| b.name().to_owned())
        .collect();
    names.extend(sc_types::registered_rich_types());
    names
}

/// Resolve a `type` name into the column's storage type and, for a rich type, the
/// overlay type name to record. A name that is neither a registered rich type nor
/// a known basic type is refused (§3.3).
pub fn resolve_field_type(name: &str) -> Result<(TypeRef, Option<String>)> {
    if let Ok(rich) = RichTypeRef::resolve(name) {
        return Ok((
            TypeRef::from_sql_type(rich.sql_type()),
            Some(name.to_owned()),
        ));
    }
    if let Some(basic) = basic_field_types().into_iter().find(|b| b.name() == name) {
        return Ok((TypeRef::Basic(basic), None));
    }
    // Also accept a raw SQL alias (e.g. `int8`, `varchar`) for the basic case.
    match BasicType::from_sql_type(name) {
        BasicType::Other(_) => Err(Error::invalid(format!(
            "unknown field type `{name}`; it is neither a basic type nor a registered \
             rich type. The types are {}",
            quoted(&field_type_names())
        ))),
        known => Ok((TypeRef::Basic(known), None)),
    }
}

// --- small shared checks ------------------------------------------------------

/// A table or field name that can be a SQL identifier without quoting games.
///
/// Stricter than "not empty" on purpose: these names end up in DDL, in generated
/// TypeScript, in REST paths and in a model's tool arguments, and a name that
/// needs quoting in one of those is a bug waiting somewhere else.
pub(crate) fn check_identifier(name: &str, what: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid(format!("a {what} needs a name")));
    }
    if name.len() > 63 {
        return Err(Error::invalid(format!(
            "{what} name `{name}` is longer than the 63 characters a database identifier allows"
        )));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or_default();
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(Error::invalid(format!(
            "{what} name `{name}` must start with a letter or underscore and contain \
             only letters, digits and underscores"
        )));
    }
    Ok(())
}

/// A role on the `1..=100` scale. Rejected rather than clamped, for the reason
/// the storage layer rejects: the nearest legal role is still a decision about
/// who reaches the data.
fn check_role(role: u8, key: &str) -> Result<u8> {
    if (1..=100).contains(&role) {
        Ok(role)
    } else {
        Err(Error::invalid(format!(
            "`{key}` must be a role between 1 and 100, got {role}"
        )))
    }
}

/// The calculated fields of `table` whose expression names `field`.
fn calc_readers(table: &Table, field: &str) -> Vec<String> {
    table
        .fields
        .iter()
        .filter(|f| {
            f.calc_expression()
                .is_some_and(|e| names_identifier(e, field))
        })
        .map(|f| format!("{}.{}", table.name, f.base.name))
        .collect()
}

/// Every `Key` field anywhere in the projection whose target is `table.field`.
fn keys_targeting(projection: &SchemaProjection, table: &str, field: &str) -> Vec<String> {
    let mut out = Vec::new();
    for other in projection.tables() {
        for f in &other.fields {
            if let DataFieldKind::Key {
                target_table,
                target_field,
                ..
            } = &f.kind
                && target_table.0 == table
                && target_field.0 == field
                && !(other.name == table && f.base.name == field)
            {
                out.push(format!("{}.{}", other.name, f.base.name));
            }
        }
    }
    out
}

/// Whether `source` uses `name` as a whole identifier — a token check, not a
/// parse, which is all a "is this field still read?" question needs and is what
/// keeps `owner_id` from matching `owner`.
fn names_identifier(source: &str, name: &str) -> bool {
    let bytes = source.as_bytes();
    let mut from = 0;
    while let Some(offset) = source[from..].find(name) {
        let start = from + offset;
        let end = start + name.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A list of names as `` `a`, `b` and `c` `` — the shape every refusal in this
/// module lists alternatives in, because these errors are read by a model that
/// can only recover if it is told what it should have said.
fn quoted(names: &[String]) -> String {
    match names.len() {
        0 => "nothing".to_owned(),
        1 => format!("`{}`", names[0]),
        _ => {
            let head: Vec<String> = names[..names.len() - 1]
                .iter()
                .map(|n| format!("`{n}`"))
                .collect();
            format!("{} and `{}`", head.join(", "), names[names.len() - 1])
        }
    }
}

/// A table's fields as a quoted list, for a "no such field" refusal.
fn field_list(table: &Table) -> String {
    let names: Vec<String> = table.fields.iter().map(|f| f.base.name.clone()).collect();
    quoted(&names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_that_would_need_quoting_is_refused() {
        assert!(check_identifier("clients", "table").is_ok());
        assert!(check_identifier("time_entries2", "table").is_ok());
        assert!(check_identifier("_internal", "field").is_ok());
        assert!(check_identifier("", "table").is_err());
        assert!(check_identifier("2fast", "table").is_err());
        assert!(check_identifier("drop table", "table").is_err());
        assert!(check_identifier("naïve", "field").is_err());
        assert!(check_identifier(&"x".repeat(64), "table").is_err());
    }

    #[test]
    fn the_type_enum_is_the_live_registry_and_nothing_invented() {
        let names = field_type_names();
        assert!(names.contains(&"text".to_owned()));
        assert!(names.contains(&"int".to_owned()));
        assert!(!names.iter().any(|n| n.contains("varchar(")));
        // Every name in the list resolves; that is what makes it safe to publish
        // as an `enum` in a tool's JSON schema.
        for name in &names {
            resolve_field_type(name)
                .unwrap_or_else(|e| panic!("`{name}` is offered but does not resolve: {e}"));
        }
        assert!(resolve_field_type("varchar(255)").is_err());
    }

    #[test]
    fn a_field_is_read_by_a_formula_only_when_it_is_a_whole_identifier() {
        assert!(names_identifier("owner === user.id", "owner"));
        assert!(names_identifier("a + owner", "owner"));
        assert!(!names_identifier("owner_id === user.id", "owner"));
        assert!(!names_identifier("the_owner", "owner"));
        assert!(names_identifier("owner", "owner"));
    }

    #[test]
    fn a_refusal_lists_the_alternatives_the_way_a_reader_can_use() {
        assert_eq!(quoted(&[]), "nothing");
        assert_eq!(quoted(&["a".to_owned()]), "`a`");
        assert_eq!(
            quoted(&["a".to_owned(), "b".to_owned(), "c".to_owned()]),
            "`a`, `b` and `c`"
        );
    }

    #[test]
    fn omitted_settings_leave_what_they_do_not_name() {
        let settings = TableSettings {
            min_role_read: Some(40),
            ..TableSettings::default()
        };
        assert!(settings.touches_access());
        assert!(settings.label.is_none());
        assert!(settings.ownership_formula.is_none());
        assert!(settings.rls_enabled.is_none());
        assert!(!TableSettings::default().touches_access());
        assert!(TableSettings::default().is_empty());
    }

    #[test]
    fn a_grant_refusal_names_the_checkbox_that_would_allow_it() {
        let grants = Grants::none();
        let err = grants
            .check(grants.drop, "drop a table", GRANT_DROP)
            .unwrap_err()
            .to_string();
        assert!(err.contains("allow_drop"), "{err}");
        assert!(err.contains("nothing was applied"), "{err}");
        assert!(Grants::all().check(true, "x", "y").is_ok());
    }
}
