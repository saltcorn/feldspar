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

/// The paragraphs of a result's `notes` that restate a table's access rules.
fn access_notes(result: &Json) -> Vec<String> {
    result["notes"]
        .as_array()
        .map(|ns| {
            ns.iter()
                .filter_map(Json::as_str)
                .filter(|n| n.starts_with("Access to `"))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn access_rules_are_restated_in_words_on_a_dry_run_and_after_applying() -> Result<()> {
    let env = Env::new().await?;
    sc_auth::bootstrap_roles(&env.catalog).await?;
    sc_auth::save_role(
        &env.catalog,
        &sc_auth::RoleDef::new(sc_types::Role::of(80), "User"),
    )
    .await?;

    // The mistake the summary is for: "users see only their own notes" as a
    // floor every user meets plus a formula that therefore changes nothing.
    let create = json!([{
        "op": "create_table", "table": "notes",
        "min_role_read": 80, "min_role_write": 80,
        "ownership_formula": "owner === user.id",
        "fields": [
            {"name": "id", "type": "int", "primary_key": true},
            {"name": "owner", "type": "int"},
        ],
    }]);
    let dry = env
        .call_tool(
            TRAIT,
            &edit_grants(),
            "edit_schema",
            json!({ "operations": create, "dry_run": true }),
            &RunCaller::system(),
        )
        .await?;
    let said = access_notes(&dry);
    assert_eq!(said.len(), 1, "{dry}");
    assert!(
        said[0].contains("Admin (1) and User (80) can read every row"),
        "{}",
        said[0]
    );
    assert!(
        said[0].contains("`owner === user.id` changes nobody's reads or writes"),
        "{}",
        said[0]
    );
    assert!(
        env.catalog.get("notes")?.is_none(),
        "a dry run applies nothing"
    );

    let applied = edit(&env, &edit_grants(), create).await?;
    assert_eq!(access_notes(&applied), said, "{applied}");

    // Fixing it: the formula now decides for User and Public.
    let fixed = edit(
        &env,
        &edit_grants(),
        json!([{"op": "alter_table", "table": "notes", "min_role_read": 1, "min_role_write": 1}]),
    )
    .await?;
    let said = access_notes(&fixed);
    assert!(
        said[0].contains(
            "User (80) and Public (100) can read only the rows where `owner === user.id` holds"
        ),
        "{}",
        said[0]
    );
    assert!(!said[0].contains("changes nobody's"), "{}", said[0]);

    // A batch that leaves the access rules alone does not repeat them.
    let field = edit(
        &env,
        &edit_grants(),
        json!([{"op": "add_field", "table": "notes", "field": "body", "type": "text"}]),
    )
    .await?;
    assert!(access_notes(&field).is_empty(), "{field}");

    // And `describe_schema` says the same of the table as it stands.
    let notes = describe(&env, "notes").await?;
    assert_eq!(notes["access_summary"], json!(said[0]), "{notes}");
    Ok(())
}

/// A secret field from `edit_schema` (TODO SF1.1, from a user report): made on
/// `create_table` and `add_field`, read back by `describe_schema` as
/// `secret: true`, which `alter_field` accepts back, and a value written to it
/// stored sealed. A field cannot become a secret or stop being one, and a
/// secret is not also a reference.
#[tokio::test]
async fn a_secret_field_is_made_described_and_stored_sealed() -> Result<()> {
    let env = Env::new().await?;
    env.catalog
        .set_secret_key(sc_catalog::SecretKey::generate()?);
    edit(
        &env,
        &edit_grants(),
        json!([
            {
                "op": "create_table", "table": "mail_accounts",
                "fields": [
                    {"name": "id", "type": "int", "primary_key": true},
                    {"name": "login", "type": "text"},
                    {"name": "imap_password", "secret": true},
                ],
            },
            {"op": "add_field", "table": "mail_accounts", "field": "api_key", "secret": true},
        ]),
    )
    .await?;
    let table = env.catalog.require("mail_accounts")?;
    assert!(table.field("imap_password").unwrap().is_secret());
    assert!(table.field("api_key").unwrap().is_secret());
    assert!(!table.field("login").unwrap().is_secret());

    let described = describe(&env, "mail_accounts").await?;
    assert_eq!(
        field(&described, "api_key")["secret"],
        json!(true),
        "{described}"
    );
    assert!(
        field(&described, "login").get("secret").is_none(),
        "{described}"
    );

    // What `describe_schema` says, sent back, changes nothing about the kind.
    edit(
        &env,
        &edit_grants(),
        json!([{
            "op": "alter_field", "table": "mail_accounts", "field": "api_key",
            "secret": true, "label": "API key",
        }]),
    )
    .await?;
    assert!(
        env.catalog
            .require("mail_accounts")?
            .field("api_key")
            .unwrap()
            .is_secret()
    );

    // A value written through the row layer is sealed in its column. (An
    // agent's row tools leave secrets out.)
    let table = env.catalog.require("mail_accounts")?;
    sc_access::rows::create_row(
        &env.catalog,
        &table,
        &json!({ "id": 1, "login": "ada", "imap_password": "hunter2" }),
        sc_catalog::Unchecked::system("test"),
    )
    .await?;
    let rows = env.rows("mail_accounts").await?;
    let stored = rows[0]["imap_password"].as_str().unwrap().to_owned();
    assert!(
        stored.starts_with(sc_catalog::secrets::ENVELOPE_PREFIX),
        "{stored}"
    );
    assert_eq!(env.catalog.secret_key()?.open(&stored)?, "hunter2");

    for (operations, expected) in [
        (
            json!([{"op": "alter_field", "table": "mail_accounts", "field": "login", "secret": true}]),
            "cannot become a secret",
        ),
        (
            json!([{
                "op": "alter_field", "table": "mail_accounts", "field": "imap_password",
                "secret": false,
            }]),
            "cannot stop being a secret",
        ),
        (
            json!([{
                "op": "add_field", "table": "mail_accounts", "field": "owner",
                "secret": true, "references": "mail_accounts",
            }]),
            "not several",
        ),
        (
            json!([{
                "op": "add_field", "table": "mail_accounts", "field": "token",
                "secret": true, "unique": true,
            }]),
            "cannot be unique",
        ),
    ] {
        let err = edit(&env, &all_grants(), operations)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{err}");
    }
    let table = env.catalog.require("mail_accounts")?;
    assert!(!table.field("login").unwrap().is_secret());
    assert!(table.field("imap_password").unwrap().is_secret());
    assert!(table.field("token").is_none());
    Ok(())
}
