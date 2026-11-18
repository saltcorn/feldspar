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

use sc_api::code_host::TableHost;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_expr::{CodeCall, CodeHost, CodeRuntime};
use sc_llm::{LlmModelDef, LlmProviderDef, ModelKind};
use sc_test_harness::Teardown;
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
                        json!({ "object": "list", "data": data }).to_string(),
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
                    (
                        "text/event-stream",
                        format!(
                            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
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
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_llm::bootstrap_llm_models(&catalog).await?;
    let (base, seen) = stub().await;
    let provider =
        LlmProviderDef::new("stub", sc_llm::OPENAI_CHAT_BACKEND).with(sc_llm::CFG_BASE_URL, base);
    sc_llm::save_llm_provider(&catalog, &provider).await?;
    sc_llm::save_llm_model(
        &catalog,
        &LlmModelDef::new(provider.id, "stub-chat").default_model(),
    )
    .await?;
    sc_llm::save_llm_model(
        &catalog,
        &LlmModelDef::new(provider.id, "stub-embed")
            .kind(ModelKind::Embedding)
            .default_model(),
    )
    .await?;
    Ok((catalog, seen))
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
