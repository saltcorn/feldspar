//! `edit_schema` and `describe_schema` on the rules the database keeps (TODO
//! SC1.1, from a user report): one-field unique on and off, unique over several
//! fields, a `string`'s allowed values, a default on an optional field, and a
//! setting an operation does not use refused rather than ignored.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Env, config};
use sc_agent::RunCaller;
use sc_error::Result;
use serde_json::{Value as Json, json};
use std::sync::Arc;

const TRAIT: &str = "admin_copilot";

fn edit_grants() -> sc_types::Attrs {
    config(&[("allow_create", json!(true)), ("allow_edit", json!(true))])
}

fn all_grants() -> sc_types::Attrs {
    config(&[
        ("allow_create", json!(true)),
        ("allow_edit", json!(true)),
        ("allow_drop", json!(true)),
    ])
}

async fn edit(env: &Env, cfg: &sc_types::Attrs, operations: Json) -> Result<Json> {
    env.call_tool(
        TRAIT,
        cfg,
        "edit_schema",
        json!({ "operations": operations }),
        &RunCaller::system(),
    )
    .await
}

/// One table of `describe_schema`'s output.
async fn describe(env: &Env, table: &str) -> Result<Json> {
    let out = env
        .call_tool(
            TRAIT,
            &edit_grants(),
            "describe_schema",
            json!({}),
            &RunCaller::system(),
        )
        .await?;
    Ok(out["tables"]
        .as_array()
        .and_then(|ts| ts.iter().find(|t| t["name"] == json!(table)))
        .cloned()
        .unwrap_or_else(|| panic!("no table `{table}` in {out}")))
}

fn field<'a>(table: &'a Json, name: &str) -> &'a Json {
    table["fields"]
        .as_array()
        .and_then(|fs| fs.iter().find(|f| f["name"] == json!(name)))
        .unwrap_or_else(|| panic!("no field `{name}` in {table}"))
}

fn constraint_names(table: &Json) -> Vec<String> {
    table["constraints"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .filter_map(|c| c["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

async fn create_documents(env: &Env) -> Result<()> {
    edit(
        env,
        &edit_grants(),
        json!([{
            "op": "create_table", "table": "documents",
            "fields": [
                {"name": "id", "type": "int", "primary_key": true},
                {"name": "code", "type": "text", "unique": true},
                {"name": "document_id", "type": "int"},
                {"name": "version_no", "type": "int"},
            ],
        }]),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn unique_reads_back_and_comes_off_again_so_the_field_can_be_dropped() -> Result<()> {
    let env = Env::new().await?;
    create_documents(&env).await?;

    // The report's second bug: a field created unique read back as not unique.
    let documents = describe(&env, "documents").await?;
    assert_eq!(
        field(&documents, "code")["unique"],
        json!(true),
        "{documents}"
    );
    assert_eq!(field(&documents, "document_id")["unique"], json!(false));
    let names = constraint_names(&documents);
    assert_eq!(names.len(), 1, "{documents}");
    let unique = documents["constraints"][0].clone();
    assert_eq!(unique["type"], json!("unique"));
    assert_eq!(unique["fields"], json!(["code"]));

    env.execute("INSERT INTO documents (code) VALUES ('A');")
        .await?;
    assert!(
        env.execute("INSERT INTO documents (code) VALUES ('A');")
            .await
            .is_err()
    );

    // Taking a rule away is a drop, and needs the grant a drop needs.
    let off =
        json!([{"op": "alter_field", "table": "documents", "field": "code", "unique": false}]);
    let err = edit(&env, &edit_grants(), off.clone())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("allow_drop"), "{err}");

    // The report's first bug: this used to succeed and change nothing.
    let result = edit(&env, &all_grants(), off).await?;
    assert_eq!(
        result["constraints_dropped"],
        json!([format!("documents.{}", names[0])])
    );
    env.execute("INSERT INTO documents (code) VALUES ('A');")
        .await?;
    let documents = describe(&env, "documents").await?;
    assert_eq!(field(&documents, "code")["unique"], json!(false));
    assert!(constraint_names(&documents).is_empty(), "{documents}");

    // On again: refused while two rows share a value, naming the rows.
    let on = json!([{"op": "alter_field", "table": "documents", "field": "code", "unique": true}]);
    let err = edit(&env, &edit_grants(), on.clone())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cannot be made unique: 1 row repeats"),
        "{err}"
    );
    env.execute("UPDATE documents SET code = 'B' WHERE id = (SELECT max(id) FROM documents);")
        .await?;
    edit(&env, &edit_grants(), on.clone()).await?;
    assert_eq!(
        field(&describe(&env, "documents").await?, "code")["unique"],
        json!(true)
    );
    // Saying it twice is not a second constraint.
    edit(&env, &edit_grants(), on).await?;
    assert_eq!(
        constraint_names(&describe(&env, "documents").await?).len(),
        1
    );

    // And the field the constraint held on to can be dropped once it is off.
    edit(
        &env,
        &all_grants(),
        json!([
            {"op": "alter_field", "table": "documents", "field": "code", "unique": false},
            {"op": "drop_field", "table": "documents", "field": "code"},
        ]),
    )
    .await?;
    assert!(env.catalog.require("documents")?.field("code").is_none());
    Ok(())
}

#[tokio::test]
async fn several_fields_are_unique_together_until_the_constraint_is_dropped() -> Result<()> {
    let env = Env::new().await?;
    create_documents(&env).await?;
    let result = edit(
        &env,
        &edit_grants(),
        json!([{
            "op": "add_constraint", "table": "documents",
            "constraint": {
                "type": "unique", "fields": ["document_id", "version_no"],
                "error_message": "This version already exists",
            },
        }]),
    )
    .await?;
    let added = result["constraints_added"][0].as_str().unwrap().to_owned();
    let name = added.trim_start_matches("documents.").to_owned();

    let documents = describe(&env, "documents").await?;
    let listed = documents["constraints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("`{name}` not listed in {documents}"));
    assert_eq!(listed["fields"], json!(["document_id", "version_no"]));
    assert_eq!(
        listed["error_message"],
        json!("This version already exists")
    );
    // Part of a joint key is not unique by itself.
    assert_eq!(field(&documents, "document_id")["unique"], json!(false));

    env.execute(
        "INSERT INTO documents (code, document_id, version_no) VALUES ('a', 1, 1), ('b', 1, 2);",
    )
    .await?;
    assert!(
        env.execute("INSERT INTO documents (code, document_id, version_no) VALUES ('c', 1, 2);")
            .await
            .is_err()
    );

    let drop =
        json!([{"op": "drop_constraint", "table": "documents", "constraint": {"name": name}}]);
    let err = edit(&env, &edit_grants(), drop.clone())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("allow_drop"), "{err}");
    edit(&env, &all_grants(), drop).await?;
    env.execute("INSERT INTO documents (code, document_id, version_no) VALUES ('c', 1, 2);")
        .await?;
    Ok(())
}

#[tokio::test]
async fn allowed_values_are_set_listed_enforced_and_checked_against_the_rows() -> Result<()> {
    let env = Env::new().await?;
    edit(
        &env,
        &edit_grants(),
        json!([{
            "op": "create_table", "table": "tickets",
            "fields": [
                {"name": "id", "type": "int", "primary_key": true},
                {"name": "state", "type": "string",
                 "attributes": {"options": ["open", "in_progress", "closed"]}},
            ],
        }]),
    )
    .await?;
    let tickets = describe(&env, "tickets").await?;
    assert_eq!(
        field(&tickets, "state")["attributes"]["options"],
        json!(["open", "in_progress", "closed"]),
        "{tickets}"
    );

    // Enforced by the row layer, for every writer and not only the UI.
    let table = env.catalog.require("tickets")?;
    let catalog = &env.catalog;
    let table = &table;
    let write = |state: &str| {
        let row = json!({ "state": state });
        async move {
            sc_access::rows::create_row(catalog, table, &row, sc_catalog::Unchecked::system("test"))
                .await
        }
    };
    write("closed").await?;
    let err = write("bogus").await.unwrap_err().to_string();
    assert!(err.contains("must be one of"), "{err}");

    // Narrowing the list while a row holds a value outside it is refused.
    let err = edit(
        &env,
        &edit_grants(),
        json!([{"op": "alter_field", "table": "tickets", "field": "state",
                "attributes": {"options": ["open", "in_progress"]}}]),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("1 row has a value outside them"), "{err}");
    // Changing something else about the field leaves the options alone, and
    // so is not checked.
    edit(
        &env,
        &edit_grants(),
        json!([{"op": "alter_field", "table": "tickets", "field": "state", "label": "State"}]),
    )
    .await?;

    // Options on a type that has none are refused, naming the type that does.
    let err = edit(
        &env,
        &edit_grants(),
        json!([{"op": "add_field", "table": "tickets", "field": "priority", "type": "text",
                "attributes": {"options": ["low", "high"]}}]),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("use type `string`"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_optional_field_takes_a_default_and_a_write_with_no_user_leaves_it_null() -> Result<()> {
    let env = Env::new().await?;
    env.catalog.set_formula_evaluator(
        Arc::new(sc_expr::DenoEvaluator::new()) as Arc<dyn sc_expr::JsEvaluator>
    )?;
    edit(
        &env,
        &edit_grants(),
        json!([{
            "op": "create_table", "table": "notes",
            "fields": [
                {"name": "id", "type": "int", "primary_key": true},
                {"name": "title", "type": "text"},
                {"name": "created_by", "type": "text", "default": "user && user.email"},
                {"name": "kind", "type": "text", "default": "'note'"},
            ],
        }]),
    )
    .await?;
    let notes = describe(&env, "notes").await?;
    assert_eq!(field(&notes, "created_by")["required"], json!(false));
    assert_eq!(
        field(&notes, "created_by")["default"],
        json!("user && user.email")
    );

    // A trigger's or a scheduled job's write: no user.
    let table = env.catalog.require("notes")?;
    let row = sc_access::rows::create_row(
        &env.catalog,
        &table,
        &json!({ "title": "From a trigger" }),
        sc_catalog::Unchecked::system("test"),
    )
    .await?;
    assert_eq!(row["created_by"], json!(null));
    assert_eq!(row["kind"], json!("note"));
    Ok(())
}

#[tokio::test]
async fn a_setting_an_operation_does_not_use_is_refused_and_nothing_changes() -> Result<()> {
    let env = Env::new().await?;
    create_documents(&env).await?;
    let err = edit(
        &env,
        &all_grants(),
        json!([
            {"op": "add_field", "table": "documents", "field": "note", "type": "text"},
            {"op": "drop_field", "table": "documents", "field": "code", "unique": false},
        ]),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("operation 1"), "{err}");
    assert!(err.contains("`drop_field` does not use `unique`"), "{err}");
    // The batch is one transaction: the first operation did not happen either.
    assert!(env.catalog.require("documents")?.field("note").is_none());

    // A null is not a setting.
    edit(
        &env,
        &all_grants(),
        json!([{"op": "drop_field", "table": "documents", "field": "version_no", "unique": null}]),
    )
    .await?;
    Ok(())
}
