//! Table fixtures shared by this module's unit tests.
//!
//! Building a [`Table`] by hand is a dozen fields of noise that says nothing
//! about the test doing it. These build the smallest table the naming and
//! schema-building code can be asked about — no catalog, no database, no
//! overlay — so a test reads as the schema it is about.

use std::sync::Arc;

use async_trait::async_trait;
use sc_catalog::{
    AccessRules, Catalog, DataField, DataFieldKind, DbId, FieldId, FileStoreId, Table, TableSource,
};
use sc_db::{
    DatabaseDriver, DbCapabilities, ForeignKey, PhysicalTable, RowStream, SchemaChange, Transaction,
};
use sc_error::{Error, Result};
use sc_query::{SqlDialect, Statement};
use sc_types::{Attrs, BasicType, TypeRef};

/// A table with `fields`, whose primary key is `id` when it has such a column.
pub fn table_of(name: &str, fields: Vec<DataField>) -> Table {
    let primary_key = fields
        .iter()
        .filter(|f| f.primary_key)
        .map(|f| f.base.name.clone())
        .collect();
    Table {
        name: name.to_owned(),
        database: DbId::primary(),
        source: TableSource::Database,
        primary_key,
        fields,
        label: name.to_owned(),
        description: String::new(),
        access: AccessRules::default(),
        attributes: Attrs::new(),
        overlay: None,
        ownership: None,
        ownership_error: None,
        rls_enabled: false,
        constraints: Vec::new(),
    }
}

/// A nullable text column.
pub fn plain_field(name: &str) -> DataField {
    DataField::plain(name, TypeRef::Basic(BasicType::Text))
}

/// A column of an explicit basic type.
pub fn typed_field(name: &str, ty: BasicType) -> DataField {
    DataField::plain(name, TypeRef::Basic(ty))
}

/// A required integer primary key.
pub fn id_field() -> DataField {
    DataField::plain("id", TypeRef::Basic(BasicType::Int))
        .required()
        .primary_key()
}

/// A `Key` column pointing at `target_table`.`target_field`.
pub fn key_field(name: &str, target_table: &str, target_field: &str) -> DataField {
    let mut field = DataField::plain(name, TypeRef::Basic(BasicType::Int));
    field.kind = DataFieldKind::Key {
        target_table: target_table.to_owned(),
        target_field: FieldId(target_field.to_owned()),
        summary_field: None,
    };
    field
}

/// A catalog over a database with no tables in it.
///
/// The routing, the SDL and the document's own validation are decided before a
/// single row is read, and this is what lets those be tested here rather than
/// only against Postgres. A resolver that *does* reach for a table finds it
/// missing and says so — which is itself worth asserting: it proves the read
/// goes to the live catalog rather than to something captured at mount.
pub async fn empty_catalog() -> Arc<Catalog> {
    catalog_of(Vec::new()).await
}

/// A catalog over a database that has exactly these tables in it, and no rows.
///
/// The tables are **introspected** rather than assembled, so what a test asks
/// about is a `Table` built the way a real one is: the keys come from the foreign
/// keys, the types from the column types, and the schema shape a formula is
/// validated against is the catalog's own. Everything a plan or a query string
/// resolves happens before a row is read, and this is what lets that be tested
/// here rather than only against Postgres.
pub async fn catalog_of(tables: Vec<PhysicalTable>) -> Arc<Catalog> {
    Arc::new(
        Catalog::init(Arc::new(EmptyDriver { tables }) as Arc<dyn DatabaseDriver>)
            .await
            .expect("a catalog"),
    )
}

/// One introspected table: `columns` as `(name, sql type, nullable)`, `keys` as
/// `(column, target table, target column)`, and `id` as the primary key when
/// there is such a column.
pub fn physical(
    name: &str,
    columns: &[(&str, &str, bool)],
    keys: &[(&str, &str, &str)],
) -> PhysicalTable {
    PhysicalTable {
        name: name.to_owned(),
        schema: None,
        columns: columns
            .iter()
            .map(|(name, sql_type, nullable)| sc_db::Column {
                name: (*name).to_owned(),
                sql_type: (*sql_type).to_owned(),
                nullable: *nullable,
                generated: None,
            })
            .collect(),
        primary_key: columns
            .iter()
            .filter(|(name, _, _)| *name == "id")
            .map(|(name, _, _)| (*name).to_owned())
            .collect(),
        foreign_keys: keys
            .iter()
            .map(|(column, table, target)| ForeignKey {
                columns: vec![(*column).to_owned()],
                referenced_table: (*table).to_owned(),
                referenced_columns: vec![(*target).to_owned()],
            })
            .collect(),
        constraints: Vec::new(),
    }
}

/// A driver over a fixed set of tables: it introspects to them and refuses to
/// run anything, so a test that accidentally depended on a query fails loudly.
struct EmptyDriver {
    tables: Vec<PhysicalTable>,
}

/// The dialect the empty driver reports; no statement ever reaches it.
struct EmptyDialect;

impl SqlDialect for EmptyDialect {
    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
    fn placeholder(&self, position: usize) -> String {
        format!("${position}")
    }
}

#[async_trait]
impl DatabaseDriver for EmptyDriver {
    async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
        Ok(self.tables.clone())
    }

    async fn query(&self, _stmt: &Statement) -> Result<RowStream> {
        Err(Error::msg("this test's catalog has no database behind it"))
    }

    async fn apply_schema(&self, _change: &SchemaChange) -> Result<()> {
        Err(Error::msg("this test's catalog has no database behind it"))
    }

    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        Err(Error::msg("this test's catalog has no database behind it"))
    }

    fn capabilities(&self) -> DbCapabilities {
        DbCapabilities::none()
    }

    fn dialect(&self) -> &dyn SqlDialect {
        &EmptyDialect
    }
}

/// A `File` column in the named store.
pub fn file_field(name: &str, store: &str) -> DataField {
    let mut field = DataField::plain(name, TypeRef::Basic(BasicType::Text));
    field.kind = DataFieldKind::File {
        store: FileStoreId(store.to_owned()),
        folder: None,
        mime_allow: Vec::new(),
        on_delete: Default::default(),
        on_conflict: Default::default(),
    };
    field
}
