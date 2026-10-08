//! Building the application's `async_graphql::dynamic::Schema` from its tables.
//!
//! The schema is **data**: an admin or an agent creates a table at runtime and
//! the API has it at the next mount, which is the whole reason the dynamic API
//! was chosen over every macro-driven library. So this module is a fold over
//! [`Table`] values producing types, and nothing here is known at compile time.
//!
//! The shape is Hasura-flavoured, because that is the quality bar the design
//! sets and the shape callers already know — with the deviations recorded in
//! docs/GRAPHQL_API.md §4, plus one more that arrived with the type mapping:
//! **`avg` has its own result type.** Hasura shares one `NumericFields` between
//! `sum` and `avg`; the mean of a `BigInt` column is not a `BigInt`, and a
//! schema that says it is has promised to round somebody's number.
//!
//! What each field *does* is [`resolve`](super::resolve)'s business; this module
//! decides only which resolver a field gets — and the aggregate result objects
//! are the one place where that mapping is not obvious: `sum` and `avg` are
//! *groups*, whose columns are the values, while `count` is a value itself, so
//! they take different resolvers over the same flat set of computed columns.

use async_graphql::dynamic::{
    Enum, Field, InputObject, InputValue, Object, Scalar, Schema, TypeRef,
};
use sc_catalog::{DataField, DataFieldKind, Table};
use sc_error::{Error, Result};

use super::args::{ARG_DISTINCT, ARG_LIMIT, ARG_OFFSET, ARG_ORDER_BY, ARG_WHERE};
use super::limits::GraphqlLimits;
use super::mutate::{self, ARG_OBJECT, ARG_PK_COLUMNS, ARG_SET};
use super::names::{
    self, FILE_VALUE, MUTATION_ROOT, ORDER_DIRECTION, QUERY_ROOT, SCALAR_NAMES, SchemaNames,
    TableNames, comparison_type_name,
};
use super::resolve;
use super::types::{CUSTOM_SCALARS, column_scalar, field_type, scalar_name};
use crate::schema::ValueType;

/// Build the application's schema from the tables its names were derived from.
///
/// Failing here is a **mount failure**, never a half-served schema: an
/// application that cannot describe its own API must not come up answering some
/// of it. Where `async-graphql`'s own error names a type, the message names the
/// table that type came from — the thing an admin can actually act on.
pub fn build_schema(
    tables: &[Table],
    names: &SchemaNames,
    limits: GraphqlLimits,
) -> Result<Schema> {
    if names.tables().is_empty() {
        return Err(Error::config(
            "this application exposes no tables that can be projected into GraphQL, so its \
             schema would have no fields; declare a table, or disable the `graphql` provider"
                .to_owned(),
        ));
    }

    // The mutation half is decided first: `Schema::build` has to be told whether
    // there is a `Mutation` root at all, and a root with no fields on it will
    // not build — which is what an application of nothing but calculated
    // columns would produce.
    let (mutation, mutation_inputs) = mutation_types(tables, names);

    let mut builder = Schema::build(QUERY_ROOT, mutation.as_ref().map(|_| MUTATION_ROOT), None)
        // Both are *validation* rules: they run over the parsed document before
        // a single resolver does, so a query that is too deep or too wide is
        // refused without a statement being issued. That is the whole property
        // — a cheap refusal, not an expensive one (docs/GRAPHQL_API.md §6).
        .limit_depth(limits.max_depth)
        .limit_complexity(limits.max_complexity);
    // Introspection is deliberately *not* disabled: see `limits`' module doc.
    for input in mutation_inputs {
        builder = builder.register(input);
    }

    // The scalars GraphQL does not have, and the two enums/objects that are the
    // same for every table.
    for scalar in CUSTOM_SCALARS {
        builder = builder.register(Scalar::new(*scalar));
    }
    builder = builder.register(order_direction_enum());
    builder = builder.register(file_value_object());
    for scalar in SCALAR_NAMES {
        builder = builder.register(comparison_input(scalar));
    }

    let mut query = Object::new(QUERY_ROOT);
    for t in names.tables() {
        // Every derived table name came from a table in the set.
        let Some(table) = tables.iter().find(|x| x.name == t.table) else {
            continue;
        };
        builder = builder.register(row_object(table, t, names, limits.aggregates));
        builder = builder.register(bool_exp_input(table, t));
        builder = builder.register(order_by_input(table, t));
        // The aggregate half of the schema is registered only when the
        // application asked for it (`GraphqlLimits::aggregates`). Absent means
        // *absent*: `X_aggregate`, its result object and the two field objects
        // under it are not types the schema has, so a document naming one is
        // `async-graphql`'s own "field not found" before a resolver runs —
        // rather than a silent null or a refusal that costs a round trip.
        if limits.aggregates {
            builder = builder.register(select_column_enum(t));
            builder = builder.register(aggregate_object(table, t));
            if let Some(obj) = numeric_fields_object(table, t, &t.numeric_object, NumericAs::Column)
            {
                builder = builder.register(obj);
            }
            if let Some(obj) = numeric_fields_object(table, t, &t.avg_object, NumericAs::Average) {
                builder = builder.register(obj);
            }
            if let Some(obj) = comparable_fields_object(table, t) {
                builder = builder.register(obj);
            }
        }
        query = add_root_fields(query, table, t, limits.aggregates);
    }

    builder = builder.register(query);
    if let Some(mutation) = mutation {
        builder = builder.register(mutation);
    }
    builder.finish().map_err(|e| schema_error(e, names))
}

/// An `async-graphql` schema error, attributed to the table it came from.
fn schema_error(err: async_graphql::dynamic::SchemaError, names: &SchemaNames) -> Error {
    let message = err.to_string();
    // The library's messages name the offending *type*; the admin knows tables.
    let culprit = message
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter_map(|word| names.table_of_type(word))
        .next();
    match culprit {
        Some(table) => Error::config(format!(
            "the GraphQL schema for table `{table}` could not be built: {message}"
        )),
        None => Error::config(format!("the GraphQL schema could not be built: {message}")),
    }
}

/// `enum OrderDirection { asc desc }`.
fn order_direction_enum() -> Enum {
    Enum::new(ORDER_DIRECTION).item("asc").item("desc")
}

/// The object a `File` field projects as: the stored path and the URL the REST
/// provider serves the bytes at.
fn file_value_object() -> Object {
    Object::new(FILE_VALUE)
        .description("A file reference: its path within its store, and the URL its bytes are served at by this application's REST API.")
        .field(Field::new(
            "path",
            TypeRef::named_nn(TypeRef::STRING),
            resolve::file_part("path"),
        ))
        .field(Field::new(
            "url",
            TypeRef::named_nn(TypeRef::STRING),
            resolve::file_part("url"),
        ))
}

/// `input StringComparison { eq ne gt gte lt lte in nin is_null like ilike }`.
///
/// One per scalar rather than one per column: `lt` means the same thing wherever
/// the scalar appears, and a type per column would be a schema nobody can read.
fn comparison_input(scalar: &str) -> InputObject {
    let mut input = InputObject::new(comparison_type_name(scalar))
        .field(InputValue::new("eq", TypeRef::named(scalar)))
        .field(InputValue::new("ne", TypeRef::named(scalar)));
    // Ordered comparisons need an ordering; embedded JSON and geometry have none.
    if scalar != names::JSON && scalar != names::GEO_JSON {
        for op in ["gt", "gte", "lt", "lte"] {
            input = input.field(InputValue::new(op, TypeRef::named(scalar)));
        }
    }
    input = input
        .field(InputValue::new("in", TypeRef::named_nn_list(scalar)))
        .field(InputValue::new("nin", TypeRef::named_nn_list(scalar)))
        .field(InputValue::new("is_null", TypeRef::named(TypeRef::BOOLEAN)));
    if scalar == TypeRef::STRING {
        input = input
            .field(InputValue::new("like", TypeRef::named(scalar)))
            .field(InputValue::new("ilike", TypeRef::named(scalar)));
    }
    input
}

/// The row object: the table's columns, then its inverse relations.
///
/// `aggregates` decides whether each relation also carries its `_aggregate`
/// field — the same switch the root fields are under, because a child aggregate
/// is the *more* expensive of the two (one correlated subquery per parent row).
fn row_object(table: &Table, t: &TableNames, names: &SchemaNames, aggregates: bool) -> Object {
    let mut object = Object::new(&t.object);
    if !table.description.is_empty() {
        object = object.description(&table.description);
    }
    for name in &t.fields {
        let Some(field) = table.field(name) else {
            continue;
        };
        object = object.field(Field::new(
            name,
            field_type(field, names),
            row_field_resolver(field, names),
        ));
    }
    for rel in &t.relations {
        let Some(child) = names.get(&rel.child_table) else {
            continue;
        };
        object = object.field(
            list_arguments(
                Field::new(
                    &rel.list_field,
                    // Nullable, where the root list is not: the child's own
                    // access rules decide this field, and a caller refused by
                    // them must lose *this field*, not the parent row that a
                    // non-null list would propagate the error up to. Partial
                    // results are the point of the deviation.
                    TypeRef::named_nn_list(&child.object),
                    resolve::child_list_field(&rel.child_table, &rel.key_field, &rel.parent_field),
                ),
                child,
            )
            .description(format!(
                "Rows of `{}` whose `{}` references this row.",
                rel.child_table, rel.key_field
            )),
        );
        if aggregates {
            object = object.field(
                Field::new(
                    &rel.aggregate_field,
                    TypeRef::named_nn(&child.aggregate_object),
                    resolve::child_aggregate_field(&rel.child_table),
                )
                .argument(InputValue::new(ARG_WHERE, TypeRef::named(&child.bool_exp)))
                .description(format!(
                    "Aggregates over the rows of `{}` that reference this row — computed by the \
                     database as a correlated subquery, not by fetching them.",
                    rel.child_table
                )),
            );
        }
    }
    object
}

/// Which resolver one row field gets — decided by the same match
/// [`field_type`] uses, so the type a field promises and the value it produces
/// cannot disagree.
fn row_field_resolver(field: &DataField, names: &SchemaNames) -> resolve::Resolver {
    let name = &field.base.name;
    match &field.kind {
        // A key whose target this application exposes resolves *through* the
        // parent query's projected Ⱶ-join; one whose target it does not carries
        // the column's own value, exactly as its type says.
        DataFieldKind::Key { target_table, .. } => match names.get(&target_table.0) {
            Some(_) => resolve::key_field(name, &target_table.0),
            None => resolve::column_field(name),
        },
        DataFieldKind::File { .. } => resolve::file_field(name),
        DataFieldKind::Plain | DataFieldKind::Calc { .. } => resolve::column_field(name),
    }
}

/// `where` / `order_by` / `limit` / `offset` on a collection field.
fn list_arguments(field: Field, t: &TableNames) -> Field {
    field
        .argument(InputValue::new(ARG_WHERE, TypeRef::named(&t.bool_exp)))
        .argument(InputValue::new(
            ARG_ORDER_BY,
            TypeRef::named_nn_list(&t.order_by),
        ))
        .argument(InputValue::new(ARG_LIMIT, TypeRef::named(TypeRef::INT)))
        .argument(InputValue::new(ARG_OFFSET, TypeRef::named(TypeRef::INT)))
}

/// `input XBoolExp` — the per-column comparisons plus `_and`/`_or`/`_not`.
fn bool_exp_input(table: &Table, t: &TableNames) -> InputObject {
    let mut input = InputObject::new(&t.bool_exp)
        .field(InputValue::new("_and", TypeRef::named_nn_list(&t.bool_exp)))
        .field(InputValue::new("_or", TypeRef::named_nn_list(&t.bool_exp)))
        .field(InputValue::new("_not", TypeRef::named(&t.bool_exp)));
    for name in &t.fields {
        let Some(field) = table.field(name) else {
            continue;
        };
        // A filter is over the *column*, including for a `Key` field, whose
        // value is the foreign key it holds. Filtering through the relation is
        // a nested question and gets a nested answer later.
        input = input.field(InputValue::new(
            name,
            TypeRef::named(comparison_type_name(column_scalar(field))),
        ));
    }
    input
}

/// `input XOrderBy` — one nullable `OrderDirection` per column.
fn order_by_input(table: &Table, t: &TableNames) -> InputObject {
    let mut input = InputObject::new(&t.order_by);
    for name in &t.fields {
        if table.field(name).is_some() {
            input = input.field(InputValue::new(name, TypeRef::named(ORDER_DIRECTION)));
        }
    }
    input
}

/// `enum XSelectColumn` — the column `count(distinct:)` names.
fn select_column_enum(t: &TableNames) -> Enum {
    let mut e = Enum::new(&t.select_column);
    for name in &t.fields {
        e = e.item(name);
    }
    e
}

/// `type XAggregate { count sum avg min max }`.
///
/// `sum`/`avg` appear only when the table has a numeric column and `min`/`max`
/// only when it has a comparable one: GraphQL has no empty object type, and a
/// `sum` over nothing is not a thing to ask for.
fn aggregate_object(table: &Table, t: &TableNames) -> Object {
    let mut object = Object::new(&t.aggregate_object).field(
        Field::new(
            "count",
            TypeRef::named_nn(TypeRef::INT),
            resolve::agg_value_field(TypeRef::INT),
        )
        .argument(InputValue::new(
            ARG_DISTINCT,
            TypeRef::named(&t.select_column),
        )),
    );
    if aggregated_columns(table, t).any(is_numeric) {
        for (name, ty) in [("sum", &t.numeric_object), ("avg", &t.avg_object)] {
            object = object.field(Field::new(
                name,
                TypeRef::named_nn(ty),
                resolve::agg_group_field(),
            ));
        }
    }
    if aggregated_columns(table, t).any(is_comparable) {
        for name in ["min", "max"] {
            object = object.field(Field::new(
                name,
                TypeRef::named_nn(&t.comparable_object),
                resolve::agg_group_field(),
            ));
        }
    }
    object
}

/// How a numeric column is typed in an aggregate's result object.
#[derive(Clone, Copy)]
enum NumericAs {
    /// `sum` — the column's own scalar (a sum of integers is an integer).
    Column,
    /// `avg` — exact rather than the column's type: the mean of integers is not
    /// an integer, and `Float` would round a decimal.
    Average,
}

/// `type XNumericFields` / `type XAvgFields`, or `None` when there is nothing
/// numeric to put in one.
fn numeric_fields_object(
    table: &Table,
    t: &TableNames,
    type_name: &str,
    as_: NumericAs,
) -> Option<Object> {
    let mut object = Object::new(type_name);
    let mut any = false;
    for field in aggregated_columns(table, t).filter(|f| is_numeric(f)) {
        let name = &field.base.name;
        let scalar = match as_ {
            NumericAs::Column => column_scalar(field),
            // Postgres averages an integer or a decimal as `numeric` and a
            // float as `double precision`; the wire types follow.
            NumericAs::Average if column_scalar(field) == scalar_name(ValueType::Float) => {
                scalar_name(ValueType::Float)
            }
            NumericAs::Average => scalar_name(ValueType::Decimal),
        };
        object = object.field(Field::new(
            name,
            TypeRef::named(scalar),
            resolve::agg_value_field(scalar),
        ));
        any = true;
    }
    any.then_some(object)
}

/// `type XComparableFields`, or `None` when nothing in the table is comparable.
fn comparable_fields_object(table: &Table, t: &TableNames) -> Option<Object> {
    let mut object = Object::new(&t.comparable_object);
    let mut any = false;
    for field in aggregated_columns(table, t).filter(|f| is_comparable(f)) {
        let name = &field.base.name;
        object = object.field(Field::new(
            name,
            TypeRef::named(column_scalar(field)),
            resolve::agg_value_field(column_scalar(field)),
        ));
        any = true;
    }
    any.then_some(object)
}

/// The columns an aggregate may be taken over: the exposed **stored** ones.
///
/// A `File` field is excluded — the minimum of a set of paths is not a question
/// anybody is asking — and so is a calculated field, which has no column for the
/// database to aggregate. A `Key` is included: it holds a real value.
fn aggregated_columns<'a>(
    table: &'a Table,
    t: &'a TableNames,
) -> impl Iterator<Item = &'a DataField> {
    t.fields
        .iter()
        .filter_map(move |name| table.field(name))
        .filter(|f| matches!(f.kind, DataFieldKind::Plain | DataFieldKind::Key { .. }))
}

/// Whether `sum`/`avg` are meaningful over this column.
fn is_numeric(field: &DataField) -> bool {
    matches!(
        column_scalar(field),
        names::BIG_INT | names::DECIMAL | "Float"
    )
}

/// Whether `min`/`max` are meaningful over this column — everything with an
/// ordering, which is everything but embedded JSON.
fn is_comparable(field: &DataField) -> bool {
    !matches!(column_scalar(field), names::JSON | names::GEO_JSON)
}

/// The root fields for one table: the list, the single row, and — when the
/// application switched them on — the aggregate.
fn add_root_fields(query: Object, table: &Table, t: &TableNames, aggregates: bool) -> Object {
    let query = query.field(
        list_arguments(
            Field::new(
                &t.list_field,
                TypeRef::named_nn_list_nn(&t.object),
                resolve::list_field(&t.table),
            ),
            t,
        )
        .description(format!("Rows of `{}`.", t.table)),
    );
    // `_by_pk` needs one column to address a row by — the same rule the row
    // layer enforces, and the same one the REST projection applies before it
    // emits `PUT`/`DELETE`.
    let by_pk = crate::rows::single_pk(table)
        .ok()
        .filter(|pk| t.fields.contains(pk))
        .and_then(|pk| table.field(&pk).map(|f| (pk.clone(), column_scalar(f))));
    let query = match by_pk {
        Some((pk, scalar)) => query.field(
            Field::new(
                &t.by_pk_field,
                TypeRef::named(&t.object),
                resolve::by_pk_field(&t.table, &pk),
            )
            .argument(InputValue::new(&pk, TypeRef::named_nn(scalar)))
            .description(format!("The row of `{}` with this primary key.", t.table)),
        ),
        None => query,
    };
    if !aggregates {
        return query;
    }
    query.field(
        Field::new(
            &t.aggregate_field,
            TypeRef::named_nn(&t.aggregate_object),
            resolve::aggregate_field(&t.table),
        )
        .argument(InputValue::new(ARG_WHERE, TypeRef::named(&t.bool_exp)))
        .description(format!("Aggregates over the rows of `{}`.", t.table)),
    )
}

/// The `Mutation` root and the input types its arguments are typed by, or
/// `None` when this application has nothing to write.
///
/// Every exposed table contributes its three mutations — the schema describes
/// what the *application* exposes, not what one caller may do with it
/// (decision 4), so a table this caller's role cannot write is here and refuses
/// at resolve time. What a table does *not* contribute is a mutation the row
/// layer could never carry out: an insert with no writable column to name, or an
/// update or delete on a table with no single primary key to address a row by —
/// the same rule that decides whether `X_by_pk` exists.
fn mutation_types(tables: &[Table], names: &SchemaNames) -> (Option<Object>, Vec<InputObject>) {
    let mut mutation = Object::new(MUTATION_ROOT);
    let mut inputs = Vec::new();
    let mut any = false;

    for t in names.tables() {
        let Some(table) = tables.iter().find(|x| x.name == t.table) else {
            continue;
        };
        if let Some(input) = write_input(table, t, &t.insert_input, WriteInput::Insert) {
            inputs.push(input);
            mutation = mutation.field(
                Field::new(
                    &t.insert_field,
                    TypeRef::named(&t.object),
                    mutate::insert_field(&t.table),
                )
                .argument(InputValue::new(
                    ARG_OBJECT,
                    TypeRef::named_nn(&t.insert_input),
                ))
                .description(format!("Insert one row of `{}`.", t.table)),
            );
            any = true;
        }
        // A row is addressed the way `_by_pk` addresses it, or not at all.
        let pk = crate::rows::single_pk(table)
            .ok()
            .filter(|pk| t.fields.contains(pk))
            .and_then(|pk| table.field(&pk).map(|f| (pk.clone(), column_scalar(f))));
        let Some((pk, scalar)) = pk else {
            continue;
        };
        if let Some(input) = write_input(table, t, &t.set_input, WriteInput::Set { pk: &pk }) {
            inputs.push(input);
            inputs.push(
                InputObject::new(&t.pk_columns)
                    .field(InputValue::new(&pk, TypeRef::named_nn(scalar))),
            );
            mutation = mutation.field(
                Field::new(
                    &t.update_by_pk_field,
                    TypeRef::named(&t.object),
                    mutate::update_by_pk_field(&t.table, &pk),
                )
                .argument(InputValue::new(
                    ARG_PK_COLUMNS,
                    TypeRef::named_nn(&t.pk_columns),
                ))
                .argument(InputValue::new(ARG_SET, TypeRef::named_nn(&t.set_input)))
                .description(format!(
                    "Update the row of `{}` with this primary key.",
                    t.table
                )),
            );
        }
        mutation = mutation.field(
            Field::new(
                &t.delete_by_pk_field,
                TypeRef::named(&t.object),
                mutate::delete_by_pk_field(&t.table, &pk),
            )
            .argument(InputValue::new(&pk, TypeRef::named_nn(scalar)))
            .description(format!(
                "Delete the row of `{}` with this primary key, returning the columns it had.",
                t.table
            )),
        );
        any = true;
    }
    (any.then_some(mutation), inputs)
}

/// Which write an input object is for — the two differ by exactly one column.
enum WriteInput<'a> {
    /// `insert_X(object:)`: every writable column.
    Insert,
    /// `update_X_by_pk(set:)`: every writable column but the primary key, which
    /// addresses the row rather than being written to it (the row layer drops a
    /// primary key from an update's assignments, and a schema that offered it
    /// would be promising something that silently does nothing).
    Set { pk: &'a str },
}

/// `input XInsertInput` / `input XSetInput`, or `None` when it would be empty —
/// which GraphQL does not allow, and which is honest: there is nothing to write.
///
/// **Every field is nullable**, including one whose column is `NOT NULL`. What is
/// required of an insert is not what is required of the *caller*: a column with a
/// database default, a sequence-backed primary key or a trigger-filled column is
/// `NOT NULL` and must not be demanded here. The row layer and the database
/// decide, and the message they give names the column.
fn write_input(
    table: &Table,
    t: &TableNames,
    type_name: &str,
    kind: WriteInput<'_>,
) -> Option<InputObject> {
    let skip = match kind {
        WriteInput::Insert => "",
        WriteInput::Set { pk } => pk,
    };
    let mut input = InputObject::new(type_name);
    let mut any = false;
    for field in writable_columns(table, t).filter(|f| f.base.name != skip) {
        input = input.field(InputValue::new(
            &field.base.name,
            TypeRef::named(write_scalar(field)),
        ));
        any = true;
    }
    any.then_some(input)
}

/// The columns a caller may write: the exposed ones that are not calculated.
///
/// A calculated field has no column behind it, and the row layer refuses a write
/// to one by name (`reject_calc_writes`). Leaving it out of the input type turns
/// that runtime refusal into something a caller's editor tells them, which is
/// the whole point of publishing an SDL.
fn writable_columns<'a>(
    table: &'a Table,
    t: &'a TableNames,
) -> impl Iterator<Item = &'a DataField> {
    t.fields
        .iter()
        .filter_map(move |name| table.field(name))
        .filter(|f| !matches!(f.kind, DataFieldKind::Calc { .. }))
}

/// The scalar a column is **written** as, which is not always the one it is read
/// as: a `File` field reads as a `FileValue` (a path and the URL its bytes are
/// served at) and is written as the path alone. There is nothing to write to a
/// URL — the bytes go to the REST provider's upload endpoint, which is the one
/// door into a file store.
fn write_scalar(field: &DataField) -> &'static str {
    match field.kind {
        DataFieldKind::File { .. } => TypeRef::STRING,
        _ => column_scalar(field),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{
        file_field, id_field, key_field, plain_field, table_of, typed_field,
    };
    use sc_types::BasicType;

    /// The SDL of an application that switched its aggregates **on** — most of
    /// these tests are about what the schema says, and the aggregate half of it
    /// only exists when somebody asked for it. What the switch itself does is
    /// asserted by `the_aggregate_fields_are_absent_when_the_switch_is_off`.
    fn sdl_of(tables: &[Table]) -> String {
        sdl_with(tables, GraphqlLimits::default().aggregates(true))
    }

    fn sdl_with(tables: &[Table], limits: GraphqlLimits) -> String {
        let names = SchemaNames::derive(tables);
        build_schema(tables, &names, limits)
            .expect("schema builds")
            .sdl()
    }

    /// The body of one SDL block, so a test can assert what is *not* in a type
    /// without the rest of the schema answering for it.
    fn between(sdl: &str, opening: &str) -> String {
        sdl.split(opening)
            .nth(1)
            .and_then(|s| s.split('}').next())
            .unwrap_or_else(|| panic!("no `{opening}` in {sdl}"))
            .to_owned()
    }

    #[test]
    fn an_application_with_no_projectable_table_is_a_mount_failure() {
        // Not an empty schema served cheerfully: a GraphQL API with no fields
        // is a configuration mistake, and this is where it is named.
        let names = SchemaNames::derive(&[]);
        let err = build_schema(&[], &names, GraphqlLimits::default()).unwrap_err();
        assert!(format!("{err}").contains("no tables"), "{err}");
    }

    #[test]
    fn the_root_carries_a_list_a_by_pk_and_an_aggregate_per_table() {
        let sdl = sdl_of(&[table_of(
            "departments",
            vec![id_field(), plain_field("name")],
        )]);
        assert!(
            sdl.contains(
                "departments(where: DepartmentsBoolExp, order_by: [DepartmentsOrderBy!], \
                 limit: Int, offset: Int): [Departments!]!"
            ),
            "{sdl}"
        );
        assert!(
            sdl.contains("departments_by_pk(id: BigInt!): Departments"),
            "{sdl}"
        );
        assert!(
            sdl.contains("departments_aggregate(where: DepartmentsBoolExp): DepartmentsAggregate!"),
            "{sdl}"
        );
    }

    #[test]
    fn a_table_with_no_single_column_primary_key_has_no_by_pk() {
        // The same rule the REST projection applies before emitting PUT/DELETE:
        // with nothing to address a row by, the field would be a promise the
        // row layer cannot keep.
        let sdl = sdl_of(&[table_of("notes", vec![plain_field("body")])]);
        assert!(sdl.contains("notes(where:"), "{sdl}");
        assert!(!sdl.contains("notes_by_pk"), "{sdl}");
    }

    #[test]
    fn a_relation_appears_on_the_parent_as_a_list_and_an_aggregate() {
        let sdl = sdl_of(&[
            table_of("departments", vec![id_field()]),
            table_of(
                "employees",
                vec![id_field(), key_field("department", "departments", "id")],
            ),
        ]);
        assert!(
            sdl.contains(
                "employees(where: EmployeesBoolExp, order_by: [EmployeesOrderBy!], \
                 limit: Int, offset: Int): [Employees!]!"
            ),
            "{sdl}"
        );
        assert!(
            sdl.contains("employees_aggregate(where: EmployeesBoolExp): EmployeesAggregate!"),
            "{sdl}"
        );
        // The outgoing key is the target's object type, not its raw value.
        assert!(sdl.contains("department: Departments"), "{sdl}");
    }

    #[test]
    fn avg_does_not_promise_to_round() {
        // A BigInt column's sum is a BigInt and its average is not.
        let sdl = sdl_of(&[table_of(
            "employees",
            vec![id_field(), typed_field("salary", BasicType::Int)],
        )]);
        assert!(sdl.contains("type EmployeesNumericFields {"), "{sdl}");
        assert!(sdl.contains("type EmployeesAvgFields {"), "{sdl}");
        let numeric = between(&sdl, "type EmployeesNumericFields {");
        assert!(numeric.contains("salary: BigInt"), "{numeric}");
        let avg = between(&sdl, "type EmployeesAvgFields {");
        assert!(avg.contains("salary: Decimal"), "{avg}");
    }

    #[test]
    fn a_table_with_nothing_numeric_has_no_sum_or_avg() {
        // GraphQL has no empty object type, so the fields that would need one
        // are simply absent rather than pointing at an unbuildable type.
        let sdl = sdl_of(&[table_of("notes", vec![plain_field("body")])]);
        let agg = between(&sdl, "type NotesAggregate {");
        assert!(agg.contains("count("), "{agg}");
        assert!(!agg.contains("sum"), "{agg}");
        assert!(!agg.contains("avg"), "{agg}");
        // Text is comparable, so min/max are there.
        assert!(agg.contains("min:"), "{agg}");
        assert!(!sdl.contains("NotesNumericFields"), "{sdl}");
    }

    #[test]
    fn a_file_field_is_a_path_and_a_url() {
        let sdl = sdl_of(&[table_of(
            "avatars",
            vec![id_field(), file_field("image", "uploads")],
        )]);
        assert!(sdl.contains("image: FileValue"), "{sdl}");
        assert!(sdl.contains("type FileValue {"), "{sdl}");
        assert!(sdl.contains("path: String!"), "{sdl}");
        assert!(sdl.contains("url: String!"), "{sdl}");
    }

    #[test]
    fn comparison_inputs_are_per_scalar_and_text_gets_the_pattern_operators() {
        let sdl = sdl_of(&[table_of("notes", vec![plain_field("body")])]);
        assert!(sdl.contains("input StringComparison {"), "{sdl}");
        assert!(sdl.contains("input BigIntComparison {"), "{sdl}");
        let string_cmp = between(&sdl, "input StringComparison {");
        for op in [
            "eq", "ne", "gt", "gte", "lt", "lte", "in", "nin", "is_null", "like", "ilike",
        ] {
            assert!(
                string_cmp.contains(op),
                "StringComparison lacks {op}: {string_cmp}"
            );
        }
        // JSON has no ordering, so it has no ordered comparisons.
        let json_cmp = between(&sdl, "input JSONComparison {");
        assert!(json_cmp.contains("eq"), "{json_cmp}");
        assert!(!json_cmp.contains("gte"), "{json_cmp}");
    }

    #[test]
    fn an_aggregate_is_wired_to_the_read_path_not_to_a_placeholder() {
        // Executed without a request context, `count` fails reaching for the
        // caller it is to be authorized against — which is the proof that it
        // *is* a read of rows, rather than a number this module invented. A
        // plausible zero here would be the exact failure decision 5 forbids.
        let tables = [table_of("departments", vec![id_field()])];
        let names = SchemaNames::derive(&tables);
        let schema = build_schema(&tables, &names, GraphqlLimits::default().aggregates(true))
            .expect("builds");
        let response = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(schema.execute("{ departments_aggregate { count } }"));
        assert!(!response.errors.is_empty(), "{response:?}");
        assert!(
            response.errors[0].message.contains("RequestContext"),
            "{:?}",
            response.errors
        );
    }

    #[test]
    fn the_aggregate_fields_are_absent_when_the_switch_is_off() {
        // Off is the default, and off means *absent from the schema*: no root
        // `X_aggregate`, no `_aggregate` on the relation, and none of the types
        // they are answered with. A schema that carried the fields and refused
        // at resolve time would be a schema that advertises what it will not do.
        let tables = [
            table_of("departments", vec![id_field()]),
            table_of(
                "employees",
                vec![
                    id_field(),
                    plain_field("name"),
                    key_field("department", "departments", "id"),
                ],
            ),
        ];
        let sdl = sdl_with(&tables, GraphqlLimits::default());
        for absent in [
            "departments_aggregate",
            "employees_aggregate",
            "DepartmentsAggregate",
            "EmployeesAggregate",
            "SelectColumn",
        ] {
            assert!(!sdl.contains(absent), "`{absent}` should be absent:\n{sdl}");
        }
        // …and the rest of the schema is untouched: the switch removes the
        // aggregates, not the reads they are taken over.
        assert!(sdl.contains("departments("), "{sdl}");
        assert!(sdl.contains("employees("), "{sdl}");
    }

    #[test]
    fn an_aggregate_against_a_switched_off_schema_is_a_validation_error() {
        // The point of *absence*: the refusal is the library's own "field not
        // found", raised over the document before a resolver — and therefore
        // before a statement — runs. A resolve-time refusal would have cost a
        // round trip to say the same thing.
        let tables = [table_of("departments", vec![id_field()])];
        let names = SchemaNames::derive(&tables);
        let schema = build_schema(&tables, &names, GraphqlLimits::default()).expect("builds");
        let response = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(schema.execute("{ departments_aggregate { count } }"));
        assert!(!response.errors.is_empty(), "{response:?}");
        let message = &response.errors[0].message;
        assert!(message.contains("departments_aggregate"), "{message}");
        // Not the "no RequestContext" a resolver would have produced: nothing
        // resolved, because there is no such field to resolve.
        assert!(!message.contains("RequestContext"), "{message}");
    }

    #[test]
    fn every_table_gets_its_three_mutations() {
        // Decision 4: one schema per application, not per role. Whether *this*
        // caller may write is decided at resolve time; the schema says what the
        // application exposes.
        let sdl = sdl_of(&[table_of(
            "departments",
            vec![id_field(), plain_field("name")],
        )]);
        assert!(
            sdl.contains("insert_departments(object: DepartmentsInsertInput!): Departments"),
            "{sdl}"
        );
        assert!(
            sdl.contains(
                "update_departments_by_pk(pk_columns: DepartmentsPkColumns!, \
                 set: DepartmentsSetInput!): Departments"
            ),
            "{sdl}"
        );
        assert!(
            sdl.contains("delete_departments_by_pk(id: BigInt!): Departments"),
            "{sdl}"
        );
        assert!(sdl.contains("mutation: Mutation"), "{sdl}");
    }

    #[test]
    fn an_insert_demands_no_column_the_database_can_fill_itself() {
        // `id` is `NOT NULL`, and requiring it on the wire would refuse every
        // insert into a table with a sequence.
        let sdl = sdl_of(&[table_of(
            "departments",
            vec![id_field(), plain_field("name")],
        )]);
        let insert = between(&sdl, "input DepartmentsInsertInput {");
        assert!(insert.contains("id: BigInt"), "{insert}");
        assert!(!insert.contains("BigInt!"), "{insert}");
    }

    #[test]
    fn the_primary_key_addresses_an_update_rather_than_being_set_by_it() {
        let sdl = sdl_of(&[table_of(
            "departments",
            vec![id_field(), plain_field("name")],
        )]);
        let set = between(&sdl, "input DepartmentsSetInput {");
        assert!(set.contains("name"), "{set}");
        assert!(!set.contains("id"), "{set}");
        // …and it is what `pk_columns` carries, non-null.
        let keys = between(&sdl, "input DepartmentsPkColumns {");
        assert!(keys.contains("id: BigInt!"), "{keys}");
    }

    #[test]
    fn a_table_with_no_single_primary_key_can_only_be_inserted_into() {
        // The same rule `_by_pk` follows: with nothing to address a row by, an
        // update and a delete would be promises the row layer cannot keep.
        let sdl = sdl_of(&[table_of("notes", vec![plain_field("body")])]);
        assert!(
            sdl.contains("insert_notes(object: NotesInsertInput!)"),
            "{sdl}"
        );
        assert!(!sdl.contains("update_notes_by_pk"), "{sdl}");
        assert!(!sdl.contains("delete_notes_by_pk"), "{sdl}");
        assert!(!sdl.contains("NotesPkColumns"), "{sdl}");
    }

    #[test]
    fn a_file_field_is_written_as_its_path_and_read_as_a_url() {
        // There is nothing to write to a URL: the bytes go through the REST
        // provider's upload endpoint, which is the one door into a store.
        let sdl = sdl_of(&[table_of(
            "avatars",
            vec![id_field(), file_field("image", "uploads")],
        )]);
        assert!(sdl.contains("image: FileValue"), "{sdl}");
        let insert = between(&sdl, "input AvatarsInsertInput {");
        assert!(insert.contains("image: String"), "{insert}");
    }

    #[test]
    fn a_calculated_field_is_not_offered_to_a_write() {
        // The row layer refuses a write to one by name; leaving it out of the
        // input type is that refusal, moved to where a caller's editor sees it.
        let mut total = plain_field("total");
        total.kind = DataFieldKind::Calc {
            expression: "1".to_owned(),
        };
        let sdl = sdl_of(&[table_of(
            "invoices",
            vec![id_field(), plain_field("ref"), total],
        )]);
        assert!(sdl.contains("total: String"), "{sdl}");
        let insert = between(&sdl, "input InvoicesInsertInput {");
        assert!(insert.contains("ref"), "{insert}");
        assert!(!insert.contains("total"), "{insert}");
    }

    #[test]
    fn a_mutation_is_wired_to_the_write_path_not_to_a_placeholder() {
        // The same proof the aggregate gets: executed with no request context,
        // the field fails reaching for the caller it would write as. A mutation
        // that cheerfully returned `null` here would be a write that never
        // happened and never said so.
        let tables = [table_of(
            "departments",
            vec![id_field(), plain_field("name")],
        )];
        let names = SchemaNames::derive(&tables);
        let schema = build_schema(&tables, &names, GraphqlLimits::default()).expect("builds");
        let response = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(
                schema.execute("mutation { insert_departments(object: { name: \"x\" }) { id } }"),
            );
        assert!(!response.errors.is_empty(), "{response:?}");
        assert!(
            response.errors[0].message.contains("RequestContext"),
            "{:?}",
            response.errors
        );
    }

    #[test]
    fn introspection_stays_on() {
        // It describes tables the application already exposes over REST, and
        // every browser tool needs it.
        let tables = [table_of("departments", vec![id_field()])];
        let names = SchemaNames::derive(&tables);
        let schema = build_schema(&tables, &names, GraphqlLimits::default()).expect("builds");
        let response = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(schema.execute("{ __schema { queryType { name } } }"));
        assert!(response.errors.is_empty(), "{:?}", response.errors);
    }
}
