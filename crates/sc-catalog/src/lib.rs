//! Catalog, Table, Field, TableProvider trait, cache (layer 4).
//!
//! This crate is the hub of the data layer (technical design §8). It holds the
//! connected database driver and an in-memory cache of its [`Table`]s, each a set
//! of [`DataField`]s built from live introspection — the MVP stores **no**
//! metadata beyond `information_schema`, so a freshly connected database is
//! immediately usable with zero setup. A [`Catalog`] can also create tables and
//! fields, keeping its cache in step, and hand out a [`TableProvider`] to run
//! queries against a table.
//!
//! The `_fd_tables` overlay ([`TableMeta`]) is the first stored metadata that
//! *adds* to introspection rather than replacing it: a table with no overlay row
//! is exactly as usable as it was before the overlay existed, which is what
//! keeps the zero-setup promise true (§9).
//!
//! A **provided** table (§8.3) is the one thing here that introspection does not
//! produce: its `_fd_tables` row is its whole definition, its fields are what a
//! module's table provider answers, and [`ProvidedTableProvider`] serves its
//! rows by asking that module and applying the [`Select`](sc_query::Select) to
//! the answer ([`inmem`]).
//!
//! Deferred (see the design): materialised providers (`Snapshot`/`Synced`),
//! **writable** table providers, and cross-process cache invalidation over a
//! bus.

mod calc;
mod caller;
mod catalog;
mod constraint;
mod db_connections;
mod events;
mod field;
mod field_meta;
mod file_stores;
pub mod inmem;
mod model_host;
mod observer;
mod origin;
mod prefetch;
mod projection;
mod provider;
mod rls;
mod table;
mod table_meta;
mod tx;
mod wakeups;

pub use caller::CallerContext;
pub use catalog::{Catalog, ProvidedTableIssue, SchemaStep};
pub use constraint::{
    ConstraintKind, META_KEY as CONSTRAINT_META_KEY, TableConstraint, constrained_fields,
    create_constraint_steps, drop_constraint_steps, formula_fields, full_text_expression,
    validate_formula, violated_constraint,
};
pub use db_connections::{
    BACKENDS, DB_CONNECTIONS_TABLE, DEFAULT_PORT, DEFAULT_SCHEMA, DbConnectionDef, DbConnectionId,
    DbConnections, POSTGRES_BACKEND, SQLITE_BACKEND, bootstrap_db_connections,
    check_db_connection_saveable, connect_all_db_connections, connect_db_connection,
    delete_db_connection, dial, list_db_connections, load_db_connection,
    load_db_connection_by_name, save_db_connection,
};
pub use events::{TableEvents, TableWrite, WriteOp};
pub use field::{Attrs, BaseField, DataField, DataFieldKind, DbId, FieldId, FileStoreId, TableId};
pub use field_meta::{
    FIELD_META_TABLE, FieldMeta, FieldMetaId, KIND_CALC, KIND_FILE, KIND_KEY, KIND_PLAIN,
    bootstrap_field_meta, delete_field_meta, file_kind_config_spec, key_kind_config_spec,
    list_field_meta, list_field_meta_for_table, load_field_meta, load_field_meta_by_field,
    save_field_meta, save_field_meta_row,
};
pub use file_stores::{
    FILE_STORES_TABLE, FileStoreConnections, NEW_LOCAL_FILE_STORE, QUERY_FILE_STORES,
    bootstrap_file_stores, check_file_store_saveable, choosable_file_stores,
    connect_all_file_stores, connect_file_store_def, delete_file_store,
    file_store_field_references, file_store_settings, list_file_stores, load_file_store,
    load_file_store_by_name, resolve_options, save_file_store, unique_file_store_name,
};
pub use model_host::{ModelHost, ModelSummary, PredictRows, check_model_calls};
pub use observer::{ReprojectedApp, SchemaChanged, SchemaObserver};
pub use origin::PublicOrigin;
pub use prefetch::prefetch_bindings;
pub use projection::SchemaProjection;
pub use provider::{
    DriverTableProvider, ProvidedTableProvider, ProvidedWrites, TableProvider, TableProviderHost,
    TableProviderHosts, TableProviderKind,
};
pub use rls::{
    Access, ROLE_GUC, clear_caller_context, disable_rls, disable_rls_sql, enable_rls,
    enable_rls_sql, run_in_context, run_in_context_read_only, set_caller_context,
};
pub use table::{AccessRules, FieldMergeIssue, Table, TableSource};
pub use table_meta::{
    ATTR_METADATA_TABLE, ATTR_OWNERSHIP_FORMULA, ATTR_PROVIDER_CONFIG, ATTR_PROVIDER_MODULE,
    ATTR_PROVIDER_NAME, ATTR_RLS_ENABLED, ProvidedTableDef, TABLE_META_TABLE, TableMeta,
    TableMetaId, bootstrap_table_meta, delete_table_meta, list_table_meta, load_table_meta,
    load_table_meta_by_name, orphan_table_meta, save_table_meta, save_table_meta_row,
};
pub use tx::SharedTx;
pub use wakeups::RunWakeups;

#[cfg(test)]
mod tests {
    use super::*;
    use sc_db::{Column, ColumnGenerator, ForeignKey, PhysicalTable};
    use sc_types::{BasicType, TypeRef};

    #[test]
    fn data_field_builder_and_column_def() {
        let f = DataField::plain("email", TypeRef::Basic(BasicType::Text))
            .label("Email address")
            .required()
            .unique();
        assert_eq!(f.base.label, "Email address");
        assert!(f.required && f.unique && !f.primary_key);

        let col = f.to_column_def();
        assert_eq!(col.name, "email");
        assert_eq!(col.sql_type, "text");
        assert!(!col.nullable); // required → NOT NULL
        assert!(col.unique);
    }

    #[test]
    fn a_key_fields_generator_travels_onto_its_column() {
        let id = DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key();
        let col = id.to_column_def();
        assert_eq!(col.sql_type, "uuid");
        assert!(!col.nullable);
        assert!(id.primary_key);
        // Nothing is invented here: a field that says nothing about filling
        // itself in gets a column that does not.
        assert!(col.generated.is_none());

        let generated = id.generated(ColumnGenerator::Default("gen_random_uuid()".into()));
        assert_eq!(
            generated.to_column_def().generated,
            Some(ColumnGenerator::Default("gen_random_uuid()".into()))
        );
    }

    #[test]
    fn base_field_label_defaults_to_name() {
        let b = BaseField::new("count", TypeRef::Basic(BasicType::Int));
        assert_eq!(b.name, "count");
        assert_eq!(b.label, "count");
        assert!(b.attributes.is_empty());
    }

    /// A physical table with a primary key and a single-column foreign key, used
    /// to check the introspection → catalog mapping.
    fn physical_with_fk() -> PhysicalTable {
        PhysicalTable {
            name: "book".into(),
            schema: Some("public".into()),
            columns: vec![
                Column {
                    name: "id".into(),
                    sql_type: "int8".into(),
                    nullable: false,
                    generated: None,
                },
                Column {
                    name: "title".into(),
                    sql_type: "text".into(),
                    nullable: false,
                    generated: None,
                },
                Column {
                    name: "author".into(),
                    sql_type: "int8".into(),
                    nullable: true,
                    generated: None,
                },
            ],
            primary_key: vec!["id".into()],
            foreign_keys: vec![ForeignKey {
                columns: vec!["author".into()],
                referenced_table: "person".into(),
                referenced_columns: vec!["id".into()],
            }],
            constraints: Vec::new(),
        }
    }

    #[test]
    fn table_from_physical_maps_pk_types_and_nullability() {
        let table = Table::from_physical(DbId::primary(), &physical_with_fk());
        assert_eq!(table.id, TableId("book".into()));
        assert_eq!(table.name, "book");
        assert_eq!(table.database, DbId::primary());
        assert_eq!(table.source, TableSource::Database);
        assert_eq!(table.primary_key, vec!["id".to_string()]);
        assert_eq!(table.access, AccessRules::default());

        let id = table.field("id").expect("id field");
        assert!(id.primary_key);
        assert!(id.required); // NOT NULL
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Int));

        let title = table.field("title").expect("title field");
        assert!(!title.primary_key);
        assert_eq!(title.kind, DataFieldKind::Plain);
    }

    #[test]
    fn table_from_physical_derives_key_kind_from_foreign_key() {
        let table = Table::from_physical(DbId::primary(), &physical_with_fk());
        let author = table.field("author").expect("author field");
        assert!(!author.required); // nullable
        assert_eq!(
            author.kind,
            DataFieldKind::Key {
                target_table: TableId("person".into()),
                target_field: FieldId("id".into()),
                summary_field: None,
            }
        );
    }

    #[test]
    fn access_rules_default_is_admin_only() {
        let rules = AccessRules::default();
        assert_eq!(rules.min_role_read, 1);
        assert_eq!(rules.min_role_write, 1);
    }

    #[test]
    fn is_system_detects_fd_prefixed_tables() {
        let mut physical = physical_with_fk();
        assert!(!Table::from_physical(DbId::primary(), &physical).is_system());
        physical.name = "_fd_config".into();
        assert!(Table::from_physical(DbId::primary(), &physical).is_system());

        // `_sc_` is Saltcorn v1's metadata namespace, and a transition project
        // runs both servers against one schema. Nothing here claims it: a v1
        // table found in the primary is an ordinary table this server can be
        // pointed at, not one of its own it would try to read a row shape out of.
        physical.name = "_sc_config".into(); // v1's
        assert!(!Table::from_physical(DbId::primary(), &physical).is_system());
    }

    #[test]
    fn apply_overlay_adds_what_the_database_cannot_know_and_nothing_else() {
        let physical = physical_with_fk();
        let plain = Table::from_physical(DbId::primary(), &physical);
        let mut table = plain.clone();

        let meta = TableMeta::new("book")
            .label("Books")
            .description("The library catalogue")
            .access(80, 40);
        table.apply_overlay(&meta);

        // Added: exactly the four things the overlay is the authority on.
        assert_eq!(table.label, "Books");
        assert_eq!(table.description, "The library catalogue");
        assert_eq!(table.access.min_role_read, 80);
        assert_eq!(table.access.min_role_write, 40);
        assert_eq!(table.overlay, Some(meta.id));

        // Untouched: everything the database is the authority on. The merge has
        // no conflict semantics because the two sets do not intersect, and this
        // is what asserts they still don't.
        assert_eq!(table.id, plain.id);
        assert_eq!(table.name, plain.name);
        assert_eq!(table.fields, plain.fields);
        assert_eq!(table.primary_key, plain.primary_key);
        assert_eq!(table.source, plain.source);
        assert_eq!(table.database, plain.database);
    }

    #[test]
    fn an_empty_label_means_none_given_not_a_blank_label() {
        let mut table = Table::from_physical(DbId::primary(), &physical_with_fk());
        table.apply_overlay(&TableMeta::new("book").access(100, 100));
        assert_eq!(table.label, "book", "the table's own name stays the label");
        assert_eq!(table.description, "");
        assert_eq!(table.access.min_role_read, 100);
    }

    #[test]
    fn a_system_table_never_takes_an_overlay() {
        // `save_table_meta` refuses to write such a row, so this only fires on
        // one inserted behind the API's back — a hand-edited database, a
        // restored dump. `_fd_*` tables are hidden from users (§9) and their
        // access is not configurable, so the row is ignored rather than obeyed.
        let mut physical = physical_with_fk();
        physical.name = "_fd_config".into();
        let mut table = Table::from_physical(DbId::primary(), &physical);
        table.apply_overlay(
            &TableMeta::new("_fd_config")
                .label("Config")
                .access(100, 100),
        );

        assert_eq!(table.access, AccessRules::default());
        assert_eq!(table.label, "_fd_config");
        assert_eq!(table.overlay, None);
    }

    #[test]
    fn attrs_and_base_field_are_re_exported_from_sc_types_not_redefined() {
        // Both moved down to `sc-types` (layer 3) so `FormField` — which carries
        // a `BaseField` and describes `Attrs` entries — can live beside them.
        // This crate re-exports both, so the old paths still resolve and every
        // existing call site is untouched. These assignments compile only if the
        // names are the *same* types, not look-alikes.
        let attrs: Attrs = sc_types::Attrs::new();
        let _: sc_types::Attrs = attrs;
        let base: BaseField = sc_types::BaseField::new("x", TypeRef::Basic(BasicType::Text));
        let _: sc_types::BaseField = base;

        // `DataField` stays here — its `Key`/`File` kinds reference catalog ids —
        // and still builds on the very same `BaseField`.
        let mut field = DataField::plain("x", TypeRef::Basic(BasicType::Text));
        field.base.attributes.insert("max".to_owned(), 10.into());
        let _: &sc_types::BaseField = &field.base;
        let _: &sc_types::Attrs = &field.base.attributes;
    }
}
