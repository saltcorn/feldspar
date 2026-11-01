//! [`TableProvider`]: presenting a data source as a queryable table.
//!
//! A provider interprets the universal query language ([`Select`], plus
//! [`Statement`] writes) and returns rows (technical design §8.3). There are two
//! implementations:
//!
//! - [`DriverTableProvider`] — the trivial one, which forwards straight to the
//!   [`DatabaseDriver`]. Every database table is served by one.
//! - [`ProvidedTableProvider`] — a table whose rows come from a **module's**
//!   table provider: `@saltcorn/rss`'s `RSS feed`, `@saltcorn/proxmox`'s cluster
//!   listings. The module answers a list of JSON objects and
//!   [`inmem`](crate::inmem) applies the `Select` to them.
//!
//! Materialisation (`None | Snapshot | Synced`) remains deferred, so the trait
//! carries only the read/write surface.
//!
//! ## The seam
//!
//! `sc-catalog` is layer 4 and a module host is layer 6, so the dependency is
//! inverted exactly as `sc-expr`'s `ModuleFnHost` is: [`TableProviderHost`] is
//! declared here, implemented in `sc-module`, and installed on the
//! [`Catalog`](crate::Catalog) by `sc-server` at boot and after every module
//! change. A process with no modules has none, and a provided table there is a
//! table with no fields and a reason — never a panic and never a silently empty
//! one.

use std::sync::Arc;

use async_trait::async_trait;
use sc_db::{DatabaseDriver, Row, RowStream};
use sc_error::{Error, Result};
use sc_query::{Delete, Expr, Insert, Projection, Select, Source, Statement, Update};
use sc_types::FormField;
use serde_json::Value as Json;

use crate::field::DataField;
use crate::inmem::{ValueRow, filter_rows, pushdown, run_select_over, value_row};

/// Presents a data source as a table: report its fields, run a `SELECT`, and (if
/// writable) apply an `INSERT`/`UPDATE`/`DELETE`.
#[async_trait]
pub trait TableProvider: Send + Sync {
    /// The fields this provider presents.
    fn fields(&self) -> Vec<DataField>;

    /// Run a `SELECT` and stream matching rows.
    async fn query(&self, select: &Select) -> Result<RowStream>;

    /// Apply a write statement (`INSERT`/`UPDATE`/`DELETE`), streaming back any
    /// `RETURNING` rows (empty when the statement returns nothing).
    async fn write(&self, change: &Statement) -> Result<RowStream>;
}

/// The trivial provider: a table backed directly by a [`DatabaseDriver`]. Every
/// database table is served by one of these.
pub struct DriverTableProvider {
    driver: Arc<dyn DatabaseDriver>,
    fields: Vec<DataField>,
}

impl DriverTableProvider {
    /// Wrap a driver and the fields of the table it serves.
    pub fn new(driver: Arc<dyn DatabaseDriver>, fields: Vec<DataField>) -> DriverTableProvider {
        DriverTableProvider { driver, fields }
    }
}

#[async_trait]
impl TableProvider for DriverTableProvider {
    fn fields(&self) -> Vec<DataField> {
        self.fields.clone()
    }

    async fn query(&self, select: &Select) -> Result<RowStream> {
        self.driver
            .query(&Statement::Select(Box::new(select.clone())))
            .await
    }

    async fn write(&self, change: &Statement) -> Result<RowStream> {
        self.driver.query(change).await
    }
}

// --- provided tables ----------------------------------------------------------

/// One table provider a module supplies, as the "new table" screen offers it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableProviderKind {
    /// The package that supplies it — `@saltcorn/rss`.
    pub module: String,
    /// Its own name within that package — `RSS feed`.
    pub provider: String,
    /// The settings it asks for, translated from the provider's own v1
    /// `configuration_workflow`. The same [`FormField`] vocabulary a file
    /// store's backend and an LLM provider use, so the admin UI renders it with
    /// no code that knows what a table provider is.
    pub config_spec: Vec<FormField>,
}

/// Which of v1's three write methods a provider answers **for one
/// configuration**.
///
/// v1 decides writability inside `get_table(cfg)`: the object it returns carries
/// `insertRow`/`updateRow`/`deleteRows`, or it does not.
/// `@saltcorn/postgres-tables` omits all three when its `read_only` flag is set,
/// which is the whole model — a capability of the *configuration*, not of the
/// provider, so two tables served by the same provider may differ.
///
/// [`NONE`](ProvidedWrites::NONE) is [`Default`], and it is what a provider that
/// could not be reached answers: a table whose module is uninstalled reads as
/// read-only rather than as writable-but-broken, which is the fail-closed rule a
/// broken ownership formula gets for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProvidedWrites {
    /// The provider answers `insertRow`.
    pub insert: bool,
    /// The provider answers `updateRow`.
    pub update: bool,
    /// The provider answers `deleteRows`.
    pub delete: bool,
}

impl ProvidedWrites {
    /// A read-only provided table: what a feed is, and what an unreachable
    /// module's table is.
    pub const NONE: ProvidedWrites = ProvidedWrites {
        insert: false,
        update: false,
        delete: false,
    };

    /// All three — what a `@saltcorn/postgres-tables` table with `read_only`
    /// off is.
    pub const ALL: ProvidedWrites = ProvidedWrites {
        insert: true,
        update: true,
        delete: true,
    };

    /// Whether anything at all may be written — what decides whether the admin
    /// UI offers a row editor or a viewer.
    pub fn any(&self) -> bool {
        self.insert || self.update || self.delete
    }

    /// Read the answer the host script sends back: `{ insert, update, delete }`,
    /// with anything missing meaning *no*.
    pub fn from_json(value: &Json) -> ProvidedWrites {
        let flag = |key: &str| value.get(key).and_then(Json::as_bool).unwrap_or(false);
        ProvidedWrites {
            insert: flag("insert"),
            update: flag("update"),
            delete: flag("delete"),
        }
    }
}

/// What supplies table providers: the seam `sc-module` implements and
/// `sc-server` installs on the catalog.
///
/// Three questions, and the split is the same one `ModuleFnHost` makes:
/// enumerating is synchronous because a form renders it in one expression, and
/// the two that reach a module are async because they reach a module.
#[async_trait]
pub trait TableProviderHost: Send + Sync {
    /// Every provider every loaded module supplies.
    fn providers(&self) -> Vec<TableProviderKind>;

    /// The fields `provider` presents for this configuration.
    ///
    /// Asked on every catalog reload rather than stored, which is v1's
    /// arrangement and the right one: the columns are the *module's* answer, so
    /// an upgraded package that presents a new column presents it, and nothing
    /// Saltcorn wrote down can disagree with the code that serves the rows.
    async fn fields(&self, module: &str, provider: &str, config: &Json) -> Result<Vec<DataField>>;

    /// Its rows, as JSON objects, for one v1 `where`/`options` pair.
    ///
    /// The pair is a **hint**: a provider may honour it (`@saltcorn/postgres-
    /// tables` turns it into SQL) or ignore it entirely (`@saltcorn/rss` answers
    /// the whole feed), and the caller applies the query to the answer either
    /// way.
    ///
    /// `table` is the Saltcorn table being read. v1 hands its provider the table
    /// row as `get_table`'s second argument and a provider reads its name off
    /// it, so the name travels; nothing else of a `Table` crosses the seam,
    /// because nothing else of it means anything on the far side.
    async fn rows(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
        filter: &Json,
        options: &Json,
    ) -> Result<Vec<Json>>;

    /// Which writes `get_table(config)` answers.
    ///
    /// Asked once per catalog reload, beside [`fields`](TableProviderHost::
    /// fields) and for its reason: the admin UI has to know before it draws a
    /// button whether there is anything behind it. The write methods below check
    /// again on the far side, because a module can be reconfigured, upgraded or
    /// removed between a reload and a write.
    async fn writes(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
    ) -> Result<ProvidedWrites>;

    /// v1's `insertRow(record)`: the new row's primary key, or [`Json::Null`]
    /// when the provider does not report one.
    async fn insert(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
        record: &Json,
    ) -> Result<Json>;

    /// v1's `updateRow(record, id)`: the changed columns, and the primary key of
    /// the one row they apply to.
    async fn update(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
        id: &Json,
        record: &Json,
    ) -> Result<()>;

    /// v1's `deleteRows(where)`: every row the v1 `where` object matches.
    async fn delete(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
        filter: &Json,
    ) -> Result<()>;
}

/// A table served by a module's table provider.
///
/// Two provider hosts as one — the composite of §8, for a server whose modules
/// are written in more than one language.
///
/// A provided table names a module and a provider; that one language's modules
/// run on a Deno worker and another's on an embedded interpreter is a fact about
/// installation, not about reading rows. So the hosts are merged here and
/// `Catalog::reload`, `Catalog::provider` and the "new table" form see one.
///
/// Routing is by the `(module, provider)` pair the table's row names, resolved
/// against what each host says it supplies. A pair nothing supplies is refused
/// here rather than by whichever host was asked first, because "no installed
/// module supplies this" is the true sentence and either host's own would name
/// only half the server.
pub struct TableProviderHosts {
    hosts: Vec<Arc<dyn TableProviderHost>>,
    providers: Vec<TableProviderKind>,
}

impl TableProviderHosts {
    /// One host over all of them, in order.
    pub fn new(hosts: Vec<Arc<dyn TableProviderHost>>) -> TableProviderHosts {
        let providers = hosts.iter().flat_map(|host| host.providers()).collect();
        TableProviderHosts { hosts, providers }
    }

    /// The host that supplies `(module, provider)`, or the refusal.
    fn route(&self, module: &str, provider: &str) -> Result<&Arc<dyn TableProviderHost>> {
        for host in &self.hosts {
            if host
                .providers()
                .iter()
                .any(|p| p.module == module && p.provider == provider)
            {
                return Ok(host);
            }
        }
        Err(Error::not_found(format!(
            "no installed module supplies the table provider `{provider}` of `{module}`; it may \
             have been uninstalled, or failed to load"
        )))
    }
}

#[async_trait]
impl TableProviderHost for TableProviderHosts {
    fn providers(&self) -> Vec<TableProviderKind> {
        self.providers.clone()
    }

    async fn fields(&self, module: &str, provider: &str, config: &Json) -> Result<Vec<DataField>> {
        self.route(module, provider)?
            .fields(module, provider, config)
            .await
    }

    async fn rows(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
        filter: &Json,
        options: &Json,
    ) -> Result<Vec<Json>> {
        self.route(module, provider)?
            .rows(module, provider, table, config, filter, options)
            .await
    }

    async fn writes(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
    ) -> Result<ProvidedWrites> {
        self.route(module, provider)?
            .writes(module, provider, table, config)
            .await
    }

    async fn insert(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
        record: &Json,
    ) -> Result<Json> {
        self.route(module, provider)?
            .insert(module, provider, table, config, record)
            .await
    }

    async fn update(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
        id: &Json,
        record: &Json,
    ) -> Result<()> {
        self.route(module, provider)?
            .update(module, provider, table, config, id, record)
            .await
    }

    async fn delete(
        &self,
        module: &str,
        provider: &str,
        table: &str,
        config: &Json,
        filter: &Json,
    ) -> Result<()> {
        self.route(module, provider)?
            .delete(module, provider, table, config, filter)
            .await
    }
}

#[cfg(test)]
mod composite_tests {
    use super::*;

    /// A host of one provider, answering with its own module's name.
    struct One(&'static str, &'static str);

    #[async_trait]
    impl TableProviderHost for One {
        fn providers(&self) -> Vec<TableProviderKind> {
            vec![TableProviderKind {
                module: self.0.to_owned(),
                provider: self.1.to_owned(),
                config_spec: Vec::new(),
            }]
        }

        async fn fields(&self, module: &str, _: &str, _: &Json) -> Result<Vec<DataField>> {
            Ok(vec![DataField::plain(
                module,
                sc_types::TypeRef::Basic(sc_types::BasicType::Text),
            )])
        }

        async fn rows(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &Json,
            _: &Json,
            _: &Json,
        ) -> Result<Vec<Json>> {
            Ok(Vec::new())
        }

        async fn writes(&self, _: &str, _: &str, _: &str, _: &Json) -> Result<ProvidedWrites> {
            Ok(ProvidedWrites::NONE)
        }

        async fn insert(&self, _: &str, _: &str, _: &str, _: &Json, _: &Json) -> Result<Json> {
            Ok(Json::Null)
        }

        async fn update(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &Json,
            _: &Json,
            _: &Json,
        ) -> Result<()> {
            Ok(())
        }

        async fn delete(&self, _: &str, _: &str, _: &str, _: &Json, _: &Json) -> Result<()> {
            Ok(())
        }
    }

    fn both() -> TableProviderHosts {
        TableProviderHosts::new(vec![
            Arc::new(One("@saltcorn/rss", "RSS feed")),
            Arc::new(One("sc-plugin-fixture", "Fixture rows")),
        ])
    }

    #[tokio::test]
    async fn a_provided_table_is_served_by_whichever_host_supplies_its_provider() {
        let hosts = both();
        let offered: Vec<String> = hosts
            .providers()
            .into_iter()
            .map(|kind| format!("{}/{}", kind.module, kind.provider))
            .collect();
        assert_eq!(
            offered,
            ["@saltcorn/rss/RSS feed", "sc-plugin-fixture/Fixture rows"]
        );

        // Answered by the second host, which is the one that has it — the
        // column it names is the module it was asked about.
        let fields = hosts
            .fields("sc-plugin-fixture", "Fixture rows", &Json::Null)
            .await
            .expect("the second host has it");
        assert_eq!(fields[0].base.name, "sc-plugin-fixture");
    }

    #[tokio::test]
    async fn a_provider_neither_supplies_is_refused_by_the_composite_itself() {
        let said = both()
            .fields("sc-plugin-fixture", "Nothing", &Json::Null)
            .await
            .expect_err("nothing supplies it")
            .to_string();
        assert!(said.contains("no installed module supplies"), "{said}");
        assert!(said.contains("Nothing"), "{said}");
    }
}

/// Reads run the `Select` over the JSON the module answers ([`crate::inmem`]).
/// Writes are the opposite shape and the difference is the whole of this type's
/// second half: **v1's write methods are not a query language**. `insertRow`
/// takes a record, `updateRow` takes a record and one primary key, `deleteRows`
/// takes a v1 `where` object — so a `Statement` is *narrowed* into them, and
/// every narrowing that cannot be made is refused by name rather than
/// approximated.
pub struct ProvidedTableProvider {
    host: Arc<dyn TableProviderHost>,
    table: String,
    module: String,
    provider: String,
    config: Json,
    fields: Vec<DataField>,
    writes: ProvidedWrites,
}

impl ProvidedTableProvider {
    /// A provider over `host` for one configured table, with the writes the
    /// module answered for this configuration at the last catalog reload.
    pub fn new(
        host: Arc<dyn TableProviderHost>,
        table: impl Into<String>,
        module: impl Into<String>,
        provider: impl Into<String>,
        config: Json,
        fields: Vec<DataField>,
        writes: ProvidedWrites,
    ) -> ProvidedTableProvider {
        ProvidedTableProvider {
            host,
            table: table.into(),
            module: module.into(),
            provider: provider.into(),
            config,
            fields,
            writes,
        }
    }

    /// The one primary-key column, or the sentence saying why a write cannot be
    /// addressed without it.
    ///
    /// v1's `updateRow(rec, id)` takes a single scalar id, so a provider that
    /// declared no key — or declared two — has given this system no way to name
    /// one row. Reads are unaffected, which is why this is asked here and not at
    /// reload: a feed with no key is a perfectly good read-only table.
    fn primary_key(&self) -> Result<&str> {
        let mut keys = self.fields.iter().filter(|f| f.primary_key);
        match (keys.next(), keys.next()) {
            (Some(field), None) => Ok(&field.base.name),
            (None, _) => Err(Error::invalid(format!(
                "`{}` cannot be written: the table provider `{}` of `{}` declares no primary \
                 key, so there is no way to say which row a change applies to",
                self.table, self.provider, self.module
            ))),
            (Some(_), Some(_)) => Err(Error::invalid(format!(
                "`{}` cannot be written: the table provider `{}` of `{}` declares more than \
                 one primary-key column, and v1's `updateRow` addresses a row by a single \
                 key",
                self.table, self.provider, self.module
            ))),
        }
    }

    /// Refuse a write this configuration does not answer, naming which one.
    fn require(&self, what: Write) -> Result<()> {
        let allowed = match what {
            Write::Insert => self.writes.insert,
            Write::Update => self.writes.update,
            Write::Delete => self.writes.delete,
        };
        if allowed {
            return Ok(());
        }
        Err(Error::invalid(format!(
            "`{}` is read-only: the table provider `{}` of `{}` supplies no `{}` for the \
             settings it is configured with",
            self.table,
            self.provider,
            self.module,
            what.v1_method()
        )))
    }

    /// Every row the statement's filter matches, as the provider currently has
    /// them.
    ///
    /// The pushdown hint goes over first, so a provider that can filter in its
    /// own backend does; the filter is then applied here regardless, because a
    /// provider is allowed to ignore it (the same contract [`query`] runs
    /// under).
    ///
    /// [`query`]: TableProvider::query
    async fn matching(&self, filter: Option<&Expr>) -> Result<Vec<ValueRow>> {
        let mut select = Select::from(Source::Table {
            name: self.table.clone(),
            alias: None,
        });
        select.filter = filter.cloned();
        let (hint, options) = pushdown(&select);
        let json = self
            .host
            .rows(
                &self.module,
                &self.provider,
                &self.table,
                &self.config,
                &hint,
                &options,
            )
            .await?;
        let rows: Vec<ValueRow> = json
            .iter()
            .map(|row| value_row(&self.fields, row))
            .collect();
        filter_rows(filter, &self.table, rows)
    }

    /// The `RETURNING` rows of a write, projected out of the rows it touched.
    ///
    /// A synthetic `SELECT <returning>` with no filter over those rows, which is
    /// exactly what `RETURNING` is, and reuses the projection, the aggregate
    /// rules and the refusals [`run_select_over`] already has.
    fn returning(&self, returning: &[Projection], rows: Vec<ValueRow>) -> Result<Vec<Row>> {
        if returning.is_empty() {
            return Ok(Vec::new());
        }
        let select = Select::from(Source::Table {
            name: self.table.clone(),
            alias: None,
        })
        .columns(returning.to_vec());
        run_select_over(&select, &self.table, rows)
    }

    /// One row's worth of `column: value` as JSON, for `insertRow`/`updateRow`.
    ///
    /// A non-literal expression is refused rather than evaluated: there is no
    /// database behind a provided table to evaluate `now()` or `a + 1` in, and
    /// guessing a value would write the wrong one silently.
    fn record(&self, pairs: &[(&str, &Expr)]) -> Result<Json> {
        let mut record = serde_json::Map::new();
        for (column, expr) in pairs {
            let Expr::Lit(value) = expr else {
                return Err(Error::invalid(format!(
                    "`{}` is served by the table provider `{}` of `{}`, which is written one \
                     value at a time: the expression written to `{column}` has to be a \
                     value, and there is no database behind this table to evaluate it in",
                    self.table, self.provider, self.module
                )));
            };
            record.insert((*column).to_owned(), sc_types::value_to_script_json(value));
        }
        Ok(Json::Object(record))
    }

    /// The primary key of one row, as the module speaks it.
    fn key_of(&self, pk: &str, row: &ValueRow) -> Json {
        row.get(pk)
            .map_or(Json::Null, sc_types::value_to_script_json)
    }

    async fn run_insert(&self, insert: &Insert) -> Result<Vec<Row>> {
        self.require(Write::Insert)?;
        let pk = self.primary_key().ok();
        let mut written = Vec::with_capacity(insert.rows.len());
        for values in &insert.rows {
            if values.len() != insert.columns.len() {
                return Err(Error::invalid(format!(
                    "`{}`: the insert names {} columns and supplies {} values",
                    self.table,
                    insert.columns.len(),
                    values.len()
                )));
            }
            let pairs: Vec<(&str, &Expr)> = insert
                .columns
                .iter()
                .map(String::as_str)
                .zip(values.iter())
                .collect();
            let record = self.record(&pairs)?;
            let key = self
                .host
                .insert(
                    &self.module,
                    &self.provider,
                    &self.table,
                    &self.config,
                    &record,
                )
                .await?;
            written.push((record, key));
        }
        if insert.returning.is_empty() {
            return Ok(Vec::new());
        }
        // What comes back is the row as the provider now has it, read through the
        // key it just answered — a remote database fills in defaults, a serial
        // key and a trigger's columns, and `RETURNING *` has to show them.
        //
        // A provider that answers no key (v1 allows it: `insertRow` may return
        // nothing) leaves only the record as written, which is not an error — it
        // is every column the caller supplied, and the caller supplied them all
        // through `create_row`.
        let mut rows = Vec::with_capacity(written.len());
        for (record, key) in written {
            match (pk, key.is_null()) {
                (Some(pk), false) => {
                    let filter = Expr::col(pk).eq(Expr::lit(sc_types::value_from_json(&key)));
                    let mut found = self.matching(Some(&filter)).await?;
                    match found.pop() {
                        Some(row) => rows.push(row),
                        None => rows.push(value_row(&self.fields, &record)),
                    }
                }
                _ => rows.push(value_row(&self.fields, &record)),
            }
        }
        self.returning(&insert.returning, rows)
    }

    async fn run_update(&self, update: &Update) -> Result<Vec<Row>> {
        self.require(Write::Update)?;
        let pk = self.primary_key()?;
        let pairs: Vec<(&str, &Expr)> = update
            .assignments
            .iter()
            .map(|a| (a.column.as_str(), &a.value))
            .collect();
        let record = self.record(&pairs)?;
        // Which rows: the filter is a `Select` this provider can already answer,
        // and its rows' keys are the addresses `updateRow` takes.
        let targets = self.matching(update.filter.as_ref()).await?;
        let mut keys = Vec::with_capacity(targets.len());
        for row in &targets {
            let key = self.key_of(pk, row);
            self.host
                .update(
                    &self.module,
                    &self.provider,
                    &self.table,
                    &self.config,
                    &key,
                    &record,
                )
                .await?;
            keys.push(key);
        }
        if update.returning.is_empty() {
            return Ok(Vec::new());
        }
        // Read back, because `updateRow` answers nothing and the caller asked
        // for the row **after** the change.
        let after = self.by_keys(pk, &keys).await?;
        self.returning(&update.returning, after)
    }

    async fn run_delete(&self, delete: &Delete) -> Result<Vec<Row>> {
        self.require(Write::Delete)?;
        let pk = self.primary_key()?;
        // Read **first**: a delete's `RETURNING` is the row as it was, and after
        // the delete there is nothing left to read. It is also what makes the
        // `where` handed to the provider exact.
        let targets = self.matching(delete.filter.as_ref()).await?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<Json> = targets.iter().map(|row| self.key_of(pk, row)).collect();
        // `{ pk: { in: [...] } }` — v1's own vocabulary, and the narrowest thing
        // that can be said: the statement's own filter may be an expression v1's
        // `where` object cannot spell, and a provider handed `{}` would delete
        // the table.
        let filter = json_in(pk, keys);
        self.host
            .delete(
                &self.module,
                &self.provider,
                &self.table,
                &self.config,
                &filter,
            )
            .await?;
        self.returning(&delete.returning, targets)
    }

    /// The rows with these keys, as the provider has them now.
    async fn by_keys(&self, pk: &str, keys: &[Json]) -> Result<Vec<ValueRow>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let set: Vec<Expr> = keys
            .iter()
            .map(|key| Expr::lit(sc_types::value_from_json(key)))
            .collect();
        let filter = Expr::In {
            e: Box::new(Expr::col(pk)),
            set: sc_query::InSet::List(set),
        };
        self.matching(Some(&filter)).await
    }
}

/// Which of the three a refusal is about.
#[derive(Debug, Clone, Copy)]
enum Write {
    Insert,
    Update,
    Delete,
}

impl Write {
    /// v1's own name for it, which is what a module author has to add.
    fn v1_method(self) -> &'static str {
        match self {
            Write::Insert => "insertRow",
            Write::Update => "updateRow",
            Write::Delete => "deleteRows",
        }
    }
}

/// v1's `{ column: { in: [...] } }`.
fn json_in(column: &str, values: Vec<Json>) -> Json {
    let mut inner = serde_json::Map::new();
    inner.insert("in".into(), Json::Array(values));
    let mut outer = serde_json::Map::new();
    outer.insert(column.to_owned(), Json::Object(inner));
    Json::Object(outer)
}

#[async_trait]
impl TableProvider for ProvidedTableProvider {
    fn fields(&self) -> Vec<DataField> {
        self.fields.clone()
    }

    async fn query(&self, select: &Select) -> Result<RowStream> {
        // The hint first, so a provider that can do the work in its own backend
        // is given the chance; then the query itself, over whatever came back.
        let (filter, options) = pushdown(select);
        let json = self
            .host
            .rows(
                &self.module,
                &self.provider,
                &self.table,
                &self.config,
                &filter,
                &options,
            )
            .await?;
        let rows: Vec<ValueRow> = json
            .iter()
            .map(|row| value_row(&self.fields, row))
            .collect();
        let out: Vec<Row> = run_select_over(select, &self.table, rows)?;
        Ok(RowStream::from_rows(out))
    }

    async fn write(&self, change: &Statement) -> Result<RowStream> {
        let rows = match change {
            Statement::Insert(insert) => self.run_insert(insert).await?,
            Statement::Update(update) => self.run_update(update).await?,
            Statement::Delete(delete) => self.run_delete(delete).await?,
            // A `Select` or a `Raw` is not a write, and a `Raw` in particular is
            // admin-authored SQL for *a database* — there is none here.
            Statement::Select(_) | Statement::Raw { .. } => {
                return Err(Error::invalid(format!(
                    "`{}` is served by the table provider `{}` of `{}`, which is written through \
                     v1's `insertRow`/`updateRow`/`deleteRows`: this statement is none of the \
                     three",
                    self.table, self.provider, self.module
                )));
            }
        };
        Ok(RowStream::from_rows(rows))
    }
}
