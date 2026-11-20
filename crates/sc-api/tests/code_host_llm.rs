//! `llm` in a code body: the configured providers, reached through the `db`
//! host as `op: "llm"` requests.
//!
//! Against an OpenAI-compatible stub that answers `/chat/completions` with a
//! streamed reply and `/embeddings` with one vector per text, so no test needs a
//! key: the body's `llm.generate` reaches the default provider's default model
//! with the system prompt and the turns it was given, `llm.embed` reaches its
//! embedding model, and a viewer-only host refuses both before anything is sent.
#![allow(
    clippy::disallowed_methods,
    reason = "test code: it drives its own servers, tasks and channels; the replacements are for production paths"
)]

use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use sc_api::code_host::TableHost;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_expr::{CodeCall, CodeHost, CodeRuntime};
use sc_llm::{LlmModelDef, LlmProviderDef, ModelKind};
use sc_test_harness::{Teardown, TestDb, TestServer};
use sc_types::Role;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the stub was asked: each request's path and JSON body.
type Seen = Arc<Mutex<Vec<(String, Json)>>>;

/// An OpenAI-compatible stub: `/chat/completions` streams `Hello` and
/// `/embeddings` answers `[len, 0]` per text. Returns its base URL.
async fn stub() -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let record = Arc::clone(&record);
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buf = [0_u8; 4096];
                let (head, body) = loop {
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
                        break (head.to_owned(), body.to_owned());
                    }
                };
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let request: Json = serde_json::from_str(&body).unwrap_or(Json::Null);
                record.lock().unwrap().push((path.clone(), request.clone()));
                let (content_type, reply) = if path.ends_with("/embeddings") {
                    let data: Vec<Json> = request["input"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let len = t.as_str().unwrap_or_default().len();
                            json!({ "object": "embedding", "index": i, "embedding": [len, 0] })
                        })
                        .collect();
                    (
                        "application/json",
                        json!({ "object": "list", "data": data,
                                "usage": { "prompt_tokens": 1000, "total_tokens": 1000 } })
                        .to_string(),
                    )
                } else {
                    let chunk = |content: &str| {
                        json!({
                            "id": "chatcmpl-1", "object": "chat.completion.chunk",
                            "created": 1_700_000_000u64, "model": "stub-chat",
                            "choices": [{ "index": 0, "delta": { "content": content },
                                          "finish_reason": null }],
                        })
                    };
                    // The usage OpenAI sends last, with no choices.
                    let usage = json!({
                        "id": "chatcmpl-1", "object": "chat.completion.chunk",
                        "created": 1_700_000_000u64, "model": "stub-chat", "choices": [],
                        "usage": { "prompt_tokens": 2000, "completion_tokens": 500,
                                   "total_tokens": 2500 },
                    });
                    (
                        "text/event-stream",
                        format!(
                            "data: {}\n\ndata: {}\n\ndata: {usage}\n\ndata: [DONE]\n\n",
                            chunk("Hel"),
                            chunk("lo")
                        ),
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
}

/// A catalog whose only provider, `stub`, points at the stub, with a default
/// chat model and a default embedding model.
async fn catalog(teardown: &Teardown) -> Result<(Arc<Catalog>, Seen)> {
    let driver = Arc::new(SqliteDriver::open_in_memory()?) as Arc<dyn DatabaseDriver>;
    let catalog = Arc::new(Catalog::init(driver).await?);
    teardown.add(catalog.teardown_hook());
    let seen = with_stub(&catalog, false).await?;
    Ok((catalog, seen))
}

/// The `stub` provider on `catalog`, its two default models priced when
/// `priced`: the chat model at 1 and 4 per million tokens, the embedding model
/// at 2.
async fn with_stub(catalog: &Catalog, priced: bool) -> Result<Seen> {
    sc_llm::bootstrap_llm_providers(catalog).await?;
    sc_llm::bootstrap_llm_models(catalog).await?;
    let (base, seen) = stub().await;
    let provider =
        LlmProviderDef::new("stub", sc_llm::OPENAI_CHAT_BACKEND).with(sc_llm::CFG_BASE_URL, base);
    sc_llm::save_llm_provider(catalog, &provider).await?;
    let mut chat = LlmModelDef::new(provider.id, "stub-chat").default_model();
    let mut embed = LlmModelDef::new(provider.id, "stub-embed")
        .kind(ModelKind::Embedding)
        .default_model();
    if priced {
        chat = chat
            .with(sc_llm::CFG_PRICE_INPUT, 1.0)
            .with(sc_llm::CFG_PRICE_OUTPUT, 4.0);
        embed = embed.with(sc_llm::CFG_PRICE_INPUT, 2.0);
    }
    sc_llm::save_llm_model(catalog, &chat).await?;
    sc_llm::save_llm_model(catalog, &embed).await?;
    Ok(seen)
}

fn body<'a>(code: &str, host: &'a dyn CodeHost) -> CodeCall<'a> {
    CodeCall {
        code: code.to_owned(),
        host: Some(host),
        ..CodeCall::default()
    }
}

#[tokio::test]
async fn a_body_generates_and_embeds_with_the_configured_provider() -> Result<()> {
    let teardown = Teardown::new();
    let (catalog, seen) = catalog(&teardown).await?;
    let host = TableHost::new(&catalog);
    let out = CodeRuntime::new()
        .run(body(
            r#"const full = await llm.generate({
                 system: "Be brief.",
                 messages: [{ role: "user", content: "Hi" },
                            { role: "assistant", content: "Hello?" }],
                 prompt: "Say hello", temperature: 0, detail: true,
               });
               return {
                 text: await llm.generate("Hi"),
                 full: { text: full.text, provider: full.provider, model: full.model },
                 one: await llm.embed("four"),
                 many: await llm.embed(["a", "bb"]),
               };"#,
            &host,
        ))
        .await?;
    assert_eq!(out["text"], json!("Hello"));
    assert_eq!(
        out["full"],
        json!({ "text": "Hello", "provider": "stub", "model": "stub-chat" })
    );
    assert_eq!(out["one"], json!([4, 0]));
    assert_eq!(out["many"], json!([[1, 0], [2, 0]]));

    let seen = seen.lock().unwrap().clone();
    let chats: Vec<&Json> = seen
        .iter()
        .filter(|(path, _)| path.ends_with("/chat/completions"))
        .map(|(_, body)| body)
        .collect();
    assert_eq!(chats.len(), 2, "{seen:?}");
    // The first call is the detailed one: the system prompt, then the turns.
    let messages = chats[0]["messages"].as_array().unwrap();
    assert_eq!(chats[0]["model"], json!("stub-chat"));
    assert_eq!(messages[0]["role"], json!("system"));
    assert!(
        messages[0]["content"].to_string().contains("Be brief."),
        "{messages:?}"
    );
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert!(
        messages[3]["content"].to_string().contains("Say hello"),
        "{messages:?}"
    );
    let embeds: Vec<&Json> = seen
        .iter()
        .filter(|(path, _)| path.ends_with("/embeddings"))
        .map(|(_, body)| body)
        .collect();
    assert_eq!(
        embeds[0],
        &json!({ "model": "stub-embed", "input": ["four"] })
    );
    Ok(())
}

async fn refused(host: &TableHost<'_>, request: Json) -> String {
    host.call(request).await.unwrap_err().to_string()
}

#[tokio::test]
async fn an_llm_request_is_refused_by_name_and_on_a_viewer_only_host() -> Result<()> {
    let teardown = Teardown::new();
    let (catalog, seen) = catalog(&teardown).await?;
    let host = TableHost::new(&catalog);
    let err = refused(
        &host,
        json!({ "op": "llm", "what": "generate", "provider": "nope",
                "messages": [{ "role": "user", "content": "x" }] }),
    )
    .await;
    assert!(err.contains("nope"), "{err}");
    let err = refused(
        &host,
        json!({ "op": "llm", "what": "generate", "model": "gpt-9",
                "messages": [{ "role": "user", "content": "x" }] }),
    )
    .await;
    assert!(err.contains("has no model named `gpt-9`"), "{err}");
    let err = refused(
        &host,
        json!({ "op": "llm", "what": "generate", "messages": [{ "role": "tool", "content": "x" }] }),
    )
    .await;
    assert!(err.contains("not `tool`"), "{err}");
    let err = refused(
        &host,
        json!({ "op": "llm", "what": "generate", "messages": [] }),
    )
    .await;
    assert!(err.contains("needs something to answer"), "{err}");
    let err = refused(&host, json!({ "op": "llm", "what": "chat" })).await;
    assert!(err.contains("it has generate and embed"), "{err}");

    // An agent's sandbox, a view's code: the viewer's authority is the ceiling,
    // and a model call is not theirs to make.
    let viewer = TableHost::new(&catalog).viewer_only();
    let err = refused(
        &viewer,
        json!({ "op": "llm", "what": "embed", "input": ["x"] }),
    )
    .await;
    assert!(err.contains("`llm` is not available here"), "{err}");
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the provider"
    );
    Ok(())
}

/// A charged call is charged to the run's caller at the rates it names, after
/// the call; an admin is not charged; nobody to charge, too little credit and a
/// rate per dollar on an unpriced model are refused before the model is
/// called.
#[tokio::test]
async fn a_charged_call_is_debited_from_the_callers_llm_credits() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = TestServer::builder().build_on(&db).await?;
    sc_auth::bootstrap(&catalog).await?;
    sc_auth::save_role(&catalog, &sc_auth::RoleDef::new(Role::of(80), "Member")).await?;
    let seen = with_stub(&catalog, true).await?;
    let user =
        sc_auth::create_user(&catalog, "ada@example.com", "a-long-password", Role::of(80)).await?;
    sc_auth::grant_credits(
        &catalog,
        sc_auth::NewCreditGrant {
            user_id: user.id(),
            amount: Decimal::from(10),
            source: "test".into(),
            key: "start".into(),
            kind: String::new(),
            expires_at: None,
            note: None,
            rollover: None,
        },
        sc_catalog::Authority::system("test"),
    )
    .await?;
    let as_ada = TableHost::new(&catalog).caused_by(Role::of(80), Some(user.fields_json()));
    let out = CodeRuntime::new()
        .run(body(
            r#"const paid = await llm.generate("Hi", {
                 detail: true, charge: { credits_per_dollar: 1000 },
               });
               await llm.embed(["a"], { charge: { credits_per_dollar: 1000 } });
               const free = await llm.generate("Hi", { detail: true });
               return { text: paid.text, credits: paid.credits, free: free.credits };"#,
            &as_ada,
        ))
        .await?;
    // 2000 in at 1 and 500 out at 4 per million is 0.004; × 1000 = 4 credits.
    assert_eq!(out["text"], json!("Hello"));
    assert_eq!(out["credits"], json!({ "charged": "4", "balance": "6" }));
    assert_eq!(out["free"], Json::Null, "no charge, no debit");
    // The embedding: 1000 tokens at 2 per million is 0.002; × 1000 = 2.
    let balance = sc_auth::credit_balance(&catalog, user.id()).await?;
    assert_eq!(balance.balance, Decimal::from(4));
    let entries = sc_auth::list_credit_entries(&catalog, user.id(), 10).await?;
    let debits: Vec<_> = entries.iter().filter(|e| e.kind == "debit").collect();
    assert_eq!(debits.len(), 2, "{entries:?}");
    assert!(
        debits
            .iter()
            .any(|e| e.model.as_deref() == Some("stub-embed")),
        "{debits:?}"
    );
    assert!(
        debits
            .iter()
            .all(|e| e.note.as_deref() == Some("llm in a code body")),
        "{debits:?}"
    );

    // An admin caller is not charged.
    let admin = TableHost::new(&catalog).caused_by(Role::ADMIN, Some(user.fields_json()));
    let out = CodeRuntime::new()
        .run(body(
            r#"return (await llm.generate("Hi", { detail: true,
                 charge: { credits_per_dollar: 1000 } })).credits;"#,
            &admin,
        ))
        .await?;
    assert_eq!(out, Json::Null);
    assert_eq!(
        sc_auth::credit_balance(&catalog, user.id()).await?.balance,
        Decimal::from(4)
    );

    let calls_before = seen.lock().unwrap().len();
    let charged = |rates: Json| {
        json!({ "op": "llm", "what": "generate", "charge": rates,
                "messages": [{ "role": "user", "content": "x" }] })
    };
    // Nobody to charge: a scheduled trigger's run.
    let nobody = TableHost::new(&catalog).caused_by(Role::PUBLIC, None);
    let err = refused(&nobody, charged(json!({ "credits_per_dollar": 1 }))).await;
    assert!(err.contains("needs a signed-in user"), "{err}");
    // Below the minimum the call names.
    let err = refused(
        &as_ada,
        charged(json!({ "credits_per_dollar": 1, "credits_min_balance": 5 })),
    )
    .await;
    assert!(
        err.contains("the balance is 4 and this call needs 5"),
        "{err}"
    );
    // A rate that is not one, and no rate at all.
    let err = refused(&as_ada, charged(json!({ "credits_per_doller": 1 }))).await;
    assert!(err.contains("`credits_per_doller` is not a rate"), "{err}");
    let err = refused(&as_ada, charged(json!({ "credits_min_balance": 1 }))).await;
    assert!(err.contains("needs a rate above zero"), "{err}");
    // A rate per dollar on a model with no prices cannot be charged.
    let stub = sc_llm::require_llm_provider(&catalog, "stub").await?;
    sc_llm::save_llm_model(&catalog, &LlmModelDef::new(stub.id, "stub-unpriced")).await?;
    let mut plan = charged(json!({ "credits_per_dollar": 1 }));
    plan["model"] = json!("stub-unpriced");
    let err = refused(&as_ada, plan).await;
    assert!(err.contains("has no prices"), "{err}");
    assert_eq!(
        seen.lock().unwrap().len(),
        calls_before,
        "no refused call reached the provider"
    );
    Ok(())
}
