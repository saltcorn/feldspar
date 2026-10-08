//! Every GraphQL name this provider emits, derived in one place.
//!
//! A GraphQL name is `/[_A-Za-z][_0-9A-Za-z]*/` — and Ⱶ and Ↄ are not among
//! those characters, nor is anything else a database will happily accept in a
//! table name. So there is a derivation, and the derivation has exactly two
//! rules:
//!
//! 1. **Nothing is mangled to fit.** A table or field whose name cannot become a
//!    GraphQL name is *omitted*, with a diagnostic saying so. Mangling invents
//!    collisions — two tables that differ only in the characters being stripped
//!    become one type — and a schema that quietly answers for the wrong table is
//!    worse than a schema that admits it is missing one.
//! 2. **A collision is also an omission with a diagnostic.** Type names are
//!    PascalCased (`blog_posts` → `BlogPosts`), which two catalog names can
//!    reach at once; the second one out loses, and says why.
//!
//! The inverse-relation rule is the other derivation, and it is the one a caller
//! notices: a child table appears on its parent as `<child>` when exactly one of
//! its key fields points here, and as `<child>_by_<key>` when more than one does
//! or when `<child>` is already the name of a column. That is the same
//! disambiguation the Ↄ operator spells `childↃkey`, arrived at from the other
//! direction.
//!
//! Everything here is a pure function of the table set: no schema is built, no
//! catalog is consulted. That is what makes the whole of the wire contract's
//! naming testable without a database.

use std::collections::BTreeSet;

use sc_catalog::{DataFieldKind, SchemaProjection, Table};

// --- the scalar type names, which are also names and so live here ------------

/// 64-bit integer. **Not** GraphQL's `Int`, which the specification fixes at 32
/// bits: our integers are `bigint`, and silently truncating one on the wire is
/// the kind of failure nobody notices until the ids get large.
pub const BIG_INT: &str = "BigInt";
/// Exact fixed-point decimal, carried as a string — a JSON number would round it
/// through an IEEE double, which is the entire reason the column is a decimal.
pub const DECIMAL: &str = "Decimal";
/// Raw bytes, carried as base64 text.
pub const BYTES: &str = "Bytes";
/// Arbitrary embedded JSON.
pub const JSON: &str = "JSON";
/// A geometry, as a GeoJSON geometry object (analytics TODO A5.1).
pub const GEO_JSON: &str = "GeoJSON";
/// A UUID in its canonical string form.
pub const UUID: &str = "UUID";
/// An ISO 8601 calendar date.
pub const DATE: &str = "Date";
/// An ISO 8601 wall-clock time.
pub const TIME: &str = "Time";
/// An RFC 3339 instant.
pub const TIMESTAMP: &str = "Timestamp";

/// The object type a `File` field projects as: the stored path plus the URL the
/// **REST** provider already serves the bytes at. A GraphQL field does not
/// become a second file-download path — one place where a file's access rules
/// are enforced is the point of having them.
pub const FILE_VALUE: &str = "FileValue";

/// The query root's type name.
pub const QUERY_ROOT: &str = "Query";
/// The mutation root's type name.
pub const MUTATION_ROOT: &str = "Mutation";
/// The sort-direction enum every `OrderBy` input's fields are typed by.
pub const ORDER_DIRECTION: &str = "OrderDirection";

/// Every scalar a column can be carried as: GraphQL's own three usable ones plus
/// the custom scalars above. (`ID` is not among them — an application's primary
/// key is a real type, and flattening it to an opaque `ID` string loses that.)
pub const SCALAR_NAMES: &[&str] = &[
    "Boolean", "Float", "String", BIG_INT, DECIMAL, BYTES, JSON, GEO_JSON, UUID, DATE, TIME,
    TIMESTAMP,
];

/// The comparison input for a scalar: `String` → `StringComparison`. One per
/// scalar rather than one per column type per table, because `eq`/`lt`/`in` mean
/// the same thing wherever the scalar appears.
pub fn comparison_type_name(scalar: &str) -> String {
    format!("{scalar}Comparison")
}

/// Every type name the provider defines regardless of the schema. A table whose
/// derived type name is one of these is omitted rather than allowed to shadow
/// it.
pub const RESERVED_TYPE_NAMES: &[&str] = &[
    BIG_INT,
    DECIMAL,
    BYTES,
    JSON,
    GEO_JSON,
    UUID,
    DATE,
    TIME,
    TIMESTAMP,
    FILE_VALUE,
    QUERY_ROOT,
    MUTATION_ROOT,
    ORDER_DIRECTION,
];

/// The three names a GraphQL **enum value** may not take. Every exposed column
/// becomes a value of its table's `SelectColumn` enum, so a column called
/// `null` cannot be exposed even though `null` is a perfectly good field name.
const RESERVED_ENUM_VALUES: &[&str] = &["true", "false", "null"];

/// Whether `s` is a legal GraphQL name.
///
/// The specification's production is `/[_A-Za-z][_0-9A-Za-z]*/`, plus the rule
/// that a leading `__` is reserved for introspection — a type called `__Type`
/// would collide with the schema's own description of itself.
pub fn is_graphql_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric()) && !s.starts_with("__")
}

/// A catalog name as a GraphQL **type** name: `blog_posts` → `BlogPosts`.
///
/// Word-splitting on non-alphanumerics is the same rule
/// [`op_name`](crate::op_name) uses for the REST client's method names, so an
/// application's two APIs agree on what the words in a table name are. The
/// result is *checked* by the caller rather than trusted: two catalog names can
/// PascalCase to one type name, and that is a collision to report, not to
/// resolve.
pub fn type_name(catalog_name: &str) -> String {
    let mut out = String::with_capacity(catalog_name.len());
    for word in catalog_name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// One inverse relation as it appears on its parent's object type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationNames {
    /// The child table's catalog name.
    pub child_table: String,
    /// The child's key column — the one referencing this parent.
    pub key_field: String,
    /// The parent column that key references (not always the primary key).
    pub parent_field: String,
    /// The list field: `employees`, or `employees_by_manager` when ambiguous.
    pub list_field: String,
    /// The aggregate field beside it: `employees_aggregate`.
    pub aggregate_field: String,
}

/// Every GraphQL name one table contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableNames {
    /// The catalog table name.
    pub table: String,
    /// The row object type: `Departments`.
    pub object: String,
    /// The aggregate result type: `DepartmentsAggregate`.
    pub aggregate_object: String,
    /// The `sum` result type: `DepartmentsNumericFields`.
    pub numeric_object: String,
    /// The `avg` result type: `DepartmentsAvgFields`.
    ///
    /// Separate from [`numeric_object`](TableNames::numeric_object) because an
    /// average is not of the summed type: the mean of a `BigInt` column is not
    /// an integer, and rounding it to one is the silent loss the whole type
    /// mapping exists to avoid.
    pub avg_object: String,
    /// The `min`/`max` result type: `DepartmentsComparableFields`.
    pub comparable_object: String,
    /// The filter input: `DepartmentsBoolExp`.
    pub bool_exp: String,
    /// The ordering input: `DepartmentsOrderBy`.
    pub order_by: String,
    /// The column enum: `DepartmentsSelectColumn`.
    pub select_column: String,
    /// The insert mutation's argument: `DepartmentsInsertInput`.
    pub insert_input: String,
    /// The update mutation's `set` argument: `DepartmentsSetInput`.
    ///
    /// Separate from [`insert_input`](TableNames::insert_input) because the two
    /// are not the same set of columns: the primary key addresses the row of an
    /// update and is not reassignable through its body, so it is absent here and
    /// present there.
    pub set_input: String,
    /// The update mutation's `pk_columns` argument: `DepartmentsPkColumns`.
    pub pk_columns: String,
    /// The root list field: `departments`.
    pub list_field: String,
    /// The root single-row field: `departments_by_pk`.
    pub by_pk_field: String,
    /// The root aggregate field: `departments_aggregate`.
    pub aggregate_field: String,
    /// The insert mutation: `insert_departments`.
    pub insert_field: String,
    /// The update mutation: `update_departments_by_pk`.
    pub update_by_pk_field: String,
    /// The delete mutation: `delete_departments_by_pk`.
    pub delete_by_pk_field: String,
    /// The columns exposed as row fields, in table order. A column whose name is
    /// not a GraphQL name is absent (and diagnosed).
    pub fields: Vec<String>,
    /// The inverse relations on this table's object type.
    pub relations: Vec<RelationNames>,
}

impl TableNames {
    /// The type names this table defines — what a collision is checked over.
    fn type_names(&self) -> [&str; 11] {
        [
            &self.object,
            &self.aggregate_object,
            &self.numeric_object,
            &self.avg_object,
            &self.comparable_object,
            &self.bool_exp,
            &self.order_by,
            &self.select_column,
            &self.insert_input,
            &self.set_input,
            &self.pk_columns,
        ]
    }

    /// The root field names this table defines, across `Query` and `Mutation`.
    fn root_field_names(&self) -> [&str; 6] {
        [
            &self.list_field,
            &self.by_pk_field,
            &self.aggregate_field,
            &self.insert_field,
            &self.update_by_pk_field,
            &self.delete_by_pk_field,
        ]
    }

    /// Derive the names for one table, before any collision check.
    fn derive(table: &Table) -> Option<TableNames> {
        let name = &table.name;
        let object = type_name(name);
        if !is_graphql_name(name) || !is_graphql_name(&object) {
            return None;
        }
        Some(TableNames {
            table: name.clone(),
            aggregate_object: format!("{object}Aggregate"),
            numeric_object: format!("{object}NumericFields"),
            avg_object: format!("{object}AvgFields"),
            comparable_object: format!("{object}ComparableFields"),
            bool_exp: format!("{object}BoolExp"),
            order_by: format!("{object}OrderBy"),
            select_column: format!("{object}SelectColumn"),
            insert_input: format!("{object}InsertInput"),
            set_input: format!("{object}SetInput"),
            pk_columns: format!("{object}PkColumns"),
            object,
            list_field: name.clone(),
            by_pk_field: format!("{name}_by_pk"),
            aggregate_field: format!("{name}_aggregate"),
            insert_field: format!("insert_{name}"),
            update_by_pk_field: format!("update_{name}_by_pk"),
            delete_by_pk_field: format!("delete_{name}_by_pk"),
            fields: Vec::new(),
            relations: Vec::new(),
        })
    }
}

/// The derived names for a whole application's table set, with the diagnostics
/// for everything that had to be left out.
#[derive(Debug, Clone, Default)]
pub struct SchemaNames {
    tables: Vec<TableNames>,
    diagnostics: Vec<String>,
}

impl SchemaNames {
    /// Derive every name for `tables` — the application's declared subset,
    /// already resolved against the catalog.
    ///
    /// Never fails: a table that cannot be named is dropped from the result and
    /// named in [`diagnostics`](SchemaNames::diagnostics), because one
    /// unnameable table must not cost an application its whole API.
    pub fn derive(tables: &[Table]) -> SchemaNames {
        let mut out = SchemaNames::default();
        let mut used_types: BTreeSet<String> = RESERVED_TYPE_NAMES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut used_root_fields: BTreeSet<String> = BTreeSet::new();

        for table in tables {
            let Some(names) = TableNames::derive(table) else {
                out.diagnostics.push(format!(
                    "table `{}` is not exposed over GraphQL: neither it nor the type name \
                     derived from it (`{}`) is a valid GraphQL name",
                    table.name,
                    type_name(&table.name)
                ));
                continue;
            };
            if let Some(clash) = names.type_names().iter().find(|n| used_types.contains(**n)) {
                out.diagnostics.push(format!(
                    "table `{}` is not exposed over GraphQL: it would define the type \
                     `{clash}`, which is already taken",
                    table.name
                ));
                continue;
            }
            if let Some(clash) = names
                .root_field_names()
                .iter()
                .find(|n| used_root_fields.contains(**n))
            {
                out.diagnostics.push(format!(
                    "table `{}` is not exposed over GraphQL: it would define the root field \
                     `{clash}`, which is already taken",
                    table.name
                ));
                continue;
            }
            used_types.extend(names.type_names().iter().map(|n| (*n).to_owned()));
            used_root_fields.extend(names.root_field_names().iter().map(|n| (*n).to_owned()));
            out.tables.push(names);
        }

        // A table none of whose columns can be named is not a table this schema
        // can describe: its object type would have no fields, which GraphQL does
        // not allow, and neither would its `OrderBy` input or `SelectColumn`
        // enum. Drop it here rather than let the schema fail to build.
        for names in &mut out.tables {
            let Some(table) = tables.iter().find(|t| t.name == names.table) else {
                continue;
            };
            derive_fields(table, names, &mut out.diagnostics);
        }
        let SchemaNames {
            tables: kept,
            diagnostics,
        } = &mut out;
        kept.retain(|names| {
            if !names.fields.is_empty() {
                return true;
            }
            diagnostics.push(format!(
                "table `{}` is not exposed over GraphQL: none of its columns has a name \
                 GraphQL can use, so it has no fields to describe",
                names.table
            ));
            false
        });

        // Relations are resolved only over the tables that survived, so a
        // relation never points at a type the schema does not define.
        let exposed: Vec<Table> = tables
            .iter()
            .filter(|t| out.tables.iter().any(|n| n.table == t.name))
            .cloned()
            .collect();
        let projection = SchemaProjection::new(exposed.clone());
        for names in &mut out.tables {
            let Some(table) = exposed.iter().find(|t| t.name == names.table) else {
                continue;
            };
            derive_relations(table, names, &projection, &exposed, &mut out.diagnostics);
        }
        out
    }

    /// The tables that are exposed, in the order they were given.
    pub fn tables(&self) -> &[TableNames] {
        &self.tables
    }

    /// The names for one catalog table, if it is exposed.
    pub fn get(&self, table: &str) -> Option<&TableNames> {
        self.tables.iter().find(|t| t.table == table)
    }

    /// Everything that was left out, and why. Reported at mount rather than
    /// swallowed: a missing table is the kind of absence nobody investigates
    /// unless something says it happened.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// The table a generated type name belongs to, if any — how a schema-build
    /// failure that names a type is reported against the table that caused it.
    pub fn table_of_type(&self, type_name: &str) -> Option<&str> {
        self.tables
            .iter()
            .find(|t| t.type_names().contains(&type_name))
            .map(|t| t.table.as_str())
    }
}

/// The columns of `table` that can be row fields, in table order.
fn derive_fields(table: &Table, names: &mut TableNames, diagnostics: &mut Vec<String>) {
    for field in &table.fields {
        let name = &field.base.name;
        // The password hash is no API's to read or write (see `user_rows`): not
        // a field of the type, so not a filter, an ordering or an input either.
        if crate::user_rows::is_hidden_column(table, name) {
            continue;
        }
        if !is_graphql_name(name) {
            diagnostics.push(format!(
                "field `{}`.`{name}` is not exposed over GraphQL: its name is not a valid \
                 GraphQL name",
                table.name
            ));
            continue;
        }
        if RESERVED_ENUM_VALUES.contains(&name.as_str()) {
            diagnostics.push(format!(
                "field `{}`.`{name}` is not exposed over GraphQL: `{name}` cannot be a value \
                 of the `{}` enum, which every column has to be",
                table.name, names.select_column
            ));
            continue;
        }
        names.fields.push(name.clone());
    }
}

/// The inverse relations on `table`, named by the rule in the module docs.
///
/// A table's *self*-references are not relations here, because
/// [`SchemaProjection::referencing_fields`] deliberately excludes them — it
/// answers "who would a drop of this table break", and a self-reference breaks
/// nothing. A tree table therefore exposes its parent key (an outgoing Ⱶ-join)
/// but not its children; expressing that needs a different question of the
/// projection than the one being asked here.
fn derive_relations(
    table: &Table,
    names: &mut TableNames,
    projection: &SchemaProjection,
    exposed: &[Table],
    diagnostics: &mut Vec<String>,
) {
    let referencing = projection.referencing_fields(&table.name);
    for (child_name, key_field) in &referencing {
        let Some(child) = exposed.iter().find(|t| t.name == *child_name) else {
            continue;
        };
        let Some(parent_field) = child.field(key_field).and_then(|f| match &f.kind {
            DataFieldKind::Key { target_field, .. } => Some(target_field.0.clone()),
            _ => None,
        }) else {
            continue;
        };
        // Ambiguous when this child points here through more than one key, and
        // when the unqualified name is already a column of the parent.
        let ambiguous = referencing.iter().filter(|(t, _)| t == child_name).count() > 1
            || names.fields.iter().any(|f| f == child_name);
        let list_field = if ambiguous {
            format!("{child_name}_by_{key_field}")
        } else {
            child_name.clone()
        };
        let aggregate_field = format!("{list_field}_aggregate");
        let taken = |n: &str| {
            names.fields.iter().any(|f| f == n)
                || names
                    .relations
                    .iter()
                    .any(|r| r.list_field == n || r.aggregate_field == n)
        };
        if !is_graphql_name(&list_field) || taken(&list_field) || taken(&aggregate_field) {
            diagnostics.push(format!(
                "relation `{child_name}`.`{key_field}` → `{}` is not exposed over GraphQL: \
                 the field name `{list_field}` is not usable on type `{}`",
                table.name, names.object
            ));
            continue;
        }
        names.relations.push(RelationNames {
            child_table: child_name.clone(),
            key_field: key_field.clone(),
            parent_field,
            list_field,
            aggregate_field,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{key_field, plain_field, table_of};

    #[test]
    fn graphql_names_are_the_specifications_and_no_more() {
        for good in ["a", "_a", "A1", "blog_posts", "_"] {
            assert!(is_graphql_name(good), "{good} should be a GraphQL name");
        }
        for bad in [
            "",
            "1a",
            "a-b",
            "a b",
            "reviewsↃbook",
            "publisherⱵname",
            "__type",
        ] {
            assert!(!is_graphql_name(bad), "{bad} should not be a GraphQL name");
        }
    }

    #[test]
    fn type_names_are_pascal_case_of_the_catalog_name() {
        assert_eq!(type_name("departments"), "Departments");
        assert_eq!(type_name("blog_posts"), "BlogPosts");
        assert_eq!(type_name("time entries"), "TimeEntries");
        // A name with nothing to capitalise yields nothing, which the caller
        // then rejects as not a GraphQL name rather than emitting `""`.
        assert_eq!(type_name("__"), "");
        assert!(!is_graphql_name(&type_name("__")));
    }

    #[test]
    fn a_table_contributes_the_whole_family_of_names() {
        let names = SchemaNames::derive(&[table_of(
            "departments",
            vec![plain_field("id"), plain_field("name")],
        )]);
        let d = names.get("departments").expect("exposed");
        assert_eq!(d.object, "Departments");
        assert_eq!(d.aggregate_object, "DepartmentsAggregate");
        assert_eq!(d.bool_exp, "DepartmentsBoolExp");
        assert_eq!(d.order_by, "DepartmentsOrderBy");
        assert_eq!(d.select_column, "DepartmentsSelectColumn");
        assert_eq!(d.insert_input, "DepartmentsInsertInput");
        assert_eq!(d.set_input, "DepartmentsSetInput");
        assert_eq!(d.pk_columns, "DepartmentsPkColumns");
        assert_eq!(d.list_field, "departments");
        assert_eq!(d.by_pk_field, "departments_by_pk");
        assert_eq!(d.aggregate_field, "departments_aggregate");
        assert_eq!(d.insert_field, "insert_departments");
        assert_eq!(d.update_by_pk_field, "update_departments_by_pk");
        assert_eq!(d.delete_by_pk_field, "delete_departments_by_pk");
        assert_eq!(d.fields, vec!["id", "name"]);
        assert!(names.diagnostics().is_empty(), "{:?}", names.diagnostics());
    }

    #[test]
    fn an_unnameable_table_is_omitted_with_a_diagnostic() {
        let names = SchemaNames::derive(&[
            table_of("ok", vec![plain_field("id")]),
            table_of("2fast", vec![plain_field("id")]),
        ]);
        assert_eq!(names.tables().len(), 1);
        assert!(names.get("2fast").is_none());
        assert_eq!(names.diagnostics().len(), 1);
        assert!(
            names.diagnostics()[0].contains("`2fast`"),
            "the diagnostic must name the table: {:?}",
            names.diagnostics()
        );
    }

    #[test]
    fn an_unnameable_field_is_omitted_and_its_table_is_not() {
        // The table survives; only the column it cannot name is missing. A
        // Ⱶ-joinfield's identifier is the realistic case.
        let names = SchemaNames::derive(&[table_of(
            "books",
            vec![plain_field("id"), plain_field("publisherⱵname")],
        )]);
        let books = names.get("books").expect("exposed");
        assert_eq!(books.fields, vec!["id"]);
        assert_eq!(names.diagnostics().len(), 1);
        assert!(names.diagnostics()[0].contains("publisher"));
    }

    #[test]
    fn two_tables_that_pascal_case_alike_do_not_both_win() {
        // Both `departments` and `_departments` reach the type `Departments`;
        // the second one out is omitted and says why, rather than silently
        // overwriting the first — which is the failure mangling always causes.
        let names = SchemaNames::derive(&[
            table_of("departments", vec![plain_field("id")]),
            table_of("_departments", vec![plain_field("id")]),
        ]);
        assert_eq!(names.tables().len(), 1);
        assert_eq!(names.tables()[0].table, "departments");
        assert!(
            names.diagnostics()[0].contains("Departments")
                && names.diagnostics()[0].contains("_departments"),
            "{:?}",
            names.diagnostics()
        );
    }

    #[test]
    fn a_table_may_not_shadow_a_built_in_type() {
        let names = SchemaNames::derive(&[table_of("query", vec![plain_field("id")])]);
        assert!(names.tables().is_empty());
        assert!(names.diagnostics()[0].contains("Query"));
    }

    #[test]
    fn one_key_back_to_the_parent_names_the_relation_after_the_child() {
        let names = SchemaNames::derive(&[
            table_of("departments", vec![plain_field("id"), plain_field("name")]),
            table_of(
                "employees",
                vec![
                    plain_field("id"),
                    key_field("department", "departments", "id"),
                ],
            ),
        ]);
        let d = names.get("departments").expect("exposed");
        assert_eq!(d.relations.len(), 1);
        assert_eq!(d.relations[0].list_field, "employees");
        assert_eq!(d.relations[0].aggregate_field, "employees_aggregate");
        assert_eq!(d.relations[0].key_field, "department");
        assert_eq!(d.relations[0].parent_field, "id");
        // The child has no inverse relations of its own.
        assert!(
            names
                .get("employees")
                .expect("exposed")
                .relations
                .is_empty()
        );
    }

    #[test]
    fn two_keys_back_to_the_parent_qualify_both_relations_by_key() {
        let names = SchemaNames::derive(&[
            table_of("departments", vec![plain_field("id")]),
            table_of(
                "employees",
                vec![
                    plain_field("id"),
                    key_field("department", "departments", "id"),
                    key_field("managed_department", "departments", "id"),
                ],
            ),
        ]);
        let d = names.get("departments").expect("exposed");
        let fields: Vec<&str> = d.relations.iter().map(|r| r.list_field.as_str()).collect();
        assert_eq!(
            fields,
            vec!["employees_by_department", "employees_by_managed_department"]
        );
    }

    #[test]
    fn a_relation_that_would_shadow_a_column_is_qualified_by_key() {
        // The parent has a column literally called `employees`; the relation
        // cannot take that name, so it takes the qualified one instead of
        // overwriting a column the caller asked for.
        let names = SchemaNames::derive(&[
            table_of(
                "departments",
                vec![plain_field("id"), plain_field("employees")],
            ),
            table_of(
                "employees",
                vec![
                    plain_field("id"),
                    key_field("department", "departments", "id"),
                ],
            ),
        ]);
        let d = names.get("departments").expect("exposed");
        assert_eq!(d.relations[0].list_field, "employees_by_department");
    }

    #[test]
    fn a_relation_to_a_table_that_is_not_exposed_is_not_invented() {
        // `2fast` is omitted, so the relation it would have contributed to
        // `departments` is not there either — a list field whose element type
        // does not exist is a schema that will not build.
        let names = SchemaNames::derive(&[
            table_of("departments", vec![plain_field("id")]),
            table_of(
                "2fast",
                vec![
                    plain_field("id"),
                    key_field("department", "departments", "id"),
                ],
            ),
        ]);
        assert!(
            names
                .get("departments")
                .expect("exposed")
                .relations
                .is_empty()
        );
    }

    #[test]
    fn a_generated_type_name_maps_back_to_its_table() {
        let names = SchemaNames::derive(&[table_of("departments", vec![plain_field("id")])]);
        assert_eq!(
            names.table_of_type("DepartmentsBoolExp"),
            Some("departments")
        );
        assert_eq!(names.table_of_type("Nothing"), None);
    }
}
