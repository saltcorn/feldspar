//! The [`Catalog`]: the hub that owns the connected database and an in-memory
//! cache of its tables.
//!
//! For the MVP the catalog is initialised straight from a
//! [`DatabaseDriver`]'s introspection — **no stored metadata beyond
//! `information_schema`** (technical design §5, §8, §9). As soon as a database is
//! connected every table is usable; there is no discovery or registration step.
//! The cache is rebuilt from introspection on [`init`](Catalog::init) and after
//! every schema mutation the catalog makes, so it always reflects the live
//! schema. Cross-process cache invalidation over a message bus is post-MVP
//! (single process for the MVP), so a simple `RwLock` guards the cache here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use sc_db::{DatabaseDriver, SchemaChange};
use sc_error::{Error, Result};
use sc_files::FileStore;
use sc_query::{Select, Source, Statement};

use crate::field::{DataField, DbId, TableId};
use crate::field_meta::{FIELD_META_TABLE, list_field_meta};
use crate::provider::{DriverTableProvider, TableProvider};
use crate::table::{FieldMergeIssue, Table};
use crate::table_meta::{TABLE_META_TABLE, list_table_meta};

/// The catalog: the connected primary database plus a cache of its tables
/// (technical design §8.1). Row data for users, workflow runs, and files is
/// deliberately not cached; for the MVP the cache holds tables only. Connected
/// **file stores** (§14.1) are registered here too — not their contents, just
/// the named store handles — so the server can resolve a store by name.
pub struct Catalog {
    /// The primary (and, for the MVP, only) database driver.
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
    /// Tables a connection offered that the catalog did **not** adopt, keyed by
    /// connection name, because a table of that name was already there.
    ///
    /// The catalog keys tables by name, so two databases offering `orders` can
    /// only produce one `orders`. The primary always wins and the loser is
    /// **named** rather than dropped in silence: an admin who connects a
    /// database and cannot find one of its tables has to be able to read why,
    /// and "it clashed with a table you already had" is the whole answer.
    ///
    /// Rebuilt from scratch on every [`reload`](Catalog::reload), like
    /// [`field_overlay_issues`](Catalog::field_overlay_issues).
    shadowed_tables: RwLock<HashMap<String, Vec<String>>>,
    /// Cache of tables keyed by id, rebuilt from introspection.
    cache: RwLock<HashMap<TableId, Table>>,
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
    /// Who observes row writes (§10.2's emit seam), installed once at boot with
    /// `sc-action`'s trigger dispatcher — `None` in a process that has none (a
    /// build tool, a test), where every write is simply unobserved.
    ///
    /// The catalog holds it because it is what the row layer (layer 8) and
    /// `sc-action` (layer 6) both already have: the write cannot name the
    /// dispatcher without inverting the layering, and this is the inversion.
    table_events: RwLock<Option<Arc<dyn crate::events::TableEvents>>>,
    /// The `_fd_fields` overlay rows that did not cleanly merge on the last
    /// [`reload`](Self::reload) (design §3.2) — a dangling row, a rich type that
    /// does not fit its column, a `Key` with no foreign key behind it. Rebuilt
    /// every reload from scratch, so it always reflects the current schema and
    /// overlay; surfaced to the admin UI by [`field_overlay_issues`](Self::field_overlay_issues).
    field_overlay_issues: RwLock<Vec<FieldMergeIssue>>,
    /// Who observes **schema** changes (Phase 7's seam), installed once at boot
    /// with `sc-server`'s mount registry — `None` in a process that has none.
    ///
    /// Held here for the reason [`table_events`](Catalog::set_table_events) is:
    /// the schema editor (layer 8) and the mount registry (layer 10) cannot name
    /// each other, and the catalog is what they both already hold.
    schema_observer: RwLock<Option<Arc<dyn crate::observer::SchemaObserver>>>,
    /// The **module functions** a formula may hoist and a code body may call
    /// (TODO "Modules in-process", §4a) — `None` in a process with no modules
    /// installed, or none at all.
    ///
    /// Held here for the reason [`table_events`](Catalog::set_table_events) is,
    /// and it is the same inversion. What implements it is `sc-module`'s worker
    /// pool; what needs it is [`prefetch_bindings`](crate::prefetch_bindings)
    /// — which resolves a formula's hoisted calls and is called from three
    /// crates, none of which can name `sc-module` — and `run_js_code`, in a
    /// fourth. The catalog is what all four already hold.
    module_functions: RwLock<Option<Arc<dyn sc_expr::ModuleFnHost>>>,
    /// The **models** a formula's `predict("…")` and a code body's model handle
    /// reach (milestone 31 §4) — `None` in a process with no model support.
    ///
    /// Held here for [`module_functions`](Catalog::set_module_functions)'
    /// reason: what implements it is `sc-server`'s `ModelServices`, over
    /// `sc-model`, which sits above this crate; what needs it is
    /// [`prefetch_bindings`](crate::prefetch_bindings), the read path and the
    /// code host, which all hold a `Catalog`.
    model_host: RwLock<Option<Arc<dyn crate::model_host::ModelHost>>>,
    /// The server's admin handlers, which the administrative tools create an
    /// application or a file store through (§13.6) — `None` in a process that
    /// built no router. See [`crate::AdminHost`].
    admin_host: RwLock<Option<Arc<dyn crate::admin_host::AdminHost>>>,
    /// The formula evaluator the **read path** computes a calculated field
    /// with when it does not translate to SQL (milestone 31 §4) — a
    /// `predict("…")`, a module function call. Installed by whoever built the
    /// server's evaluator; `None` in a process that never did, where such a
    /// field fails its read naming itself.
    formula_evaluator: RwLock<Option<Arc<dyn sc_expr::JsEvaluator>>>,
    /// What supplies **table providers** (§8.3) — `None` in a process with no
    /// modules, which is what a `sc-catalog` test and a server with nothing
    /// installed both are.
    ///
    /// Held here for [`module_functions`](Catalog::set_module_functions)'
    /// reason: the implementation is `sc-module`'s worker pool (layer 6) and
    /// what needs it is [`reload`](Catalog::reload) and
    /// [`provider`](Catalog::provider) (layer 4), so the trait is declared here
    /// and installed from above.
    table_providers: RwLock<Option<Arc<dyn crate::provider::TableProviderHost>>>,
    /// Why a stored provided table is not the table an admin expected, keyed by
    /// table name: a module that is not installed, a provider it does not
    /// supply, a `fields(cfg)` that threw.
    ///
    /// The same arrangement as [`field_overlay_issues`] and for the same reason:
    /// a provided table whose module is down must still be *in* the tables list,
    /// with no fields and a sentence saying why, rather than vanishing from the
    /// admin UI that is the only place it can be fixed from.
    ///
    /// [`field_overlay_issues`]: Catalog::field_overlay_issues
    provided_table_issues: RwLock<Vec<ProvidedTableIssue>>,
    /// Where this deployment's applications are reachable in a browser
    /// (`crate::origin`), set once at boot by the process that knows — the
    /// server from its command line, a command-line build from its
    /// `feldspar.toml` environment. `None` where nobody said, which is a normal
    /// state: a server with no base domain serves no applications.
    public_origin: RwLock<Option<crate::PublicOrigin>>,
    /// Configuration values this **host** pins over `_fd_config` — the TLS keys
    /// an `[environments.*]` section of `feldspar.toml` may give. Set once at
    /// boot by the process that read the file, and read by `sc-config`, which
    /// is what gives the values their meaning; empty where nothing is pinned.
    host_config: RwLock<sc_types::Attrs>,
    /// How many times this catalog has been (re)loaded — the **generation
    /// stamp**, bumped by [`reload`](Catalog::reload) and by nothing else.
    ///
    /// It exists so that "does this isolate already have the schema?" is one
    /// integer comparison (TODO "the v1 `Table` API" §2). A guest that answers
    /// v1's synchronous `Table.findOne` has to hold the whole schema *before*
    /// its run starts, and the alternative to a stamp is hashing a megabyte of
    /// JSON on every firing of every trigger to discover that nothing changed.
    ///
    /// Monotonic, never reset, and no part of any identity: it says *when*, not
    /// *what*. Two catalogs of the same database in the same process have
    /// generations of their own, which is right — an isolate is told by the
    /// catalog whose snapshot it holds.
    generation: AtomicU64,
    /// The serialised schema snapshot for [`generation`](Catalog::generation),
    /// built once and shared by every run at that generation.
    ///
    /// Held here for [`table_events`](Catalog::set_table_events)' reason,
    /// inverted the same way: what *builds* it is `sc-api` (layer 8), which
    /// knows what v1 calls each of a field's properties; what needs it is every
    /// code body, workflow step and module action; and the catalog is what they
    /// all already hold. The stamp on the snapshot is what makes a stale one
    /// unusable rather than wrong — [`code_schema`](Catalog::code_schema) hands
    /// back nothing once the generation has moved on.
    ///
    /// [`table_events`]: Catalog::set_table_events
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
    /// [`table_events`]: Catalog::set_table_events
    run_wakeups: crate::RunWakeups,
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
    /// Raw DDL generated by trusted code — policies, and nothing else so far.
    Sql(String),
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
            shadowed_tables: RwLock::new(HashMap::new()),
            cache: RwLock::new(HashMap::new()),
            file_stores: RwLock::new(HashMap::new()),
            file_store_errors: RwLock::new(HashMap::new()),
            field_overlay_issues: RwLock::new(Vec::new()),
            schema_observer: RwLock::new(None),
            module_functions: RwLock::new(None),
            model_host: RwLock::new(None),
            admin_host: RwLock::new(None),
            formula_evaluator: RwLock::new(None),
            table_providers: RwLock::new(None),
            provided_table_issues: RwLock::new(Vec::new()),
            public_origin: RwLock::new(None),
            host_config: RwLock::new(sc_types::Attrs::new()),
            table_events: RwLock::new(None),
            generation: AtomicU64::new(0),
            code_schema: RwLock::new(None),
            run_wakeups: crate::RunWakeups::new(),
        };
        catalog.reload().await?;
        Ok(catalog)
    }

    /// The primary database driver.
    pub fn primary(&self) -> &Arc<dyn DatabaseDriver> {
        &self.primary
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
    pub async fn reload(&self) -> Result<()> {
        let physicals = self.primary.introspect().await?;
        let mut map = HashMap::with_capacity(physicals.len());
        for physical in &physicals {
            let table = Table::from_physical(self.primary_db.clone(), physical);
            map.insert(table.id.clone(), table);
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
                if map.contains_key(&table.id) {
                    shadowed
                        .entry(name.clone())
                        .or_default()
                        .push(table.name.clone());
                    continue;
                }
                map.insert(table.id.clone(), table);
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
        if map.contains_key(&TableId(TABLE_META_TABLE.to_owned())) {
            for meta in list_table_meta(self).await? {
                // A row carrying a provider is a **definition**, not an overlay:
                // it is what makes the table exist, so the table is built here
                // rather than looked up. Everything else about the row — the
                // label, the description, the access rules — then applies to it
                // exactly as it would to an introspected table.
                if let Some(def) = meta.provider() {
                    let id = TableId(meta.table_name.clone());
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
                if let Some(table) = map.get_mut(&TableId(meta.table_name.clone())) {
                    table.apply_overlay(&meta);
                }
            }
        }
        self.set_provided_table_issues(provided_issues)?;

        // Then the `_fd_fields` overlay, onto the fields the table now has. Same
        // "only when the table exists" guard and same bootstrapping reason as
        // above. The merge reports rather than fails, so its issues are collected
        // here and stored beside the cache.
        let mut field_issues = Vec::new();
        if map.contains_key(&TableId(FIELD_META_TABLE.to_owned())) {
            for meta in list_field_meta(self).await? {
                match map.get_mut(&TableId(meta.table_name.clone())) {
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
        let mut ownership_errors: Vec<(TableId, String)> = Vec::new();
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

        // The DB I/O is done; take the locks only to swap in the new snapshots so
        // they are never held across an await.
        let mut guard = self
            .cache
            .write()
            .map_err(|_| Error::msg("catalog cache lock poisoned"))?;
        *guard = map;
        drop(guard);
        let mut issues = self
            .field_overlay_issues
            .write()
            .map_err(|_| Error::msg("catalog field-overlay issue lock poisoned"))?;
        *issues = field_issues;
        drop(issues);
        let mut guard = self
            .shadowed_tables
            .write()
            .map_err(|_| Error::msg("catalog shadowed-table lock poisoned"))?;
        *guard = shadowed;
        drop(guard);
        // Last, and after every swap above: a run that reads the generation and
        // then the tables must never get a number older than what it goes on to
        // read. The snapshot built at the old number is dropped here rather than
        // left to be recognised as stale, so a reloaded catalog is not also
        // holding a megabyte of the schema it used to have.
        let mut schema = self
            .code_schema
            .write()
            .map_err(|_| Error::msg("catalog code-schema lock poisoned"))?;
        *schema = None;
        drop(schema);
        self.generation.fetch_add(1, Ordering::SeqCst);
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
        self.generation.load(Ordering::SeqCst)
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
        let guard = self
            .field_overlay_issues
            .read()
            .map_err(|_| Error::msg("catalog field-overlay issue lock poisoned"))?;
        Ok(guard.clone())
    }

    /// The catalog described as an `sc_expr` [`SchemaShape`] — what formula
    /// validation and translation (§7.3) see: every table's fields, each Key
    /// field's target, and the user object's fields. This is the projection
    /// that keeps `sc-expr` below the catalog in the dependency graph: the
    /// catalog describes tables *to* it, never the other way around.
    pub fn schema_shape(&self) -> Result<sc_expr::SchemaShape> {
        let guard = self
            .cache
            .read()
            .map_err(|_| Error::msg("catalog cache lock poisoned"))?;
        Ok(self.with_module_functions(schema_shape_of(&guard)))
    }

    /// The module functions installed on this catalog, as `(function, module)`
    /// pairs — what a [`SchemaShape`](sc_expr::SchemaShape) declares so a
    /// formula may call one (§4b). Empty on a server with no modules.
    pub fn module_function_names(&self) -> Vec<(String, String)> {
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
        let guard = self
            .cache
            .read()
            .map_err(|_| Error::msg("catalog cache lock poisoned"))?;
        Ok(guard.get(&TableId(name.to_owned())).cloned())
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
        let guard = self
            .cache
            .read()
            .map_err(|_| Error::msg("catalog cache lock poisoned"))?;
        let mut tables: Vec<Table> = guard.values().cloned().collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tables)
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
        self.driver_named(database)?
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
            Some(table) => self.driver_for(&table),
            None => Ok(self.primary.clone()),
        }
    }

    /// Add a field to an existing table, then reload the cache and return the
    /// updated [`Table`].
    pub async fn create_field(&self, table: &str, field: &DataField) -> Result<Table> {
        self.driver_for_table(table)?
            .apply_schema(&SchemaChange::AddColumn {
                table: table.to_owned(),
                column: field.to_column_def(),
            })
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
            driver
                .apply_schema(&SchemaChange::DropColumn {
                    table: table.to_owned(),
                    column: field.to_owned(),
                    if_exists: false,
                })
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
        let mut tx = self.driver_named(database)?.begin().await?;
        for step in steps {
            let outcome = match step {
                SchemaStep::Change(change) => tx.apply_schema(change).await,
                SchemaStep::Sql(sql) => tx.batch(sql).await,
            };
            if let Err(e) = outcome {
                let _ = tx.rollback().await;
                return Err(e);
            }
        }
        tx.commit().await
    }

    /// Install the listener for schema changes — `sc-server`'s mount registry,
    /// once, at boot (Phase 7's schema seam).
    ///
    /// Replaces any previous one, for the reason
    /// [`set_table_events`](Self::set_table_events) does.
    pub fn set_schema_observer(&self, observer: Arc<dyn crate::observer::SchemaObserver>) {
        if let Ok(mut guard) = self.schema_observer.write() {
            *guard = Some(observer);
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
        let observer = {
            let guard = self
                .schema_observer
                .read()
                .map_err(|_| Error::msg("catalog schema-observer lock poisoned"))?;
            guard.clone()
        };
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

    /// What this process believes about workflow runs that want the engine.
    ///
    /// The runs table is the authority; this is the cache that keeps an idle
    /// process from asking it every few seconds (see [`crate::RunWakeups`]).
    /// Written by whoever writes a run row, read by the engine's queue.
    pub fn run_wakeups(&self) -> &crate::RunWakeups {
        &self.run_wakeups
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
    /// constraint, and the admin needs telling that the answer is to give the
    /// stored rows a value by hand (or drop the table) because this prototype has
    /// no migration framework to do it for them.
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
                         it would be refused. There is no migration framework here — add the \
                         column and backfill it by hand, or drop `{name}` and let it be recreated",
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
    pub fn provider(&self, table: &Table) -> Result<Arc<dyn TableProvider>> {
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
            self.driver_for(table)?,
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
        *self
            .table_providers
            .write()
            .map_err(|_| Error::msg("catalog table-providers lock poisoned"))? = Some(providers);
        Ok(())
    }

    /// What supplies table providers, or `None` where nothing does.
    pub fn table_providers(&self) -> Option<Arc<dyn crate::provider::TableProviderHost>> {
        self.table_providers.read().ok()?.clone()
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
        self.provided_table_issues
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn set_provided_table_issues(&self, issues: Vec<ProvidedTableIssue>) -> Result<()> {
        *self
            .provided_table_issues
            .write()
            .map_err(|_| Error::msg("catalog provided-table issues lock poisoned"))? = issues;
        Ok(())
    }

    /// The driver of the database hosting `table` — the primary, or the
    /// secondary connection whose name the table's [`DbId`] carries.
    pub fn driver_for(&self, table: &Table) -> Result<Arc<dyn DatabaseDriver>> {
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
    pub fn driver_named(&self, database: &DbId) -> Result<Arc<dyn DatabaseDriver>> {
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
    pub fn connect_database(&self, name: &str, driver: Arc<dyn DatabaseDriver>) -> Result<()> {
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

    /// The connected secondary database with the given name, if any.
    pub fn database(&self, name: &str) -> Result<Option<Arc<dyn DatabaseDriver>>> {
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
    pub fn record_database_error(&self, name: &str, error: impl Into<String>) -> Result<()> {
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
        let guard = self
            .shadowed_tables
            .read()
            .map_err(|_| Error::msg("catalog shadowed-table lock poisoned"))?;
        Ok(guard.get(name).cloned().unwrap_or_default())
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

    /// Install the listener for row writes — `sc-action`'s trigger dispatcher,
    /// once, at boot (§10.2's emit seam).
    ///
    /// Replaces any previous one rather than refusing: a process installs exactly
    /// one, and a test that installs a second means the second.
    pub fn set_table_events(&self, events: Arc<dyn crate::events::TableEvents>) -> Result<()> {
        let mut guard = self
            .table_events
            .write()
            .map_err(|_| Error::msg("catalog table-events lock poisoned"))?;
        *guard = Some(events);
        Ok(())
    }

    /// Install the module functions — `sc-server`'s `ModuleServices`, at boot
    /// and again after every module change (install, configure, delete, Reload).
    ///
    /// Replaces any previous one rather than refusing, for
    /// [`set_table_events`](Catalog::set_table_events)' reason: the module set
    /// is rebuilt whole on every change, so the second install *is* the answer.
    pub fn set_module_functions(&self, functions: Arc<dyn sc_expr::ModuleFnHost>) -> Result<()> {
        let mut guard = self
            .module_functions
            .write()
            .map_err(|_| Error::msg("catalog module-functions lock poisoned"))?;
        *guard = Some(functions);
        Ok(())
    }

    /// The installed module functions, cloned out of the lock — `None` where
    /// nobody installed any, which is what makes `modfn` unbound in a code body
    /// and a module function in a formula an unknown identifier.
    ///
    /// A poisoned lock reads as "there are none": this is asked on the read and
    /// write paths, where the honest answer to "is the module registry broken"
    /// is a formula that fails naming its call rather than every query failing.
    pub fn module_functions(&self) -> Option<Arc<dyn sc_expr::ModuleFnHost>> {
        self.module_functions.read().ok()?.clone()
    }

    /// Install the models — `sc-server`'s `ModelServices`, at startup and
    /// again whenever the module set (and so the provider registry) is rebuilt.
    ///
    /// Replaces any previous one, for
    /// [`set_module_functions`](Catalog::set_module_functions)' reason.
    pub fn set_model_host(&self, host: Arc<dyn crate::model_host::ModelHost>) -> Result<()> {
        let mut guard = self
            .model_host
            .write()
            .map_err(|_| Error::msg("catalog model-host lock poisoned"))?;
        *guard = Some(host);
        Ok(())
    }

    /// The installed models, cloned out of the lock — `None` where nobody
    /// installed any, which makes a `predict("…")` fail naming its call and a
    /// code body's `models.get` fail saying there are no models here.
    ///
    /// A poisoned lock reads as "there are none", for
    /// [`module_functions`](Catalog::module_functions)' reason.
    pub fn model_host(&self) -> Option<Arc<dyn crate::model_host::ModelHost>> {
        self.model_host.read().ok()?.clone()
    }

    /// Install the server's admin handlers (§13.6) — `sc-server`, when it
    /// builds its router. Replaces any previous one.
    pub fn set_admin_host(&self, host: Arc<dyn crate::admin_host::AdminHost>) -> Result<()> {
        let mut guard = self
            .admin_host
            .write()
            .map_err(|_| Error::msg("catalog admin-host lock poisoned"))?;
        *guard = Some(host);
        Ok(())
    }

    /// The installed admin handlers, or `None` where no server installed any
    /// (a CLI command, a test with no router).
    pub fn admin_host(&self) -> Option<Arc<dyn crate::admin_host::AdminHost>> {
        self.admin_host.read().ok()?.clone()
    }

    /// Install the formula evaluator the read path computes an untranslatable
    /// calculated field with — the server's own, the one its triggers use.
    ///
    /// Replaces any previous one, for
    /// [`set_module_functions`](Catalog::set_module_functions)' reason.
    pub fn set_formula_evaluator(&self, evaluator: Arc<dyn sc_expr::JsEvaluator>) -> Result<()> {
        let mut guard = self
            .formula_evaluator
            .write()
            .map_err(|_| Error::msg("catalog formula-evaluator lock poisoned"))?;
        *guard = Some(evaluator);
        Ok(())
    }

    /// The installed formula evaluator, cloned out of the lock — `None` where
    /// nobody installed one.
    pub fn formula_evaluator(&self) -> Option<Arc<dyn sc_expr::JsEvaluator>> {
        self.formula_evaluator.read().ok()?.clone()
    }

    /// Whether anything listens for `op` on `table` — the question the row layer
    /// asks **before** doing any work for the feature, so a write nobody observes
    /// pays one lock and one lookup rather than a query.
    ///
    /// A poisoned lock reads as "nobody listens": this is asked on the write
    /// path, where the honest answer to "is the listener registry broken" is to
    /// let the write proceed unobserved rather than to fail it.
    pub fn observes_writes(&self, table: &str, op: crate::events::WriteOp) -> bool {
        match self.listener() {
            Ok(Some(events)) => events.observes(table, op),
            _ => false,
        }
    }

    /// Hand one committed write to the listener, if there is one.
    ///
    /// An `Err` is the *dispatch* failing, never the write — which has already
    /// happened by the time this is called (decision 1: after commit). The caller
    /// reports it and returns the row.
    pub async fn emit_write(&self, write: crate::events::TableWrite<'_>) -> Result<()> {
        match self.listener()? {
            Some(events) => events.emit(self, write).await,
            None => Ok(()),
        }
    }

    /// The installed listener, **cloned out of the lock**: dispatch runs actions,
    /// which write rows, which come back here — so the guard must not be held
    /// across the await.
    fn listener(&self) -> Result<Option<Arc<dyn crate::events::TableEvents>>> {
        let guard = self
            .table_events
            .read()
            .map_err(|_| Error::msg("catalog table-events lock poisoned"))?;
        Ok(guard.clone())
    }

    /// The connected file store with the given name, if any.
    pub fn file_store(&self, name: &str) -> Result<Option<Arc<dyn FileStore>>> {
        let guard = self
            .file_stores
            .read()
            .map_err(|_| Error::msg("catalog file-store registry lock poisoned"))?;
        Ok(guard.get(name).cloned())
    }

    /// The connected file store with the given name, or a
    /// [`NotFound`](Error::NotFound) error.
    pub fn require_file_store(&self, name: &str) -> Result<Arc<dyn FileStore>> {
        self.file_store(name)?
            .ok_or_else(|| Error::not_found(format!("file store `{name}` is not connected")))
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

/// Project a table cache into the [`sc_expr::SchemaShape`] formula validation
/// and translation consume.
fn schema_shape_of(map: &HashMap<TableId, Table>) -> sc_expr::SchemaShape {
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
    let mut shape = sc_expr::SchemaShape::new();
    let mut users: Option<&Table> = None;
    for table in tables {
        if table.name == USERS_TABLE {
            users = Some(table);
        }
        let mut table_shape = sc_expr::TableShape::new();
        for field in &table.fields {
            table_shape = match &field.kind {
                crate::field::DataFieldKind::Key {
                    target_table,
                    target_field,
                    ..
                } => table_shape.key_field(&field.base.name, &target_table.0, &target_field.0),
                _ => table_shape.field(&field.base.name),
            };
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
                .map(|f| f.base.name.as_str())
                .filter(|name| *name != USERS_PASSWORD_COLUMN),
        );
    }
    shape
}
