//! `feldspar api-token create|list|revoke` (TODO AT1), through the **real
//! binary** against a **real Postgres**: a token minted from a terminal is one
//! the application's authentication accepts, as the user it names, and a
//! revoke from a terminal is one it then refuses. The two kinds of token do not
//! revoke each other.

#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the binary a test drives directly"
)]
use sc_types::Role;
use std::process::{Command, Stdio};

use sc_app::{API_TOKENS_ATTRIBUTE, Application, FrameworkRef, save_application};
use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;
use serde_json::Value as Json;

const MEMBER: &str = "member@example.com";

fn feldspar(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .args(args)
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
async fn a_token_minted_for_a_user_calls_their_application_until_revoked() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    db.on_teardown(catalog.teardown_hook());
    sc_auth::create_user(
        &catalog,
        "admin@example.com",
        "a-long-password",
        Role::ADMIN,
    )
    .await?;
    sc_auth::save_role(&catalog, &sc_auth::RoleDef::new(Role::of(80), "Member")).await?;
    sc_auth::create_user(&catalog, MEMBER, "a-long-password", Role::of(80)).await?;
    let framework = FrameworkRef::new("none")
        .with("store", "site")
        .with("source", "crm");
    let off =
        save_application(&catalog, &Application::new("CRM", "crm", framework.clone())).await?;

    // Minted for a member, on an application that does not take tokens yet:
    // the command says so.
    let (ok, stdout, stderr) = feldspar(&[
        "api-token",
        "create",
        "--app",
        "crm",
        "--label",
        "nightly export",
        "--email",
        MEMBER,
        "--expires-in-days",
        "30",
        "--database-url",
        &url,
    ]);
    assert!(ok, "{stderr}");
    let secret = stdout.trim().to_owned();
    assert!(
        secret.starts_with(sc_auth::APP_API_TOKEN_PREFIX),
        "{stdout}"
    );
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(
        stderr.contains("curl -H \"Authorization: Bearer"),
        "{stderr}"
    );
    assert!(
        stderr.contains("does not accept API tokens yet"),
        "{stderr}"
    );

    // The application's authentication accepts it, as the member.
    let caller = sc_auth::authenticate_app_token(&catalog, off.id.0, &secret).await?;
    assert_eq!(caller.user.role(), Role::of(80));
    assert!(caller.token.expires_at.is_some());
    // …and the MCP server does not.
    assert!(
        sc_auth::authenticate_api_token(&catalog, &secret)
            .await
            .is_err()
    );

    // Turned on, the warning goes.
    let mut on = off.clone();
    on.attributes
        .insert(API_TOKENS_ATTRIBUTE.to_owned(), Json::Bool(true));
    save_application(&catalog, &on).await?;
    let (ok, _, stderr) = feldspar(&[
        "api-token",
        "create",
        "--app",
        "crm",
        "--label",
        "second",
        "--email",
        MEMBER,
        "--database-url",
        &url,
    ]);
    assert!(ok, "{stderr}");
    assert!(!stderr.contains("does not accept"), "{stderr}");

    // Listed, with whose it is, never the secret; and not among the MCP tokens.
    let (ok, listing, stderr) = feldspar(&["api-token", "list", "--json", "--database-url", &url]);
    assert!(ok, "{stderr}");
    assert!(!listing.contains(&secret));
    let listed: Json = serde_json::from_str(&listing).expect("list --json is JSON");
    assert_eq!(listed.as_array().map(Vec::len), Some(2), "{listing}");
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["app"] == "crm" && t["email"] == MEMBER)
    );
    let (_, mcp, _) = feldspar(&["mcp-token", "list", "--json", "--database-url", &url]);
    assert_eq!(
        serde_json::from_str::<Json>(&mcp).unwrap(),
        Json::Array(vec![])
    );

    let id = caller.token.id.to_string();
    // `mcp-token revoke` will not take an application's token.
    let (ok, _, stderr) = feldspar(&["mcp-token", "revoke", &id, "--database-url", &url]);
    assert!(!ok);
    assert!(stderr.contains("api-token revoke"), "{stderr}");

    let (ok, _, stderr) = feldspar(&["api-token", "revoke", &id, "--database-url", &url]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("revoked"), "{stderr}");
    let err = sc_auth::authenticate_app_token(&catalog, off.id.0, &secret)
        .await
        .expect_err("revoked");
    assert!(err.to_string().contains("revoked"), "{err}");

    // An unknown application is named.
    let (ok, _, stderr) = feldspar(&[
        "api-token",
        "create",
        "--app",
        "nope",
        "--label",
        "x",
        "--admin",
        "--database-url",
        &url,
    ]);
    assert!(!ok);
    assert!(
        stderr.contains("no application with the subdomain `nope`"),
        "{stderr}"
    );
    Ok(())
}
