//! Database layer (layer 2): the [`DatabaseDriver`] trait and the data types it
//! exchanges with the rest of Saltcorn.
//!
//! A driver is one connected database. This crate owns the *contract* — the
//! trait plus its introspection descriptors ([`PhysicalTable`]), schema-change
//! requests ([`SchemaChange`]), result rows ([`Row`]/[`RowStream`]), transaction
//! handle ([`Transaction`]), and capability advertisement ([`DbCapabilities`]) —
//! while a concrete backend such as `sc-db-postgres` implements it (technical
//! design §5).
//!
//! Design invariants that live here as types rather than prose:
//!
//! - **No table discovery step.** Everything reachable through a connection is
//!   returned by [`DatabaseDriver::introspect`]; there is no register/enable
//!   phase.
//! - **No implicit primary key.** [`SchemaChange::CreateTable`] carries an
//!   explicit (possibly empty, possibly composite) primary key — creating a
//!   table never invents an `id` column.
//! - **Composite keys and FKs to non-PK columns** are first-class: primary keys
//!   and foreign-key column lists are always `Vec<String>`.

mod capabilities;
mod driver;
mod row;
mod schema;
mod spatial;

pub use capabilities::DbCapabilities;
pub use driver::{DatabaseDriver, Transaction};
pub use row::{Row, RowStream};
pub use schema::{
    Column, ColumnDef, ColumnGenerator, ColumnRef, CommentTarget, DescribedColumn, ForeignKey,
    IndexOn, PhysicalConstraint, PhysicalConstraintKind, PhysicalTable, SchemaChange,
};
pub use spatial::SpatialSupport;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use sc_error::Result;
    use sc_query::{Expr, Projection, Select, Source, SqlDialect, Statement, Value};

    use super::*;

    /// A minimal dialect so the mock driver can return one from `dialect()`.
    struct MockDialect;

    impl SqlDialect for MockDialect {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }

    /// An in-memory `DatabaseDriver` used only to prove the trait is usable and
    /// object-safe. `introspect`/`apply_schema` share a table list; `query`
    /// returns two canned rows.
    struct MockDriver {
        tables: Mutex<Vec<PhysicalTable>>,
        dialect: MockDialect,
    }

    impl MockDriver {
        fn new() -> Self {
            MockDriver {
                tables: Mutex::new(Vec::new()),
                dialect: MockDialect,
            }
        }
    }

    #[async_trait]
    impl DatabaseDriver for MockDriver {
        async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
            Ok(self
                .tables
                .lock()
                .map_err(|_| sc_error::Error::msg("mock lock poisoned"))?
                .clone())
        }

        async fn query(&self, _stmt: &Statement) -> Result<RowStream> {
            let cols = Arc::new(vec!["id".to_string(), "email".to_string()]);
            let rows = vec![
                Row::new(
                    cols.clone(),
                    vec![Value::Int(1), Value::Text("a@b.c".into())],
                )?,
                Row::new(cols, vec![Value::Int(2), Value::Text("d@e.f".into())])?,
            ];
            Ok(RowStream::from_rows(rows))
        }

        async fn apply_schema(&self, change: &SchemaChange) -> Result<()> {
            let mut tables = self
                .tables
                .lock()
                .map_err(|_| sc_error::Error::msg("mock lock poisoned"))?;
            match change {
                SchemaChange::CreateTable {
                    name,
                    columns,
                    primary_key,
                    // The mock advertises no capabilities, so it is never asked
                    // for an unlogged table — and would give an ordinary one.
                    ..
                } => {
                    tables.push(PhysicalTable {
                        name: name.clone(),
                        schema: None,
                        columns: columns
                            .iter()
                            .map(|c| Column {
                                name: c.name.clone(),
                                sql_type: c.sql_type.clone(),
                                nullable: c.nullable,
                                generated: c.generated.clone(),
                            })
                            .collect(),
                        primary_key: primary_key.clone(),
                        foreign_keys: Vec::new(),
                        constraints: Vec::new(),
                    });
                    Ok(())
                }
                other => Err(sc_error::Error::msg(format!(
                    "mock does not implement {other:?}"
                ))),
            }
        }

        async fn begin(&self) -> Result<Box<dyn Transaction>> {
            Ok(Box::new(MockTx { committed: false }))
        }

        fn capabilities(&self) -> DbCapabilities {
            DbCapabilities {
                composite_pk: true,
                returning: true,
                ..DbCapabilities::none()
            }
        }

        fn dialect(&self) -> &dyn SqlDialect {
            &self.dialect
        }
    }

    struct MockTx {
        committed: bool,
    }

    #[async_trait]
    impl Transaction for MockTx {
        async fn query(&mut self, _stmt: &Statement) -> Result<RowStream> {
            Ok(RowStream::empty())
        }
        async fn apply_schema(&mut self, _change: &SchemaChange) -> Result<()> {
            Ok(())
        }
        async fn commit(mut self: Box<Self>) -> Result<()> {
            self.committed = true;
            Ok(())
        }
        async fn rollback(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn driver_is_object_safe_and_usable() {
        // Held as a trait object exactly as the catalog will hold it.
        let driver: Arc<dyn DatabaseDriver> = Arc::new(MockDriver::new());

        // Capabilities are advertised, not assumed.
        let caps = driver.capabilities();
        assert!(caps.composite_pk);
        assert!(caps.returning);
        assert!(!caps.row_level_security);

        // Introspection starts empty (no discovery step, just a live read).
        assert!(driver.introspect().await.unwrap().is_empty());

        // A create-table with an explicit composite primary key and no invented
        // id column.
        let change = SchemaChange::CreateTable {
            name: "member".into(),
            columns: vec![
                ColumnDef::new("org", "int8").not_null(),
                ColumnDef::new("user_id", "int8").not_null(),
                ColumnDef::new("email", "text").unique(),
            ],
            primary_key: vec!["org".into(), "user_id".into()],
            unlogged: false,
        };
        driver.apply_schema(&change).await.unwrap();

        // The change is reflected in a subsequent introspect.
        let tables = driver.introspect().await.unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "member");
        assert_eq!(tables[0].primary_key, vec!["org", "user_id"]);
        assert_eq!(tables[0].columns.len(), 3);
        assert!(!tables[0].columns[0].nullable);

        // A query streams rows that are addressable by name and position.
        let stmt: Statement = Select::from(Source::table("member"))
            .columns(vec![Projection::expr(Expr::col("id"))])
            .into();
        let rows = driver
            .query(&stmt)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("id"), Some(&Value::Int(1)));
        assert_eq!(rows[0].get("email"), Some(&Value::Text("a@b.c".into())));
        assert_eq!(rows[1].get_index(0), Some(&Value::Int(2)));
        assert_eq!(rows[0].get("missing"), None);

        // The dialect renders a statement for this backend.
        let (sql, _binds) = driver.dialect().render(&stmt).unwrap();
        assert_eq!(sql, "SELECT \"id\" FROM \"member\"");

        // A transaction can be opened and committed exactly once.
        let mut tx = driver.begin().await.unwrap();
        assert!(
            tx.query(&stmt)
                .await
                .unwrap()
                .try_collect()
                .await
                .unwrap()
                .is_empty()
        );
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn row_length_mismatch_is_rejected() {
        let cols = Arc::new(vec!["a".to_string(), "b".to_string()]);
        assert!(Row::new(cols, vec![Value::Int(1)]).is_err());
    }

    #[test]
    fn schema_descriptors_round_trip_through_serde() {
        let table = PhysicalTable {
            name: "member".into(),
            schema: Some("public".into()),
            columns: vec![Column {
                name: "org".into(),
                sql_type: "int8".into(),
                nullable: false,
                generated: Some(ColumnGenerator::Identity),
            }],
            primary_key: vec!["org".into()],
            foreign_keys: vec![ForeignKey {
                columns: vec!["org".into()],
                referenced_table: "organisation".into(),
                referenced_columns: vec!["id".into()],
            }],
            constraints: Vec::new(),
        };
        let json = serde_json::to_string(&table).unwrap();
        let back: PhysicalTable = serde_json::from_str(&json).unwrap();
        assert_eq!(table, back);

        // The SchemaChange enum tags on `op`.
        let change = SchemaChange::DropColumn {
            table: "member".into(),
            column: "email".into(),
            if_exists: true,
        };
        let json = serde_json::to_string(&change).unwrap();
        assert!(json.contains("\"op\":\"drop_column\""));
        let back: SchemaChange = serde_json::from_str(&json).unwrap();
        assert_eq!(change, back);
    }
}
