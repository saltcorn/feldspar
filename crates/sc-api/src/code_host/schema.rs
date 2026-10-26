//! The **schema snapshot**: this server's tables, in v1's vocabulary, as one
//! serialised value a guest holds before its run starts (TODO "the v1 `Table`
//! API" §2).
//!
//! # Why it exists
//!
//! v1's `Table.findOne` is synchronous. Eight years of plugins are written
//!
//! ```js
//! const table = Table.findOne("books");   // not awaited
//! const pk = table.pk_name;               // not awaited
//! for (const f of table.fields) { … }     // not awaited
//! ```
//!
//! and a host round trip cannot answer that — a `Table` that answered a promise
//! would break every line after it. So the division is v1's own: **metadata is
//! local and synchronous, data is a host call and asynchronous**, and this is
//! the local half. Every table this catalog has, with the property names v1's
//! `Table` and `Field` carry, built once and handed to the isolate before
//! anything runs.
//!
//! # It is a read of the catalog, not a second source of truth
//!
//! Everything here comes off the same [`Catalog`] a [`Plan`](super::Plan) is
//! resolved against, so a guest's `books.getField("author").is_fkey` and the
//! server's own answer to "is `author` a key" cannot disagree — one of them is
//! computed from the other. It goes stale exactly when the catalog reloads,
//! which bumps [`Catalog::generation`], which is what a run carries instead of
//! the snapshot itself.
//!
//! # What is translated, and what is not
//!
//! v1 spells some of this differently and some of it it does not have at all.
//! The translations are named where they are made:
//!
//! - A field's **id is its name**, because this server's fields are identified
//!   by name (§9), and so is a table's. A plugin keying a map by `f.id` gets a
//!   stable key either way.
//! - v1's `type` is a *type object* for a plain field and a **string** for a key
//!   (`"Key to books"`) or a file (`"File"`); `typename` is the string form of
//!   whichever it is. Both are carried, because v1 code reads both.
//! - v1's ownership is a **field** (`ownership_field_id`); here it is a formula
//!   (§7.3). Both are carried: the formula's source always, and the field only
//!   when the formula says exactly what v1's field says — `owner === user.id`
//!   and nothing else. A formula this server can evaluate and v1 could not
//!   express is not reduced to a field that would mean something narrower.
//! - `fieldview` and `sublabel` are `_sc_fields` columns in v1 and attributes
//!   here, so they are read out of the attribute bag and are null where nobody
//!   set them.
//! - `stored` is always false: this server has no stored calculated fields
//!   (§6.2 recomputes its own), and a `calculated` field here is v1's
//!   non-stored kind exactly.
//!
//! Nothing about *changing* a table is in here, because nothing in this
//! milestone can: `Table.create`, `Field.update` and every DDL method stay
//! stubs that throw naming themselves.

use std::collections::HashMap;
use std::sync::Arc;

use sc_catalog::{Catalog, DataField, DataFieldKind, Table};
use sc_error::Result;
use sc_expr::{Ast, BinaryOp, Formula, MemberProp, SchemaSnapshot};
use sc_types::{BasicType, TypeRef};
use serde_json::{Map, Value as Json, json};

/// The attribute a field's v1 `fieldview` is kept in.
const ATTR_FIELDVIEW: &str = "fieldview";
/// The attribute a field's v1 `sublabel` is kept in.
const ATTR_SUBLABEL: &str = "sublabel";

/// The snapshot for `catalog`'s current generation, built once and cached on the
/// catalog itself.
///
/// The generation is read **before** the tables are: a reload racing this build
/// produces a snapshot stamped with the older number, which
/// [`Catalog::code_schema`] then declines to hand back — so the cost of losing
/// that race is one rebuild, never a guest holding a schema that claims to be
/// current and is not.
pub fn snapshot(catalog: &Catalog) -> Result<Arc<SchemaSnapshot>> {
    if let Some(held) = catalog.code_schema() {
        return Ok(held);
    }
    let generation = catalog.generation();
    let json = serde_json::to_string(&schema_json(catalog)?)
        .map_err(|e| sc_error::Error::msg(format!("serialise the schema snapshot: {e}")))?;
    let built = Arc::new(SchemaSnapshot::new(generation, json));
    catalog.set_code_schema(Arc::clone(&built));
    Ok(built)
}

/// The snapshot's JSON: `{ "tables": [ … ] }`, tables in the catalog's own
/// (name) order.
///
/// An object rather than a bare array so that a later milestone can put
/// something beside the tables — the plugin's own configuration, `getState()`'s
/// three lists — without every guest that parses this having to be changed on
/// the same day.
fn schema_json(catalog: &Catalog) -> Result<Json> {
    let tables = catalog.tables()?;
    // A key's `reftype` is the *target* column's type, so a field cannot be
    // described from its own table alone. Built once here rather than looked up
    // per field, which on a schema with a hundred key fields is the difference
    // between one pass and a hundred scans.
    let by_name: HashMap<&str, &Table> = tables.iter().map(|t| (t.name.as_str(), t)).collect();
    let json: Vec<Json> = tables.iter().map(|t| table_json(&by_name, t)).collect();
    Ok(json!({ "tables": json }))
}

/// One table, in v1's vocabulary.
fn table_json(by_name: &HashMap<&str, &Table>, table: &Table) -> Json {
    // A hidden column (the password hash) is not described: a v1 pattern turns
    // the fields it is told of into filters on URL state, and one on the hash
    // would be an oracle for it.
    let fields: Vec<Json> = table
        .fields
        .iter()
        .filter(|f| !crate::user_rows::is_hidden_column(table, &f.base.name))
        .map(|f| field_json(by_name, table, f))
        .collect();
    json!({
        // Its id *is* its name (§9), for a table as for a field.
        "id": table.name,
        "name": table.name,
        "label": table.label,
        "description": table.description,
        // The whole key, in key order, because this server allows a composite
        // one and v1's `pk_name` is only the first of it.
        "primary_key": table.primary_key,
        // v1's spelling of the access rules, and v1's meaning: a role number,
        // lower being more privileged, that a caller must be at or below.
        "min_role_read": table.access.min_role_read,
        "min_role_write": table.access.min_role_write,
        // The formula's *source*, not its tree: what a v1 plugin does with
        // `ownership_formula` is show it or log it, and the tree is this
        // server's business.
        "ownership_formula": table.ownership.as_ref().map(Formula::source),
        "ownership_field_id": table.ownership.as_ref().and_then(ownership_field),
        // The module's provider where a provider is what serves this table
        // (§8.3), so a guest can tell a fed table from a stored one, and null
        // for an ordinary database table.
        "provider_name": table.source.provider().map(|(_, provider)| provider),
        "provider_module": table.source.provider().map(|(module, _)| module),
        // Not v1's, and named as this server's own: `_fd_*` is where this
        // server keeps its own rows, and a `Table.find()` that listed them
        // would put them in front of a plugin that only ever wanted the
        // application's tables.
        "is_system": table.is_system(),
        // v1's `table.constraints`, in v1's shape: Edit finds the row a state
        // names through a jointly-unique key as well as through a unique field.
        "constraints": table.constraints.iter().map(constraint_json).collect::<Vec<_>>(),
        "fields": fields,
    })
}

/// One constraint as v1's `TableConstraint` is shaped: a `type` and a
/// `configuration` — `fields` for a unique key, `field` for an index (v1's
/// indexes are over one), `formula` for a row constraint.
fn constraint_json(constraint: &sc_catalog::TableConstraint) -> Json {
    use sc_catalog::ConstraintKind;
    let errormsg = constraint.error_message.clone();
    let (kind, configuration) = match &constraint.kind {
        ConstraintKind::Unique { fields } => {
            ("Unique", json!({ "fields": fields, "errormsg": errormsg }))
        }
        ConstraintKind::Index { fields, .. } => ("Index", json!({ "field": fields.first() })),
        ConstraintKind::FullTextSearch { .. } => ("Index", json!({ "field": "_fts" })),
        ConstraintKind::VectorIndex { field } => ("Index", json!({ "field": field })),
        ConstraintKind::Formula { formula } => (
            "Formula",
            json!({ "formula": formula, "errormsg": errormsg }),
        ),
    };
    json!({ "name": constraint.name, "type": kind, "configuration": configuration })
}

/// One field, as §7's property list.
///
/// `table` is the field's own table, which is where the two identity properties
/// come from: v1's `table_id` (a number there, the table's name here) and its
/// `table` (the `Table` object, which the guest resolves for itself rather than
/// nesting a copy of every table inside every field).
fn field_json(by_name: &HashMap<&str, &Table>, table: &Table, field: &DataField) -> Json {
    let mut out = Map::new();
    out.insert("name".to_owned(), json!(field.base.name));
    out.insert("label".to_owned(), json!(field.base.label));
    // v1's two type properties. `type` is an object for a plain field and a
    // string for a key or a file; `typename` is the string form either way,
    // which is what most plugin code actually reads.
    let (type_json, typename) = field_type(field);
    out.insert("type".to_owned(), type_json);
    out.insert("typename".to_owned(), json!(typename));
    out.insert("required".to_owned(), json!(field.required));
    out.insert("is_unique".to_owned(), json!(field.unique));
    out.insert("primary_key".to_owned(), json!(field.primary_key));
    // v1's calculated fields come in two kinds and this server has one of them:
    // computed on read, never on disk. So `stored` is false rather than absent,
    // because a plugin that branches on it must take the branch that is true
    // here.
    out.insert("calculated".to_owned(), json!(field.is_calc()));
    out.insert("stored".to_owned(), json!(false));
    out.insert("expression".to_owned(), json!(field.calc_expression()));
    // A key *and* a file are `is_fkey` in v1, because both carry a string type
    // there. A file's target is a path in a store and not a table, so it has a
    // name for nothing to reference: null rather than v1's `_sc_files`, which
    // is a table this server does not have and a plugin must not be told it
    // can read.
    let key = match &field.kind {
        DataFieldKind::Key {
            target_table,
            target_field,
            ..
        } => Some((target_table.0.as_str(), target_field.0.as_str())),
        _ => None,
    };
    out.insert(
        "is_fkey".to_owned(),
        json!(matches!(
            field.kind,
            DataFieldKind::Key { .. } | DataFieldKind::File { .. }
        )),
    );
    out.insert("reftable_name".to_owned(), json!(key.map(|(t, _)| t)));
    out.insert("refname".to_owned(), json!(key.map(|(_, f)| f)));
    // v1's `reftype` is the *type* of the referenced column, which is what a
    // form needs to coerce a key's value with. Null when the target table is
    // not in the catalog at all — a foreign key across a connection the server
    // has since lost — rather than a type invented to fill the hole.
    let reftype = key.and_then(|(table, field)| {
        let target = by_name.get(table)?.field(field)?;
        Some(v1_type_name(&target.base.type_))
    });
    out.insert("reftype".to_owned(), json!(reftype));
    out.insert("attributes".to_owned(), Json::Object(v1_attributes(field)));
    out.insert("fieldview".to_owned(), attr(field, ATTR_FIELDVIEW));
    out.insert("sublabel".to_owned(), attr(field, ATTR_SUBLABEL));
    // The table's *name*: v1's `table_id` is a number and this server's tables
    // are identified by name (§9). Its `table` — the `Table` object — is
    // resolved in the guest from this, because nesting one would put a copy of
    // every table inside every field of it.
    out.insert("table_id".to_owned(), json!(table.name));
    // What the column is called in SQL, for `sql_name`. The same as the name
    // here, carried anyway so the guest never has to know that.
    out.insert("sql_name".to_owned(), json!(field.base.name));
    out.insert("sql_type".to_owned(), json!(field.base.type_.sql_type()));
    Json::Object(out)
}

/// A field's attributes, with the two v1 keeps there and this server keeps
/// elsewhere put where v1 looks: a key's `summary_field` (the column its options
/// are labelled by, which this server holds on the key itself), and `day_only`
/// on a `date` column (v1's one `Date` type is a timestamp unless it says so).
fn v1_attributes(field: &DataField) -> Map<String, Json> {
    let mut attributes = field.base.attributes.clone();
    if let DataFieldKind::Key {
        summary_field: Some(summary),
        ..
    } = &field.kind
    {
        attributes
            .entry("summary_field")
            .or_insert_with(|| json!(summary.0));
    }
    if field.base.type_.as_basic() == Some(&BasicType::Date) {
        attributes.entry("day_only").or_insert(json!(true));
    }
    attributes
}

/// v1's `type` and `typename` for one field.
///
/// A key is `"Key to <table>"` and a file is `"File"` — strings, in v1, which is
/// exactly why `is_fkey` is `typeof type === "string"` there. Everything else is
/// the type object a plugin reads `.name` and `.sql_name` off.
fn field_type(field: &DataField) -> (Json, String) {
    match &field.kind {
        DataFieldKind::Key { target_table, .. } => {
            let name = format!("Key to {}", target_table.0);
            (json!(name), name)
        }
        DataFieldKind::File { .. } => (json!("File"), "File".to_owned()),
        _ => {
            let name = v1_type_name(&field.base.type_);
            (
                json!({ "name": name, "sql_name": field.base.type_.sql_type() }),
                name,
            )
        }
    }
}

/// What v1 calls this type.
///
/// A v1 plugin branches on `field.type.name` — `"String"`, `"Integer"`,
/// `"Bool"` — so a snapshot that answered this server's own spelling (`"text"`,
/// `"int"`) would send every such branch to its default, which is a plugin
/// quietly rendering the wrong widget. Where v1 has the type, it gets v1's name.
///
/// Where it does **not**, this server's own name stands: `decimal`, `bytes`,
/// `uuid`, `time` are types v1 never had, and calling a `numeric` column
/// `"Float"` to make a branch fire is how a plugin comes to round somebody's
/// money. A default branch is the right answer for a type v1 cannot describe.
///
/// `Date` and `Timestamp` are the one place two of ours meet one of theirs: v1's
/// `Date` *is* a timestamp, has no date-only counterpart, and a plugin that
/// formats one formats the other correctly. `sql_name` still tells them apart.
///
/// A **rich** type keeps its own name, which is already a Saltcorn type name —
/// except the two that *are* v1's types under this server's spelling: `string`
/// is v1's `String` and `integer` its `Integer` (a restored v1 backup's columns
/// are exactly these), and a v1 view pattern looks a field's type up in v1's
/// registry by v1's name.
fn v1_type_name(type_: &TypeRef) -> String {
    let Some(basic) = type_.as_basic() else {
        return v1_rich_type_name(type_.name()).to_owned();
    };
    match basic {
        BasicType::Text => "String",
        BasicType::Int => "Integer",
        BasicType::Float => "Float",
        BasicType::Bool => "Bool",
        BasicType::Date | BasicType::Timestamp => "Date",
        BasicType::Json => "JSON",
        other => other.name(),
    }
    .to_owned()
}

/// v1's name for a rich type, which is its own unless it is one of v1's.
fn v1_rich_type_name(name: &str) -> &str {
    match name {
        n if n == sc_types::StringType::NAME => "String",
        n if n == sc_types::IntegerType::NAME => "Integer",
        other => other,
    }
}

/// One of a field's attributes, or null.
fn attr(field: &DataField, name: &str) -> Json {
    field
        .base
        .attributes
        .get(name)
        .cloned()
        .unwrap_or(Json::Null)
}

/// The field a v1 `ownership_field_id` would name, when this table's ownership
/// formula says exactly what such a field says and nothing more.
///
/// v1's ownership field means one thing: *this column holds the id of the user
/// who owns the row*. So `owner === user.id` (in either order) is that field,
/// and everything else — a formula over a Ⱶ-path, a role test, an `||` of two
/// columns — is **not**, and answers `None`. Reducing a wider rule to a field
/// would hand a plugin a narrower rule that looks authoritative; the formula's
/// source is carried beside this for the plugin that wants to know the whole of
/// it.
fn ownership_field(formula: &Formula) -> Option<String> {
    let Ast::Binary { op, l, r } = formula.ast() else {
        return None;
    };
    if !matches!(op, BinaryOp::StrictEq | BinaryOp::Eq) {
        return None;
    }
    let field = |side: &Ast| match side {
        // A Ⱶ-path is a single identifier too, and is not a column of this
        // table — so anything with the join character in it is not this.
        Ast::Ident(name) if !name.contains(sc_expr::JOIN) => Some(name.clone()),
        _ => None,
    };
    let user_id = |side: &Ast| {
        matches!(side, Ast::Member { obj, prop, .. }
            if matches!(&**obj, Ast::Ident(name) if name == "user")
                && matches!(prop, MemberProp::Static(p) if p == "id"))
    };
    match (user_id(l), user_id(r)) {
        (true, false) => field(r),
        (false, true) => field(l),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{catalog_of, id_field, physical, plain_field, table_of};
    use sc_catalog::AccessRules;
    use sc_db::PhysicalTable;

    /// `books` with an author key and `authors` behind it — the smallest schema
    /// with a foreign key in it.
    async fn library() -> Arc<Catalog> {
        let tables: Vec<PhysicalTable> = vec![
            physical(
                "books",
                &[
                    ("id", "int8", false),
                    ("title", "text", true),
                    ("pages", "int8", true),
                    ("author", "int8", true),
                ],
                &[("author", "authors", "id")],
            ),
            physical(
                "authors",
                &[("id", "int8", false), ("name", "text", true)],
                &[],
            ),
        ];
        catalog_of(tables).await
    }

    /// The snapshot's tables, by name.
    fn tables_of(snapshot: &SchemaSnapshot) -> HashMap<String, Json> {
        let parsed: Json = serde_json::from_str(snapshot.json()).expect("valid JSON");
        parsed["tables"]
            .as_array()
            .expect("an array of tables")
            .iter()
            .map(|t| (t["name"].as_str().expect("a name").to_owned(), t.clone()))
            .collect()
    }

    #[tokio::test]
    async fn a_field_carries_v1s_property_names_with_v1s_values() {
        let catalog = library().await;
        let snapshot = snapshot(&catalog).unwrap();
        let tables = tables_of(&snapshot);
        let books = &tables["books"];

        // The table half: v1's spelling of the access rules, its id being its
        // name, and the whole primary key rather than only its first column.
        assert_eq!(books["id"], json!("books"));
        assert_eq!(books["primary_key"], json!(["id"]));
        assert_eq!(books["min_role_read"], json!(1));
        assert_eq!(books["min_role_write"], json!(1));
        assert_eq!(books["ownership_formula"], Json::Null);
        assert_eq!(books["provider_name"], Json::Null);
        assert_eq!(books["is_system"], json!(false));

        let field = |name: &str| -> Json {
            books["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["name"] == json!(name))
                .unwrap_or_else(|| panic!("no field `{name}`"))
                .clone()
        };

        // A plain field: `type` is the type *object* a plugin reads `.name`
        // off, and `typename` is the string form of the same thing.
        let title = field("title");
        assert_eq!(title["type"]["name"], json!("String"));
        assert_eq!(title["typename"], json!("String"));
        assert_eq!(title["is_fkey"], json!(false));
        assert_eq!(title["reftable_name"], Json::Null);
        assert_eq!(title["required"], json!(false));
        assert_eq!(title["primary_key"], json!(false));
        assert_eq!(title["calculated"], json!(false));
        assert_eq!(title["stored"], json!(false));
        assert_eq!(title["expression"], Json::Null);
        assert_eq!(title["table_id"], json!("books"));
        assert_eq!(title["attributes"], json!({}));
        assert_eq!(title["fieldview"], Json::Null);
        assert_eq!(title["sublabel"], Json::Null);

        // The primary key is required and says so on both properties.
        let id = field("id");
        assert_eq!(id["primary_key"], json!(true));
        assert_eq!(id["required"], json!(true));

        // A key field: the definition of done's own assertion — `is_fkey` is
        // true — plus v1's three reference properties, `reftype` resolved
        // through the *target's* column and not guessed.
        let author = field("author");
        assert_eq!(author["is_fkey"], json!(true));
        assert_eq!(author["type"], json!("Key to authors"));
        assert_eq!(author["typename"], json!("Key to authors"));
        assert_eq!(author["reftable_name"], json!("authors"));
        assert_eq!(author["refname"], json!("id"));
        assert_eq!(author["reftype"], json!("Integer"));
    }

    #[test]
    fn a_constraint_is_shaped_as_v1s() {
        use sc_catalog::{ConstraintKind, TableConstraint};
        let unique = TableConstraint::new(
            "books_title_author_key",
            ConstraintKind::Unique {
                fields: vec!["title".to_owned(), "author".to_owned()],
            },
        )
        .message("A book by that author already has that title");
        assert_eq!(
            constraint_json(&unique),
            json!({
                "name": "books_title_author_key",
                "type": "Unique",
                "configuration": {
                    "fields": ["title", "author"],
                    "errormsg": "A book by that author already has that title",
                },
            })
        );
    }

    #[test]
    fn the_rich_types_that_are_v1s_carry_v1s_names() {
        assert_eq!(v1_rich_type_name("string"), "String");
        assert_eq!(v1_rich_type_name("integer"), "Integer");
        // Any other rich type is not v1's, and keeps the name it has.
        assert_eq!(v1_rich_type_name("colour"), "colour");
    }

    #[tokio::test]
    async fn a_reload_bumps_the_generation_and_drops_the_snapshot_built_before_it() {
        let catalog = library().await;
        let first = snapshot(&catalog).unwrap();
        assert_eq!(first.generation(), catalog.generation());

        // A second ask at the same generation is the *same* snapshot: serialised
        // once, which is the whole reason the stamp exists.
        let again = snapshot(&catalog).unwrap();
        assert!(
            Arc::ptr_eq(&first, &again),
            "the schema was serialised twice"
        );

        catalog.reload().await.unwrap();
        assert_eq!(
            catalog.generation(),
            first.generation() + 1,
            "a reload bumps the stamp"
        );
        assert!(
            catalog.code_schema().is_none(),
            "a snapshot built before the reload is not handed out after it"
        );
        let after = snapshot(&catalog).unwrap();
        assert_eq!(after.generation(), first.generation() + 1);
        assert!(!Arc::ptr_eq(&first, &after));
    }

    #[test]
    fn only_v1s_own_ownership_shape_reduces_to_a_field() {
        let field = |source: &str| ownership_field(&Formula::parse(source).unwrap());
        // What v1's `ownership_field_id` means, in either order.
        assert_eq!(field("owner === user.id"), Some("owner".to_owned()));
        assert_eq!(field("user.id === owner"), Some("owner".to_owned()));
        assert_eq!(field("owner == user.id"), Some("owner".to_owned()));
        // Everything wider is *not* that field: reducing it would hand a plugin
        // a narrower rule than the one this server enforces.
        for wider in [
            "owner === user.id || user.role_id === 1",
            "user.id === 1",
            "ownerⱵboss === user.id",
            "owner === user.email",
            "owner > user.id",
        ] {
            assert_eq!(field(wider), None, "{wider}");
        }
    }

    #[test]
    fn a_table_carries_its_ownership_rule_both_ways_it_can_be_read() {
        let mut table = table_of("notes", vec![id_field(), plain_field("body")]);
        table.access = AccessRules {
            min_role_read: 40,
            min_role_write: 20,
        };
        table.label = "Notes".to_owned();
        table.description = "what somebody wrote down".to_owned();
        table.ownership = Some(Formula::parse("owner === user.id").unwrap());
        let json = table_json(&HashMap::new(), &table);

        assert_eq!(json["label"], "Notes");
        assert_eq!(json["description"], "what somebody wrote down");
        assert_eq!(json["min_role_read"], json!(40));
        assert_eq!(json["min_role_write"], json!(20));
        // The whole rule, and — because this one says exactly what a v1
        // ownership *field* says — the field as well.
        assert_eq!(json["ownership_formula"], json!("owner === user.id"));
        assert_eq!(json["ownership_field_id"], json!("owner"));

        // A rule v1 could not have expressed keeps its source and gives up the
        // field, rather than being reduced to the narrower half of itself.
        table.ownership = Some(Formula::parse("owner === user.id || user.role_id === 1").unwrap());
        let json = table_json(&HashMap::new(), &table);
        assert_eq!(
            json["ownership_formula"],
            json!("owner === user.id || user.role_id === 1")
        );
        assert_eq!(json["ownership_field_id"], Json::Null);
    }

    #[test]
    fn a_calculated_field_is_v1s_non_stored_kind_and_says_so() {
        let mut calc = plain_field("mine");
        calc.kind = DataFieldKind::Calc {
            expression: "owner === user.id".to_owned(),
        };
        calc.base.label = "Is mine".to_owned();
        calc.base
            .attributes
            .insert("sublabel".to_owned(), json!("only yours"));
        let table = table_of("notes", vec![id_field(), calc]);
        let json = table_json(&HashMap::new(), &table);
        let field = &json["fields"].as_array().unwrap()[1];

        assert_eq!(field["label"], "Is mine");
        assert_eq!(field["calculated"], json!(true));
        // False rather than absent: this server computes on read and has no
        // stored kind, so a plugin that branches on `stored` must take the
        // branch that is true here.
        assert_eq!(field["stored"], json!(false));
        assert_eq!(field["expression"], json!("owner === user.id"));
        // The two `_sc_fields` columns v1 has and this server keeps in the
        // attribute bag.
        assert_eq!(field["sublabel"], json!("only yours"));
        assert_eq!(field["fieldview"], Json::Null);
    }
}
