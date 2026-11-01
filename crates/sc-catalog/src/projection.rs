//! [`SchemaProjection`]: the schema as a change *will leave it*, rather than as
//! it is (TODO Phase 7).
//!
//! A batch of schema operations validates against the schema it **ends** with,
//! not the one it began with: a `Key` pointing at a table created three
//! operations earlier resolves, and an ownership formula naming a field added two
//! operations earlier validates. Neither is possible against the live catalog,
//! whose cache only changes when a reload runs — and a reload cannot run in the
//! middle of a batch, because the batch's DDL is still uncommitted.
//!
//! So the resolution happens against a projection: a plain `Vec<Table>` seeded
//! from the catalog and edited operation by operation. Everything the validators
//! need off the catalog — the [`SchemaShape`](sc_expr::SchemaShape) formulas
//! validate against, the user field types the GUC translator casts with — is
//! derived from that vector instead, so one code path serves both "the live
//! schema" ([`SchemaProjection::live`]) and "the schema after this batch".

use std::collections::BTreeMap;

use sc_error::Result;

use crate::catalog::{Catalog, USERS_PASSWORD_COLUMN, USERS_TABLE, schema_shape_of_tables};
use crate::table::Table;

/// A set of tables treated as *the* schema: the live one, or the one a batch of
/// operations will leave behind.
#[derive(Debug, Clone, Default)]
pub struct SchemaProjection {
    tables: Vec<Table>,
    /// The module functions in scope, as `(function, module)` pairs (§4b) —
    /// empty for a projection nobody built from a live catalog.
    module_functions: Vec<(String, String)>,
}

impl SchemaProjection {
    /// A projection over exactly these tables, and no module functions.
    ///
    /// No module functions because nothing here can know of any: a projection
    /// built from a list of tables is what a test and the GraphQL name planner
    /// build, and neither has a server behind it. A projection that must
    /// validate a formula calling one comes from [`live`](Self::live).
    pub fn new(tables: Vec<Table>) -> SchemaProjection {
        SchemaProjection {
            tables,
            module_functions: Vec::new(),
        }
    }

    /// The catalog's current tables — the projection that changes nothing.
    ///
    /// Carries the catalog's module functions (§4b), so a calc field saved
    /// through the schema editor may call one: what an edit is validated
    /// against is the schema it will leave behind, and the modules it will
    /// leave behind are the ones installed now.
    pub fn live(catalog: &Catalog) -> Result<SchemaProjection> {
        Ok(SchemaProjection {
            tables: catalog.tables()?,
            module_functions: catalog.module_function_names(),
        })
    }

    /// Every table in the projection, in the order it was built.
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    /// The table with this name, if the projection has one.
    pub fn get(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// The table with this name, mutably.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.iter_mut().find(|t| t.name == name)
    }

    /// Add a table to the projection.
    pub fn insert(&mut self, table: Table) {
        match self.tables.iter_mut().find(|t| t.name == table.name) {
            Some(existing) => *existing = table,
            None => self.tables.push(table),
        }
    }

    /// Remove a table from the projection, reporting whether one was there.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.tables.len();
        self.tables.retain(|t| t.name != name);
        before != self.tables.len()
    }

    /// This projection as the [`SchemaShape`](sc_expr::SchemaShape) formula
    /// validation and translation consume — the same projection
    /// [`Catalog::schema_shape`] makes of the live cache.
    pub fn shape(&self) -> sc_expr::SchemaShape {
        let mut shape = schema_shape_of_tables(self.tables.iter());
        for (name, module) in &self.module_functions {
            shape = shape.module_function(name, module);
        }
        shape
    }

    /// Each user field mapped to its SQL type, as
    /// [`Catalog::user_field_types`] derives it from the live cache — but from
    /// *this* schema, so a batch that adds a column to `users` and then writes a
    /// formula naming it translates.
    pub fn user_field_types(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        if let Some(users) = self.get(USERS_TABLE) {
            for field in &users.fields {
                if field.base.name != USERS_PASSWORD_COLUMN {
                    map.insert(
                        field.base.name.clone(),
                        field.base.type_.sql_type().to_owned(),
                    );
                }
            }
        }
        map
    }

    /// Every `Key` field, anywhere in the projection, that points at `table` —
    /// as `(table name, field name, target field)`.
    ///
    /// The question a drop has to ask before it issues DDL: dropping a table
    /// another table references is a foreign-key error from the database, and a
    /// foreign-key error is not something a model (or an admin) can act on. This
    /// is what lets the refusal name the fields to remove first.
    pub fn referencing_fields(&self, table: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for other in &self.tables {
            for field in &other.fields {
                if let crate::field::DataFieldKind::Key { target_table, .. } = &field.kind
                    && target_table == table
                    && other.name != table
                {
                    out.push((other.name.clone(), field.base.name.clone()));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::{DataField, DataFieldKind, DbId, FieldId};
    use crate::table::TableSource;
    use sc_types::{BasicType, TypeRef};

    fn table(name: &str, fields: Vec<DataField>) -> Table {
        Table {
            name: name.to_owned(),
            database: DbId::primary(),
            source: TableSource::Database,
            primary_key: vec!["id".to_owned()],
            fields,
            label: name.to_owned(),
            description: String::new(),
            access: crate::table::AccessRules::default(),
            attributes: sc_types::Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    fn key(name: &str, target: &str) -> DataField {
        let mut field = DataField::plain(name, TypeRef::Basic(BasicType::Int));
        field.kind = DataFieldKind::Key {
            target_table: target.to_owned(),
            target_field: FieldId("id".to_owned()),
            summary_field: None,
        };
        field
    }

    #[test]
    fn a_projection_answers_who_points_at_a_table() {
        let id = DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key();
        let projection = SchemaProjection::new(vec![
            table("clients", vec![id.clone()]),
            table("matters", vec![id.clone(), key("client", "clients")]),
            table(
                "time_entries",
                vec![id.clone(), key("matter", "matters"), key("who", "clients")],
            ),
        ]);
        assert_eq!(
            projection.referencing_fields("clients"),
            vec![
                ("matters".to_owned(), "client".to_owned()),
                ("time_entries".to_owned(), "who".to_owned()),
            ]
        );
        assert!(projection.referencing_fields("time_entries").is_empty());
    }

    #[test]
    fn a_projected_table_is_in_the_shape_the_live_one_is_not() {
        let id = DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key();
        let mut projection = SchemaProjection::new(vec![table("clients", vec![id.clone()])]);
        projection.insert(table("matters", vec![id, key("client", "clients")]));
        let shape = projection.shape();
        assert!(shape.tables.contains_key("matters"));
        // …and removing it takes it back out, so a drop earlier in a batch makes
        // a later reference to it fail validation rather than fail in the DDL.
        assert!(projection.remove("matters"));
        assert!(!projection.shape().tables.contains_key("matters"));
    }
}
