//! The SDL snapshot: the one test that catches every accidental change to the
//! GraphQL wire contract.
//!
//! Every later phase of this milestone touches the schema builder — filters,
//! ordering, child lists, aggregates, mutations — and each of those is a change
//! somebody meant to make. This asserts that nothing *else* changed with it. A
//! diff here is not a failure to fix by re-running with `--bless`; it is a
//! question to answer: is this the contract we meant to publish?
//!
//! The fixture is deliberately small and deliberately awkward: a parent, a child
//! with **two** keys back to it (so the disambiguating relation names appear), a
//! `File` field, a table whose name GraphQL cannot spell (so its omission is
//! part of the snapshot too), and one column of every scalar family.
//!
//! No database: a schema is a pure function of the tables.

use sc_api::{GraphqlLimits, GraphqlProvider};
use sc_catalog::{
    AccessRules, DataField, DataFieldKind, DbId, FieldId, FileStoreId, Table, TableSource,
};
use sc_types::{Attrs, BasicType, TypeRef};

/// The expected SDL of an application that switched its **aggregates on**,
/// checked in beside this test.
const SNAPSHOT: &str = include_str!("snapshots/graphql_schema.graphql");

/// …and of the same application with the switch off, which is the default. Two
/// files rather than one plus a rule about what to subtract: the switch changes
/// which *types* the schema has, not only which fields, and "the aggregates are
/// gone and nothing else moved" is a claim only a whole-schema diff can make.
const SNAPSHOT_NO_AGGREGATES: &str = include_str!("snapshots/graphql_schema_no_aggregates.graphql");

fn table(name: &str, fields: Vec<DataField>) -> Table {
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

fn column(name: &str, ty: BasicType) -> DataField {
    DataField::plain(name, TypeRef::Basic(ty))
}

fn id() -> DataField {
    column("id", BasicType::Int).required().primary_key()
}

fn key(name: &str, target_table: &str, target_field: &str) -> DataField {
    let mut field = column(name, BasicType::Int);
    field.kind = DataFieldKind::Key {
        target_table: target_table.to_owned(),
        target_field: FieldId(target_field.to_owned()),
        summary_field: None,
    };
    field
}

fn file(name: &str) -> DataField {
    let mut field = column(name, BasicType::Text);
    field.kind = DataFieldKind::File {
        store: FileStoreId("uploads".to_owned()),
        folder: None,
        mime_allow: Vec::new(),
        on_delete: Default::default(),
        on_conflict: Default::default(),
    };
    field
}

/// The fixture the snapshot is of.
fn fixture() -> Vec<Table> {
    vec![
        table(
            "departments",
            vec![
                id(),
                column("name", BasicType::Text).required(),
                column("founded", BasicType::Date),
                file("logo"),
            ],
        ),
        table(
            "employees",
            vec![
                id(),
                column("name", BasicType::Text).required(),
                column("salary", BasicType::Decimal),
                column("hours", BasicType::Float),
                column("active", BasicType::Bool),
                column("profile", BasicType::Json),
                column("token", BasicType::Uuid),
                // Two keys back to the same parent, which is what makes the
                // relation names `_by_`-qualified rather than bare.
                key("department", "departments", "id"),
                key("managed_department", "departments", "id"),
            ],
        ),
        // Not a GraphQL name, so it is absent from the snapshot entirely — the
        // omission is part of the contract, not a silent gap.
        table("2 fast", vec![id()]),
    ]
}

/// The projection under `limits`, over the fixture.
#[allow(clippy::expect_used)]
fn sdl_of(limits: GraphqlLimits) -> String {
    GraphqlProvider::project_with("/graphql", &fixture(), limits)
        .expect("the schema builds")
        .sdl()
}

/// Compare an SDL against its checked-in snapshot, naming the file to replace.
fn assert_snapshot(sdl: &str, snapshot: &str, file: &str) {
    if sdl.trim() != snapshot.trim() {
        // Print the whole thing: a diff of a schema is only readable whole.
        panic!(
            "the GraphQL SDL changed.\n\
             If the change is intended, replace \
             crates/sc-api/tests/snapshots/{file} with:\n\
             ----------------------------------------\n{sdl}\
             ----------------------------------------\n"
        );
    }
}

#[test]
fn the_sdl_is_what_it_was_with_aggregates_on() {
    assert_snapshot(
        &sdl_of(GraphqlLimits::new().aggregates(true)),
        SNAPSHOT,
        "graphql_schema.graphql",
    );
}

#[test]
fn the_sdl_is_what_it_was_with_aggregates_off() {
    // The default, and therefore the schema most applications serve. Everything
    // the other snapshot has apart from `X_aggregate`, the `_aggregate` field on
    // the relations, the `XAggregate`/`XNumericFields`/`XComparableFields`
    // objects and the `XSelectColumn` enum `count(distinct:)` names.
    assert_snapshot(
        &sdl_of(GraphqlLimits::new()),
        SNAPSHOT_NO_AGGREGATES,
        "graphql_schema_no_aggregates.graphql",
    );
}

#[test]
fn the_unnameable_table_is_reported_rather_than_silently_missing() {
    let provider = GraphqlProvider::project("/graphql", &fixture()).expect("the schema builds");
    assert_eq!(provider.diagnostics().len(), 1);
    assert!(
        provider.diagnostics()[0].contains("2 fast"),
        "{:?}",
        provider.diagnostics()
    );
    assert!(!provider.sdl().contains("Fast"), "{}", provider.sdl());
}
