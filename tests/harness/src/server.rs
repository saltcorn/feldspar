//! A fresh database with a catalog over it: what almost every test above the
//! catalog starts from (the `catalog` feature).
//!
//! ```ignore
//! let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
//! sc_auth::bootstrap(&catalog).await?;
//! ```
//!
//! The builder registers the catalog's teardown with the database, so the
//! services a test installs on the catalog are freed when the test ends
//! (CLAUDE.md, "Tests: tear the catalog down"). Keep `db` alive for the test:
//! dropping it tears the catalog down and deletes the database.

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;

use crate::TestDb;

/// A per-test database, and the catalog read from it.
pub struct TestServer {
    /// The database. Dropping it tears the catalog down and deletes it.
    pub db: TestDb,
    /// The catalog, torn down with `db`.
    pub catalog: Arc<Catalog>,
    /// The driver the catalog reads through.
    pub driver: Arc<PgDriver>,
}

impl TestServer {
    /// A builder for a fresh database with a catalog over it.
    pub fn builder() -> TestServerBuilder {
        TestServerBuilder::default()
    }
}

/// How the database is prepared before the catalog first reads it.
#[derive(Default)]
pub struct TestServerBuilder {
    db: Option<TestDb>,
    sql: Vec<String>,
    keep_template_users: bool,
}

impl TestServerBuilder {
    /// Start from `db` rather than a new database: one made with
    /// [`TestDb::with_postgis`] or [`TestDb::with_pgvector`].
    pub fn on(mut self, db: TestDb) -> Self {
        self.db = Some(db);
        self
    }

    /// Run `sql` on the database before the catalog reads it: the tables a
    /// test starts with. Statements run in the order given.
    pub fn sql(mut self, sql: impl Into<String>) -> Self {
        self.sql.push(sql.into());
        self
    }

    /// Keep the `users` tables the template carries (see
    /// [`TestDb::drop_template_users`]); they are dropped otherwise.
    pub fn keep_template_users(mut self) -> Self {
        self.keep_template_users = true;
        self
    }

    /// Make the database, run the SQL, and read the catalog, with its
    /// teardown registered on the database.
    pub async fn build(mut self) -> Result<TestServer> {
        let db = match self.db.take() {
            Some(db) => db,
            None => TestDb::new().await?,
        };
        let (catalog, driver) = self.prepare(&db).await?;
        Ok(TestServer {
            db,
            catalog,
            driver,
        })
    }

    /// As [`build`](Self::build), on a database the caller keeps: for a
    /// helper that is handed one.
    pub async fn build_on(self, db: &TestDb) -> Result<Arc<Catalog>> {
        Ok(self.prepare(db).await?.0)
    }

    async fn prepare(&self, db: &TestDb) -> Result<(Arc<Catalog>, Arc<PgDriver>)> {
        if !self.keep_template_users {
            db.drop_template_users().await?;
        }
        for sql in &self.sql {
            db.execute(sql).await?;
        }
        let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
        let catalog = Arc::new(Catalog::init(driver.clone() as Arc<dyn DatabaseDriver>).await?);
        db.on_teardown(catalog.teardown_hook());
        Ok((catalog, driver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_catalog_reads_the_sql_and_is_torn_down_with_the_database() -> Result<()> {
        let TestServer { db, catalog, .. } = TestServer::builder()
            .sql("CREATE SCHEMA tenant; CREATE TABLE tenant.users (id int)")
            .sql("CREATE TABLE posts (id serial primary key, title text)")
            .build()
            .await?;
        assert!(catalog.get("posts")?.is_some());
        // The template's `users` tables were dropped before the SQL ran, so
        // the one the SQL made is still there.
        let left = db
            .client()
            .await?
            .query(
                "SELECT 1 FROM information_schema.tables WHERE table_name = 'users'",
                &[],
            )
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?;
        assert_eq!(left.len(), 1);
        db.drop_template_users().await?;
        let left = db
            .client()
            .await?
            .query(
                "SELECT 1 FROM information_schema.tables WHERE table_name = 'users'",
                &[],
            )
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?;
        assert!(left.is_empty());

        // The teardown hook holds the catalog until the database goes.
        let held = Arc::strong_count(&catalog);
        drop(db);
        assert_eq!(Arc::strong_count(&catalog), held - 1);
        Ok(())
    }
}
