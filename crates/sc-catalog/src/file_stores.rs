//! The `_fd_file_stores` table: its schema, bootstrap, and the
//! [`FileStoreDef`] ⇄ row mapping (design §9, §14.1).
//!
//! A file store, like an application (§13.2), has **nothing to introspect it
//! from**: `information_schema` knows about tables, not about which directories
//! an admin has connected. So — by the same §9 argument that gives
//! `_fd_applications` its existence — a store's row *is* the store's definition,
//! and this is the second stored-metadata table Saltcorn needs. It is not an
//! overlay: without the row there is no store at all.
//!
//! This module lives in `sc-catalog` rather than `sc-files`, where
//! [`FileStoreDef`] does, for a reason that is not negotiable: persistence needs
//! a [`Catalog`], and `sc-files` cannot have one, because `sc-catalog` already
//! depends on `sc-files`. `sc-catalog` is the lowest crate holding both the
//! `FileStore` trait and a `Catalog`, and it already owns the connected-store
//! registry ([`Catalog::connect_file_store`]), so definition and instance stay
//! within one crate of each other.
//!
//! **Reading is strict**, exactly as it is for applications: a column that is
//! missing or of the wrong shape is an [`Error::invalid`] naming the store and
//! the column, not a silently defaulted field. A half-understood row is a
//! misconfigured store the admin needs told about.
//!
//! **Deleting a definition never touches the bytes.** See
//! [`delete_file_store`].

use sc_db::Row;
use sc_error::{Error, Result};
use sc_files::{FileStoreDef, FileStoreDefId, connect_from_def, validate_file_store_config};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::Value as Json;

use crate::catalog::Catalog;
use crate::field::{DataField, DataFieldKind};
use crate::table::Table;

/// Name of the file-stores table in the primary database.
pub const FILE_STORES_TABLE: &str = "_fd_file_stores";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The store's name — the key it is connected and referenced under (§14.1).
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// The backend serving the store (`local`, later `s3`, …).
pub const COL_BACKEND: &str = "backend";
/// The backend's settings (JSON object).
pub const COL_CONFIG: &str = "config";
/// The store-wide minimum role, or NULL for unrestricted.
pub const COL_MIN_ROLE: &str = "min_role";
/// The sparse per-store values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// The fields of the `_fd_file_stores` table, in declaration order.
///
/// `name` carries the `UNIQUE` constraint, for the same reason an application's
/// `subdomain` does: it is the key everything resolves through — a `File` field's
/// `FileStoreId`, an application's store subset, the catalog's registry — so two
/// definitions claiming one name is not a state the system can serve. The
/// database is the authority because two admins saving concurrently cannot see
/// each other's transaction.
fn file_store_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_BACKEND, text()).required(),
        DataField::plain(COL_CONFIG, json()).required(),
        // Nullable: "no floor" is a real state, and distinct from role 100
        // (public) only in intent, but the admin UI shows them differently.
        DataField::plain(COL_MIN_ROLE, int()),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
        // When it was last saved (`edited`): bookkeeping for the entities list.
        crate::edited::updated_at_field(),
    ]
}

/// Ensure the `_fd_file_stores` table exists, creating it if absent, and return
/// it.
///
/// Idempotent, and safe against a database that has never seen Saltcorn — the
/// same contract as `sc_app::bootstrap`. Call once at startup, after the
/// [`Catalog`] is initialised and before loading stored stores. An existing
/// table is reconciled additively
/// ([`bootstrap_table`](Catalog::bootstrap_table)), so a column added by a later
/// release appears without a migration step.
pub async fn bootstrap_file_stores(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(FILE_STORES_TABLE, &file_store_fields())
        .await
}

/// Save a file-store definition: insert its row, or update it in place if a row
/// with its [`FileStoreDefId`] already exists.
///
/// The name is unique, and this checks for a clash first so the admin gets an
/// [`Error::invalid`] naming the store that already holds it rather than a raw
/// constraint violation. The database's `UNIQUE` constraint remains the
/// authority: this check and the write are not one transaction.
///
/// The backend's settings are checked against the spec that backend declares
/// ([`validate_file_store_config`]). This is the point of doing it on save: a
/// missing or ill-typed setting is the admin's to fix and the admin is standing
/// in front of the form, whereas the same mistake found at connect time is a
/// store that silently never comes up.
///
/// Saving does **not** connect the store, and deliberately does not require that
/// it *could* be connected. A definition can be saved and unconnected — that is
/// what a store with a since-unmounted path is, and it must stay editable in
/// that state, since editing it is how the admin fixes it. So the settings are
/// validated structurally here and reachability is left to
/// [`connect_from_def`](sc_files::connect_from_def).
pub async fn save_file_store(catalog: &Catalog, def: &FileStoreDef) -> Result<()> {
    check_file_store_saveable(catalog, def).await?;

    let columns = store_columns();
    let values = store_values(def);

    if load_file_store(catalog, def.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            // The id is the row's identity, not something to reassign.
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = sc_query::Update::new(FILE_STORES_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(def.id.0)));
        run(catalog, Statement::from(update)).await?;
    } else {
        let insert = Insert::row(
            FILE_STORES_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await?;
    }
    Ok(())
}

/// Everything [`save_file_store`] checks before it writes: a name, a backend,
/// settings that match that backend's spec, and no other store already holding
/// the name.
///
/// Public and separate because a caller sometimes needs to know a definition
/// *would* save before doing something irreversible. Creating a store whose
/// backend has to build something first — a git store's clone — is the case:
/// the clone must happen before the row exists, or a failed one leaves a row the
/// admin's corrected retry collides with, but cloning under a name that is
/// already taken would be work thrown away and a directory left behind. So the
/// cheap checks run first, then the creation, then the write.
///
/// Runs again inside `save_file_store`, which is deliberate: this is a
/// convenience for callers that want to look before they leap, never a
/// substitute for the check on the write path.
pub async fn check_file_store_saveable(catalog: &Catalog, def: &FileStoreDef) -> Result<()> {
    let name = def.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a file store needs a name"));
    }
    if def.backend.trim().is_empty() {
        return Err(Error::invalid(format!(
            "file store `{name}` needs a backend"
        )));
    }
    if name == NEW_LOCAL_FILE_STORE {
        return Err(Error::invalid(format!(
            "`{name}` is reserved: it is how the admin UI asks for a new store"
        )));
    }
    validate_file_store_config(def)?;

    if let Some(other) = load_file_store_by_name(catalog, name).await?
        && other.id != def.id
    {
        return Err(Error::invalid(format!(
            "file store name `{name}` is already used; \
             each store is connected under its own name"
        )));
    }
    Ok(())
}

/// Load the definition with this id, if it exists.
pub async fn load_file_store(
    catalog: &Catalog,
    id: FileStoreDefId,
) -> Result<Option<FileStoreDef>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the definition named `name`, if any — the lookup everything that
/// references a store by name needs.
pub async fn load_file_store_by_name(
    catalog: &Catalog,
    name: &str,
) -> Result<Option<FileStoreDef>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// Every stored definition, ordered by name — what the server connects at boot
/// and what the admin UI lists.
pub async fn list_file_stores(catalog: &Catalog) -> Result<Vec<FileStoreDef>> {
    let select = Select::from(Source::table(FILE_STORES_TABLE));
    let mut defs: Vec<FileStoreDef> = rows(catalog, select)
        .await?
        .iter()
        .map(file_store_from_row)
        .collect::<Result<_>>()?;
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(defs)
}

/// Delete a file-store definition, returning whether one was there to delete.
///
/// **This removes the row and nothing else.** The directory and every byte in it
/// are left exactly as they were: a store definition is a connection to data
/// that exists independently of Saltcorn — very often an admin's own directory,
/// possibly a git working tree — and disconnecting it is not consent to destroy
/// it. An admin who wants the files gone deletes them with the file manager.
///
/// Refuses if anything still references the store, naming the referents rather
/// than leaving them dangling. `extra_referents` is how references this crate
/// cannot see are supplied: `sc-catalog` can scan its own tables for `File`
/// fields (see [`file_store_field_references`]), but an *application*'s store
/// subset lives in `sc-app`, which sits above this crate and cannot be reached
/// from here. Callers that know about applications must collect those first —
/// `sc_app::applications_using_file_store` does exactly that — and pass them in.
/// A caller that passes an empty slice gets the catalog-level check only, which
/// is correct for a system with no applications and wrong for one with them.
///
/// Disconnecting the live store so it stops resolving is the caller's job (TODO
/// §1.3); this only removes the definition.
pub async fn delete_file_store(
    catalog: &Catalog,
    id: FileStoreDefId,
    extra_referents: &[String],
) -> Result<bool> {
    let Some(def) = load_file_store(catalog, id).await? else {
        return Ok(false);
    };

    let mut referents = file_store_field_references(catalog, &def.name)?;
    referents.extend(extra_referents.iter().cloned());
    if !referents.is_empty() {
        return Err(Error::invalid(format!(
            "file store `{}` is still used by {}; \
             remove those references before deleting it",
            def.name,
            referents.join(", ")
        )));
    }

    let delete = Delete::from(FILE_STORES_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(def.id.0)));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The outcome of connecting the stored file stores (see
/// [`connect_all_file_stores`]).
///
/// Both halves are reported because both are things the operator and the admin
/// UI need to know: a store that failed is not a store that stopped existing,
/// and boot must be able to say which ones came up and which did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileStoreConnections {
    /// Names of the stores that connected.
    pub connected: Vec<String>,
    /// Stores that did not connect, as `(name, reason)`.
    pub failed: Vec<(String, String)>,
}

impl FileStoreConnections {
    /// Whether every stored store connected.
    pub fn all_connected(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Connect one stored definition into the catalog's registry, recording the
/// reason on failure so the admin UI can show a defined-but-unusable store.
///
/// Returns the error as well as recording it, so a caller acting on one store —
/// an admin saving from the form — can react, while a caller sweeping every
/// store at boot can ignore the return and read the record later.
pub fn connect_file_store_def(catalog: &Catalog, def: &FileStoreDef) -> Result<()> {
    match connect_from_def(def) {
        Ok(store) => catalog.connect_file_store(store),
        Err(e) => {
            // The whole causal chain, not just `e.to_string()`: the outermost
            // context reads "connecting file store `docs`", which tells an admin
            // that it failed but not that the directory is missing or which one.
            // This message is shown in the UI, so it has to be actionable.
            catalog.record_file_store_error(&def.name, sc_error::format_causes(&e))?;
            Err(e)
        }
    }
}

/// Load every stored file-store definition and connect each one, returning what
/// happened.
///
/// **One store that fails must not stop the others, and must not stop the
/// server.** This is the same rule the MVP applied to an application whose build
/// fails (§13.2): a single unmounted disk is a thing for the admin to fix in the
/// UI, not a reason for a server that otherwise works to refuse to boot. So this
/// returns a report rather than an error, and only a failure to *read the table*
/// — which means the metadata itself is unreachable — is an `Err`.
///
/// Called at boot, and again whenever the whole set needs re-establishing.
/// Failures are recorded on the catalog, so the admin API can answer "why is
/// this store not connected?" long after boot has scrolled past.
pub async fn connect_all_file_stores(catalog: &Catalog) -> Result<FileStoreConnections> {
    let mut report = FileStoreConnections::default();
    for def in list_file_stores(catalog).await? {
        match connect_file_store_def(catalog, &def) {
            Ok(()) => report.connected.push(def.name),
            // Same reasoning as above: the report reaches the boot log and
            // the admin, so it carries the cause, not just the context.
            Err(e) => report.failed.push((def.name, sc_error::format_causes(&e))),
        }
    }
    Ok(report)
}

/// The [`ServerQuery`](sc_types::OptionsSource::ServerQuery) name that yields the
/// file stores an admin may choose from.
///
/// A constant because it is a contract between whoever *declares* a setting (a
/// framework, possibly in a guest language) and whoever *answers* it
/// ([`resolve_options`]) — the two are in different crates and, eventually,
/// different languages.
pub const QUERY_FILE_STORES: &str = "file_stores";

/// The prefix of a [`ServerQuery`](sc_types::OptionsSource::ServerQuery) whose
/// setting names a **file of a kind** in the application's own file store, as
/// a path relative to that store: `store_files:png,jpg,jpeg` is an app icon,
/// `store_files:jks,keystore,p12` a keystore.
///
/// Not answered by [`resolve_options`]: the files are a property of whichever
/// store the application's store setting names, which the form knows and a
/// framework's spec does not. The spec says which settings ask and for what
/// ([`store_file_settings`]); the form fills them from `listStoreFiles`. On save
/// the value is unrestricted, as for any query this server does not resolve.
pub const QUERY_STORE_FILES: &str = "store_files:";

/// The value a setting answered by [`QUERY_FILE_STORES`] carries to mean "no
/// store yet — create a local one for me", in place of a store's name.
///
/// Offered by the admin UI as the last choice in a new application's store
/// picker, and replaced by the server with the name of the store it creates
/// before anything is validated or saved, so no stored configuration ever holds
/// it. It is a *protocol* value — it travels in the JSON, and the UI's copy has to
/// match — and it is reserved as a store name ([`check_file_store_saveable`]), so
/// it can never be mistaken for a store that exists.
pub const NEW_LOCAL_FILE_STORE: &str = "__new_local_file_store__";

/// The settings in `spec` whose value names a file store — the ones answered by
/// [`QUERY_FILE_STORES`].
///
/// Read off the **unresolved** spec: [`resolve_options`] replaces the query with
/// the list it answers, and after that a store picker is indistinguishable from
/// any other select.
pub fn file_store_settings(spec: &[FormField]) -> Vec<String> {
    spec.iter()
        .filter(|field| field.query() == Some(QUERY_FILE_STORES))
        .map(|field| field.base.name.clone())
        .collect()
}

/// A setting that picks a file from the application's store (an app icon, a
/// keystore), and the file extensions it accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreFileSetting {
    /// The setting's name — `app_icon`.
    pub name: String,
    /// The extensions it accepts, lowercase and without the dot — `png`, `jpg`.
    pub extensions: Vec<String>,
}

/// The settings in `spec` whose value is a file in the application's store
/// ([`QUERY_STORE_FILES`]). Read off the unresolved spec, as
/// [`file_store_settings`] is.
pub fn store_file_settings(spec: &[FormField]) -> Vec<StoreFileSetting> {
    spec.iter()
        .filter_map(|field| {
            let extensions = field.query()?.strip_prefix(QUERY_STORE_FILES)?;
            Some(StoreFileSetting {
                name: field.base.name.clone(),
                extensions: extensions
                    .split(',')
                    .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect(),
            })
        })
        .collect()
}

/// A store name that is not in `taken`: `base` itself when it is free, otherwise
/// `base` with the first of 1, 2, 3, … appended that makes it free.
///
/// Used to name a store created on an application's behalf after the
/// application's subdomain, so the second app called `todo` gets `todo1` rather
/// than a refusal.
pub fn unique_file_store_name(base: &str, taken: &[String]) -> String {
    let free = |name: &str| !taken.iter().any(|t| t == name);
    if free(base) {
        return base.to_owned();
    }
    (1u64..)
        .map(|n| format!("{base}{n}"))
        .find(|candidate| free(candidate))
        .unwrap_or_else(|| base.to_owned())
}

/// Resolve every [`ServerQuery`](sc_types::OptionsSource::ServerQuery) in a
/// settings spec to a concrete list, leaving the rest untouched.
///
/// This is the single place the server answers "what may this setting be?", and
/// it runs at both points a spec is used: when the admin API hands a spec to the
/// UI (so the form renders a select), and when a config is validated on save (so
/// an impossible value is rejected). Doing it here is what keeps the *client*
/// from needing a query evaluator — the UI only ever sees resolved options — and
/// is why this did not have to wait for `ui/form-runtime` (§12).
///
/// An unknown query name resolves to **no options**, which leaves the field
/// unrestricted rather than rejecting every value. A spec declaring a query this
/// server does not know is a version skew (a plugin newer than the host), and
/// the safe reading of "I cannot answer that" is to not constrain — refusing
/// everything would make the extension unusable rather than merely unassisted.
pub async fn resolve_options(catalog: &Catalog, spec: Vec<FormField>) -> Result<Vec<FormField>> {
    let mut out = Vec::with_capacity(spec.len());
    for field in spec {
        match field.query() {
            Some(QUERY_FILE_STORES) => {
                let names = choosable_file_stores(catalog).await?;
                out.push(field.with_resolved_options(names));
            }
            Some(_) => out.push(field.with_resolved_options(Vec::<String>::new())),
            None => out.push(field),
        }
    }
    Ok(out)
}

/// The file stores an admin may pick: every **defined** store, plus any
/// connected without a definition (`--file-store`), sorted and deduplicated.
///
/// Defined-but-unconnected stores are included deliberately. A store whose disk
/// is currently unmounted is still a legitimate choice — §1.1 made exactly that
/// state savable and editable — and excluding it would mean an app could not be
/// configured against a store the admin is in the middle of repairing, or worse,
/// that re-saving an existing app started failing because of an unrelated
/// outage.
pub async fn choosable_file_stores(catalog: &Catalog) -> Result<Vec<String>> {
    // A catalog with no `_fd_file_stores` table has no stored definitions — that
    // is what its absence *means*, so answering "none" is correct rather than
    // lenient. Asking the catalog (a cache lookup) instead of running the query
    // and swallowing the failure keeps a genuine database error an error: this
    // is consulted while validating an app's config and while listing
    // frameworks, and neither should report "no stores exist" when the truth is
    // "the database is unreachable".
    let mut names: Vec<String> = if catalog.get(FILE_STORES_TABLE)?.is_some() {
        list_file_stores(catalog)
            .await?
            .into_iter()
            .map(|def| def.name)
            .collect()
    } else {
        Vec::new()
    };
    names.extend(catalog.file_store_names()?);
    names.sort();
    names.dedup();
    Ok(names)
}

/// Every `File` field in the catalog that references the store named `name`,
/// described as `table.field` — the catalog-level half of the reference check
/// [`delete_file_store`] performs.
///
/// Public because the admin UI wants to show what a store is used by *before*
/// the admin tries to delete it, not only as the error when they do.
///
/// **This finds nothing in the MVP, and the reason is worth knowing.**
/// [`DataFieldKind::File`] is modelled but persisted nowhere: the catalog builds
/// its fields from introspection, and a column cannot say "I am a path in store
/// `uploads`" — [`Table::from_physical`](crate::Table::from_physical) can only
/// derive `Plain` or `Key`. Storing that needs the `_fd_fields` overlay, which
/// §9 places out of MVP scope. So this scan is correct against whatever the
/// catalog holds and inert until the overlay exists, and the *application*-level
/// references passed as `extra_referents` are what actually protect a store
/// today. Do not read a passing delete as proof nothing points at the store.
pub fn file_store_field_references(catalog: &Catalog, name: &str) -> Result<Vec<String>> {
    let mut refs = Vec::new();
    for table in catalog.tables()? {
        for field in &table.fields {
            if let DataFieldKind::File { store, .. } = &field.kind
                && store.0 == name
            {
                refs.push(format!("{}.{}", table.name, field.base.name));
            }
        }
    }
    refs.sort();
    Ok(refs)
}

/// The row's columns, in the order [`store_values`] produces them.
fn store_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_BACKEND,
        COL_CONFIG,
        COL_MIN_ROLE,
        COL_ATTRIBUTES,
        crate::edited::COL_UPDATED_AT,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The definition serialised to its row's values, in [`store_columns`] order.
fn store_values(def: &FileStoreDef) -> Vec<Value> {
    vec![
        Value::Uuid(def.id.0),
        Value::Text(def.name.trim().to_owned()),
        Value::Text(def.description.clone()),
        Value::Text(def.backend.trim().to_owned()),
        Value::Json(Json::Object(def.config.clone())),
        match def.min_role {
            Some(role) => Value::Int(i64::from(role)),
            None => Value::Null,
        },
        Value::Json(Json::Object(def.attributes.clone())),
        // Every save is an edit (`sc_catalog::edited`).
        crate::edited::updated_now(),
    ]
}

/// Rebuild a [`FileStoreDef`] from its `_fd_file_stores` row. The strictness
/// note in the module docs applies throughout.
fn file_store_from_row(row: &Row) -> Result<FileStoreDef> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => FileStoreDefId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };

    // A NULL description is "none given", not a broken row.
    let description = match row.get(COL_DESCRIPTION) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad_column(COL_DESCRIPTION, "text", other)),
    };

    // A NULL min_role is "unrestricted". A value outside 1–100 is not clamped:
    // roles are a fixed scale (`sc-auth`), so an out-of-range one is a corrupt
    // row, and silently reading it as the nearest valid role would quietly
    // change who can reach the store.
    let min_role = match row.get(COL_MIN_ROLE) {
        Some(Value::Null) | None => None,
        Some(Value::Int(i)) => Some(u8::try_from(*i).ok().filter(|r| (1..=100).contains(r)).ok_or_else(
            || {
                Error::invalid(format!(
                    "{FILE_STORES_TABLE}.{COL_MIN_ROLE} should be a role between 1 and 100, got {i}"
                ))
            },
        )?),
        other => return Err(bad_column(COL_MIN_ROLE, "an integer role", other)),
    };

    Ok(FileStoreDef {
        id,
        name: text(row, COL_NAME)?,
        description,
        backend: text(row, COL_BACKEND)?,
        config: object(row, COL_CONFIG)?,
        min_role,
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

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{FILE_STORES_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{FILE_STORES_TABLE}.{column} should be {expected}, got {}",
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

/// Load the single definition matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<FileStoreDef>> {
    let select = Select::from(Source::table(FILE_STORES_TABLE))
        .filter(filter)
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(file_store_from_row(row)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = file_store_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        // §9: every system metadata table MUST have id (UUID), name,
        // description, attributes (JSON object).
        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
        // A description is optional; NULL reads back as "".
        assert!(!by_name(COL_DESCRIPTION).required);
    }

    #[test]
    fn a_taken_name_gets_the_first_free_number_appended() {
        let taken = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert_eq!(unique_file_store_name("todo", &taken(&[])), "todo");
        assert_eq!(unique_file_store_name("todo", &taken(&["apps"])), "todo");
        assert_eq!(unique_file_store_name("todo", &taken(&["todo"])), "todo1");
        assert_eq!(
            unique_file_store_name("todo", &taken(&["todo", "todo1", "todo3"])),
            "todo2"
        );
    }

    #[test]
    fn the_store_settings_are_the_ones_the_file_store_query_answers() {
        let spec = [
            FormField::new("store", BasicType::Text).server_query(QUERY_FILE_STORES),
            FormField::new("project", BasicType::Text),
            FormField::new("other", BasicType::Text).server_query("something_else"),
        ];
        assert_eq!(file_store_settings(&spec), ["store"]);
        // Once resolved, a picker is just a select — which is why this reads the
        // spec before `resolve_options` does.
        let resolved = spec[0].clone().with_resolved_options(["apps"]);
        assert!(file_store_settings(&[resolved]).is_empty());
    }

    #[test]
    fn name_is_the_unique_key() {
        // Everything resolves a store by name, so two rows cannot claim one.
        let fields = file_store_fields();
        let name = fields.iter().find(|f| f.base.name == COL_NAME).unwrap();
        assert!(name.required && name.unique);
        assert_eq!(name.base.type_, TypeRef::Basic(BasicType::Text));
    }

    #[test]
    fn min_role_is_nullable_because_unrestricted_is_a_real_state() {
        let fields = file_store_fields();
        let min_role = fields.iter().find(|f| f.base.name == COL_MIN_ROLE).unwrap();
        assert!(!min_role.required);
        assert_eq!(min_role.base.type_, TypeRef::Basic(BasicType::Int));
    }

    #[test]
    fn the_table_is_a_hidden_system_table() {
        assert!(FILE_STORES_TABLE.starts_with("_fd_"));
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        // The insert pairs these two positionally, so a column added to one and
        // not the other would write settings into the wrong column.
        let def = FileStoreDef::local("apps", "/srv/apps");
        assert_eq!(store_columns().len(), store_values(&def).len());
        assert_eq!(store_columns().len(), file_store_fields().len());
    }

    #[test]
    fn an_unset_min_role_is_written_as_null() {
        let values = store_values(&FileStoreDef::local("apps", "/srv/apps"));
        let idx = store_columns()
            .iter()
            .position(|c| c == COL_MIN_ROLE)
            .unwrap();
        assert_eq!(values[idx], Value::Null);

        let restricted = FileStoreDef::local("apps", "/srv/apps").min_role(40);
        assert_eq!(store_values(&restricted)[idx], Value::Int(40));
    }
}
