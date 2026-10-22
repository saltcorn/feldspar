//! A `Secret` field (TODO-imap M2) on SQLite, surface by surface.
//!
//! The column holds an envelope, never the plaintext. Every surface a client
//! reads — REST (list, `select`, a write's answer), GraphQL, a live change's
//! row, a CSV export, the generated client's row type — gives `null` or the
//! redaction sentinel, and the plaintext appears in none of their output.
//! Server-side code — a code body's `db` reads — sees the plaintext. A save
//! that sends the sentinel back keeps the stored value, and a secret can be no
//! key, no unique column, no filter and no ordering.

use std::sync::Arc;

use sc_api::code_host::TableHost;
use sc_api::schema_edit::{self, ApplyOptions, FieldSettings, FieldSpec, Operation};
use sc_api::{ApiProvider, ApiRequest, GraphqlProvider, Method, RestProvider, rows};
use sc_auth::User;
use sc_catalog::{
    Catalog, DataField, DataFieldKind, SecretKey, bootstrap_field_meta, bootstrap_table_meta,
};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_expr::CodeHost;
use sc_query::{Select, Source, Statement};
use sc_types::{BasicType, SECRET_SENTINEL, TypeRef};
use serde_json::{Value as Json, json};

const PLAINTEXT: &str = "Grüße-€-pässwort";

/// `accounts(id, label, password secret)` on an in-memory SQLite, the field
/// made a secret through the schema editor, and the catalog given a key of
/// its own (so no test writes a key file into the data directory).
async fn accounts() -> Result<Arc<Catalog>> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    catalog.set_secret_key(SecretKey::generate()?);
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog
        .create_table(
            "accounts",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(ColumnGenerator::Identity),
                DataField::plain("label", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    catalog.reload().await?;
    add_field(&catalog, secret("password")).await?;
    Ok(catalog)
}

fn secret(name: &str) -> FieldSpec {
    FieldSpec {
        name: name.to_owned(),
        kind: DataFieldKind::Secret,
        ..FieldSpec::default()
    }
}

async fn add_field(catalog: &Catalog, field: FieldSpec) -> Result<()> {
    schema_edit::apply(
        catalog,
        &[Operation::AddField {
            table: "accounts".to_owned(),
            field,
        }],
        &ApplyOptions::default(),
    )
    .await
    .map(|_| ())
}

fn admin() -> Result<User> {
    User::new(uuid::Uuid::new_v4(), sc_auth::ROLE_ADMIN)
}

/// What the column itself holds, read past the row layer.
async fn stored(catalog: &Catalog) -> Result<Vec<Json>> {
    let select = Select::from(Source::table("accounts"));
    let rows = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(rows.iter().map(rows::row_to_json).collect())
}

/// Assert `output` never carries the plaintext, anywhere in it.
fn never_plaintext(surface: &str, output: &Json) {
    assert!(
        !output.to_string().contains(PLAINTEXT),
        "{surface} carried the plaintext: {output}"
    );
}

#[tokio::test]
async fn the_column_holds_an_envelope_and_every_client_surface_the_sentinel() -> Result<()> {
    let catalog = accounts().await?;
    let table = catalog.require("accounts")?;
    assert!(table.field("password").unwrap().is_secret());
    let user = admin()?;
    let rest = RestProvider::project("/api", std::slice::from_ref(&table));

    // REST create: written, and answered with the sentinel.
    let created = rest
        .handle(
            ApiRequest::new(Method::Post, "/api/accounts")
                .body(json!({ "label": "orders@", "password": PLAINTEXT })),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.body["password"], SECRET_SENTINEL);
    never_plaintext("REST create", &created.body);
    let id = created.body["id"].to_string();

    // The column: an envelope that opens to the plaintext with this key.
    let raw = stored(&catalog).await?;
    let envelope = raw[0]["password"].as_str().unwrap().to_owned();
    assert!(sc_catalog::secrets::is_envelope(&envelope), "{envelope}");
    never_plaintext("the column", &json!(raw));
    assert_eq!(catalog.secret_key()?.open(&envelope)?, PLAINTEXT);

    // REST list, plain and with a `select`.
    for request in [
        ApiRequest::get("/api/accounts"),
        ApiRequest::get("/api/accounts").query("select", "id,password"),
    ] {
        let list = rest.handle(request, &catalog, Some(&user)).await?;
        assert_eq!(list.body[0]["password"], SECRET_SENTINEL, "{}", list.body);
        never_plaintext("REST list", &list.body);
    }

    // REST update: the sentinel sent back keeps the value…
    let kept = rest
        .handle(
            ApiRequest::new(Method::Put, format!("/api/accounts/{id}"))
                .body(json!({ "label": "orders", "password": SECRET_SENTINEL })),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(kept.status, 200, "{}", kept.body);
    assert_eq!(kept.body["password"], SECRET_SENTINEL);
    assert_eq!(stored(&catalog).await?[0]["password"], json!(envelope));
    // …and so does a save that sends nothing else.
    let untouched = rest
        .handle(
            ApiRequest::new(Method::Put, format!("/api/accounts/{id}"))
                .body(json!({ "password": SECRET_SENTINEL })),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(untouched.status, 200, "{}", untouched.body);
    assert_eq!(stored(&catalog).await?[0]["password"], json!(envelope));

    // GraphQL: the sentinel, through a query and a mutation's answer.
    let graphql = GraphqlProvider::project("/graphql", std::slice::from_ref(&table))?;
    let read = graphql
        .handle(
            ApiRequest::new(Method::Post, "/graphql")
                .body(json!({ "query": "{ accounts { id password } }" })),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(
        read.body["data"]["accounts"][0]["password"], SECRET_SENTINEL,
        "{}",
        read.body
    );
    never_plaintext("GraphQL query", &read.body);
    let inserted = graphql
        .handle(
            ApiRequest::new(Method::Post, "/graphql").body(json!({
                "query": format!(
                    "mutation {{ insert_accounts(object: {{ label: \"b\", password: \"{PLAINTEXT}\" }}) {{ password }} }}"
                )
            })),
            &catalog,
            Some(&user),
        )
        .await?;
    never_plaintext("GraphQL mutation", &inserted.body);
    assert_eq!(
        inserted.body["data"]["insert_accounts"]["password"], SECRET_SENTINEL,
        "{}",
        inserted.body
    );

    // A live change's row (`useLiveRows`) is held to the same projection.
    let written = rows::list_rows(&catalog, &table).await?;
    let live = sc_api::api_row(&table, &written[0]);
    assert_eq!(live["password"], SECRET_SENTINEL);
    never_plaintext("a live change", &live);

    // A CSV export, which the data editor offers.
    let csv = sc_api::csv::export_table(&catalog, &table, None).await?;
    assert!(
        csv.contains(SECRET_SENTINEL) && !csv.contains(PLAINTEXT),
        "{csv}"
    );

    // Writing a new value replaces it; null clears it, and reads as null.
    rows::update_row(&catalog, &table, &id, &json!({ "password": "new" })).await?;
    let replaced = stored(&catalog).await?[0]["password"].clone();
    assert_ne!(replaced, json!(envelope));
    rows::update_row(&catalog, &table, &id, &json!({ "password": null })).await?;
    let list = rest
        .handle(ApiRequest::get("/api/accounts"), &catalog, Some(&user))
        .await?;
    assert_eq!(list.body[0]["password"], Json::Null);
    Ok(())
}

#[tokio::test]
async fn server_side_code_reads_the_plaintext_and_a_view_does_not() -> Result<()> {
    let catalog = accounts().await?;
    let table = catalog.require("accounts")?;
    rows::create_row(
        &catalog,
        &table,
        &json!({ "label": "a", "password": PLAINTEXT }),
    )
    .await?;
    let plan = json!({ "op": "select", "table": "accounts", "select": ["label", "password"] });

    // A code body, a module action, a service: plaintext.
    let body = TableHost::new(&catalog).call(plan.clone()).await?;
    assert_eq!(body[0]["password"], PLAINTEXT, "{body}");
    // Its insert's answer too.
    let inserted = TableHost::new(&catalog)
        .call(json!({ "op": "insert", "table": "accounts", "values": { "label": "b", "password": "two" } }))
        .await?;
    assert_eq!(inserted["password"], "two", "{inserted}");

    // A view, rendered for somebody looking: the sentinel.
    let view = TableHost::new(&catalog)
        .caused_by(sc_auth::ROLE_ADMIN, None)
        .viewer_only()
        .call(plan)
        .await?;
    assert_eq!(view[0]["password"], SECRET_SENTINEL, "{view}");
    never_plaintext("a view", &view);

    // Server-side code still cannot filter or order on one: it would compare
    // ciphertext.
    for plan in [
        json!({ "op": "select", "table": "accounts", "where": { "password": PLAINTEXT } }),
        json!({ "op": "select", "table": "accounts", "order": [ { "field": "password" } ] }),
    ] {
        let e = TableHost::new(&catalog).call(plan).await.unwrap_err();
        assert!(e.to_string().contains("secret"), "{e}");
    }
    Ok(())
}

#[tokio::test]
async fn a_secret_is_no_key_no_constraint_no_filter_and_no_ordering() -> Result<()> {
    let catalog = accounts().await?;
    let refused = |e: sc_error::Error, what: &str| {
        let text = e.to_string();
        assert!(text.contains("secret"), "{what}: {text}");
    };
    // Created as a key, unique, typed, or defaulted: refused.
    for (what, spec) in [
        (
            "primary key",
            FieldSpec {
                primary_key: true,
                ..secret("k")
            },
        ),
        (
            "unique",
            FieldSpec {
                unique: true,
                ..secret("u")
            },
        ),
        (
            "typed",
            FieldSpec {
                type_name: "integer".into(),
                ..secret("t")
            },
        ),
        (
            "defaulted",
            FieldSpec {
                required: true,
                default_expression: Some("'x'".into()),
                ..secret("d")
            },
        ),
    ] {
        refused(add_field(&catalog, spec).await.unwrap_err(), what);
    }
    // A plain field cannot become a secret, nor a secret stop being one.
    let alter = |field: &str, kind: DataFieldKind| Operation::AlterField {
        table: "accounts".into(),
        field: field.into(),
        settings: FieldSettings {
            kind: Some(kind),
            ..FieldSettings::default()
        },
    };
    for op in [
        alter("label", DataFieldKind::Secret),
        alter("password", DataFieldKind::Plain),
    ] {
        let e = schema_edit::apply(&catalog, &[op], &ApplyOptions::default())
            .await
            .unwrap_err();
        refused(e, "a kind change");
    }
    // Indexed: refused.
    let index = Operation::AddConstraint {
        table: "accounts".into(),
        given_name: String::new(),
        constraint: sc_catalog::TableConstraint::new(
            "",
            sc_catalog::ConstraintKind::Unique {
                fields: vec!["password".into()],
            },
        ),
    };
    refused(
        schema_edit::apply(&catalog, &[index], &ApplyOptions::default())
            .await
            .unwrap_err(),
        "a unique constraint",
    );

    // Filtered or ordered on, by REST: refused.
    let table = catalog.require("accounts")?;
    let rest = RestProvider::project("/api", std::slice::from_ref(&table));
    let user = admin()?;
    for (key, value) in [("password", "eq.x"), ("order", "password")] {
        let request = ApiRequest::get("/api/accounts").query(key, value);
        match rest.handle(request, &catalog, Some(&user)).await {
            Ok(response) => {
                assert_eq!(response.status, 400, "{key}: {}", response.body);
                assert!(
                    response.body.to_string().contains("secret"),
                    "{}",
                    response.body
                );
            }
            Err(e) => refused(e, key),
        }
    }
    // A formula cannot name it: an ownership rule over it reads an unknown field.
    let fields: Vec<String> = table.fields.iter().map(|f| f.base.name.clone()).collect();
    refused(
        sc_api::filter::queryable_field(&table, &fields, "password", "where").unwrap_err(),
        "a GraphQL where",
    );
    let shape = catalog.schema_shape()?;
    assert!(
        sc_expr::Formula::parse("password !== null")?
            .validate(&shape, "accounts")
            .is_err(),
        "a formula naming a secret is refused"
    );
    Ok(())
}

#[tokio::test]
async fn the_generated_row_type_says_a_secret_is_write_only() -> Result<()> {
    let catalog = accounts().await?;
    let table = catalog.require("accounts")?;
    let provider = RestProvider::project("/api", &[table]);
    let client = sc_api::generate_client(ApiProvider::endpoints(&provider));
    let start = client
        .find("export interface AccountsRow {")
        .expect("a row type");
    let row_type = &client[start..start + client[start..].find("\n}").unwrap()];
    assert!(row_type.contains("password: string | null;"), "{row_type}");
    assert!(row_type.contains("write-only"), "{row_type}");
    Ok(())
}

#[tokio::test]
async fn a_server_with_secret_values_and_no_key_file_refuses_to_start() -> Result<()> {
    let catalog = accounts().await?;
    let dir = std::env::temp_dir().join(format!("sc-secret-start-{}", uuid::Uuid::new_v4()));
    let path = dir.join(sc_catalog::secrets::SECRET_KEY_FILE);
    // No values yet: starting without a key is fine (one is made when needed).
    sc_catalog::secrets::check_key_at(&catalog, &path).await?;

    let table = catalog.require("accounts")?;
    rows::create_row(&catalog, &table, &json!({ "password": PLAINTEXT })).await?;
    let message = sc_catalog::secrets::check_key_at(&catalog, &path)
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains("`accounts`"), "{message}");

    // With the file in place, it starts.
    SecretKey::load_or_create(&path)?;
    sc_catalog::secrets::check_key_at(&catalog, &path).await?;
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
