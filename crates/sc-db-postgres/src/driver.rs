//! [`PgDriver`] — a pooled connection to one Postgres database, and its
//! [`DatabaseDriver`] implementation.
//!
//! The driver owns a `deadpool-postgres` pool and renders statements with
//! [`PgDialect`]. Query and DDL execution live in [`crate::exec`] (shared with
//! transactions); introspection in [`crate::introspect`]; DDL rendering in
//! [`crate::ddl`]; transactions in [`crate::transaction`]. This module wires
//! them together and exposes them both as inherent methods (convenient, and what
//! the crate's tests use) and through the `DatabaseDriver` trait (for the
//! catalog's `Arc<dyn DatabaseDriver>`).

use async_trait::async_trait;
use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod};
use std::sync::{Arc, RwLock};

use sc_db::{
    DatabaseDriver, DbCapabilities, DescribedColumn, PhysicalTable, RowStream, SchemaChange,
    SpatialSupport, Transaction,
};
use sc_error::{Error, Result};
use sc_query::{SqlDialect, Statement};
use tokio_postgres::{Config, NoTls};

use crate::dialect::PgDialect;

/// A connection pool to one Postgres database, plus its SQL dialect.
///
/// Cheap to clone conceptually (the underlying `Pool` is an `Arc`), though it is
/// normally held once behind an `Arc<dyn DatabaseDriver>` by the catalog.
pub struct PgDriver {
    pool: Pool,
    dialect: PgDialect,
    /// The one schema this connection presents, when it is scoped to one.
    ///
    /// `None` is the primary database's shape: every non-system schema the
    /// connection can see, which is what the zero-setup promise of §9 means by
    /// "everything reachable through a connection". A *secondary* connection —
    /// one an admin added in the Connections screen — names a schema instead,
    /// because two schemas of a foreign database would contribute two tables
    /// under one name and the catalog keys tables by name.
    ///
    /// It is enforced in two places, and both are needed: `search_path` on the
    /// connection, so an unqualified name in a rendered statement resolves in
    /// that schema and nowhere else, and a filter over
    /// [`introspect`](PgDriver::introspect), so nothing outside it is ever
    /// offered as a table.
    schema: Option<String>,
    /// Whether the database has PostGIS, as last found out (see
    /// [`sc_db::SpatialSupport`]). Shared, so a clone answers the same.
    spatial: Arc<RwLock<SpatialSupport>>,
}

/// What an admin types to reach another Postgres database (§9's Connections
/// screen): the parts of a connection rather than a URL.
///
/// A URL would be one box and fewer types, and is deliberately not what this is.
/// The parts are stored as columns, so the password is a column that can be
/// declared secret and redacted on the way out; a URL would carry the password
/// inside a text field that every reader would have to remember to scrub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgConnectParams {
    /// Host name or, when it starts with `/`, a Unix socket directory.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// The database to connect to.
    pub database: String,
    /// The role to connect as.
    pub user: String,
    /// That role's password; empty for a connection that needs none (a Unix
    /// socket with peer authentication, say).
    pub password: String,
    /// The one schema the connection presents (see [`PgDriver::schema`]).
    pub schema: String,
}

impl PgDriver {
    /// Wrap an already-built connection pool, presenting every schema it can
    /// see.
    pub fn from_pool(pool: Pool) -> Self {
        PgDriver {
            pool,
            dialect: PgDialect::new(),
            schema: None,
            spatial: Arc::new(RwLock::new(SpatialSupport::Unavailable {
                reason: "PostGIS has not been looked for in this database yet".to_owned(),
            })),
        }
    }

    /// The same driver scoped to one schema — see [`PgDriver::schema`].
    ///
    /// This alone does **not** set `search_path`: a pool that was built without
    /// it would keep resolving unqualified names by the server's default. Use
    /// [`connect_params`](PgDriver::connect_params), which does both.
    pub fn scoped_to(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// The schema this connection is scoped to, if it is scoped to one.
    pub fn scope(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// Build a pooled driver from the parts an admin typed, scoped to one
    /// schema.
    ///
    /// The scope is applied to the *connection* as `search_path`, not bolted
    /// onto each rendered statement. That is the difference between a scope and
    /// a prefix: every statement the query layer renders — a select, an insert,
    /// a `RETURNING` — names a table unqualified, and none of them has to learn
    /// that this particular connection is special.
    pub fn connect_params(params: &PgConnectParams) -> Result<Self> {
        let schema = params.schema.trim();
        if schema.is_empty() {
            return Err(Error::config("a database connection needs a schema"));
        }
        // `search_path` is a connection option, not a bind parameter, so it is
        // interpolated — and therefore checked. A schema whose name needs
        // quoting is refused rather than quoted, because the answer to "what
        // does `-c search_path=a b` mean" is nothing anybody should have to
        // know.
        if !schema
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        {
            return Err(Error::config(format!(
                "schema `{schema}` is not a plain identifier; \
                 letters, digits, `_` and `$` only"
            )));
        }

        let mut config = Config::new();
        config
            .host(&params.host)
            .port(params.port)
            .dbname(&params.database)
            .user(&params.user)
            .options(format!("-c search_path={schema}"));
        // An empty password is "no password", not the empty password: setting it
        // makes libpq offer one, which a socket connection using peer
        // authentication refuses.
        if !params.password.is_empty() {
            config.password(&params.password);
        }

        Ok(Self::from_config(&config)?.scoped_to(schema))
    }

    /// Build a pooled driver from a libpq/URL connection string, e.g.
    /// `postgres://user:pass@host:5432/dbname`.
    pub async fn connect(url: &str) -> Result<Self> {
        let config = url
            .parse::<Config>()
            .map_err(|e| Error::config(format!("invalid postgres connection string: {e}")))?;
        Self::from_config(&config)
    }

    /// Build a pooled driver from a parsed tokio-postgres [`Config`].
    pub fn from_config(config: &Config) -> Result<Self> {
        let mgr_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let manager = Manager::from_config(config.clone(), NoTls, mgr_config);
        let pool = Pool::builder(manager)
            .max_size(8)
            .build()
            .map_err(|e| Error::database(format!("build pool: {e}")))?;
        Ok(Self::from_pool(pool))
    }

    /// The underlying connection pool.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// The Postgres SQL dialect used to render statements for this driver.
    pub fn dialect(&self) -> &PgDialect {
        &self.dialect
    }

    /// What this Postgres backend supports.
    pub fn capabilities(&self) -> DbCapabilities {
        DbCapabilities {
            row_level_security: true,
            composite_pk: true,
            listen_notify: true,
            returning: true,
            identity_sequences: true,
            unlogged_tables: true,
            native_temporal_types: true,
        }
    }

    /// Read the live schema of every user table reachable through the
    /// connection (there is no discovery step — see [`crate::introspect`]).
    pub async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
        let client = self.client().await?;
        let mut tables = crate::introspect::introspect(&client).await?;
        if let Some(schema) = &self.schema {
            tables.retain(|t| t.schema.as_deref() == Some(schema.as_str()));
        }
        Ok(tables)
    }

    /// Render `stmt` to Postgres SQL, run it on a pooled connection, and return
    /// the rows.
    pub async fn query(&self, stmt: &Statement) -> Result<RowStream> {
        let client = self.client().await?;
        crate::exec::run_query(&client, &self.dialect, stmt).await
    }

    /// Apply a single schema change (create/drop table, add/drop column).
    /// Creating a table emits exactly the columns given — no `id` column is
    /// invented (see [`crate::ddl`]).
    pub async fn apply_schema(&self, change: &SchemaChange) -> Result<()> {
        let client = self.client().await?;
        crate::exec::run_ddl(&client, &self.dialect, change).await
    }

    /// Render `change` as the Postgres DDL `apply_schema` would run, without
    /// running it — the one renderer, borrowed by the generated `schema.sql` an
    /// application's project carries (§13.3).
    pub fn render_ddl(&self, change: &SchemaChange) -> Result<String> {
        crate::ddl::render(&self.dialect, change)
    }

    /// Prepare `sql` (parameters typed by `param_types`) and report the result
    /// columns Postgres says it will produce, without running it — how a custom
    /// SQL query is typed, and how one that will not prepare is refused at the
    /// keyboard (see [`crate::exec::describe`]).
    pub async fn describe(
        &self,
        sql: &str,
        param_types: &[String],
    ) -> Result<Vec<DescribedColumn>> {
        let client = self.client().await?;
        crate::exec::describe(&client, sql, param_types).await
    }

    /// Begin a transaction on a dedicated pooled connection. Metadata mutations
    /// run inside one; the returned handle is committed or rolled back exactly
    /// once (dropping it rolls back).
    pub async fn begin(&self) -> Result<Box<dyn Transaction>> {
        let client = self.client().await?;
        let tx = crate::transaction::PgTransaction::begin(client, self.dialect).await?;
        Ok(Box::new(tx))
    }

    /// Whether the database has PostGIS, as last found out.
    pub fn spatial(&self) -> SpatialSupport {
        self.spatial
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Ask the database whether it has PostGIS, and remember the answer.
    pub async fn detect_spatial(&self) -> Result<SpatialSupport> {
        let client = self.client().await?;
        let found = crate::spatial::detect(&client).await?;
        self.remember_spatial(&found);
        Ok(found)
    }

    /// Install PostGIS where the role may, and remember the answer.
    pub async fn enable_spatial(&self) -> Result<SpatialSupport> {
        let client = self.client().await?;
        let found = crate::spatial::enable(&client).await?;
        self.remember_spatial(&found);
        Ok(found)
    }

    fn remember_spatial(&self, found: &SpatialSupport) {
        match self.spatial.write() {
            Ok(mut s) => *s = found.clone(),
            Err(e) => *e.into_inner() = found.clone(),
        }
    }

    /// Check out a pooled connection.
    async fn client(&self) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|e| Error::database(format!("checkout connection: {e}")))
    }
}

// The trait impl delegates to the inherent methods above. Inherent methods
// shadow trait methods in resolution, so the `PgDriver::method(self, …)` calls
// below refer to the inherent implementations (no recursion).
#[async_trait]
impl DatabaseDriver for PgDriver {
    async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
        PgDriver::introspect(self).await
    }

    async fn query(&self, stmt: &Statement) -> Result<RowStream> {
        PgDriver::query(self, stmt).await
    }

    async fn apply_schema(&self, change: &SchemaChange) -> Result<()> {
        PgDriver::apply_schema(self, change).await
    }

    fn render_ddl(&self, change: &SchemaChange) -> Result<String> {
        PgDriver::render_ddl(self, change)
    }

    async fn describe(&self, sql: &str, param_types: &[String]) -> Result<Vec<DescribedColumn>> {
        PgDriver::describe(self, sql, param_types).await
    }

    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        PgDriver::begin(self).await
    }

    fn capabilities(&self) -> DbCapabilities {
        PgDriver::capabilities(self)
    }

    fn dialect(&self) -> &dyn SqlDialect {
        &self.dialect
    }

    fn spatial(&self) -> SpatialSupport {
        PgDriver::spatial(self)
    }

    async fn detect_spatial(&self) -> Result<SpatialSupport> {
        PgDriver::detect_spatial(self).await
    }

    async fn enable_spatial(&self) -> Result<SpatialSupport> {
        PgDriver::enable_spatial(self).await
    }
}
