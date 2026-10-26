//! Retrieval against a real database (TODO-agent-skills AS7).
//!
//! - `search_table` finds rows by full-text search, phrase by phrase, each row
//!   once; it searches hidden fields without showing them, formats rows as
//!   documents when told to, and in `every_message` mode offers no tool and
//!   puts what the user's message finds in the system prompt instead.
//! - `query_table` offers `search` only over a table with a full-text index,
//!   and with `show_results` shows the rows to the person and tells the model
//!   only how many there were.
//! - `similar_rows` orders by a vector field's distance to embedded text
//!   (through an OpenAI-compatible stub), optionally following a relation from
//!   chunks to documents.
//! - Every one of them reads **as the caller**: two users under an ownership
//!   formula get two answers, in either mode.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Env, as_user, config, titles};
use sc_agent::{DisplayPart, RunCaller, Turn};
use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation};
use sc_catalog::{ConstraintKind, TableConstraint};
use sc_core_traits::{
    CFG_DOC_FORMAT, CFG_FIELDS, CFG_HIDDEN_FIELDS, CFG_LIMIT, CFG_MODE, CFG_RELATION,
    CFG_RESULTS_VIEW, CFG_SHOW_RESULTS, CFG_TABLE, CFG_VECTOR_FIELD,
};
use sc_error::Result;
use sc_llm::LlmMessage;
use sc_types::Attrs;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What `on_turn` of `trait_` appends to the system prompt for a turn whose
/// latest user message is `message`, as `caller`.
async fn turn_text(
    env: &Env,
    trait_: &str,
    cfg: &Attrs,
    message: &str,
    caller: &RunCaller,
) -> Result<String> {
    let messages = vec![LlmMessage::User {
        content: message.to_owned(),
    }];
    let mut turn = Turn::new(caller, "librarian", 1);
    turn.catalog = Some(&env.catalog);
    turn.evaluator = env.evaluator.as_ref();
    turn.messages = &messages;
    env.registry
        .require(trait_)?
        .on_turn(cfg, &mut turn)
        .await?;
    Ok(turn.extra_context().join("\n\n"))
}

// --- search_table ------------------------------------------------------------

fn search_books(extra: &[(&str, Json)]) -> Attrs {
    let mut cfg = config(&[(CFG_TABLE, json!("books"))]);
    for (k, v) in extra {
        cfg.insert((*k).to_owned(), v.clone());
    }
    cfg
}

#[tokio::test]
async fn search_table_finds_rows_by_any_phrase_and_shows_only_what_it_may() -> Result<()> {
    let env = Env::new().await?;
    let cfg = search_books(&[(CFG_HIDDEN_FIELDS, json!(["notes"]))]);

    // The tool, derived from the table, taking a list of phrases.
    let tools = env.tools("search_table", &cfg);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "search_books");
    assert_eq!(tools[0].parameters["required"], json!(["phrases"]));
    assert!(
        !tools[0].description.contains("notes"),
        "{}",
        tools[0].description
    );

    let system = RunCaller::system();
    let found = env
        .call("search_table", &cfg, json!({"phrases": ["dune"]}), &system)
        .await?;
    assert_eq!(titles(&found), ["Dune"]);
    // Two phrases: a row matching either, each once.
    let found = env
        .call(
            "search_table",
            &cfg,
            json!({"phrases": ["dune", "emma", "\"dune\""]}),
            &system,
        )
        .await?;
    assert_eq!(titles(&found), ["Dune", "Emma"]);
    assert_eq!(found["count"], json!(2));

    // A hidden field is searched, but not shown.
    let found = env
        .call("search_table", &cfg, json!({"phrases": ["copy"]}), &system)
        .await?;
    let mut got = titles(&found);
    got.sort();
    assert_eq!(got, ["Dune", "Emma"]);
    for row in found["rows"].as_array().unwrap() {
        assert!(row.get("notes").is_none(), "{row}");
        assert!(row.get("title").is_some(), "{row}");
    }

    // Nothing found is said, not an empty list to puzzle over.
    let found = env
        .call(
            "search_table",
            &cfg,
            json!({"phrases": ["whales"]}),
            &system,
        )
        .await?;
    assert_eq!(found["count"], json!(0));
    assert!(found["message"].as_str().unwrap().contains("whales"));

    // An argument it does not take, and no phrase, are refused by name.
    let err = env
        .call("search_table", &cfg, json!({"phrase": "dune"}), &system)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("phrase"), "{err}");
    let err = env
        .call("search_table", &cfg, json!({"phrases": []}), &system)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("phrases"), "{err}");
    Ok(())
}

#[tokio::test]
async fn search_table_formats_documents_and_checks_its_configuration() -> Result<()> {
    let env = Env::new().await?;
    let cfg = search_books(&[(CFG_DOC_FORMAT, json!("{{title}}, {{pages}} pages"))]);
    env.check("search_table", &cfg).await?;
    let found = env
        .call(
            "search_table",
            &cfg,
            json!({"phrases": ["ilium"]}),
            &RunCaller::system(),
        )
        .await?;
    assert_eq!(found["documents"], json!(["Ilium, 576 pages"]));
    assert!(found.get("rows").is_none());

    // A template naming a field the model is not shown, a hidden field that
    // is not there, a mode that is not one, and a table with no text: each
    // refused on save, by name.
    for (cfg, needle) in [
        (
            search_books(&[
                (CFG_DOC_FORMAT, json!("{{title}}: {{notes}}")),
                (CFG_HIDDEN_FIELDS, json!("notes")),
            ]),
            "notes",
        ),
        (
            search_books(&[(CFG_HIDDEN_FIELDS, json!(["nope"]))]),
            "nope",
        ),
        (search_books(&[(CFG_MODE, json!("always"))]), "always"),
        (
            search_books(&[(CFG_FIELDS, json!(["id", "pages"]))]),
            "text field",
        ),
    ] {
        let err = env.check("search_table", &cfg).await.unwrap_err();
        assert!(err.to_string().contains(needle), "{needle}: {err}");
    }
    Ok(())
}

#[tokio::test]
async fn search_table_reads_as_the_caller_in_both_modes() -> Result<()> {
    let env = Env::new().await?;
    env.own("books", "owner === user.email").await?;
    let cfg = search_books(&[]);
    let ada = as_user("ada@example.com");
    let bob = as_user("bob@example.com");

    // `copy` is in Ada's note and in Bob's: each finds their own.
    let ask = json!({"phrases": ["copy"]});
    assert_eq!(
        titles(&env.call("search_table", &cfg, ask.clone(), &ada).await?),
        ["Dune"]
    );
    assert_eq!(
        titles(&env.call("search_table", &cfg, ask, &bob).await?),
        ["Emma"]
    );

    // Every message: no tool, and the user's own message is looked up — as
    // that user.
    let every = search_books(&[
        (CFG_MODE, json!("every_message")),
        (CFG_DOC_FORMAT, json!("Book: {{title}}")),
    ]);
    env.check("search_table", &every).await?;
    assert!(env.tools("search_table", &every).is_empty());
    let message = "What can you tell me about Dune or Emma?";
    let text = turn_text(&env, "search_table", &every, message, &ada).await?;
    assert!(text.contains("Book: Dune"), "{text}");
    assert!(!text.contains("Emma"), "{text}");
    let text = turn_text(&env, "search_table", &every, message, &bob).await?;
    assert!(text.contains("Book: Emma"), "{text}");
    assert!(!text.contains("Dune"), "{text}");
    // A message that finds nothing adds nothing.
    let text = turn_text(&env, "search_table", &every, "whales", &ada).await?;
    assert!(text.is_empty(), "{text}");

    // A tool-mode instance adds nothing to the turn.
    let text = turn_text(&env, "search_table", &cfg, message, &ada).await?;
    assert!(text.is_empty(), "{text}");
    Ok(())
}

// --- query_table: search and show_results ------------------------------------

/// Give `books` a full-text search constraint.
async fn index_books(env: &Env) -> Result<()> {
    schema_edit::apply(
        &env.catalog,
        &[Operation::AddConstraint {
            table: "books".into(),
            given_name: String::new(),
            constraint: TableConstraint::new(
                "",
                ConstraintKind::FullTextSearch {
                    language: "english".into(),
                },
            ),
        }],
        &ApplyOptions::default(),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn query_table_offers_search_only_over_an_indexed_table() -> Result<()> {
    let env = Env::new().await?;
    let cfg = config(&[(CFG_TABLE, json!("books"))]);
    let system = RunCaller::system();

    // No index: no `search` in the schema, and one guessed is refused saying why.
    let tools = env.tools("query_table", &cfg);
    assert!(tools[0].parameters["properties"].get("search").is_none());
    let err = env
        .call("query_table", &cfg, json!({"search": "dune"}), &system)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("full-text"), "{err}");

    index_books(&env).await?;
    let tools = env.tools("query_table", &cfg);
    assert!(tools[0].parameters["properties"].get("search").is_some());
    assert!(tools[0].description.contains("search"));
    let found = env
        .call("query_table", &cfg, json!({"search": "dune"}), &system)
        .await?;
    assert_eq!(titles(&found), ["Dune"]);
    // With a filter and an order of its own.
    let found = env
        .call(
            "query_table",
            &cfg,
            json!({"search": "copy", "order_by": "id", "descending": true}),
            &system,
        )
        .await?;
    assert_eq!(titles(&found), ["Emma", "Dune"]);
    let found = env
        .call(
            "query_table",
            &cfg,
            json!({"search": "copy", "where": {"pages": {"gt": 450}}}),
            &system,
        )
        .await?;
    assert_eq!(titles(&found), ["Emma"]);

    // An allow-list without the field that matched keeps it from matching.
    let narrow = config(&[
        (CFG_TABLE, json!("books")),
        (CFG_FIELDS, json!(["id", "title"])),
    ]);
    let found = env
        .call("query_table", &narrow, json!({"search": "copy"}), &system)
        .await?;
    assert_eq!(found["count"], json!(0));

    // The caller's rows only.
    env.own("books", "owner === user.email").await?;
    let found = env
        .call(
            "query_table",
            &cfg,
            json!({"search": "copy"}),
            &as_user("bob@example.com"),
        )
        .await?;
    assert_eq!(titles(&found), ["Emma"]);
    Ok(())
}

#[tokio::test]
async fn query_table_shows_results_to_the_person_and_summarises_them_for_the_model() -> Result<()> {
    let env = Env::new().await?;
    env.own("books", "owner === user.email").await?;
    let cfg = config(&[
        (CFG_TABLE, json!("books")),
        (CFG_FIELDS, json!(["id", "title"])),
        (CFG_SHOW_RESULTS, json!(true)),
        (CFG_RESULTS_VIEW, json!("BookList")),
    ]);
    env.check("query_table", &cfg).await?;
    let tools = env.tools("query_table", &cfg);
    assert!(
        tools[0].description.contains("do not repeat"),
        "{}",
        tools[0].description
    );

    let ada = as_user("ada@example.com");
    let mut state = Json::Null;
    let mut ctx = env.context(&ada, &mut state);
    let result = env
        .registry
        .require("query_table")?
        .call(&cfg, "query_books", &json!({"order_by": "id"}), &mut ctx)
        .await?;
    // The model gets a count and the keys, not the rows.
    assert!(result.get("rows").is_none(), "{result}");
    assert_eq!(result["count"], json!(2));
    assert_eq!(result["keys"], json!({"id": [1, 3]}));
    assert_eq!(result["shown_to_user"], json!(true));
    // The person gets the rows — Ada's only — through the named view.
    assert_eq!(ctx.extras.display.len(), 1);
    match &ctx.extras.display[0] {
        DisplayPart::Rows {
            view,
            columns,
            rows,
            ..
        } => {
            assert_eq!(view.as_deref(), Some("BookList"));
            assert_eq!(columns, &["id", "title"]);
            assert_eq!(
                rows.iter()
                    .map(|r| r["title"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["Dune", "Ilium"]
            );
        }
        other => panic!("not rows: {other:?}"),
    }

    // A malformed setting is refused on save.
    let bad = config(&[
        (CFG_TABLE, json!("books")),
        (CFG_SHOW_RESULTS, json!("yes")),
    ]);
    assert!(env.check("query_table", &bad).await.is_err());
    Ok(())
}

// --- similar_rows ------------------------------------------------------------

/// Documents and their chunks, the chunks carrying three-dimensional vectors.
const CHUNKS: &str = "
    CREATE TABLE docs (id bigint primary key, title text, owner text);
    CREATE TABLE chunks (
        id bigint primary key,
        doc bigint references docs(id),
        body text,
        owner text
    );
    INSERT INTO docs VALUES
        (1, 'Fruit facts', 'ada@example.com'),
        (2, 'Car manual',  'bob@example.com'),
        (3, 'Odds and ends', 'ada@example.com');
";

/// The chunks' vectors: the stub embeds text with `fruit` as `[1, 0, 0]` and
/// with `car` as `[0, 1, 0]`.
const VECTORS: [(i64, i64, &str, &str, [f32; 3]); 5] = [
    (1, 1, "apples are fruit", "ada@example.com", [1.0, 0.0, 0.0]),
    (2, 1, "pears too", "ada@example.com", [0.9, 0.1, 0.0]),
    (3, 2, "the engine", "bob@example.com", [0.0, 1.0, 0.0]),
    (4, 3, "string", "ada@example.com", [0.0, 0.0, 1.0]),
    (
        5,
        2,
        "fruit in the glovebox",
        "bob@example.com",
        [0.8, 0.2, 0.0],
    ),
];

/// An environment with pgvector, the chunks, and a provider `stub` that
/// embeds through [`embeddings_stub`]; `None` where this machine has no
/// pgvector template.
async fn chunks_env() -> Result<Option<Env>> {
    let Some(db) = sc_test_harness::TestDb::with_pgvector().await? else {
        return Ok(None);
    };
    let env = Env::with_db(db, CHUNKS).await?;
    sc_llm::save_llm_provider(
        &env.catalog,
        &sc_llm::LlmProviderDef::new("stub", sc_llm::OPENAI_CHAT_BACKEND)
            .with(sc_llm::CFG_BASE_URL, embeddings_stub().await),
    )
    .await?;
    let attributes: Attrs = serde_json::from_value(json!({
        "dimensions": 3,
        "embed_provider": "stub",
        "embed_model": "text-embedding-3-small",
    }))
    .unwrap();
    schema_edit::apply(
        &env.catalog,
        &[Operation::AddField {
            table: "chunks".into(),
            field: FieldSpec {
                name: "embedding".into(),
                type_name: "vector".into(),
                attributes,
                ..FieldSpec::default()
            },
        }],
        &ApplyOptions::default(),
    )
    .await?;
    let chunks = env.catalog.require("chunks")?;
    for (id, doc, body, owner, v) in VECTORS {
        sc_api::rows::create_row(
            &env.catalog,
            &chunks,
            &json!({"id": id, "doc": doc, "body": body, "owner": owner, "embedding": v}),
        )
        .await?;
    }
    Ok(Some(env))
}

fn similar(extra: &[(&str, Json)]) -> Attrs {
    let mut cfg = config(&[
        (CFG_TABLE, json!("chunks")),
        (CFG_VECTOR_FIELD, json!("embedding")),
    ]);
    for (k, v) in extra {
        cfg.insert((*k).to_owned(), v.clone());
    }
    cfg
}

fn bodies(result: &Json) -> Vec<String> {
    result["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| r["body"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn similar_rows_finds_the_nearest_rows_and_follows_a_relation() -> Result<()> {
    let Some(env) = chunks_env().await? else {
        return Ok(());
    };
    let system = RunCaller::system();

    // The vector rows themselves, nearest first, without their vectors.
    let cfg = similar(&[(CFG_LIMIT, json!(2))]);
    env.check("similar_rows", &cfg).await?;
    let tools = env.tools("similar_rows", &cfg);
    assert_eq!(tools[0].name, "search_similar_chunks");
    assert!(
        !tools[0].description.contains("embedding"),
        "{}",
        tools[0].description
    );
    let found = env
        .call(
            "similar_rows",
            &cfg,
            json!({"phrase_or_question": "tell me about fruit"}),
            &system,
        )
        .await?;
    assert_eq!(bodies(&found), ["apples are fruit", "pears too"]);
    assert!(found["rows"][0].get("embedding").is_none());

    // Through the relation: documents, each once, by their nearest chunk.
    let docs = similar(&[(CFG_RELATION, json!("doc")), (CFG_LIMIT, json!(2))]);
    env.check("similar_rows", &docs).await?;
    let found = env
        .call(
            "similar_rows",
            &docs,
            json!({"phrase_or_question": "fruit"}),
            &system,
        )
        .await?;
    assert_eq!(found["table"], json!("docs"));
    assert_eq!(titles(&found), ["Fruit facts", "Car manual"]);

    // A document format, over the documents' fields.
    let formatted = similar(&[
        (CFG_RELATION, json!("doc")),
        (CFG_LIMIT, json!(1)),
        (CFG_DOC_FORMAT, json!("Doc {{id}}: {{title}}")),
    ]);
    env.check("similar_rows", &formatted).await?;
    let found = env
        .call(
            "similar_rows",
            &formatted,
            json!({"phrase_or_question": "car"}),
            &system,
        )
        .await?;
    assert_eq!(found["documents"], json!(["Doc 2: Car manual"]));

    // What cannot work is refused on save, by name: a field that is not a
    // vector, a relation that is not a key, a template naming a chunk's field
    // over documents.
    for (cfg, needle) in [
        (
            similar(&[(CFG_VECTOR_FIELD, json!("body"))]),
            "not a vector field",
        ),
        (similar(&[(CFG_RELATION, json!("body"))]), "not a key field"),
        (
            similar(&[
                (CFG_RELATION, json!("doc")),
                (CFG_DOC_FORMAT, json!("{{body}}")),
            ]),
            "body",
        ),
    ] {
        let err = env.check("similar_rows", &cfg).await.unwrap_err();
        assert!(err.to_string().contains(needle), "{needle}: {err}");
    }
    Ok(())
}

#[tokio::test]
async fn similar_rows_reads_chunks_and_documents_as_the_caller() -> Result<()> {
    let Some(env) = chunks_env().await? else {
        return Ok(());
    };
    env.own("chunks", "owner === user.email").await?;
    env.own("docs", "owner === user.email").await?;
    let docs = similar(&[(CFG_RELATION, json!("doc")), (CFG_LIMIT, json!(2))]);
    let ask = json!({"phrase_or_question": "fruit"});

    // Ada's chunks lead to Ada's documents; Bob's nearest chunk is his own,
    // even though Ada's are nearer.
    let ada = as_user("ada@example.com");
    let bob = as_user("bob@example.com");
    let found = env.call("similar_rows", &docs, ask.clone(), &ada).await?;
    assert_eq!(titles(&found), ["Fruit facts", "Odds and ends"]);
    let found = env.call("similar_rows", &docs, ask, &bob).await?;
    assert_eq!(titles(&found), ["Car manual"]);

    // Every message: no tool, and the user's message is embedded and looked
    // up as that user.
    let every = similar(&[
        (CFG_RELATION, json!("doc")),
        (CFG_LIMIT, json!(1)),
        (CFG_MODE, json!("every_message")),
        (CFG_DOC_FORMAT, json!("Doc: {{title}}")),
    ]);
    env.check("similar_rows", &every).await?;
    assert!(env.tools("similar_rows", &every).is_empty());
    let text = turn_text(&env, "similar_rows", &every, "any fruit tips?", &ada).await?;
    assert!(text.contains("Doc: Fruit facts"), "{text}");
    let text = turn_text(&env, "similar_rows", &every, "any fruit tips?", &bob).await?;
    assert!(text.contains("Doc: Car manual"), "{text}");
    assert!(!text.contains("Fruit facts"), "{text}");
    Ok(())
}

/// An OpenAI-compatible `/embeddings` stub: text containing `fruit` is
/// `[1, 0, 0]`, `car` is `[0, 1, 0]`, anything else `[0, 0, 1]`. Returns its
/// base URL.
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
