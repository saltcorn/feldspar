//! The [`Catalog`]: the hub that owns the connected database and an in-memory
//! cache of its tables.
//!
//! The catalog is read from a [`DatabaseDriver`]'s introspection, with the
//! `_fd_tables` / `_fd_fields` overlays on top (technical design §5, §8, §9). As
//! soon as a database is connected every table is usable; there is no discovery
//! or registration step. The cache is rebuilt on [`init`](Catalog::init) and
//! after every schema change the catalog makes, so it always reflects the live
//! schema. The server is one process, so the cache is swapped in place under a
//! lock; nothing outside the process invalidates it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sc_db::{DatabaseDriver, SchemaChange};
use sc_error::{Error, Result};
use sc_files::FileStore;
use sc_query::{Select, Source, Statement};

use crate::field::{DataField, DbId};
use crate::field_meta::{FIELD_META_TABLE, list_field_meta};
use crate::provider::{DriverTableProvider, TableProvider};
use crate::services::Services;
use crate::table::{FieldMergeIssue, Table};
use crate::table_meta::{TABLE_META_TABLE, list_table_meta};

/// The catalog: the connected primary database plus a cache of its tables
/// (technical design §8.1). Row data for users, workflow runs, and files is
/// deliberately not cached; the cache holds tables only. Connected
/// **file stores** (§14.1) are registered here too — not their contents, just
/// the named store handles — so the server can resolve a store by name.
pub struct Catalog {
    /// The primary database's driver.
    primary: Arc<dyn DatabaseDriver>,
    /// The primary database's id, stamped onto every [`Table`] it hosts.
    primary_db: DbId,
    /// The **secondary** databases an admin has connected (§9's Connections
    /// screen), keyed by the connection's name — which is also the [`DbId`]
    /// stamped onto every table they contribute.
    ///
    /// They are held here, beside the primary, because that is what makes their
    /// tables ordinary: [`reload`](Catalog::reload) introspects them into the
    /// same cache, so a foreign table is found by
    /// [`get`](Catalog::get) and served by [`provider`](Catalog::provider) with
    /// nothing above this crate knowing which database it came from — except
    /// where it should, which is the badge in the admin UI and the refusal to
    /// run DDL against it.
    databases: RwLock<HashMap<String, Arc<dyn DatabaseDriver>>>,
    /// Why a *stored* connection is **not** in [`databases`], keyed by name —
    /// the same arrangement, for the same reason, as
    /// [`file_store_errors`](Catalog::file_store_errors).
    database_errors: RwLock<HashMap<String, String>>,
    /// The schema as of the last [`reload`](Catalog::reload): the tables, what
    /// did not merge, which tables a connection lost to a name clash, and the
    /// generation stamp — one immutable value, replaced whole by the reload that
    /// built the next one, so a reader never sees half of one reload and half of
    /// another. See [`CatalogSchema`].
    schema: RwLock<Arc<CatalogSchema>>,
    /// Connected file stores, keyed by their [`name`](FileStore::name). Only the
    /// store handles live here (files keep no database row, design §9); the bytes
    /// and per-file metadata stay in the store itself.
    file_stores: RwLock<HashMap<String, Arc<dyn FileStore>>>,
    /// Why a stored file store is **not** in [`file_stores`], keyed by name.
    ///
    /// A store that failed to connect must not vanish silently: its definition is
    /// still there, the admin still needs to see it in the list, and the reason —
    /// "directory /srv/docs does not exist" — is the only thing that tells them
    /// what to fix. Absence of a name here means either "connected" or "never
    /// attempted"; the definition list plus [`file_store`](Self::file_store)
    /// distinguishes those.
    file_store_errors: RwLock<HashMap<String, String>>,
    /// The services the layers above install: the write and schema observers,
    /// the module, live, model and admin hosts, the formula evaluator, the table
    /// providers and the typed extensions. One value, so
    /// [`teardown`](Catalog::teardown) empties all of it (see [`Services`]).
    services: RwLock<Services>,
    /// The code libraries a body may import (TODO F3), as last loaded from
    /// `_fd_code_libraries` — `None` in a process that never loaded them. See
    /// [`crate::code_libraries`].
    pub(crate) code_libraries: RwLock<Option<Arc<crate::code_libraries::LoadedLibraries>>>,
    /// Which tables the installed modules **declared** (TODO-imap M4.3), and
    /// which of their fields — what refuses dropping them, and what the table
    /// list badges. Installed whole on every module reload, like the table
    /// providers; empty in a process with no such module.
    module_tables: RwLock<Arc<crate::module_tables::ModuleTables>>,
    /// Where this deployment's applications are reachable in a browser
    /// (`crate::origin`), set once at boot by the process that knows — the
    /// server from its command line, a command-line build from its
    /// `feldspar.toml` environment. `None` where nobody said, which is a normal
    /// state: a server with no base domain serves no applications.
    public_origin: RwLock<Option<crate::PublicOrigin>>,
    /// Whether this process serves an environment marked `production` in
    /// `feldspar.toml`: a checklist that writes data is
    /// refused here unless it is allowed on production. Set once at boot by the
    /// process that read the file; `false` where nobody said.
    production: std::sync::atomic::AtomicBool,
    /// Configuration values this **host** pins over `_fd_config` — the TLS keys
    /// an `[environments.*]` section of `feldspar.toml` may give. Set once at
    /// boot by the process that read the file, and read by `sc-config`, which
    /// is what gives the values their meaning; empty where nothing is pinned.
    host_config: RwLock<sc_types::Attrs>,
    /// The serialised schema snapshot for [`generation`](Catalog::generation),
    /// built once and shared by every run at that generation.
    ///
    /// Held here for [`table_events`](Catalog::add_table_events)' reason,
    /// inverted the same way: what *builds* it is `sc-api` (layer 8), which
    /// knows what v1 calls each of a field's properties; what needs it is every
    /// code body, workflow step and module action; and the catalog is what they
    /// all already hold. The stamp on the snapshot is what makes a stale one
    /// unusable rather than wrong — [`code_schema`](Catalog::code_schema) hands
    /// back nothing once the generation has moved on.
    ///
    /// [`table_events`]: Catalog::add_table_events
    code_schema: RwLock<Option<Arc<sc_expr::SchemaSnapshot>>>,
    /// Whether anything in this database wants the workflow engine, and when
    /// ([`crate::RunWakeups`]).
    ///
    /// Not table metadata, and here for the reason [`table_events`] is: the
    /// engine's queue (layer 9) polls it and the run store (layer 6) maintains
    /// it, neither may name the other, and the catalog is what both already
    /// hold. Without it a deployment with no runs at all pays an SQL round trip
    /// every few seconds to be told so.
    ///
    /// [`table_events`]: Catalog::add_table_events
    run_wakeups: crate::RunWakeups,
    /// The instance key Secret fields are sealed with (TODO-imap M2), read
    /// from — or created at — [`secret_key_path`](crate::secrets::secret_key_path)
    /// the first time it is needed, unless one was
    /// [installed](Catalog::set_secret_key) first.
    secret_key: RwLock<Option<Arc<crate::SecretKey>>>,
}

/// Why a **provided** table (§8.3) is not the table an admin expected.
///
/// Reported rather than thrown, on [`FieldMergeIssue`]'s grounds: the table is
/// still in the catalog, still in the list, and still the thing the admin has to
/// open to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidedTableIssue {
    /// The table it is about.
    pub table: String,
    /// What is wrong, in a sentence an admin can act on.
    pub problem: String,
}

/// The schema a catalog presents, as one [`reload`](Catalog::reload) built it.
///
/// Immutable: a reload builds a new one and swaps it in whole, so the tables,
/// the merge issues, the shadowed names and the generation a reader is handed
/// all come from the same reload. [`Catalog::schema`] hands out the current
/// one; a caller that needs two of its parts to agree reads both from it.
#[derive(Debug, Clone, Default)]
pub struct CatalogSchema {
    /// The tables, keyed by name: an [`Ident`](sc_query::Ident) each, so
    /// every table the catalog serves has a name a database can hold (security
    /// review 2026-11 §7). Looked up by `&str`.
    tables: HashMap<sc_query::Ident, Table>,
    /// The `_fd_fields` overlay rows that did not cleanly merge (design §3.2).
    field_overlay_issues: Vec<FieldMergeIssue>,
    /// Tables a connection offered that the catalog did not adopt, keyed by
    /// connection name, because a table of that name was already there. The
    /// primary always wins and the loser is named rather than dropped in
    /// silence.
    shadowed_tables: HashMap<String, Vec<String>>,
    /// Why a stored provided table is not the table an admin expected.
    provided_table_issues: Vec<ProvidedTableIssue>,
    /// How many times the catalog had been loaded when this was built — the
    /// **generation stamp**: one integer comparison answers "does this isolate
    /// already have the schema?" (TODO "the v1 `Table` API" §2). Monotonic,
    /// never reset, and no part of any identity: it says *when*, not *what*.
    generation: u64,
}

impl CatalogSchema {
    /// The table called `name`, if there is one.
    pub fn get(&self, name: &str) -> Option<&Table> {
        self.tables.get(name)
    }

    /// Every table, sorted by name. Includes system (`_fd_*`) tables.
    pub fn tables(&self) -> Vec<&Table> {
        let mut tables: Vec<&Table> = self.tables.values().collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        tables
    }

    /// The `_fd_fields` overlay rows that did not cleanly merge.
    pub fn field_overlay_issues(&self) -> &[FieldMergeIssue] {
        &self.field_overlay_issues
    }

    /// The tables the connection `name` offered and the catalog did not adopt.
    pub fn shadowed_tables(&self, name: &str) -> &[String] {
        self.shadowed_tables.get(name).map_or(&[], Vec::as_slice)
    }

    /// What went wrong building the provided tables.
    pub fn provided_table_issues(&self) -> &[ProvidedTableIssue] {
        &self.provided_table_issues
    }

    /// The generation this schema was built at.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// The configuration a provided table's provider is called with — the
/// `provider_config` attribute the `_fd_tables` row carries, as the JSON object
/// v1 hands to `fields(cfg)` and `get_table(cfg)`.
fn provided_config(table: &Table) -> serde_json::Value {
    match table
        .attributes
        .get(crate::table_meta::ATTR_PROVIDER_CONFIG)
    {
        Some(serde_json::Value::Object(map)) => serde_json::Value::Object(map.clone()),
        _ => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Apply one schema change on its own. On SQLite it runs in a transaction that
/// keeps the altered table's change-log triggers (see
/// [`Catalog::apply_schema_batch_in`]).
async fn apply_one(driver: &dyn DatabaseDriver, change: &SchemaChange) -> Result<()> {
    if !driver.dialect().is_sqlite() {
        return driver.apply_schema(change).await;
    }
    let mut tx = driver.begin().await?;
    let outcome = crate::change_log_sqlite::apply_keeping_triggers(tx.as_mut(), change).await;
    sc_db::finish(tx, outcome).await
}

/// One step of a transactional schema batch ([`Catalog::apply_schema_batch`]).
///
/// Two shapes, because the DDL a schema change needs comes in two: the
/// structured [`SchemaChange`]s the driver renders, and the raw SQL that models
/// what `SchemaChange` deliberately does not — the row-level-security policies,
/// whose `CREATE POLICY` carries an arbitrary boolean expression (§7.3). Both go
/// through the same [`Transaction`](sc_db::Transaction), in the order given.
#[derive(Debug, Clone)]
pub enum SchemaStep {
    /// A structured create/drop table or add/drop column.
    Change(SchemaChange),
    /// Raw DDL generated by trusted code: policies, triggers, indexes the
    /// structured change cannot express. A [`TrustedSql`](sc_query::TrustedSql),
    /// like a [`Statement::Raw`](sc_query::Statement), because it reaches the
    /// multi-statement executor (security review 2026-11 D2): a `String` from
    /// anywhere no longer does.
    Sql(sc_query::TrustedSql),
}

/// A database's SQL dialect and capabilities, without its driver
/// ([`Catalog::driver_info`]): what a caller that renders or plans a statement
/// needs to know about the database, and nothing it could run a statement with.
#[derive(Clone)]
pub struct DriverInfo(Arc<dyn DatabaseDriver>);

impl DriverInfo {
    /// The database's SQL dialect.
    pub fn dialect(&self) -> &dyn sc_query::SqlDialect {
        self.0.dialect()
    }

    /// What the database supports.
    pub fn capabilities(&self) -> sc_db::DbCapabilities {
        self.0.capabilities()
    }

    /// Whether this is a SQLite database.
    pub fn is_sqlite(&self) -> bool {
        self.0.dialect().is_sqlite()
    }

    /// The DDL `change` would run, rendered and not run.
    pub fn render_ddl(&self, change: &sc_db::SchemaChange) -> Result<String> {
        self.0.render_ddl(change)
    }

    /// Whether the database can store geometries (PostGIS), and why not.
    pub fn spatial(&self) -> sc_db::SpatialSupport {
        self.0.spatial()
    }

    /// Whether the database can store vectors (pgvector), and why not.
    pub fn vector(&self) -> sc_db::VectorSupport {
        self.0.vector()
    }
}

impl Catalog {
    /// Build a catalog from a primary driver, loading its tables from
    /// introspection.
    pub async fn init(primary: Arc<dyn DatabaseDriver>) -> Result<Catalog> {
        let catalog = Catalog {
            primary,
            primary_db: DbId::primary(),
            databases: RwLock::new(HashMap::new()),
            database_errors: RwLock::new(HashMap::new()),
            file_stores: RwLock::new(HashMap::new()),
            file_store_errors: RwLock::new(HashMap::new()),
            code_libraries: RwLock::new(None),
            module_tables: RwLock::new(Arc::new(crate::module_tables::ModuleTables::new())),
            public_origin: RwLock::new(None),
            production: std::sync::atomic::AtomicBool::new(false),
            host_config: RwLock::new(sc_types::Attrs::new()),
            code_schema: RwLock::new(None),
            run_wakeups: crate::RunWakeups::new(),
            secret_key: RwLock::new(None),
            schema: RwLock::new(Arc::new(CatalogSchema::default())),
            services: RwLock::new(Services::default()),
        };
        // Whether the database has PostGIS. Asking is not
        // installing it — `bootstrap_spatial` does that, on a server's boot —
        // and a database that cannot say is one without geometry, not a
        // catalog that cannot start.
        let _ = catalog.primary.detect_spatial().await;
        // And pgvector (TODO-agent-skills AS2.2), on the same terms.
        let _ = catalog.primary.detect_vector().await;
        catalog.reload().await?;
        Ok(catalog)
    }

    /// The primary database driver: every statement it runs reaches every row,
    /// so it takes an [`Unchecked`](crate::Unchecked).
    ///
    /// A crate's own internal tables (`_fd_*`) are reached through one
    /// crate-private helper per crate that makes the token once, with the
    /// reason, and is listed in docs/SECURITY_REVIEW_2026-10.md's inventory.
    /// What a caller needs to know about the database without running a
    /// statement — its dialect, its capabilities — is
    /// [`primary_info`](Catalog::primary_info).
    pub fn primary(&self, _access: crate::Unchecked<'_>) -> &Arc<dyn DatabaseDriver> {
        &self.primary
    }

    /// [`primary`](Catalog::primary) for this crate's own statements.
    pub(crate) fn primary_driver(&self) -> &Arc<dyn DatabaseDriver> {
        &self.primary
    }

    /// The primary database's SQL dialect.
    pub fn primary_dialect(&self) -> &dyn sc_query::SqlDialect {
        self.primary.dialect()
    }

    /// The primary database's dialect and capabilities, without its driver.
    pub fn primary_info(&self) -> DriverInfo {
        DriverInfo(Arc::clone(&self.primary))
    }

    /// The id of the primary database — what a table's
    /// [`database`](crate::Table::database) is compared against to ask whether it
    /// lives there (a [`SharedTx`](crate::SharedTx) serves one database, and a
    /// table on another connection is not reachable from it).
    pub fn primary_db(&self) -> &DbId {
        &self.primary_db
    }

    /// Re-introspect the primary database and rebuild the table cache, applying
    /// the `_fd_tables` overlay on top. Called after every schema change the
    /// catalog applies, and after every overlay change, so the cache never
    /// drifts from either source.
    ///
    /// **Introspection is still what makes a table exist.** The overlay only
    /// adds to a table already found, so a database with no `_fd_tables` table
    /// — or one whose rows describe tables that are not there — behaves exactly
    /// as it did before the overlay existed. That is the zero-setup promise of
    /// §9 in one line of code: the loop below can only ever modify entries the
    /// introspection loop above it created.
    #[allow(clippy::too_many_lines)]
    pub async fn reload(&self) -> Result<()> {
        let physicals = self.primary.introspect().await?;
        let mut map: HashMap<sc_query::Ident, Table> = HashMap::with_capacity(physicals.len());
        for physical in &physicals {
            let table = Table::from_physical(self.primary_db.clone(), physical);
            map.insert(sc_query::Ident::existing(&table.name)?, table);
        }

        // Then every secondary connection, on the same terms: introspection is
        // what makes a table exist here too, so a connected database needs no
        // registration step and a disconnected one contributes nothing.
        //
        // **The primary wins every name.** It hosts `users` and the `_fd_*`
        // tables, and a foreign table quietly taking one of those names would
        // repoint authentication at somebody else's database. So the insert is
        // conditional and the losers are recorded (see `shadowed_tables`).
        // Connections are read out of the lock first: introspection awaits, and
        // the guard must not be held across it.
        let mut shadowed: HashMap<String, Vec<String>> = HashMap::new();
        for (name, driver) in self.connected_databases()? {
            let foreign = match driver.introspect().await {
                Ok(tables) => {
                    self.clear_database_error(&name)?;
                    tables
                }
                // A database that was reachable when it was connected and is not
                // now is exactly the file-store case: the connection stays
                // defined, its tables drop out of the catalog, and the reason is
                // recorded for the admin to read. Failing the whole reload would
                // take the primary database's tables down with it.
                Err(e) => {
                    self.record_database_error(&name, e.to_string())?;
                    continue;
                }
            };
            for physical in &foreign {
                let table = Table::from_physical(DbId(name.clone()), physical);
                if map.contains_key(table.name.as_str()) {
                    shadowed
                        .entry(name.clone())
                        .or_default()
                        .push(table.name.clone());
                    continue;
                }
                map.insert(sc_query::Ident::existing(&table.name)?, table);
            }
        }
        for names in shadowed.values_mut() {
            names.sort();
        }

        // Only query the overlay when the database has one. Asking first is not
        // defensiveness — it is required: `bootstrap_table_meta` creates the
        // table *through* `create_table`, which reloads, so this runs at least
        // once on a database where `_fd_tables` genuinely does not exist yet.
        // Selecting from it there would make bootstrapping impossible.
        let mut provided_issues: Vec<ProvidedTableIssue> = Vec::new();
        if map.contains_key(TABLE_META_TABLE) {
            for meta in list_table_meta(self).await? {
                // A row carrying a provider is a **definition**, not an overlay:
                // it is what makes the table exist, so the table is built here
                // rather than looked up. Everything else about the row — the
                // label, the description, the access rules — then applies to it
                // exactly as it would to an introspected table.
                if let Some(def) = meta.provider() {
                    // A stored row's name, loaded as it was stored: only a
                    // NUL, which no database holds in a name, is refused —
                    // and then this one table is not served, said why.
                    let id = match sc_query::Ident::existing(&meta.table_name) {
                        Ok(id) => id,
                        Err(e) => {
                            provided_issues.push(ProvidedTableIssue {
                                table: meta.table_name.clone(),
                                problem: e.sentence("provided table"),
                            });
                            continue;
                        }
                    };
                    if map.contains_key(&id) {
                        // The database wins the name, exactly as the primary wins
                        // a name a secondary connection offers, and for the same
                        // reason: the rows that are already there are somebody's
                        // data, and shadowing them would repoint every read of
                        // that name at a feed. Recorded rather than silent.
                        provided_issues.push(ProvidedTableIssue {
                            table: meta.table_name.clone(),
                            problem: format!(
                                "a table called `{}` already exists in the database, so the \
                                 provided table of that name is not served; rename or delete one \
                                 of them",
                                meta.table_name
                            ),
                        });
                        continue;
                    }
                    let (fields, fields_issue) = self.provided_fields(&meta.table_name, &def).await;
                    // And what it may be *written* through: v1 decides that
                    // inside `get_table(cfg)`, so it is a second question with
                    // the same shape as the first, and a provider that could not
                    // be reached answers "nothing" rather than blocking the
                    // reload.
                    //
                    // Asked only when the first was answered: a module that is
                    // gone fails both, and one table with one thing wrong with
                    // it should put one sentence in front of the admin.
                    let (writes, writes_issue) = match fields_issue.is_some() {
                        true => (crate::provider::ProvidedWrites::NONE, None),
                        false => self.provided_writes(&meta.table_name, &def).await,
                    };
                    for problem in fields_issue.into_iter().chain(writes_issue) {
                        provided_issues.push(ProvidedTableIssue {
                            table: meta.table_name.clone(),
                            problem,
                        });
                    }
                    let mut table = Table::provided(
                        self.primary_db.clone(),
                        &meta.table_name,
                        &def.module,
                        &def.provider,
                        fields,
                        writes,
                    );
                    table.apply_overlay(&meta);
                    map.insert(id, table);
                    continue;
                }
                // An overlay for a table that is not here is not an error and is
                // not dropped; see `orphan_table_meta` for why it is kept.
                if let Some(table) = map.get_mut(meta.table_name.as_str()) {
                    table.apply_overlay(&meta);
                }
            }
        }

        // Then the `_fd_fields` overlay, onto the fields the table now has. Same
        // "only when the table exists" guard and same bootstrapping reason as
        // above. The merge reports rather than fails, so its issues are collected
        // here and stored beside the cache.
        let mut field_issues = Vec::new();
        if map.contains_key(FIELD_META_TABLE) {
            for meta in list_field_meta(self).await? {
                match map.get_mut(meta.table_name.as_str()) {
                    Some(table) => field_issues.extend(table.apply_field_overlay(&meta)),
                    // A field overlay whose whole table is gone is a dangling row
                    // too — kept (like an orphan table overlay) and reported.
                    None => field_issues.push(FieldMergeIssue {
                        table: meta.table_name.clone(),
                        field: meta.field_name.clone(),
                        message: format!(
                            "field overlay names table `{}`, which is not in the catalog",
                            meta.table_name
                        ),
                    }),
                }
            }
        }

        // Calculated fields (Phase 8) are validated and dependency-ordered here,
        // after every field overlay has merged and before ownership validation —
        // an ownership formula may reference a calc field, so the calc fields
        // must have settled first. Invalid ones are dropped (fail closed) and
        // reported like any other field-overlay issue. The shape passed in still
        // contains them, so a calc field reading another resolves.
        let calc_shape = self.with_module_functions(schema_shape_of(&map));
        field_issues.extend(crate::calc::merge_calc_fields(&mut map, &calc_shape));

        // Ownership formulas were *parsed* by `apply_overlay`; validation needs
        // the whole schema (a Ⱶ-path crosses tables), so it runs here, after
        // every table has merged. A formula that fails validation is cleared —
        // it **grants nothing** (fail closed) — and the reason is left on the
        // table for the admin UI, exactly like a field-overlay issue: reported,
        // never fatal, the table stays usable at its `min_role`s.
        let shape = self.with_module_functions(schema_shape_of(&map));
        let mut ownership_errors: Vec<(sc_query::Ident, String)> = Vec::new();
        for (id, table) in &map {
            let Some(formula) = &table.ownership else {
                continue;
            };
            match formula.validate(&shape, &table.name) {
                Err(e) => ownership_errors.push((id.clone(), e.to_string())),
                // An ownership formula may not call a module function (§4b),
                // and the check is here as well as on save because a module can
                // be *installed* after a formula was stored: the same source
                // that validated yesterday would start calling a module today.
                // Cleared, like any other invalid rule, so it grants nothing.
                Ok(analysis) => {
                    // `_request` only as `column === _request.key`
                    // (TODO-type-hardening H7.5): checked on save, and here for
                    // a formula that came in another way (a restore). The keys
                    // it reads are the GUCs `set_caller_context` sets.
                    match sc_expr::check_request_uses(formula, &shape, &table.name) {
                        Err(e) => {
                            ownership_errors.push((id.clone(), e.to_string()));
                            continue;
                        }
                        Ok(uses) => crate::rls::note_request_keys(uses.keys),
                    }
                    if let Some(call) = analysis.first_module_call() {
                        ownership_errors.push((
                            id.clone(),
                            format!(
                                "an ownership formula may not call the module function `{}`: a \
                                 rule that decides who may read a row must fail closed, so \
                                 a module that is down would deny every read of this table",
                                call.function
                            ),
                        ));
                    } else if let Some(call) = analysis.first_model_call() {
                        // The same rule for a prediction (milestone 31 §4): a
                        // provider that is slow or has no active fit would
                        // deny every read.
                        ownership_errors.push((
                            id.clone(),
                            format!(
                                "an ownership formula may not call `{}`: a rule that decides \
                                 who may read a row must fail closed, so a model that cannot \
                                 answer would deny every read of this table",
                                call.key
                            ),
                        ));
                    }
                }
            }
        }
        for (id, message) in ownership_errors {
            if let Some(table) = map.get_mut(&id) {
                table.ownership = None;
                table.ownership_error = Some(message);
            }
        }

        // The DB I/O is done; take the lock only to swap the new schema in, so
        // it is never held across an await. The generation is the previous
        // one's plus one, read under the same lock, so two reloads racing each
        // other still number their results in the order they were swapped in.
        {
            let mut guard = self
                .schema
                .write()
                .map_err(|_| Error::msg("catalog schema lock poisoned"))?;
            let generation = guard.generation + 1;
            *guard = Arc::new(CatalogSchema {
                tables: map,
                field_overlay_issues: field_issues,
                shadowed_tables: shadowed,
                provided_table_issues: provided_issues,
                generation,
            });
        }
        // The code-schema snapshot built at the old generation is dropped here
        // rather than left to be recognised as stale, so a reloaded catalog is
        // not also holding a megabyte of the schema it used to have. One built
        // across the swap carries the old stamp and is never handed out.
        let mut schema = self
            .code_schema
            .write()
            .map_err(|_| Error::msg("catalog code-schema lock poisoned"))?;
        *schema = None;
        Ok(())
    }

    /// Whether [`reload`](Self::reload) reads the system table `name` — that is,
    /// whether a change to its rows changes what the catalog presents. Only the
    /// two overlay tables: everything else `reload` knows, it introspects.
    ///
    /// A caller writing such a table's rows directly (not through
    /// `save_table_meta` / `save_field_meta`, which reload themselves) reloads
    /// when this says so, and not otherwise.
    pub fn reload_reads(name: &str) -> bool {
        name == TABLE_META_TABLE || name == FIELD_META_TABLE
    }

    /// How many times this catalog has been loaded — the **generation stamp**
    /// (see [`generation`](Catalog::generation)). One past the last reload, and
    /// therefore never 0 on a catalog that [`init`](Catalog::init) built.
    pub fn generation(&self) -> u64 {
        self.schema().generation
    }

    /// The schema as the last [`reload`](Catalog::reload) built it — one
    /// immutable value, so everything read from it comes from the same reload.
    /// A poisoned lock reads as the empty schema of generation 0, which no
    /// table lookup mistakes for a table.
    pub fn schema(&self) -> Arc<CatalogSchema> {
        match self.schema.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(_) => Arc::new(CatalogSchema::default()),
        }
    }

    /// The schema snapshot for the **current** generation, or `None` when
    /// nothing has built one since the last reload.
    ///
    /// Stale is `None` rather than stale: a snapshot stamped with a generation
    /// this catalog has moved past describes tables that may no longer be
    /// there, and handing it to a guest would be a `Table.findOne` answering
    /// confidently about a column that was dropped.
    pub fn code_schema(&self) -> Option<Arc<sc_expr::SchemaSnapshot>> {
        let held = self.code_schema.read().ok()?.clone()?;
        (held.generation() == self.generation()).then_some(held)
    }

    /// Record a snapshot as this catalog's, for the next run at its generation
    /// to be handed without building it again.
    ///
    /// Stored whatever its stamp, and read back only when the stamp still
    /// matches: a snapshot built across a reload is simply never handed out,
    /// and the next caller builds one that is.
    pub fn set_code_schema(&self, snapshot: Arc<sc_expr::SchemaSnapshot>) {
        if let Ok(mut guard) = self.code_schema.write() {
            *guard = Some(snapshot);
        }
    }

    /// The `_fd_fields` overlay rows that did not cleanly merge on the last
    /// [`reload`](Self::reload), for the admin UI to surface (design §3.2). Empty
    /// when every stored field overlay applied cleanly, which is the ordinary
    /// case.
    pub fn field_overlay_issues(&self) -> Result<Vec<FieldMergeIssue>> {
        Ok(self.schema().field_overlay_issues.clone())
    }

    /// The catalog described as an `sc_expr` [`SchemaShape`] — what formula
    /// validation and translation (§7.3) see: every table's fields, each Key
    /// field's target, and the user object's fields. This is the projection
    /// that keeps `sc-expr` below the catalog in the dependency graph: the
    /// catalog describes tables *to* it, never the other way around.
    pub fn schema_shape(&self) -> Result<sc_expr::SchemaShape> {
        Ok(self.with_module_functions(schema_shape_of(&self.schema().tables)))
    }

    /// The module functions installed on this catalog, as `(function, module)`
    /// pairs — what a [`SchemaShape`](sc_expr::SchemaShape) declares so a
    /// formula may call one (§4b). Empty on a server with no modules.
    pub(crate) fn module_function_names(&self) -> Vec<(String, String)> {
        self.module_functions().map_or_else(Vec::new, |host| {
            host.functions()
                .into_iter()
                .map(|function| (function.name, function.module))
                .collect()
        })
    }

    /// `shape`, with this catalog's module functions declared on it.
    fn with_module_functions(&self, shape: sc_expr::SchemaShape) -> sc_expr::SchemaShape {
        let mut shape = shape;
        for (name, module) in self.module_function_names() {
            shape = shape.module_function(name, module);
        }
        shape
    }

    /// Each user field mapped to its SQL type — what the GUC translation
    /// (`UserEnv::Guc`) casts a `user.x` extraction to when generating RLS
    /// policies, and what the save-time RLS check uses. The password hash is
    /// excluded for the same reason [`schema_shape`](Self::schema_shape)
    /// excludes it: not formula business.
    pub fn user_field_types(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let mut map = std::collections::BTreeMap::new();
        if let Some(users) = self.get(USERS_TABLE)? {
            for field in &users.fields {
                if field.base.name != USERS_PASSWORD_COLUMN {
                    map.insert(
                        field.base.name.clone(),
                        field.base.type_.sql_type().to_owned(),
                    );
                }
            }
        }
        Ok(map)
    }

    /// The cached table with the given name, if present.
    pub fn get(&self, name: &str) -> Result<Option<Table>> {
        Ok(self.schema().tables.get(name).cloned())
    }

    /// The cached table with the given name, or a [`NotFound`](Error::NotFound)
    /// error.
    pub fn require(&self, name: &str) -> Result<Table> {
        self.get(name)?
            .ok_or_else(|| Error::not_found(format!("table `{name}` is not in the catalog")))
    }

    /// All cached tables, sorted by name. Includes system (`_fd_*`) tables; use
    /// [`Table::is_system`] to filter.
    pub fn tables(&self) -> Result<Vec<Table>> {
        Ok(self.schema().tables().into_iter().cloned().collect())
    }

    /// Create a table with the given fields, then reload the cache and return the
    /// resulting [`Table`]. Primary-key columns are those fields marked
    /// [`primary_key`](DataField::primary_key); no `id` column is invented.
    pub async fn create_table(
        &self,
        name: impl Into<String>,
        fields: &[DataField],
    ) -> Result<Table> {
        self.create_table_inner(name.into(), fields, false).await
    }

    /// Create a table whose rows are **not worth a WAL record**: Postgres's
    /// `UNLOGGED`, where the primary database supports it (design §7.2).
    ///
    /// Identical to [`create_table`](Self::create_table) in every other respect,
    /// and identical in *every* respect on a backend that does not advertise
    /// [`unlogged_tables`](sc_db::DbCapabilities::unlogged_tables) — the flag
    /// buys write throughput, never semantics, so falling back to an ordinary
    /// table is correct rather than a failure. The trade it makes is real and
    /// belongs to the caller: an unclean shutdown truncates the table, and its
    /// contents never reach a physical standby.
    pub async fn create_unlogged_table(
        &self,
        name: impl Into<String>,
        fields: &[DataField],
    ) -> Result<Table> {
        let unlogged = self.primary.capabilities().unlogged_tables;
        self.create_table_inner(name.into(), fields, unlogged).await
    }

    /// Create a table **in a named database** — the primary, or one of the
    /// connections an admin has added — then reload and return it.
    ///
    /// The database is named rather than inferred, because at this moment there
    /// is nothing to infer it from: the table does not exist yet, so there is no
    /// row in the cache carrying a [`DbId`]. It is the one schema operation that
    /// has to be told, and every later one — add a field, drop a column, drop the
    /// table — reads the answer back off the table it created.
    pub async fn create_table_in(
        &self,
        database: &DbId,
        name: impl Into<String>,
        fields: &[DataField],
    ) -> Result<Table> {
        self.create_table_in_inner(database, name.into(), fields, false)
            .await
    }

    /// The shared body of the two create-table entry points.
    async fn create_table_inner(
        &self,
        name: String,
        fields: &[DataField],
        unlogged: bool,
    ) -> Result<Table> {
        self.create_table_in_inner(&self.primary_db.clone(), name, fields, unlogged)
            .await
    }

    /// The shared body of every create-table entry point, database and all.
    async fn create_table_in_inner(
        &self,
        database: &DbId,
        name: String,
        fields: &[DataField],
        unlogged: bool,
    ) -> Result<Table> {
        if fields.is_empty() {
            return Err(Error::invalid(format!(
                "cannot create table `{name}` with no fields"
            )));
        }
        let columns = fields.iter().map(DataField::to_column_def).collect();
        let primary_key = fields
            .iter()
            .filter(|f| f.primary_key)
            .map(|f| f.base.name.clone())
            .collect();
        self.driver_of(database)?
            .apply_schema(&SchemaChange::CreateTable {
                name: name.clone(),
                columns,
                primary_key,
                unlogged,
            })
            .await?;
        self.reload().await?;
        self.require(&name)
    }

    /// The driver that DDL naming `table` must be sent to: the driver of the
    /// database that hosts it.
    ///
    /// The whole reason schema changes are routed rather than sent to
    /// [`primary`](Catalog::primary): a table an admin created on a connection
    /// is theirs to alter and to drop, and the same `ALTER TABLE` sent to the
    /// primary would either fail confusingly or — if a table of that name
    /// existed there too — alter the wrong one, in the wrong database, with no
    /// error to say so.
    ///
    /// A table that is **not in the catalog** routes to the primary, and that is
    /// deliberate rather than a fallback: the callers that create one
    /// legitimately name a table before it exists, and a create names its own
    /// database (see [`create_table_in`](Catalog::create_table_in)) rather than
    /// asking here.
    fn driver_for_table(&self, name: &str) -> Result<Arc<dyn DatabaseDriver>> {
        match self.get(name)? {
            Some(table) => self.driver_of_table(&table),
            None => Ok(self.primary.clone()),
        }
    }

    /// Add a field to an existing table, then reload the cache and return the
    /// updated [`Table`].
    pub async fn create_field(&self, table: &str, field: &DataField) -> Result<Table> {
        let driver = self.driver_for_table(table)?;
        apply_one(
            driver.as_ref(),
            &SchemaChange::AddColumn {
                table: table.to_owned(),
                column: field.to_column_def(),
            },
        )
        .await?;
        self.reload().await?;
        self.require(table)
    }

    /// Drop a table and the overlay rows that describe it, then reload.
    ///
    /// **The overlay goes with the table.** §1.1 keeps an overlay whose table has
    /// vanished on purpose — a restore or an external migration must not lose a
    /// table's access rules — but that rule reads a *deliberate* drop as an
    /// accident. A row left behind by a drop Saltcorn itself performed would be
    /// indistinguishable from a kept orphan, so the two must be told apart at the
    /// one moment anything can: here.
    ///
    /// Refusing what the database would refuse — a table another table's `Key`
    /// references — is the caller's, not this: see
    /// [`SchemaProjection::referencing_fields`](crate::SchemaProjection::referencing_fields)
    /// and `sc_api::schema_edit`, which refuse **by name** before any DDL is
    /// issued. A foreign-key violation out of Postgres is not something an agent
    /// or an admin can act on.
    pub async fn drop_table(&self, name: &str) -> Result<()> {
        // The driver is resolved *before* the overlay is forgotten: after it,
        // `_fd_tables` no longer says which database the table is in, and the
        // cache lookup this needs would be gone with it.
        let driver = self.driver_for_table(name)?;
        self.forget_table_meta(name).await?;
        driver
            .apply_schema(&SchemaChange::DropTable {
                name: name.to_owned(),
                if_exists: false,
            })
            .await?;
        self.reload().await
    }

    /// Drop a column and its `_fd_fields` overlay row, then reload.
    ///
    /// A **calculated** field has no column, so this deletes only the overlay
    /// that introduces it — dropping the column that is not there would be an
    /// error naming a column the admin never created.
    pub async fn drop_field(&self, table: &str, field: &str) -> Result<Table> {
        let driver = self.driver_for_table(table)?;
        let is_calc = self
            .get(table)?
            .and_then(|t| t.field(field).map(DataField::is_calc))
            .unwrap_or(false);
        self.forget_field_meta(table, field).await?;
        if !is_calc {
            apply_one(
                driver.as_ref(),
                &SchemaChange::DropColumn {
                    table: table.to_owned(),
                    column: field.to_owned(),
                    if_exists: false,
                },
            )
            .await?;
        }
        self.reload().await?;
        self.require(table)
    }

    /// Delete the `_fd_tables` row for `name` and every `_fd_fields` row for its
    /// fields, without touching the table. The overlay half of
    /// [`drop_table`](Self::drop_table), split out so a batch can do it after its
    /// own DDL has committed.
    pub async fn forget_table_meta(&self, name: &str) -> Result<()> {
        crate::edited::forget_table_edit(self, name).await?;
        if self.get(FIELD_META_TABLE)?.is_some() {
            for meta in crate::field_meta::list_field_meta_for_table(self, name).await? {
                crate::field_meta::delete_field_meta_row(self, meta.id).await?;
            }
        }
        if self.get(TABLE_META_TABLE)?.is_some()
            && let Some(meta) = crate::table_meta::load_table_meta_by_name(self, name).await?
        {
            crate::table_meta::delete_table_meta_row(self, meta.id).await?;
        }
        Ok(())
    }

    /// Delete the `_fd_fields` row for one field, without touching the column.
    pub async fn forget_field_meta(&self, table: &str, field: &str) -> Result<()> {
        if self.get(FIELD_META_TABLE)?.is_some()
            && let Some(meta) =
                crate::field_meta::load_field_meta_by_field(self, table, field).await?
        {
            crate::field_meta::delete_field_meta_row(self, meta.id).await?;
        }
        Ok(())
    }

    /// Apply a whole list of schema steps in **one** transaction, committing only
    /// if every one of them succeeded (Phase 7).
    ///
    /// This is what makes a batch of schema operations atomic: a refused
    /// operation rolls the transaction back, so a half-built schema is never a
    /// state anybody has to clean up by hand. `Sql` steps carry the DDL
    /// [`SchemaChange`] does not model — the row-level-security policies — which
    /// is why they join the same transaction rather than needing one of their own.
    ///
    /// **The catalog is not reloaded here**: a batch reloads once, when it is
    /// done, rather than once per operation.
    pub async fn apply_schema_batch(&self, steps: &[SchemaStep]) -> Result<()> {
        self.apply_schema_batch_in(&self.primary_db.clone(), steps)
            .await
    }

    /// [`apply_schema_batch`](Self::apply_schema_batch) against a **named**
    /// database.
    ///
    /// One batch, one database, and that is a constraint rather than an
    /// oversight: a batch is one transaction, and a transaction cannot span two
    /// Postgres servers. A caller with operations for two databases issues two
    /// batches and knows that the second failing does not undo the first —
    /// `sc_api::schema_edit` refuses such a batch outright rather than pretending
    /// otherwise.
    pub async fn apply_schema_batch_in(&self, database: &DbId, steps: &[SchemaStep]) -> Result<()> {
        let driver = self.driver_of(database)?;
        let sqlite = driver.dialect().is_sqlite();
        let mut tx = driver.begin().await?;
        for step in steps {
            let outcome = match step {
                // SQLite's change-log triggers are generated from the table's
                // columns, so they follow every change to them (§14.12).
                SchemaStep::Change(change) if sqlite => {
                    crate::change_log_sqlite::apply_keeping_triggers(tx.as_mut(), change).await
                }
                SchemaStep::Change(change) => tx.apply_schema(change).await,
                SchemaStep::Sql(sql) => tx.batch(sql.as_str()).await,
            };
            if let Err(e) = outcome {
                sc_db::abandon(tx).await;
                return Err(e);
            }
        }
        tx.commit().await
    }

    /// Install the listener for schema changes — `sc-server`'s mount registry,
    /// once, at boot (Phase 7's schema seam).
    ///
    /// Replaces any previous one rather than refusing: a process installs
    /// exactly one, and a test that installs a second means the second.
    pub fn set_schema_observer(&self, observer: Arc<dyn crate::observer::SchemaObserver>) {
        self.install(|services| services.schema_observer = Some(observer));
    }

    /// Install what follows a table rename into the rows other crates own —
    /// `sc-server`'s, once, at boot.
    pub fn set_table_rename_hook(&self, hook: Arc<dyn crate::observer::TableRenameHook>) {
        self.install(|services| services.rename_hook = Some(hook));
    }

    /// Tell the installed rename hook, if any, that `from` is now `to`, and hand
    /// back its notes. A process with none rewrites nothing beyond the catalog's
    /// own rows and says nothing.
    pub async fn notify_table_renamed(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let hook = self.service(|services| services.rename_hook.clone())?;
        match hook {
            Some(hook) => hook.table_renamed(self, from, to).await,
            None => Ok(Vec::new()),
        }
    }

    /// Tell the installed observer, if any, that the schema moved, and hand back
    /// the applications it re-projected.
    ///
    /// Called **after** the DDL committed and the cache reloaded. An `Err` is the
    /// *reaction* failing, never the change; the caller reports it beside the
    /// result rather than pretending the schema stayed put. A process with no
    /// observer — a build tool, a test — re-projects nothing and says so with an
    /// empty list.
    pub fn notify_schema_changed(
        &self,
        change: &crate::observer::SchemaChanged,
    ) -> Result<Vec<crate::observer::ReprojectedApp>> {
        let observer = self.service(|services| services.schema_observer.clone())?;
        match observer {
            Some(observer) => observer.schema_changed(self, change),
            None => Ok(Vec::new()),
        }
    }

    /// Record where this deployment's applications are reachable in a browser
    /// (see [`crate::origin`]), replacing anything set before.
    ///
    /// Called once at boot by the process that knows: `feldspar serve` from its
    /// `--base-domain`/`--bind`, a command-line build from the `feldspar.toml`
    /// environment it connected with. It is not database state and is not
    /// persisted — it is a fact about *this process's* view of the deployment,
    /// held here because the project generator that needs it already has a
    /// catalog and nothing else it could ask.
    pub fn set_public_origin(&self, origin: crate::PublicOrigin) {
        if let Ok(mut guard) = self.public_origin.write() {
            *guard = Some(origin);
        }
    }

    /// Where applications are reachable, if this process was told.
    ///
    /// `None` is a normal answer — no base domain means no application is
    /// addressable — and every caller is expected to have something sensible to
    /// say instead of a hostname it made up.
    pub fn public_origin(&self) -> Option<crate::PublicOrigin> {
        self.public_origin
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Record whether this process serves a **production** environment (see
    /// [`is_production`](Catalog::is_production)). Called once at boot by the
    /// process that read `feldspar.toml`.
    pub fn set_production(&self, production: bool) {
        self.production
            .store(production, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this process serves an environment marked `production` — where
    /// a checklist that writes data is refused unless it is allowed there.
    pub fn is_production(&self) -> bool {
        self.production.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Replace the configuration values this host pins over `_fd_config`.
    ///
    /// Not checked here: this layer does not know what a setting is.
    /// `sc_config::set_host_config` checks each against its declaration and is
    /// the way to call this.
    pub fn set_host_config(&self, values: sc_types::Attrs) {
        if let Ok(mut guard) = self.host_config.write() {
            *guard = values;
        }
    }

    /// The configuration values this host pins, empty where it pins none.
    pub fn host_config(&self) -> sc_types::Attrs {
        self.host_config
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Store `value` as this catalog's `T`, or remove it with `None` — a
    /// service of a crate above this one whose type no trait here describes
    /// (see [`Extensions`](crate::Extensions)). `sc_llm::set_host_llm_provider`
    /// stores the host's LLM provider this way, `sc_app::install_stream_registry`
    /// the stream provider registry.
    pub fn set_extension<T: Send + Sync + 'static>(&self, value: Option<Arc<T>>) {
        self.install(|services| services.extensions.set(value));
    }

    /// This catalog's `T`, if one was stored.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.service(|services| services.extensions.get::<T>())
            .ok()
            .flatten()
    }

    /// What this process believes about workflow runs that want the engine.
    ///
    /// The runs table is the authority; this is the cache that keeps an idle
    /// process from asking it every few seconds (see [`crate::RunWakeups`]).
    /// Written by whoever writes a run row, read by the engine's queue.
    pub fn run_wakeups(&self) -> &crate::RunWakeups {
        &self.run_wakeups
    }

    /// The instance key Secret fields are sealed with: the one
    /// [installed](Catalog::set_secret_key), else the data directory's
    /// `secrets.key`, created the first time a value needs it.
    pub fn secret_key(&self) -> Result<Arc<crate::SecretKey>> {
        if let Some(key) = self
            .secret_key
            .read()
            .map_err(|_| Error::msg("secret key lock poisoned"))?
            .as_ref()
        {
            return Ok(Arc::clone(key));
        }
        let mut slot = self
            .secret_key
            .write()
            .map_err(|_| Error::msg("secret key lock poisoned"))?;
        if let Some(key) = slot.as_ref() {
            return Ok(Arc::clone(key));
        }
        let key = Arc::new(crate::SecretKey::load_or_create(
            &crate::secrets::secret_key_path()?,
        )?);
        *slot = Some(Arc::clone(&key));
        Ok(key)
    }

    /// Whether a key is already in hand — installed, or read once already —
    /// without reading or creating the file.
    pub(crate) fn has_secret_key(&self) -> bool {
        self.secret_key.read().is_ok_and(|key| key.is_some())
    }

    /// Use `key` for Secret fields from now on, instead of the data
    /// directory's: what a test does, so it never writes a key file into the
    /// developer's own data directory.
    pub fn set_secret_key(&self, key: crate::SecretKey) {
        if let Ok(mut slot) = self.secret_key.write() {
            *slot = Some(Arc::new(key));
        }
    }

    /// Ensure a system metadata table exists with (at least) `fields`, creating
    /// it if absent and **additively reconciling** it if it is already there.
    ///
    /// This is the one-time bootstrap every `_fd_*` table performs
    /// (`_fd_applications`, `_fd_triggers`, `_fd_file_stores`), factored here so
    /// there is one answer to "what happens when a release adds a column".
    ///
    /// The design bans a migration framework for now, and without one an existing
    /// database would simply lack the new column — every read of that table would
    /// then fail on a database that was working yesterday. So a declared column
    /// the table does not have is **created** ([`create_field`](Self::create_field)),
    /// which is the subset of migration that is always safe: nothing is dropped,
    /// nothing is renamed, and no data is rewritten. A column that exists is left
    /// exactly as it is — this never re-types or re-constrains one, because that
    /// *is* a migration and needs a framework that can decide what to do with the
    /// rows already there.
    ///
    /// The corollary a caller must respect: **a field added to an existing
    /// table's declaration cannot be `required`** unless the table is empty. The
    /// rows already stored have no value for it, so `NOT NULL` would be rejected
    /// by the database (or, worse, accepted with an invented default). Such a
    /// column is nullable, and its reader treats `NULL` as the empty value.
    ///
    /// A `required` column *is* added to a table that has no rows — there is
    /// nothing for `NOT NULL` to contradict, and a deployment that has the table
    /// but has never written to it is exactly the one a new release should be
    /// able to start against. When there are rows, this stops with a sentence
    /// naming the table and the column rather than letting the database refuse
    /// the `ALTER`: the raw error says only that a null value violates a
    /// constraint. Giving stored rows a value is a migration's job (`sc-migrate`,
    /// technical design §5.3), so reaching this means a release added a required
    /// column without one.
    pub async fn bootstrap_table(&self, name: &str, fields: &[DataField]) -> Result<Table> {
        let Some(existing) = self.get(name)? else {
            return self.create_table(name, fields).await;
        };
        let mut table = existing;
        // Emptiness is asked **once**, and only if some missing column needs the
        // answer: it is a query against the table, and the common case — nothing
        // missing — must stay the no-op it has always been.
        let mut empty: Option<bool> = None;
        for field in fields {
            if table.field(&field.base.name).is_some() {
                continue;
            }
            if field.required {
                let is_empty = match empty {
                    Some(known) => known,
                    None => *empty.insert(self.table_is_empty(name).await?),
                };
                if !is_empty {
                    return Err(Error::invalid(format!(
                        "`{name}` is missing the column `{}`, which is declared NOT NULL, and the \
                         table already has rows: the rows stored have no value for it, so adding \
                         it would be refused. This release should have brought a migration that \
                         fills it in; add the column and backfill it by hand, or drop `{name}` \
                         and let it be recreated",
                        field.base.name
                    )));
                }
            }
            table = self.create_field(name, field).await?;
        }
        Ok(table)
    }

    /// Whether `name` holds no rows — asked by [`bootstrap_table`](Self::bootstrap_table)
    /// before it adds a `NOT NULL` column to a table that is already there.
    ///
    /// `LIMIT 1` rather than `count(*)`: the question is "is there a row", and on
    /// a table with a million of them the count would read every one to answer it.
    async fn table_is_empty(&self, name: &str) -> Result<bool> {
        let select = Select::from(Source::table(name)).limit(1);
        let rows = self
            .driver_for_table(name)?
            .query(&Statement::from(select))
            .await?
            .try_collect()
            .await?;
        Ok(rows.is_empty())
    }

    /// A provider that serves the given table's rows: the trivial
    /// [`DriverTableProvider`] over the driver of the database that **hosts**
    /// it.
    ///
    /// It returns a `Result` for one reason, and it is the reason the whole
    /// multi-database arrangement is safe: a table stamped with a connection
    /// that is no longer connected has no driver, and the honest answer is to
    /// say so. Falling back to the primary would run somebody's query against
    /// the wrong database — the same table name, different rows — which is a
    /// failure nobody would see until the data was wrong.
    ///
    /// A provider reads and writes every row, so it takes an
    /// [`Unchecked`](crate::Unchecked): a caller's rows
    /// are reached through `sc_access::Scoped`.
    pub fn provider(
        &self,
        table: &Table,
        _access: crate::Unchecked<'_>,
    ) -> Result<Arc<dyn TableProvider>> {
        self.provider_of(table)
    }

    /// [`provider`](Catalog::provider) for this crate's own reads and writes
    /// (prefetching a formula's join values, the change log, table metadata).
    pub(crate) fn provider_of(&self, table: &Table) -> Result<Arc<dyn TableProvider>> {
        // A **provided** table has no driver at all: its rows come from a
        // module. The dispatch is here rather than in the caller because that is
        // the whole point of the trait — `rows.rs`, the REST provider, a view and
        // an agent all read through `Catalog::provider`, and none of them should
        // learn what a table provider is.
        if let Some((module, provider)) = table.provider() {
            let host = self.table_providers().ok_or_else(|| {
                Error::not_found(format!(
                    "`{}` is served by the table provider `{provider}` of `{module}`, and this \
                     process has no module host: it was started without modules",
                    table.name
                ))
            })?;
            let config = provided_config(table);
            return Ok(Arc::new(crate::provider::ProvidedTableProvider::new(
                host,
                &table.name,
                module,
                provider,
                config,
                table.fields.clone(),
                table.provided_writes(),
            )));
        }
        Ok(Arc::new(DriverTableProvider::new(
            self.driver_of_table(table)?,
            table.fields.clone(),
        )))
    }

    /// The fields a provided table presents, and what went wrong asking for
    /// them.
    ///
    /// Never fatal: a module that is not installed, will not load, or whose
    /// `fields(cfg)` threw leaves the table in the catalog with **no** fields and
    /// a sentence. That is v1's behaviour (`table.fields = []`) and it is the
    /// only one that leaves the admin able to fix it — a reload that failed here
    /// would take every other table down with it.
    async fn provided_fields(
        &self,
        name: &str,
        def: &crate::table_meta::ProvidedTableDef,
    ) -> (Vec<DataField>, Option<String>) {
        let Some(host) = self.table_providers() else {
            return (
                Vec::new(),
                Some(format!(
                    "`{name}` is served by the table provider `{}` of `{}`, and this process has \
                     no module host, so its columns are not known here",
                    def.provider, def.module
                )),
            );
        };
        match host
            .fields(&def.module, &def.provider, &def.configuration_json())
            .await
        {
            Ok(fields) => (fields, None),
            Err(e) => (
                Vec::new(),
                Some(format!(
                    "the table provider `{}` of `{}` could not say what columns `{name}` has: {}",
                    def.provider,
                    def.module,
                    sc_error::format_chain(&e)
                )),
            ),
        }
    }

    /// Which writes a provided table's provider answers, and what went wrong
    /// asking.
    ///
    /// The same never-fatal contract [`provided_fields`](Self::provided_fields)
    /// has, with the fail-closed answer: a provider that cannot be reached is
    /// [`ProvidedWrites::NONE`], so a table whose module went away becomes
    /// read-only rather than writable-but-broken.
    ///
    /// **No issue is recorded when the host has none.** A process with no module
    /// host already reports that on the fields, and saying the same thing twice
    /// on one table would put two sentences in front of an admin with one thing
    /// to fix.
    async fn provided_writes(
        &self,
        name: &str,
        def: &crate::table_meta::ProvidedTableDef,
    ) -> (crate::provider::ProvidedWrites, Option<String>) {
        let Some(host) = self.table_providers() else {
            return (crate::provider::ProvidedWrites::NONE, None);
        };
        match host
            .writes(&def.module, &def.provider, name, &def.configuration_json())
            .await
        {
            Ok(writes) => (writes, None),
            Err(e) => (
                crate::provider::ProvidedWrites::NONE,
                Some(format!(
                    "the table provider `{}` of `{}` could not say whether `{name}` can be \
                     written, so it is read-only here: {}",
                    def.provider,
                    def.module,
                    sc_error::format_chain(&e)
                )),
            ),
        }
    }

    /// Install what supplies table providers (§8.3) — `sc-server`'s
    /// `ModuleServices`, at boot and again after every module change.
    ///
    /// Installing it does **not** reload: the caller does that, because a module
    /// change reloads the catalog once at the end of a sequence that also swaps
    /// the action registry and the module functions.
    pub fn set_table_providers(
        &self,
        providers: Arc<dyn crate::provider::TableProviderHost>,
    ) -> Result<()> {
        self.try_install(|services| services.table_providers = Some(providers))
    }

    /// What supplies table providers, or `None` where nothing does.
    pub fn table_providers(&self) -> Option<Arc<dyn crate::provider::TableProviderHost>> {
        self.service(|services| services.table_providers.clone())
            .ok()
            .flatten()
    }

    /// Every provider every loaded module supplies — what the "new table" screen
    /// offers. Empty on a server with no modules.
    pub fn table_provider_kinds(&self) -> Vec<crate::provider::TableProviderKind> {
        self.table_providers()
            .map_or_else(Vec::new, |host| host.providers())
    }

    /// What went wrong building the provided tables on the last
    /// [`reload`](Self::reload), for the admin UI to show on the table it is
    /// about.
    pub fn provided_table_issues(&self) -> Vec<ProvidedTableIssue> {
        self.schema().provided_table_issues.clone()
    }

    /// The driver of the database hosting `table` — the primary, or the
    /// secondary connection whose name the table's [`DbId`] carries.
    ///
    /// The driver runs any statement, so it takes an
    /// [`Unchecked`](crate::Unchecked). What a caller
    /// only needs to know about the database — its dialect, its capabilities —
    /// is [`driver_info`](Catalog::driver_info).
    pub fn driver_for(
        &self,
        table: &Table,
        _access: crate::Unchecked<'_>,
    ) -> Result<Arc<dyn DatabaseDriver>> {
        self.driver_of_table(table)
    }

    /// [`driver_for`](Catalog::driver_for) for this crate's own use.
    pub(crate) fn driver_of_table(&self, table: &Table) -> Result<Arc<dyn DatabaseDriver>> {
        if table.database == self.primary_db {
            return Ok(self.primary.clone());
        }
        self.database(&table.database.0)?.ok_or_else(|| {
            Error::not_found(format!(
                "table `{}` is served by database connection `{}`, which is not connected",
                table.name, table.database.0
            ))
        })
    }

    /// The driver of the database with this id: the primary, or a connected
    /// secondary.
    ///
    /// Gated like [`driver_for`](Catalog::driver_for).
    pub fn driver_named(
        &self,
        database: &DbId,
        _access: crate::Unchecked<'_>,
    ) -> Result<Arc<dyn DatabaseDriver>> {
        self.driver_of(database)
    }

    /// What a caller may know about the database with this id without being
    /// handed its driver: its SQL dialect and capabilities. An error when no
    /// such connection is connected, as for [`driver_named`](Catalog::driver_named).
    pub fn driver_info(&self, database: &DbId) -> Result<DriverInfo> {
        self.driver_of(database).map(DriverInfo)
    }

    /// [`driver_named`](Catalog::driver_named) for this crate's own use.
    pub(crate) fn driver_of(&self, database: &DbId) -> Result<Arc<dyn DatabaseDriver>> {
        if *database == self.primary_db {
            return Ok(self.primary.clone());
        }
        self.database(&database.0)?.ok_or_else(|| {
            Error::not_found(format!(
                "database connection `{}` is not connected",
                database.0
            ))
        })
    }

    /// Register a secondary database under `name`, making its tables eligible
    /// for the next [`reload`](Catalog::reload).
    ///
    /// Replaces any driver already connected under that name — re-connecting a
    /// name re-points it, which is what editing a connection's host does — and
    /// clears any recorded [`database_error`](Catalog::database_error), since
    /// the connection demonstrably works now.
    ///
    /// `primary` is refused rather than shadowed: it is the [`DbId`] every table
    /// of the primary database carries, and a second holder of that name would
    /// make `driver_for` ambiguous in the one direction that matters.
    pub(crate) fn connect_database(
        &self,
        name: &str,
        driver: Arc<dyn DatabaseDriver>,
    ) -> Result<()> {
        if name == self.primary_db.0 {
            return Err(Error::invalid(format!(
                "`{name}` is the name of the primary database and cannot name a connection"
            )));
        }
        let mut guard = self
            .databases
            .write()
            .map_err(|_| Error::msg("catalog database registry lock poisoned"))?;
        guard.insert(name.to_owned(), driver);
        drop(guard);
        self.clear_database_error(name)
    }

    /// Disconnect the secondary database named `name`, returning whether one was
    /// connected. Also clears any recorded connection error for it.
    ///
    /// The caller reloads afterwards: this removes the driver, and it is the
    /// reload that removes its tables from the cache.
    pub fn disconnect_database(&self, name: &str) -> Result<bool> {
        let mut guard = self
            .databases
            .write()
            .map_err(|_| Error::msg("catalog database registry lock poisoned"))?;
        let existed = guard.remove(name).is_some();
        drop(guard);
        self.clear_database_error(name)?;
        Ok(existed)
    }

    /// Whether a secondary database is connected under `name`.
    pub fn is_connected(&self, name: &str) -> Result<bool> {
        Ok(self.database(name)?.is_some())
    }

    /// The connected secondary database with the given name, if any. Crate-
    /// private: outside this crate a driver is reached
    /// through [`driver_named`](Catalog::driver_named) and an `Unchecked`.
    pub(crate) fn database(&self, name: &str) -> Result<Option<Arc<dyn DatabaseDriver>>> {
        let guard = self
            .databases
            .read()
            .map_err(|_| Error::msg("catalog database registry lock poisoned"))?;
        Ok(guard.get(name).cloned())
    }

    /// The names of every connected secondary database, sorted.
    pub fn database_names(&self) -> Result<Vec<String>> {
        let guard = self
            .databases
            .read()
            .map_err(|_| Error::msg("catalog database registry lock poisoned"))?;
        let mut names: Vec<String> = guard.keys().cloned().collect();
        drop(guard);
        names.sort();
        Ok(names)
    }

    /// Every connected secondary database as `(name, driver)`, cloned out of the
    /// lock so a caller may await between them.
    fn connected_databases(&self) -> Result<Vec<(String, Arc<dyn DatabaseDriver>)>> {
        let guard = self
            .databases
            .read()
            .map_err(|_| Error::msg("catalog database registry lock poisoned"))?;
        let mut pairs: Vec<(String, Arc<dyn DatabaseDriver>)> = guard
            .iter()
            .map(|(name, driver)| (name.clone(), driver.clone()))
            .collect();
        drop(guard);
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(pairs)
    }

    /// Record why the connection named `name` is not usable, so the admin UI can
    /// show a connection that is defined but not connected, with the reason.
    pub(crate) fn record_database_error(&self, name: &str, error: impl Into<String>) -> Result<()> {
        let mut guard = self
            .database_errors
            .write()
            .map_err(|_| Error::msg("catalog database error registry lock poisoned"))?;
        guard.insert(name.to_owned(), error.into());
        Ok(())
    }

    /// Why the connection named `name` is not usable, if it is not.
    pub fn database_error(&self, name: &str) -> Result<Option<String>> {
        let guard = self
            .database_errors
            .read()
            .map_err(|_| Error::msg("catalog database error registry lock poisoned"))?;
        Ok(guard.get(name).cloned())
    }

    /// Forget any recorded connection error for `name`.
    fn clear_database_error(&self, name: &str) -> Result<()> {
        let mut guard = self
            .database_errors
            .write()
            .map_err(|_| Error::msg("catalog database error registry lock poisoned"))?;
        guard.remove(name);
        Ok(())
    }

    /// The tables the connection named `name` offered and the catalog did not
    /// adopt, because something already held the name (see
    /// [`shadowed_tables`](Catalog::shadowed_tables)). Sorted; empty is the
    /// ordinary case.
    pub fn shadowed_tables(&self, name: &str) -> Result<Vec<String>> {
        Ok(self.schema().shadowed_tables(name).to_vec())
    }

    /// Connect a named file store, making it resolvable by
    /// [`file_store`](Self::file_store). A store whose name is already connected
    /// is replaced (re-connecting the same name re-points it), which is what an
    /// edit to a store's path does.
    ///
    /// Connecting clears any recorded [`file_store_error`](Self::file_store_error)
    /// for that name: the store is demonstrably working now, so a stale reason it
    /// once failed would be shown to the admin as if it were current.
    pub fn connect_file_store(&self, store: Arc<dyn FileStore>) -> Result<()> {
        let name = store.name().to_owned();
        let mut guard = self
            .file_stores
            .write()
            .map_err(|_| Error::msg("catalog file-store registry lock poisoned"))?;
        guard.insert(name.clone(), store);
        drop(guard);
        self.clear_file_store_error(&name)
    }

    /// Disconnect the file store named `name`, returning whether one was
    /// connected. Also clears any recorded connection error for it.
    ///
    /// The counterpart to [`connect_file_store`](Self::connect_file_store), and
    /// what a deleted or renamed store needs: without this a store's definition
    /// could be removed while its handle kept serving, so the file manager would
    /// happily browse a store the admin had just deleted. Re-pointing an existing
    /// store does *not* need this — connecting the same name replaces it.
    ///
    /// This removes the handle only. Nothing on disk is touched; see
    /// [`delete_file_store`](crate::delete_file_store) for why that separation
    /// matters.
    pub fn disconnect_file_store(&self, name: &str) -> Result<bool> {
        let mut guard = self
            .file_stores
            .write()
            .map_err(|_| Error::msg("catalog file-store registry lock poisoned"))?;
        let existed = guard.remove(name).is_some();
        drop(guard);
        self.clear_file_store_error(name)?;
        Ok(existed)
    }

    /// Record why the store named `name` could not be connected, so the admin UI
    /// can show a store that is defined but not usable, with the reason.
    pub fn record_file_store_error(&self, name: &str, error: impl Into<String>) -> Result<()> {
        let mut guard = self
            .file_store_errors
            .write()
            .map_err(|_| Error::msg("catalog file-store error registry lock poisoned"))?;
        guard.insert(name.to_owned(), error.into());
        Ok(())
    }

    /// Why the store named `name` is not connected, if it failed to connect.
    pub fn file_store_error(&self, name: &str) -> Result<Option<String>> {
        let guard = self
            .file_store_errors
            .read()
            .map_err(|_| Error::msg("catalog file-store error registry lock poisoned"))?;
        Ok(guard.get(name).cloned())
    }

    /// Forget any recorded connection error for `name`.
    fn clear_file_store_error(&self, name: &str) -> Result<()> {
        let mut guard = self
            .file_store_errors
            .write()
            .map_err(|_| Error::msg("catalog file-store error registry lock poisoned"))?;
        guard.remove(name);
        Ok(())
    }

    /// Add a listener for row writes under `key` — `sc-action`'s trigger
    /// dispatcher as [`TRIGGER_EVENTS`](crate::events::TRIGGER_EVENTS) and the
    /// live hub as [`LIVE_EVENTS`](crate::events::LIVE_EVENTS), once each, at
    /// boot (§10.2's emit seam).
    ///
    /// **Adds** beside listeners under other keys: the seam has more than one
    /// observer, and a second kind of listener switching the first off would
    /// silently stop every table trigger. **Replaces** a listener already under
    /// the same key: a process (or a test) that installs a second trigger
    /// dispatcher means the second, and two would fire every trigger twice.
    pub fn add_table_events(
        &self,
        key: &str,
        events: Arc<dyn crate::events::TableEvents>,
    ) -> Result<()> {
        self.try_install(|services| {
            match services
                .table_events
                .iter_mut()
                .find(|(existing, _)| existing == key)
            {
                Some(slot) => slot.1 = events,
                None => services.table_events.push((key.to_owned(), events)),
            }
        })
    }

    /// Take the listener under `key` off the seam again. `false` when there
    /// was none.
    pub fn remove_table_events(&self, key: &str) -> bool {
        let Ok(mut services) = self.services.write() else {
            return false;
        };
        let before = services.table_events.len();
        services
            .table_events
            .retain(|(existing, _)| existing != key);
        services.table_events.len() != before
    }

    /// Empty the [`Services`] a layer above installed — the table-event
    /// listeners, the observer and rename hook, the module, live, model and
    /// admin hosts, the formula evaluator, the table providers and the
    /// extensions (the host LLM provider, the stream registry) — and the file
    /// stores.
    ///
    /// Those services hold an `Arc<Catalog>` of their own (the trigger
    /// dispatcher through its mailer, `ModelServices`, `AppMounts` …), so a
    /// catalog with any of them installed is in a reference cycle and is never
    /// dropped. A server lives as long as its process and does not care; a test
    /// binary builds a catalog per test, and without this every one of them —
    /// with its formula isolate's threads, the runtime handle that isolate
    /// anchors to and its pool's sockets — outlived its test. Tests register it
    /// on their `TestDb` (`db.on_teardown(catalog.teardown_hook())`), which runs
    /// it when the test ends.
    ///
    /// The services are one value, replaced by an empty one, so a service added
    /// to [`Services`] is cleared here with nothing to remember. Each table-event
    /// listener's own [`teardown`](crate::TableEvents::teardown) runs first, for
    /// the cycles that do not pass through the catalog (the trigger dispatcher
    /// and the workflow engine hold each other).
    ///
    /// The catalog still answers schema questions afterwards; it just has
    /// nothing installed, as if freshly [`init`](Catalog::init)ed.
    pub fn teardown(&self) {
        let services = match self.services.write() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for (_, listener) in &services.table_events {
            listener.teardown();
        }
        // Dropped outside the lock: a service's own drop may reach back in.
        drop(services);
        let stores = match self.file_stores.write() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        drop(stores);
    }

    /// Change the installed [`Services`]. A poisoned lock installs nothing,
    /// for the setters that have no error to report.
    fn install(&self, change: impl FnOnce(&mut Services)) {
        if let Ok(mut services) = self.services.write() {
            change(&mut services);
        }
    }

    /// [`install`](Catalog::install), with a poisoned lock an error.
    fn try_install(&self, change: impl FnOnce(&mut Services)) -> Result<()> {
        let mut services = self
            .services
            .write()
            .map_err(|_| Error::msg("catalog services lock poisoned"))?;
        change(&mut services);
        Ok(())
    }

    /// Read something out of the installed [`Services`].
    fn service<T>(&self, read: impl FnOnce(&Services) -> T) -> Result<T> {
        let services = self
            .services
            .read()
            .map_err(|_| Error::msg("catalog services lock poisoned"))?;
        Ok(read(&services))
    }

    /// [`teardown`](Catalog::teardown) as a closure that owns a handle to this
    /// catalog — what a test hands its harness guard:
    /// `db.on_teardown(catalog.teardown_hook())`.
    pub fn teardown_hook(self: &Arc<Self>) -> impl FnOnce() + Send + 'static {
        let catalog = Arc::clone(self);
        move || catalog.teardown()
    }

    /// Install the module functions — `sc-server`'s `ModuleServices`, at boot
    /// and again after every module change (install, configure, delete, Reload).
    ///
    /// Replaces any previous one rather than refusing: the module set is
    /// rebuilt whole on every change, so the second install *is* the answer.
    pub fn set_module_functions(&self, functions: Arc<dyn sc_expr::ModuleFnHost>) -> Result<()> {
        self.try_install(|services| services.module_functions = Some(functions))
    }

    /// The installed module functions, cloned out of the lock — `None` where
    /// nobody installed any, which is what makes `modfn` unbound in a code body
    /// and a module function in a formula an unknown identifier.
    ///
    /// A poisoned lock reads as "there are none": this is asked on the read and
    /// write paths, where the honest answer to "is the module registry broken"
    /// is a formula that fails naming its call rather than every query failing.
    pub fn module_functions(&self) -> Option<Arc<dyn sc_expr::ModuleFnHost>> {
        self.service(|services| services.module_functions.clone())
            .ok()
            .flatten()
    }

    /// Install the tables the modules declare (TODO-imap M4.3) — `sc-server`'s
    /// `ModuleServices`, after every module reload. Replaces the previous set
    /// whole, so an uninstalled module's tables stop being claimed at once.
    pub fn set_module_tables(&self, tables: crate::module_tables::ModuleTables) -> Result<()> {
        let mut guard = self
            .module_tables
            .write()
            .map_err(|_| Error::msg("catalog module-tables lock poisoned"))?;
        *guard = Arc::new(tables);
        Ok(())
    }

    /// The tables the installed modules declare. Empty where none do; a
    /// poisoned lock reads as empty too, which costs only the protection
    /// against dropping a module's table, never a read.
    pub fn module_tables(&self) -> Arc<crate::module_tables::ModuleTables> {
        self.module_tables
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_default()
    }

    /// Install live updates' publishing side — `sc-server`'s live hub, at boot.
    ///
    /// Replaces any previous one: a process has one hub, and a test that
    /// installs a second means the second.
    pub fn set_live_host(&self, host: Arc<dyn sc_expr::LiveHost>) -> Result<()> {
        self.try_install(|services| services.live_host = Some(host))
    }

    /// The installed live host, cloned out of the lock — `None` where nobody
    /// installed one. A poisoned lock reads as "there is none", for
    /// [`module_functions`](Catalog::module_functions)' reason.
    pub fn live_host(&self) -> Option<Arc<dyn sc_expr::LiveHost>> {
        self.service(|services| services.live_host.clone())
            .ok()
            .flatten()
    }

    /// Install the models — `sc-server`'s `ModelServices`, at startup and
    /// again whenever the module set (and so the provider registry) is rebuilt.
    ///
    /// Replaces any previous one, for
    /// [`set_module_functions`](Catalog::set_module_functions)' reason.
    pub fn set_model_host(&self, host: Arc<dyn crate::model_host::ModelHost>) -> Result<()> {
        self.try_install(|services| services.model_host = Some(host))
    }

    /// The installed models, cloned out of the lock — `None` where nobody
    /// installed any, which makes a `predict("…")` fail naming its call and a
    /// code body's `models.get` fail saying there are no models here.
    ///
    /// A poisoned lock reads as "there are none", for
    /// [`module_functions`](Catalog::module_functions)' reason.
    pub fn model_host(&self) -> Option<Arc<dyn crate::model_host::ModelHost>> {
        self.service(|services| services.model_host.clone())
            .ok()
            .flatten()
    }

    /// Install the server's admin handlers (§13.6) — `sc-server`, when it
    /// builds its router. Replaces any previous one.
    pub fn set_admin_host(&self, host: Arc<dyn crate::admin_host::AdminHost>) -> Result<()> {
        self.try_install(|services| services.admin_host = Some(host))
    }

    /// The installed admin handlers, or `None` where no server installed any
    /// (a CLI command, a test with no router).
    pub fn admin_host(&self) -> Option<Arc<dyn crate::admin_host::AdminHost>> {
        self.service(|services| services.admin_host.clone())
            .ok()
            .flatten()
    }

    /// Install the formula evaluator the read path computes an untranslatable
    /// calculated field with — the server's own, the one its triggers use.
    ///
    /// Replaces any previous one, for
    /// [`set_module_functions`](Catalog::set_module_functions)' reason.
    pub fn set_formula_evaluator(&self, evaluator: Arc<dyn sc_expr::JsEvaluator>) -> Result<()> {
        self.try_install(|services| services.formula_evaluator = Some(evaluator))
    }

    /// The installed formula evaluator, cloned out of the lock — `None` where
    /// nobody installed one.
    pub fn formula_evaluator(&self) -> Option<Arc<dyn sc_expr::JsEvaluator>> {
        self.service(|services| services.formula_evaluator.clone())
            .ok()
            .flatten()
    }

    /// Whether anything listens for `op` on `table` — the question the row layer
    /// asks **before** doing any work for the feature, so a write nobody observes
    /// pays one lock and a lookup per listener rather than a query.
    ///
    /// A poisoned lock reads as "nobody listens": this is asked on the write
    /// path, where the honest answer to "is the listener registry broken" is to
    /// let the write proceed unobserved rather than to fail it.
    pub fn observes_writes(&self, table: &str, op: crate::events::WriteOp) -> bool {
        match self.services.read() {
            Ok(services) => services
                .table_events
                .iter()
                .any(|(_, events)| events.observes(table, op)),
            Err(_) => false,
        }
    }

    /// Hand one committed write to every listener that observes it.
    ///
    /// An `Err` is the *dispatch* failing, never the write — which has already
    /// happened by the time this is called (decision 1: after commit). The caller
    /// reports it and returns the row. Every observing listener is handed the
    /// write even when an earlier one failed — one listener's error must not
    /// starve another — and the first error is the one returned.
    pub async fn emit_write(&self, write: crate::events::TableWrite<'_>) -> Result<()> {
        let listeners: Vec<Arc<dyn crate::events::TableEvents>> = self
            .listeners()?
            .into_iter()
            .filter(|events| events.observes(&write.table.name, write.op))
            .collect();
        let mut first_error = None;
        for events in listeners {
            if let Err(e) = events.emit(self, write.clone()).await {
                first_error.get_or_insert(e);
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The installed listeners, **cloned out of the lock**: dispatch runs actions,
    /// which write rows, which come back here — so the guard must not be held
    /// across the await.
    fn listeners(&self) -> Result<Vec<Arc<dyn crate::events::TableEvents>>> {
        self.service(|services| {
            services
                .table_events
                .iter()
                .map(|(_, events)| Arc::clone(events))
                .collect()
        })
    }

    /// The connected file store with the given name, if any — the raw store,
    /// whose operations check nobody's access, so it takes an
    /// [`Unchecked`](crate::Unchecked). A caller's
    /// files are reached through [`checked_file_store`](Catalog::checked_file_store).
    pub fn file_store(
        &self,
        name: &str,
        _access: crate::Unchecked<'_>,
    ) -> Result<Option<Arc<dyn FileStore>>> {
        self.raw_file_store(name)
    }

    /// The connected file store with the given name, or a
    /// [`NotFound`](Error::NotFound) error. Takes an [`Unchecked`](crate::Unchecked),
    /// as [`file_store`](Catalog::file_store) does.
    pub fn require_file_store(
        &self,
        name: &str,
        _access: crate::Unchecked<'_>,
    ) -> Result<Arc<dyn FileStore>> {
        self.require_raw_file_store(name)
    }

    /// [`file_store`](Catalog::file_store) for this crate's own use.
    pub(crate) fn raw_file_store(&self, name: &str) -> Result<Option<Arc<dyn FileStore>>> {
        let guard = self
            .file_stores
            .read()
            .map_err(|_| Error::msg("catalog file-store registry lock poisoned"))?;
        Ok(guard.get(name).cloned())
    }

    /// [`require_file_store`](Catalog::require_file_store) for this crate's own use.
    pub(crate) fn require_raw_file_store(&self, name: &str) -> Result<Arc<dyn FileStore>> {
        self.raw_file_store(name)?
            .ok_or_else(|| Error::not_found(format!("file store `{name}` is not connected")))
    }

    /// The connected store `name` as `who` reaches it: every operation checks
    /// the path-cumulative rule first ([`sc_files::CheckedStore`]), against the
    /// store-wide floor this reads from the stored definition itself
    /// ([`open_checked_store`](crate::open_checked_store)).
    ///
    /// `who` is an authority, not a caller-built `FileCaller` and floor
    /// (security review 2026-11 #14, TODO-type-hardening H3.2): a binding at
    /// the administrator's authority is an [`AdminPrincipal`](crate::AdminPrincipal)
    /// or a greppable `Authority::system(`.
    pub async fn checked_file_store<'a>(
        &self,
        name: &str,
        who: impl Into<crate::Authority<'a>>,
    ) -> Result<sc_files::CheckedStore> {
        crate::open_checked_store(self, name, who).await
    }

    /// The connected store `name` bound to the administrator `admin`, without
    /// reading its floor, which an administrator clears: what a process over
    /// the whole tree (the IDE's language server, the terminal) and the
    /// server's own temporary folders open. Only an administrator's binding
    /// answers [`CheckedStore::local_root`](sc_files::CheckedStore::local_root).
    pub fn admin_file_store(
        &self,
        name: &str,
        admin: &crate::AdminPrincipal,
    ) -> Result<sc_files::CheckedStore> {
        Ok(sc_files::CheckedStore::new(
            self.require_raw_file_store(name)?,
            None,
            admin.file_caller(),
        ))
    }

    /// Whether the connected store of that name is a git working copy;
    /// `None` when no store of that name is connected.
    pub fn file_store_is_git_repo(&self, name: &str) -> Result<Option<bool>> {
        Ok(self.raw_file_store(name)?.map(|store| store.is_git_repo()))
    }

    /// Whether a file store of that name is connected.
    pub fn has_file_store(&self, name: &str) -> Result<bool> {
        Ok(self.raw_file_store(name)?.is_some())
    }

    /// The names of every connected file store, sorted.
    pub fn file_store_names(&self) -> Result<Vec<String>> {
        let guard = self
            .file_stores
            .read()
            .map_err(|_| Error::msg("catalog file-store registry lock poisoned"))?;
        let mut names: Vec<String> = guard.keys().cloned().collect();
        names.sort();
        Ok(names)
    }
}

/// The user table's name and its password column. These mirror `sc-auth`'s
/// constants — that crate sits *above* this one in the layering, so the names
/// are restated here rather than imported. Both are design constants (§7.1:
/// users live in a table called `users`; the hash column is not deletable), not
/// configuration.
pub(crate) const USERS_TABLE: &str = "users";
pub(crate) const USERS_PASSWORD_COLUMN: &str = "password_hash";

/// The [`sc_expr::FieldKind`] of a field, from its type's SQL spelling (a rich
/// type stores as one of the basic ones): what lets a formula's time and
/// number constructs translate (TODO-type-hardening H7.2). A calculated field
/// is left [`Unknown`](sc_expr::FieldKind::Unknown): it has no column, and its
/// definition is inlined where it is read.
fn field_kind(field: &DataField) -> sc_expr::FieldKind {
    use sc_expr::FieldKind;
    if field.is_calc() {
        return FieldKind::Unknown;
    }
    match field.base.type_.sql_type() {
        "date" => FieldKind::Date,
        "timestamptz"
        | "timestamp"
        | "timestamp with time zone"
        | "timestamp without time zone" => FieldKind::Timestamp,
        "int2" | "int4" | "int8" | "smallint" | "integer" | "bigint" => FieldKind::Integer,
        "float4" | "float8" | "real" | "double precision" | "numeric" => FieldKind::Number,
        "text" | "varchar" | "bpchar" | "character varying" => FieldKind::Text,
        _ => FieldKind::Other,
    }
}

/// Whether a column default makes a value nobody chose: a random UUID or
/// random bytes.
fn is_random_default(sql: &str) -> bool {
    let sql = sql.to_ascii_lowercase();
    ["gen_random_uuid", "uuid_generate_v4", "gen_random_bytes"]
        .iter()
        .any(|f| sql.contains(f))
}

/// Project a table cache into the [`sc_expr::SchemaShape`] formula validation
/// and translation consume.
fn schema_shape_of(map: &HashMap<sc_query::Ident, Table>) -> sc_expr::SchemaShape {
    schema_shape_of_tables(map.values())
}

/// Project any set of tables into the [`sc_expr::SchemaShape`] formula
/// validation and translation consume: field names, Key targets, and the user
/// object's fields — everything the user table has except the password hash,
/// which a formula has no business reading (`sc-auth` never exposes it on a
/// `User` either, so a formula naming it would bind nothing and always deny).
///
/// Taken over an iterator rather than the cache, because a schema *edit*
/// validates against the schema it will leave behind rather than the one in the
/// cache (Phase 7) — see [`SchemaProjection`](crate::SchemaProjection).
pub(crate) fn schema_shape_of_tables<'a>(
    tables: impl Iterator<Item = &'a Table>,
) -> sc_expr::SchemaShape {
    // `old` is an ownership formula's own row before an update
    // (TODO-type-hardening H7.3); the formulas that have no update refuse it.
    // `_request` is an ownership formula's presented values (H7.5); the
    // formulas that are not ownership formulas refuse it too.
    let mut shape = sc_expr::SchemaShape::new().with_old_row().with_request();
    let mut users: Option<&Table> = None;
    for table in tables {
        if table.name == USERS_TABLE {
            users = Some(table);
        }
        let mut table_shape = sc_expr::TableShape::new();
        for field in &table.fields {
            // A collaborative document is bytes no formula can read (live
            // updates L5.1): left out, a formula naming it is an unknown field.
            // A secret (TODO-imap M2) likewise: its column holds envelopes, so a
            // filter, an ordering, an ownership rule or a calculated field over
            // it could only compare ciphertext — or, computed on read, carry
            // the plaintext out to a client.
            // The password hash likewise: no formula has a use for it, and a
            // filter over it is an oracle for it although it is never returned.
            if field.is_collab_doc()
                || field.is_secret()
                || (table.name == USERS_TABLE && field.base.name == USERS_PASSWORD_COLUMN)
            {
                continue;
            }
            table_shape = match &field.kind {
                crate::field::DataFieldKind::Key {
                    target_table,
                    target_field,
                    ..
                } => table_shape.key_field(&field.base.name, target_table, &target_field.0),
                _ => table_shape.field(&field.base.name),
            }
            .with_kind(&field.base.name, field_kind(field))
            .with_field(&field.base.name, |shape| {
                // What `column === _request.key` needs of the column
                // (TODO-type-hardening H7.5).
                shape.unique = table.is_unique(&field.base.name);
                shape.primary_key = field.primary_key;
                shape.identity = matches!(field.generated, Some(sc_db::ColumnGenerator::Identity));
                shape.random_default = matches!(
                    &field.generated,
                    Some(sc_db::ColumnGenerator::Default(sql)) if is_random_default(sql)
                );
            });
        }
        // A single-column primary key lets aggregations over this table as a
        // child (Phase 7) count rows and break `maxBy`/`minBy` ties; a composite
        // or absent key leaves it unset, so those simply do not translate.
        if let [pk] = table.primary_key.as_slice() {
            table_shape = table_shape.primary_key(pk);
        }
        shape = shape.table(&table.name, table_shape);
    }
    if let Some(users) = users {
        shape = shape.user_fields(
            users
                .fields
                .iter()
                .filter(|f| !f.is_collab_doc() && !f.is_secret())
                .map(|f| f.base.name.as_str())
                .filter(|name| *name != USERS_PASSWORD_COLUMN),
        );
    }
    shape
}
