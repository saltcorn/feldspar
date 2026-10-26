//! Vector fields on both backends (TODO-agent-skills AS2).
//!
//! The same contract against Postgres (pgvector) and SQLite (`f32` blobs):
//! a vector written through the row API reads back as the array it was, a
//! vector of the wrong length is refused on write, and a nearest read returns
//! the k rows nearest by cosine distance, nearest first. Then what text adds:
//! an `embed_from` field computed on insert and update through an
//! OpenAI-compatible stub, back-filled by `embed_rows`, and a nearest read
//! given text rather than a vector — through the row API, REST and GraphQL.

use std::sync::Arc;

use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation};
use sc_api::{query_string, rows, vector};
use sc_auth::ROLE_ADMIN;
use sc_catalog::{
    Catalog, ConstraintKind, DataField, TableConstraint, bootstrap_field_meta, bootstrap_table_meta,
};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A catalog over `driver` with `docs(id, title)` and the formula engine.
async fn catalog(driver: Arc<dyn DatabaseDriver>) -> Result<Arc<Catalog>> {
    let catalog = Arc::new(Catalog::init(driver).await?);
    catalog.set_formula_evaluator(Arc::new(DenoEvaluator::new()) as Arc<dyn JsEvaluator>)?;
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog
        .create_table(
            "docs",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    catalog.reload().await?;
    Ok(catalog)
}

async fn apply(catalog: &Catalog, op: Operation) -> Result<()> {
    schema_edit::apply(catalog, &[op], &ApplyOptions::default()).await?;
    Ok(())
}

/// Add the vector field `embedding` with `attributes`, created as the type
/// picker offers it: `vector`, with its size an attribute.
async fn add_vector(catalog: &Catalog, attributes: Json) -> Result<()> {
    let attributes: Attrs = serde_json::from_value(attributes).expect("an object");
    apply(
        catalog,
        Operation::AddField {
            table: "docs".into(),
            field: FieldSpec {
                name: "embedding".into(),
                type_name: "vector".into(),
                attributes,
                ..FieldSpec::default()
            },
        },
    )
    .await
}

/// The titles a `GET` with these query-string pairs answers, in order.
async fn titles(catalog: &Catalog, pairs: &[(&str, &str)]) -> Result<Vec<String>> {
    let table = catalog.require("docs")?;
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let query = query_string::row_query(&table, &pairs, 50)?;
    let found = sc_api::read_rows_as(catalog, &table, &query, ROLE_ADMIN, None, None).await?;
    Ok(title_list(&found))
}

fn title_list(body: &Json) -> Vec<String> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["title"].as_str().map(str::to_owned))
        .collect()
}

/// What both backends must agree on.
async fn the_common_contract(catalog: &Catalog) -> Result<()> {
    add_vector(catalog, json!({ "dimensions": 3 })).await?;
    let table = catalog.require("docs")?;
    assert_eq!(
        table.field("embedding").unwrap().base.type_,
        TypeRef::Basic(BasicType::Vector(3))
    );
    for (title, v) in [
        ("east", json!([1, 0, 0])),
        ("north", json!([0, 1, 0])),
        ("east by north", json!([0.9, 0.1, 0])),
        ("up", json!([0, 0, 1])),
        ("nowhere", Json::Null),
    ] {
        rows::create_row(catalog, &table, &json!({ "title": title, "embedding": v })).await?;
    }
    // Read back as the array that was written.
    let east = titles(catalog, &[("title", "eq.east")]).await?;
    assert_eq!(east, ["east"]);
    let all = rows::list_rows(catalog, &table).await?;
    let row = all
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["title"] == "east by north")
        .unwrap();
    assert_eq!(row["embedding"], json!([0.9, 0.1, 0.0]));

    // The wrong length is refused on write, saying both lengths.
    let err = rows::create_row(
        catalog,
        &table,
        &json!({ "title": "bad", "embedding": [1, 2] }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("2 dimensions") && err.contains("of 3"),
        "{err}"
    );

    // Nearest first, k of them.
    assert_eq!(
        titles(catalog, &[("nearest", "embedding.[1,0,0]"), ("limit", "2")]).await?,
        ["east", "east by north"]
    );
    // With a filter: nearest among what it leaves.
    assert_eq!(
        titles(
            catalog,
            &[
                ("nearest", "embedding.[1,0,0]"),
                ("title", "ne.east"),
                ("limit", "2")
            ]
        )
        .await?,
        ["east by north", "north"]
    );
    // A row with no vector is near nothing, so it is never one of the nearest.
    let every = titles(catalog, &[("nearest", "embedding.[0,0,1]")]).await?;
    assert_eq!(every.first().map(String::as_str), Some("up"));
    assert_eq!(every.len(), 4);
    assert!(!every.contains(&"nowhere".to_owned()));

    // The wrong length to be near, a field that is not a vector, and text with
    // no model to embed it with are each refused by name.
    let err = titles(catalog, &[("nearest", "embedding.[1,0]")])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("2 dimensions"), "{err}");
    let err = titles(catalog, &[("nearest", "title.[1,0,0]")])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not a vector field") && err.contains("`embedding`"),
        "{err}"
    );
    let err = titles(catalog, &[("nearest", "embedding.compass points")])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no embedding model"), "{err}");
    Ok(())
}

#[tokio::test]
async fn postgres_stores_pgvector_and_reads_nearest_first() -> Result<()> {
    let Some(db) = TestDb::with_pgvector().await? else {
        return Ok(());
    };
    let catalog = catalog(Arc::new(PgDriver::from_pool(db.pool().clone()))).await?;
    the_common_contract(&catalog).await?;

    // An HNSW index, read back as what it is, and the read still answers.
    apply(
        &catalog,
        Operation::AddConstraint {
            table: "docs".into(),
            given_name: String::new(),
            constraint: TableConstraint::new(
                "",
                ConstraintKind::VectorIndex {
                    field: "embedding".into(),
                },
            ),
        },
    )
    .await?;
    let table = catalog.require("docs")?;
    let index = sc_catalog::vector_index(&table, "embedding").expect("the vector index");
    assert_eq!(index.name, "sc_vx_docs_embedding");
    assert_eq!(
        titles(
            &catalog,
            &[("nearest", "embedding.[0,1,0]"), ("limit", "1")]
        )
        .await?,
        ["north"]
    );
    Ok(())
}

#[tokio::test]
async fn postgres_without_pgvector_refuses_a_vector_field_naming_the_extension() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(Arc::new(PgDriver::from_pool(db.pool().clone()))).await?;
    if catalog.primary().vector().is_available() {
        // This machine's default template has pgvector; nothing to refuse.
        return Ok(());
    }
    let err = add_vector(&catalog, json!({ "dimensions": 3 }))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cannot be a vector") && err.contains("pgvector"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_stores_blobs_and_reads_nearest_first() -> Result<()> {
    let catalog = catalog(Arc::new(SqliteDriver::open_in_memory()?)).await?;
    the_common_contract(&catalog).await?;
    // There is no vector index on SQLite, and the refusal says what happens
    // instead.
    let err = apply(
        &catalog,
        Operation::AddConstraint {
            table: "docs".into(),
            given_name: String::new(),
            constraint: TableConstraint::new(
                "",
                ConstraintKind::VectorIndex {
                    field: "embedding".into(),
                },
            ),
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("PostgreSQL with pgvector"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_field_needs_its_dimensions_and_a_model_to_embed_with() -> Result<()> {
    let catalog = catalog(Arc::new(SqliteDriver::open_in_memory()?)).await?;
    let err = add_vector(&catalog, json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs its number of dimensions"), "{err}");
    let err = add_vector(&catalog, json!({ "dimensions": 0 }))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("between 1 and 16000"), "{err}");
    let err = add_vector(&catalog, json!({ "dimensions": 3, "embed_from": "title" }))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs `embed_model`"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_scan_past_the_limit_is_refused_on_sqlite() -> Result<()> {
    let catalog = catalog(Arc::new(SqliteDriver::open_in_memory()?)).await?;
    add_vector(&catalog, json!({ "dimensions": 2 })).await?;
    let rows = sc_catalog::VECTOR_SCAN_LIMIT + 1;
    let fill = sc_query::Statement::raw(
        format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {rows}) \
             INSERT INTO docs (title) SELECT 'row' FROM n"
        ),
        Vec::new(),
    );
    catalog.primary().query(&fill).await?.try_collect().await?;
    let err = titles(&catalog, &[("nearest", "embedding.[1,0]")])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("more than 50000 rows"), "{err}");
    Ok(())
}

/// An OpenAI-compatible `/embeddings` stub: every text containing `fruit` is
/// `[1, 0, 0]`, `car` is `[0, 1, 0]`, and anything else `[0, 0, 1]`. Serves
/// every request until the test ends; returns its base URL.
async fn embeddings_stub() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buf = [0_u8; 4096];
                // The head, then as much body as it announces.
                loop {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let request: Json = serde_json::from_str(body).unwrap_or(Json::Null);
                let data: Vec<Json> = request["input"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let t = t.as_str().unwrap_or_default();
                        let v = if t.contains("fruit") {
                            json!([1, 0, 0])
                        } else if t.contains("car") {
                            json!([0, 1, 0])
                        } else {
                            json!([0, 0, 1])
                        };
                        json!({ "object": "embedding", "index": i, "embedding": v })
                    })
                    .collect();
                let reply = json!({ "object": "list", "data": data }).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{addr}/v1")
}

/// The catalog with a provider `stub` pointing at [`embeddings_stub`].
async fn with_provider(catalog: &Catalog) -> Result<()> {
    sc_llm::bootstrap_llm_providers(catalog).await?;
    let provider = sc_llm::LlmProviderDef::new("stub", sc_llm::OPENAI_CHAT_BACKEND)
        .with(sc_llm::CFG_BASE_URL, embeddings_stub().await);
    sc_llm::save_llm_provider(catalog, &provider).await
}

/// A field of three dimensions embedded from `title` by the stub provider.
fn embedded() -> Json {
    json!({
        "dimensions": 3,
        "embed_from": "title",
        "embed_provider": "stub",
        "embed_model": "text-embedding-3-small",
    })
}

#[tokio::test]
async fn an_embedded_field_is_computed_back_filled_and_searched_by_text() -> Result<()> {
    let catalog = catalog(Arc::new(SqliteDriver::open_in_memory()?)).await?;
    with_provider(&catalog).await?;
    let table = catalog.require("docs")?;
    // Rows from before the field: back-filled.
    for title in ["fruit salad", "car wash", ""] {
        rows::create_row(&catalog, &table, &json!({ "title": title })).await?;
    }
    add_vector(&catalog, embedded()).await?;
    let table = catalog.require("docs")?;
    let report = vector::embed_rows(&catalog, &table, "embedding", None).await?;
    assert_eq!(report.embedded, 2);
    // An empty title gives no text, and the vector stays empty.
    assert_eq!(report.skipped, 1);
    let again = vector::embed_rows(&catalog, &table, "embedding", None).await?;
    assert_eq!(again.embedded, 0, "only the empty ones are embedded");

    // Computed on insert…
    let red_car = rows::create_row(&catalog, &table, &json!({ "title": "red car" })).await?;
    assert_eq!(red_car["embedding"], json!([0.0, 1.0, 0.0]));
    // …and again on an update that changes what the formula reads.
    let id = red_car["id"].to_string();
    let updated =
        rows::update_row(&catalog, &table, &id, &json!({ "title": "fruit cake" })).await?;
    assert_eq!(updated["embedding"], json!([1.0, 0.0, 0.0]));

    // A nearest read given text embeds it with the field's model.
    let near = titles(
        &catalog,
        &[("nearest", "embedding.fresh fruit"), ("limit", "2")],
    )
    .await?;
    let mut near_sorted = near.clone();
    near_sorted.sort();
    assert_eq!(near_sorted, ["fruit cake", "fruit salad"]);

    // A field computed from a formula is written by that formula only when
    // the write leaves it out.
    let given = rows::create_row(
        &catalog,
        &table,
        &json!({ "title": "fruit", "embedding": [0, 0, 1] }),
    )
    .await?;
    assert_eq!(given["embedding"], json!([0.0, 0.0, 1.0]));

    Ok(())
}

#[tokio::test]
async fn rest_and_graphql_take_a_nearest() -> Result<()> {
    use sc_api::{ApiProvider, ApiRequest, GraphqlProvider, Method, RestProvider};

    let catalog = catalog(Arc::new(SqliteDriver::open_in_memory()?)).await?;
    with_provider(&catalog).await?;
    add_vector(&catalog, embedded()).await?;
    let table = catalog.require("docs")?;
    for title in ["fruit salad", "car wash", "weather"] {
        rows::create_row(&catalog, &table, &json!({ "title": title })).await?;
    }
    let user = sc_auth::User::new(uuid::Uuid::new_v4(), ROLE_ADMIN)?;

    let rest = RestProvider::project("/api", std::slice::from_ref(&table));
    let list = rest
        .handle(
            ApiRequest::get("/api/docs")
                .query("nearest", "embedding.[0,1,0]")
                .query("limit", "1"),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(list.status, 200, "{}", list.body);
    assert_eq!(title_list(&list.body), ["car wash"]);
    // The list endpoint declares it, for this table, so a client can send it.
    let endpoint = rest.endpoints().find("listDocs").expect("listDocs");
    assert!(endpoint.query.iter().any(|q| q.name == "nearest"));

    let graphql = GraphqlProvider::project("/graphql", std::slice::from_ref(&table))?;
    let read = graphql
        .handle(
            ApiRequest::new(Method::Post, "/graphql").body(json!({
                "query": r#"{ docs(nearest: { field: "embedding", to: "a fruit bowl" }, limit: 1) { title embedding } }"#
            })),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(
        title_list(&read.body["data"]["docs"]),
        ["fruit salad"],
        "{}",
        read.body
    );
    assert_eq!(
        read.body["data"]["docs"][0]["embedding"],
        json!([1.0, 0.0, 0.0])
    );
    Ok(())
}
