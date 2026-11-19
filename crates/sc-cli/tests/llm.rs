//! `feldspar llm list|set-provider|set-model|set-default`: an LLM provider set
//! up from a terminal, driven through the **real binary** against a **real
//! Postgres**.
//!
//! The claims: what the commands save is what `sc_llm` reads (so an agent and a
//! code body's `llm.generate()` can use it), the key is read from the
//! environment and never printed, a second `set-provider` that names no key
//! keeps the stored one, and a key on the command line is refused.

#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]
use std::process::{Command, Stdio};

use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

const KEY: &str = "sk-test-not-a-real-key-123";

/// Run `feldspar <args…>` with `OPENAI_KEY` set, returning (success, stdout,
/// stderr).
fn feldspar(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .args(args)
        .env("OPENAI_KEY", KEY)
        .stdin(Stdio::null())
        .output()
        .expect("run the feldspar binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn a_provider_set_up_from_the_cli_is_the_one_sc_llm_reads() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let run = |args: &[&str]| {
        let mut all = vec!["llm"];
        all.extend_from_slice(args);
        all.extend_from_slice(&["--database-url", &url]);
        let (ok, stdout, stderr) = feldspar(&all);
        assert!(
            !stdout.contains(KEY) && !stderr.contains(KEY),
            "the key was printed: {stdout} {stderr}"
        );
        (ok, stdout, stderr)
    };

    let (ok, _, stderr) = run(&[
        "set-provider",
        "OpenAI",
        "--backend",
        "openai_responses",
        "--api-key-env",
        "OPENAI_KEY",
        "--description",
        "the vendor",
    ]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("added LLM provider `OpenAI`"), "{stderr}");
    assert!(stderr.contains("has no models yet"), "{stderr}");
    let (ok, _, stderr) = run(&["set-model", "OpenAI", "gpt-5-mini", "--default"]);
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = run(&[
        "set-model",
        "OpenAI",
        "text-embedding-3-small",
        "--kind",
        "embedding",
        "--default",
    ]);
    assert!(ok, "{stderr}");
    // A second provider, and then the first made the default.
    let (ok, _, stderr) = run(&[
        "set-provider",
        "Local",
        "--backend",
        "openai_chat",
        "--base-url",
        "http://127.0.0.1:11434/v1",
    ]);
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = run(&["set-default", "OpenAI"]);
    assert!(ok, "{stderr}");

    // An update that names no key keeps the stored one.
    let (ok, _, stderr) = run(&["set-provider", "OpenAI", "--description", "renamed"]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("updated LLM provider `OpenAI`"), "{stderr}");

    let (ok, stdout, stderr) = run(&["list", "--json"]);
    assert!(ok, "{stderr}");
    let listed: Json = serde_json::from_str(&stdout).expect("JSON");
    let openai = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == json!("OpenAI"))
        .expect("listed");
    assert_eq!(openai["has_api_key"], json!(true));
    assert_eq!(openai["is_default"], json!(true));
    assert_eq!(openai["description"], json!("renamed"));
    assert_eq!(
        openai["models"],
        json!([
            { "name": "gpt-5-mini", "kind": "chat", "is_default": true },
            { "name": "text-embedding-3-small", "kind": "embedding", "is_default": true },
        ])
    );

    // What sc-llm reads is what was saved: the key, the default provider, and
    // the default model of each kind — what `llm.generate()` resolves.
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    db.on_teardown(catalog.teardown_hook());
    let default = sc_llm::default_llm_provider(&catalog)
        .await?
        .expect("a default");
    assert_eq!(default.name, "OpenAI");
    assert_eq!(default.api_key().expose(), KEY);
    let chat = sc_llm::require_llm_model(&catalog, &default, None).await?;
    assert_eq!(chat.name, "gpt-5-mini");
    let (_, embed) =
        sc_llm::resolve_model(&catalog, None, None, sc_llm::ModelKind::Embedding).await?;
    assert_eq!(embed.name, "text-embedding-3-small");

    // A key on the command line is refused, and a backend cannot change.
    let (ok, _, stderr) = run(&["set-provider", "OpenAI", "--api-key", "sk-x"]);
    assert!(
        !ok && stderr.contains("does not go on the command line"),
        "{stderr}"
    );
    let (ok, _, stderr) = run(&["set-provider", "OpenAI", "--backend", "anthropic"]);
    assert!(
        !ok && stderr.contains("a backend is fixed once saved"),
        "{stderr}"
    );
    let (ok, _, stderr) = run(&["set-provider", "New"]);
    assert!(!ok && stderr.contains("needs `--backend`"), "{stderr}");
    Ok(())
}
