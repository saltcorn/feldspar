//! The `_fd_tables` overlay: its schema, bootstrap, and the [`TableMeta`] ⇄ row
//! mapping (design §9).
//!
//! This is the first **overlay** table Saltcorn stores, and that word carries a
//! rule the other `_fd_*` tables do not have to keep. A file store or an
//! application *is* its row — delete the row and the object is gone (§13.2,
//! §14.1). A table is not: it exists in the database whether or not this table
//! has a row for it, and it must stay exactly as usable with no row as it is
//! today. So everything here **adds** to what introspection already yields —
//! access rules, a label, a description, sparse attributes — and nothing here
//! may restate a fact the database already knows. That is not a stylistic
//! preference: it is what keeps §9's "legacy databases just work" true, and it
//! is why a column like `nullable` must never appear in this table.
//!
//! The merge — where a stored row becomes part of the [`Table`] the rest of the
//! system sees — is [`Table::apply_overlay`], applied by
//! [`Catalog::reload`](crate::Catalog::reload); the precedence rule is stated
//! there. This module is the storage under it.
//!
//! The shape follows the `_fd_file_stores` module column for column:
//! one column per value every row has, sparse values in `attributes`, and
//! **strict reads** — a missing or ill-typed column is an [`Error::invalid`]
//! naming the table and the column, never a silent default. Strictness matters
//! more here than it does for a store, because the values being read decide who
//! may read and write rows: a `min_role_read` that fails to parse must not
//! degrade to *some* role.
//!
//! Two deliberate differences from `_fd_file_stores`, both about the same thing:
//!
//! - **The access columns are `NOT NULL`.** A store's `min_role` is nullable
//!   because "no floor" is a real state for it. A table's rules are always a
//!   pair of roles — a row that exists says who may read and who may write, and
//!   a NULL there would be a third state ("inherit from what?") that has no
//!   answer.
//! - **A system (`_fd_*`) table may not have a row.** System tables are hidden
//!   from users (§9) and their access is not the admin's to widen; the check is
//!   here, on save, rather than only in the merge, so the state never exists.
//!
//! ## The one exception: a **provided** table's row is not an overlay
//!
//! `_fd_tables` holds two kinds of row, and the paragraphs above are about the
//! first. A row carrying a [`ProvidedTableDef`] — a module, a provider within
//! it, and that provider's configuration — is a **definition**: the table it
//! names exists *because the row does*, there is nothing in the database to
//! introspect, and deleting the row deletes the table. That is the same
//! relationship `_fd_triggers` has to a trigger, in a table whose other rows
//! have the opposite one.
//!
//! It is stated here rather than left implicit because the rule above ("nothing
//! here may restate a fact the database already knows") is what keeps §9's
//! "legacy databases just work" true, and a reader has to be able to see that
//! this does not weaken it: a provided table is not a database table, so there
//! is no fact of the database for its row to contradict. The design anticipated
//! it — §9's `_fd_tables` line reads "access rules, label/description,
//! attributes, **provided-table defs**".
//!
//! ## The other exception: a **metadata table** is a system table made visible
//!
//! A row naming an `_fd_*` table is refused — unless it carries
//! [`ATTR_METADATA_TABLE`]. Such a row is how an admin puts one of Saltcorn's
//! own metadata tables (`_fd_triggers`, `_fd_file_stores`, …) in the tables
//! list, with its rows editable and its access rules, ownership formula, label
//! and description theirs to set like any other table's. What it does *not*
//! make configurable is the table's shape: the schema is Saltcorn's, so its
//! fields take no overlay ([`save_field_meta`](crate::save_field_meta) still
//! refuses them) and the schema editor still refuses every DDL operation. Nor
//! can it switch on row-level security, which is enforced with
//! `FORCE ROW LEVEL SECURITY` and would therefore apply to the server's own
//! reads of its metadata. Deleting the row hides the table again; the table
//! itself, being Saltcorn's, is never dropped.

use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::catalog::Catalog;
use crate::field::DataField;
use crate::table::{AccessRules, Table};

/// Name of the table-overlay table in the primary database.
pub const TABLE_META_TABLE: &str = "_fd_tables";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The name of the table this row overlays — §9's required `name` column, which
/// for an overlay *is* the subject's name, and the key the merge joins on.
pub const COL_NAME: &str = "name";
/// The human label shown instead of the table name, when one is given.
pub const COL_LABEL: &str = "label";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// Least-privileged role that may read the table's rows.
pub const COL_MIN_ROLE_READ: &str = "min_role_read";
/// Least-privileged role that may create, update or delete the table's rows.
pub const COL_MIN_ROLE_WRITE: &str = "min_role_write";
/// The sparse per-table values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// Attribute key holding the table's ownership formula source (§7.3).
pub const ATTR_OWNERSHIP_FORMULA: &str = "ownership_formula";
/// Attribute key holding the RLS flag (GOALS: attributes, not a column).
pub const ATTR_RLS_ENABLED: &str = "rls_enabled";

/// Attribute key holding the package name of the module whose **table
/// provider** serves this table's rows (§8.3).
pub const ATTR_PROVIDER_MODULE: &str = "provider_module";
/// Attribute key holding the provider's own name within that module.
pub const ATTR_PROVIDER_NAME: &str = "provider_name";
/// Attribute key holding the provider's configuration — the object handed to
/// v1's `fields(cfg)` and `get_table(cfg)`.
pub const ATTR_PROVIDER_CONFIG: &str = "provider_config";

/// Attribute key marking a row for an `_fd_*` table as the admin's choice to
/// show that **metadata table** in the tables list (see the module docs).
/// `true` when set; absent otherwise.
pub const ATTR_METADATA_TABLE: &str = "metadata_table";

/// What makes a `_fd_tables` row a **definition** rather than an overlay: the
/// module, the provider within it, and the configuration an admin filled in.
///
/// Three attributes rather than three columns, on §9's own rule: a value present
/// on a handful of rows out of every table in the database is sparse, and it
/// keeps company with `ownership_formula` and `rls_enabled`, which are there for
/// the same reason.
///
/// **The module is stored as well as the provider**, which v1 does not do. v1
/// keys `table_providers` globally in one process's state, so the provider's
/// name is its whole identity; here the call has to reach the worker *that
/// module* was loaded on, and two modules may each supply a provider called
/// `Table`. Which one was meant is the admin's answer when the table is created,
/// never a lookup that could start resolving elsewhere when a second module is
/// installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidedTableDef {
    /// The package name — `@saltcorn/rss`.
    pub module: String,
    /// The provider's own name within that package — `RSS feed`.
    pub provider: String,
    /// The configuration the admin filled in, as the module's own
    /// `configuration_workflow` declared it.
    pub configuration: Attrs,
}

impl ProvidedTableDef {
    /// A definition with no configuration yet — what "create a table with this
    /// provider" starts from, before the settings form has been filled in.
    pub fn new(module: impl Into<String>, provider: impl Into<String>) -> ProvidedTableDef {
        ProvidedTableDef {
            module: module.into(),
            provider: provider.into(),
            configuration: Attrs::new(),
        }
    }

    /// Set the configuration.
    pub fn configuration(mut self, configuration: Attrs) -> ProvidedTableDef {
        self.configuration = configuration;
        self
    }

    /// The configuration as the JSON object a module is handed.
    pub fn configuration_json(&self) -> Json {
        Json::Object(self.configuration.clone())
    }
}

/// Identifies a stored table-overlay row.
///
/// Distinct from [`TableId`](crate::TableId), which is a table's *name* and is
/// what everything else references. This id is the row's identity, so an overlay
/// keeps its identity across edits — and, when table renaming eventually exists,
/// across a rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableMetaId(pub Uuid);

impl TableMetaId {
    /// A fresh random id, for an overlay that has never been saved.
    pub fn new() -> TableMetaId {
        TableMetaId(Uuid::new_v4())
    }
}

impl Default for TableMetaId {
    fn default() -> TableMetaId {
        TableMetaId::new()
    }
}

/// The stored overlay for one table (design §9).
///
/// Deliberately **not** a [`Table`]: a `Table` is the merged view the rest of
/// the system sees — fields, primary key, source, all of which come from
/// introspection — while this is only the part an admin configured. Keeping the
/// two types apart is what stops "the overlay" and "the table" from drifting
/// into one another, and what makes "no row" expressible at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMeta {
    /// The row's identity.
    pub id: TableMetaId,
    /// The name of the table this overlays. Unique across overlay rows.
    pub table_name: String,
    /// A human label; empty when none was given, in which case the table's own
    /// name is the label.
    pub label: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// Who may read and who may write this table's rows.
    pub access: AccessRules,
    /// Sparse per-table values (§9).
    pub attributes: Attrs,
}

impl TableMeta {
    /// An overlay for `table_name` carrying nothing but the default (admin-only)
    /// access rules — the base to add settings to.
    ///
    /// The default matches [`AccessRules::default`] rather than something more
    /// permissive on purpose: creating an overlay row must never be the thing
    /// that widens access to a table.
    pub fn new(table_name: impl Into<String>) -> TableMeta {
        TableMeta {
            id: TableMetaId::new(),
            table_name: table_name.into(),
            label: String::new(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
        }
    }

    /// Set the human label.
    pub fn label(mut self, label: impl Into<String>) -> TableMeta {
        self.label = label.into();
        self
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> TableMeta {
        self.description = description.into();
        self
    }

    /// Set both access roles.
    pub fn access(mut self, min_role_read: u8, min_role_write: u8) -> TableMeta {
        self.access = AccessRules {
            min_role_read,
            min_role_write,
        };
        self
    }

    /// Give the row a specific id — for a caller updating a known row rather
    /// than creating one.
    pub fn id(mut self, id: TableMetaId) -> TableMeta {
        self.id = id;
        self
    }

    // --- ownership formula & RLS (§7.3, TODO Phase 4) -----------------------
    //
    // Both live in `attributes` rather than as columns: they are sparse (§9's
    // rule — most tables have neither), and GOALS names attributes for
    // `rls_enabled` explicitly. The typed accessors below are the only places
    // that spell the keys.

    /// The table's ownership formula source, if one is set. Storage-neutral:
    /// validation happens in the API on save and in the merge on load.
    pub fn ownership_formula(&self) -> Option<&str> {
        self.attributes
            .get(ATTR_OWNERSHIP_FORMULA)
            .and_then(Json::as_str)
            .filter(|s| !s.trim().is_empty())
    }

    /// Set or clear the ownership formula (`None` — or an empty string —
    /// removes the key rather than storing an empty formula).
    pub fn set_ownership_formula(&mut self, formula: Option<&str>) {
        match formula.map(str::trim).filter(|s| !s.is_empty()) {
            Some(source) => {
                self.attributes
                    .insert(ATTR_OWNERSHIP_FORMULA.into(), Json::String(source.into()));
            }
            None => {
                self.attributes.remove(ATTR_OWNERSHIP_FORMULA);
            }
        }
    }

    /// Whether row-level security is enabled for this table (GOALS: "the table
    /// has a rls_enabled field in its json attributes"). Absent means false.
    pub fn rls_enabled(&self) -> bool {
        self.attributes
            .get(ATTR_RLS_ENABLED)
            .and_then(Json::as_bool)
            .unwrap_or(false)
    }

    /// Set or clear the RLS flag (`false` removes the key — absence is the
    /// false state, so a table that never touched RLS has no residue).
    pub fn set_rls_enabled(&mut self, enabled: bool) {
        if enabled {
            self.attributes
                .insert(ATTR_RLS_ENABLED.into(), Json::Bool(true));
        } else {
            self.attributes.remove(ATTR_RLS_ENABLED);
        }
    }

    // --- provided tables (§8.3) ---------------------------------------------

    /// The table provider that serves this table's rows, when the row is a
    /// **definition** rather than an overlay.
    ///
    /// Both names are required for the row to count as a definition: a row
    /// carrying a module and no provider (or the other way round) names nothing
    /// that can be called, and reading it as a provided table would produce a
    /// table nobody can serve. Such a row reads as an ordinary overlay, which is
    /// what it is.
    pub fn provider(&self) -> Option<ProvidedTableDef> {
        let module = non_empty(self.attributes.get(ATTR_PROVIDER_MODULE))?;
        let provider = non_empty(self.attributes.get(ATTR_PROVIDER_NAME))?;
        let configuration = match self.attributes.get(ATTR_PROVIDER_CONFIG) {
            Some(Json::Object(map)) => map.clone(),
            _ => Attrs::new(),
        };
        Some(ProvidedTableDef {
            module,
            provider,
            configuration,
        })
    }

    /// Set or clear the table provider. `None` removes all three keys, turning
    /// the definition back into a plain overlay.
    pub fn set_provider(&mut self, def: Option<&ProvidedTableDef>) {
        match def {
            Some(def) => {
                self.attributes.insert(
                    ATTR_PROVIDER_MODULE.into(),
                    Json::String(def.module.clone()),
                );
                self.attributes.insert(
                    ATTR_PROVIDER_NAME.into(),
                    Json::String(def.provider.clone()),
                );
                self.attributes.insert(
                    ATTR_PROVIDER_CONFIG.into(),
                    Json::Object(def.configuration.clone()),
                );
            }
            None => {
                self.attributes.remove(ATTR_PROVIDER_MODULE);
                self.attributes.remove(ATTR_PROVIDER_NAME);
                self.attributes.remove(ATTR_PROVIDER_CONFIG);
            }
        }
    }

    // --- metadata tables ----------------------------------------------------

    /// Whether this row exposes a system (`_fd_*`) table as a **metadata
    /// table** an admin can see and configure.
    pub fn is_metadata_table(&self) -> bool {
        is_metadata_flag(&self.attributes)
    }

    /// Mark or unmark the row as exposing a metadata table (`false` removes the
    /// key — absence is the false state).
    pub fn set_metadata_table(&mut self, exposed: bool) {
        if exposed {
            self.attributes
                .insert(ATTR_METADATA_TABLE.into(), Json::Bool(true));
        } else {
            self.attributes.remove(ATTR_METADATA_TABLE);
        }
    }
}

/// Whether a set of table attributes carries the metadata-table flag — shared
/// by the stored row and the merged [`Table`], whose attributes are the row's.
pub(crate) fn is_metadata_flag(attributes: &Attrs) -> bool {
    attributes
        .get(ATTR_METADATA_TABLE)
        .and_then(Json::as_bool)
        .unwrap_or(false)
}

/// A non-empty string attribute, or `None` — the shape both provider names have
/// to be for the row to name a provider at all.
fn non_empty(value: Option<&Json>) -> Option<String> {
    value
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The fields of the `_fd_tables` table, in declaration order.
///
/// `name` carries the `UNIQUE` constraint: it is the key the merge joins on, and
/// two overlay rows for one table is not a state that can be merged — there
/// would be no rule saying which one's access rules apply. As with
/// `_fd_file_stores`, the database is the authority, because two admins saving
/// concurrently cannot see each other's transaction.
fn table_meta_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_LABEL, text()),
        DataField::plain(COL_DESCRIPTION, text()),
        // Required, unlike a store's `min_role`: see the module docs.
        DataField::plain(COL_MIN_ROLE_READ, int()).required(),
        DataField::plain(COL_MIN_ROLE_WRITE, int()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure the `_fd_tables` table exists, creating it if absent, and return it.
///
/// Idempotent, and safe against a database that has never seen Saltcorn — the
/// same contract as [`bootstrap_file_stores`](crate::bootstrap_file_stores).
/// Call once at startup, after the [`Catalog`] is initialised.
pub async fn bootstrap_table_meta(catalog: &Catalog) -> Result<Table> {
    // A table's last edit time is kept beside its overlay rather than in it
    // (see `edited`), and is needed wherever tables are configured.
    crate::edited::bootstrap_table_edits(catalog).await?;
    if let Some(existing) = catalog.get(TABLE_META_TABLE)? {
        return Ok(existing);
    }
    catalog
        .create_table(TABLE_META_TABLE, &table_meta_fields())
        .await
}

/// Save a table overlay: insert its row, or update it in place if a row with its
/// [`TableMetaId`] already exists.
///
/// What is checked here, and why each is checked *here* rather than at merge
/// time:
///
/// - **A name** — an overlay of nothing has no meaning.
/// - **Roles in `1..=100`** — the role scale is fixed (`sc-auth`), so an
///   out-of-range role is not a value to clamp; clamping would silently decide
///   who can reach the data.
/// - **Not a system table**, unless the row exposes it as a metadata table —
///   `_fd_*` tables are hidden from users (§9) until an admin chooses to show
///   one, and refusing any other row means the merge never has to decide what
///   to do with one. An exposed row may not turn on RLS (see the module docs)
///   nor name a table provider, and the flag means nothing on a table that is
///   not a system table, so it is refused there rather than stored as residue.
/// - **The name is not already claimed by another row** — the database's
///   `UNIQUE` constraint remains the authority, but this check names the
///   conflict rather than surfacing a raw constraint violation.
///
/// Deliberately **not** checked: that the table exists. See
/// [`orphan_table_meta`] for why an overlay may legitimately outlive its table,
/// and note that requiring existence here would make the row unsavable in
/// exactly the situation where an admin is repairing one.
///
/// **Saving reloads the catalog cache**, exactly as a schema change does. Access
/// rules are read from the cached [`Table`] on the request path, so without the
/// reload an admin would set a role, watch it save, and watch the running server
/// go on enforcing the old one — with the stored row and the served behaviour
/// disagreeing until something else happened to reload.
pub async fn save_table_meta(catalog: &Catalog, meta: &TableMeta) -> Result<()> {
    save_table_meta_row(catalog, meta).await?;
    catalog.reload().await
}

/// Write the row **without reloading** — the half of [`save_table_meta`] a
/// batch uses, which reloads once at the end rather than once per operation
/// (Phase 7). Every check [`save_table_meta`] performs is performed here; only
/// the reload is the caller's.
pub async fn save_table_meta_row(catalog: &Catalog, meta: &TableMeta) -> Result<()> {
    let name = meta.table_name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a table overlay needs a table name"));
    }
    check_metadata_rules(name, meta)?;
    validate_access(name, &meta.access)?;

    if let Some(other) = load_table_meta_by_name(catalog, name).await?
        && other.id != meta.id
    {
        return Err(Error::invalid(format!(
            "table `{name}` already has an overlay row; \
             a table has at most one"
        )));
    }

    let columns = meta_columns();
    let values = meta_values(meta);
    if load_table_meta(catalog, meta.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            // The id is the row's identity, not something to reassign.
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = sc_query::Update::new(TABLE_META_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(meta.id.0)));
        run(catalog, Statement::from(update)).await?;
    } else {
        let insert = Insert::row(
            TABLE_META_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await?;
    }
    crate::edited::touch_tables(catalog, &[name]).await
}

/// Load the overlay with this id, if it exists.
pub async fn load_table_meta(catalog: &Catalog, id: TableMetaId) -> Result<Option<TableMeta>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the overlay for the table named `name`, if any — the lookup the merge
/// performs.
pub async fn load_table_meta_by_name(catalog: &Catalog, name: &str) -> Result<Option<TableMeta>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// Every stored overlay, ordered by table name.
pub async fn list_table_meta(catalog: &Catalog) -> Result<Vec<TableMeta>> {
    let select = Select::from(Source::table(TABLE_META_TABLE));
    let mut metas: Vec<TableMeta> = rows(catalog, select)
        .await?
        .iter()
        .map(table_meta_from_row)
        .collect::<Result<_>>()?;
    metas.sort_by(|a, b| a.table_name.cmp(&b.table_name));
    Ok(metas)
}

/// The overlays whose table is not in the catalog, ordered by table name.
///
/// **An orphan is kept, not deleted.** A table that is dropped and recreated —
/// by a restore, or by a migration run outside Saltcorn — would otherwise
/// silently lose its access rules, and losing access rules is the worst kind of
/// silent loss: the table comes back with [`AccessRules::default`], which is
/// admin-only, so an application that served it stops working with nothing
/// anywhere saying why. Keeping the row means recreating the table restores its
/// configuration, and this function is how the admin UI can say "these rows
/// describe tables that are not here" instead of the admin discovering it.
///
/// Names the catalog does not know about are the definition of "not here": the
/// cache is rebuilt from introspection on every reload, so a table absent from
/// it is absent from the database.
pub async fn orphan_table_meta(catalog: &Catalog) -> Result<Vec<TableMeta>> {
    let mut orphans = Vec::new();
    for meta in list_table_meta(catalog).await? {
        if catalog.get(&meta.table_name)?.is_none() {
            orphans.push(meta);
        }
    }
    Ok(orphans)
}

/// Delete a table overlay, returning whether one was there to delete.
///
/// **This removes the row and nothing else** — the table, its columns and every
/// row in it are untouched. Deleting an overlay is "forget what I configured",
/// which returns the table to its unconfigured state
/// ([`AccessRules::default`]), not "drop the table". The parallel with
/// [`delete_file_store`](crate::delete_file_store) is exact, and stronger here:
/// for an overlay, the subject existing independently of its row is not a
/// caveat, it is the whole point.
///
/// Reloads the cache for the same reason [`save_table_meta`] does: the table
/// reverts to [`AccessRules::default`], and that must take effect now rather
/// than at the next restart.
pub async fn delete_table_meta(catalog: &Catalog, id: TableMetaId) -> Result<bool> {
    let deleted = delete_table_meta_row(catalog, id).await?;
    if deleted {
        catalog.reload().await?;
    }
    Ok(deleted)
}

/// Delete the row **without reloading** — the half of [`delete_table_meta`] a
/// batch uses, which reloads once at the end rather than once per row (Phase 7).
pub(crate) async fn delete_table_meta_row(catalog: &Catalog, id: TableMetaId) -> Result<bool> {
    if load_table_meta(catalog, id).await?.is_none() {
        return Ok(false);
    }
    let delete = Delete::from(TABLE_META_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The system-table half of [`save_table_meta_row`]'s checks: a row for an
/// `_fd_*` table must expose it as a metadata table, and a row exposing one
/// must be for an `_fd_*` table, carry no provider and leave RLS off.
fn check_metadata_rules(name: &str, meta: &TableMeta) -> Result<()> {
    let system = name.starts_with("_fd_");
    match (system, meta.is_metadata_table()) {
        (true, false) => Err(Error::invalid(format!(
            "`{name}` is a system table; its access rules are not configurable until it is \
             added as a metadata table"
        ))),
        (false, true) => Err(Error::invalid(format!(
            "`{name}` is not one of Saltcorn's metadata tables, so it cannot be added as one"
        ))),
        (true, true) if meta.provider().is_some() => Err(Error::invalid(format!(
            "`{name}` is a metadata table; it cannot also be served by a table provider"
        ))),
        (true, true) if meta.rls_enabled() => Err(Error::invalid(format!(
            "`{name}` is a metadata table: row-level security is forced on every reader, \
             Saltcorn included, so enabling it would lock the server out of its own metadata. \
             Its roles and ownership formula still apply."
        ))),
        _ => Ok(()),
    }
}

/// Check both roles are on the `1..=100` scale, naming the table and which role
/// is wrong.
fn validate_access(table: &str, access: &AccessRules) -> Result<()> {
    for (column, role) in [
        (COL_MIN_ROLE_READ, access.min_role_read),
        (COL_MIN_ROLE_WRITE, access.min_role_write),
    ] {
        if !(1..=100).contains(&role) {
            return Err(Error::invalid(format!(
                "table `{table}`: {column} should be a role between 1 and 100, got {role}"
            )));
        }
    }
    Ok(())
}

/// The row's columns, in the order [`meta_values`] produces them.
fn meta_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_LABEL,
        COL_DESCRIPTION,
        COL_MIN_ROLE_READ,
        COL_MIN_ROLE_WRITE,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The overlay serialised to its row's values, in [`meta_columns`] order.
fn meta_values(meta: &TableMeta) -> Vec<Value> {
    vec![
        Value::Uuid(meta.id.0),
        Value::Text(meta.table_name.trim().to_owned()),
        Value::Text(meta.label.clone()),
        Value::Text(meta.description.clone()),
        Value::Int(i64::from(meta.access.min_role_read)),
        Value::Int(i64::from(meta.access.min_role_write)),
        Value::Json(Json::Object(meta.attributes.clone())),
    ]
}

/// Rebuild a [`TableMeta`] from its `_fd_tables` row. The strictness note in the
/// module docs applies throughout.
fn table_meta_from_row(row: &Row) -> Result<TableMeta> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => TableMetaId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    Ok(TableMeta {
        id,
        table_name: text(row, COL_NAME)?,
        label: optional_text(row, COL_LABEL)?,
        description: optional_text(row, COL_DESCRIPTION)?,
        access: AccessRules {
            min_role_read: role(row, COL_MIN_ROLE_READ)?,
            min_role_write: role(row, COL_MIN_ROLE_WRITE)?,
        },
        attributes: object(row, COL_ATTRIBUTES)?,
    })
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A text column where NULL means "none given", not a broken row.
fn optional_text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        Some(Value::Null) | None => Ok(String::new()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A role column: an integer on the `1..=100` scale.
///
/// An out-of-range value is a corrupt row, not something to clamp — the nearest
/// valid role would quietly change who can reach the data, which is precisely
/// what this column decides.
fn role(row: &Row, column: &str) -> Result<u8> {
    match row.get(column) {
        Some(Value::Int(i)) => u8::try_from(*i)
            .ok()
            .filter(|r| (1..=100).contains(r))
            .ok_or_else(|| {
                Error::invalid(format!(
                    "{TABLE_META_TABLE}.{column} should be a role between 1 and 100, got {i}"
                ))
            }),
        other => Err(bad_column(column, "an integer role", other)),
    }
}

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{TABLE_META_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{TABLE_META_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

/// Load the single overlay matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<TableMeta>> {
    let select = Select::from(Source::table(TABLE_META_TABLE))
        .filter(filter)
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(table_meta_from_row(row)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = table_meta_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        // §9: every system metadata table MUST have id (UUID), name,
        // description, attributes (JSON object).
        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required);
        assert!(!by_name(COL_DESCRIPTION).required);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
    }

    #[test]
    fn name_is_the_unique_key_the_merge_joins_on() {
        let fields = table_meta_fields();
        let name = fields.iter().find(|f| f.base.name == COL_NAME).unwrap();
        assert!(name.required && name.unique);
        assert_eq!(name.base.type_, TypeRef::Basic(BasicType::Text));
    }

    #[test]
    fn both_access_roles_are_required_because_the_pair_is_the_state() {
        // Unlike a file store's nullable `min_role`, a NULL here would be a
        // third state with no meaning: a row that exists states both roles.
        let fields = table_meta_fields();
        for column in [COL_MIN_ROLE_READ, COL_MIN_ROLE_WRITE] {
            let f = fields.iter().find(|f| f.base.name == column).unwrap();
            assert!(f.required, "{column} should be NOT NULL");
            assert_eq!(f.base.type_, TypeRef::Basic(BasicType::Int));
        }
    }

    #[test]
    fn the_overlay_carries_nothing_the_database_already_knows() {
        // The precedence rule of §1.2 holds only while the two sets do not
        // intersect. A column here named for a fact introspection yields —
        // nullability, a type, the primary key — would break it, so the column
        // set is asserted in full rather than merely spot-checked.
        let names: Vec<String> = table_meta_fields()
            .iter()
            .map(|f| f.base.name.clone())
            .collect();
        assert_eq!(
            names,
            vec![
                COL_ID,
                COL_NAME,
                COL_LABEL,
                COL_DESCRIPTION,
                COL_MIN_ROLE_READ,
                COL_MIN_ROLE_WRITE,
                COL_ATTRIBUTES,
            ]
        );
    }

    #[test]
    fn the_table_is_a_hidden_system_table() {
        assert!(TABLE_META_TABLE.starts_with("_fd_"));
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        // The insert pairs these two positionally, so a column added to one and
        // not the other would write a role into the wrong column.
        let meta = TableMeta::new("books");
        assert_eq!(meta_columns().len(), meta_values(&meta).len());
        assert_eq!(meta_columns().len(), table_meta_fields().len());
    }

    #[test]
    fn a_new_overlay_is_admin_only() {
        // Creating an overlay row must never be the thing that widens access.
        let meta = TableMeta::new("books");
        assert_eq!(meta.access, AccessRules::default());
        assert_eq!(meta.access.min_role_read, 1);
        assert_eq!(meta.access.min_role_write, 1);
    }

    #[test]
    fn ownership_attributes_round_trip_through_their_accessors() {
        let mut meta = TableMeta::new("books");
        assert_eq!(meta.ownership_formula(), None);
        assert!(!meta.rls_enabled());

        meta.set_ownership_formula(Some("  owner === user.id  "));
        assert_eq!(meta.ownership_formula(), Some("owner === user.id"));
        meta.set_rls_enabled(true);
        assert!(meta.rls_enabled());

        // Clearing removes the keys entirely — absence is the false state, so
        // a table that never touched either carries no residue in attributes.
        meta.set_ownership_formula(None);
        meta.set_rls_enabled(false);
        assert!(meta.attributes.is_empty(), "{:?}", meta.attributes);

        // An empty or blank formula is a clear, not an empty formula.
        meta.set_ownership_formula(Some("   "));
        assert_eq!(meta.ownership_formula(), None);
        assert!(meta.attributes.is_empty());
    }

    #[test]
    fn a_provided_table_definition_round_trips_through_the_row() {
        let mut meta = TableMeta::new("headlines");
        // No provider is the ordinary case: a `_fd_tables` row is an overlay
        // unless it says otherwise.
        assert_eq!(meta.provider(), None);

        let mut configuration = Attrs::new();
        configuration.insert(
            "url".into(),
            Json::String("https://example.org/feed".into()),
        );
        let def = ProvidedTableDef::new("@saltcorn/rss", "RSS feed").configuration(configuration);
        meta.set_provider(Some(&def));
        assert_eq!(meta.provider().as_ref(), Some(&def));
        assert_eq!(
            meta.provider().unwrap().configuration_json(),
            serde_json::json!({ "url": "https://example.org/feed" })
        );

        // Clearing takes all three keys with it, so a table that stopped being
        // provided leaves no residue an admin would have to explain.
        meta.set_provider(None);
        assert_eq!(meta.provider(), None);
        assert!(meta.attributes.is_empty(), "{:?}", meta.attributes);
    }

    #[test]
    fn half_a_provider_is_no_provider_rather_than_a_table_nobody_can_serve() {
        // A row naming a module and no provider names nothing that can be
        // called. Reading it as a provided table would put a table in the
        // catalog whose rows can never be fetched; reading it as an overlay is
        // what it is.
        let mut meta = TableMeta::new("headlines");
        meta.attributes.insert(
            ATTR_PROVIDER_MODULE.into(),
            Json::String("@saltcorn/rss".into()),
        );
        assert_eq!(meta.provider(), None);
        meta.attributes
            .insert(ATTR_PROVIDER_NAME.into(), Json::String("  ".into()));
        assert_eq!(meta.provider(), None);
    }

    #[test]
    fn a_system_table_takes_a_row_only_as_a_metadata_table() {
        let mut meta = TableMeta::new("_fd_triggers");
        let err = check_metadata_rules("_fd_triggers", &meta).unwrap_err();
        assert!(err.to_string().contains("system table"), "{err}");

        meta.set_metadata_table(true);
        assert!(meta.is_metadata_table());
        assert!(check_metadata_rules("_fd_triggers", &meta).is_ok());

        // RLS is FORCE'd on every reader, the server included.
        meta.set_rls_enabled(true);
        let err = check_metadata_rules("_fd_triggers", &meta).unwrap_err();
        assert!(err.to_string().contains("row-level security"), "{err}");
        meta.set_rls_enabled(false);

        meta.set_provider(Some(&ProvidedTableDef::new("@saltcorn/rss", "RSS feed")));
        assert!(check_metadata_rules("_fd_triggers", &meta).is_err());
        meta.set_provider(None);

        // The flag on an ordinary table names nothing to expose.
        let mut books = TableMeta::new("books");
        books.set_metadata_table(true);
        assert!(check_metadata_rules("books", &books).is_err());

        // Clearing leaves no residue.
        meta.set_metadata_table(false);
        assert!(meta.attributes.is_empty(), "{:?}", meta.attributes);
    }

    #[test]
    fn roles_outside_the_scale_are_rejected_rather_than_clamped() {
        assert!(validate_access("books", &AccessRules::default()).is_ok());
        assert!(
            validate_access(
                "books",
                &AccessRules {
                    min_role_read: 100,
                    min_role_write: 1,
                },
            )
            .is_ok()
        );
        let err = validate_access(
            "books",
            &AccessRules {
                min_role_read: 0,
                min_role_write: 1,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains(COL_MIN_ROLE_READ), "{err}");
        assert!(
            validate_access(
                "books",
                &AccessRules {
                    min_role_read: 1,
                    min_role_write: 101,
                },
            )
            .is_err()
        );
    }
}
