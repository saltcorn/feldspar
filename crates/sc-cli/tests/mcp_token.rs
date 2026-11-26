//! `feldspar mcp-token create|list|revoke` and `feldspar app list --json`.
//!
//! Driven through the **real binary** against a **real Postgres**: the claim is
//! that a token minted from a terminal is one the server's own authentication
//! accepts — with the grants the flags asked for — and that a revoke from a
//! terminal is one it then refuses; and that `app list --json` names the
//! directory a build would run in.

#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]
use sc_types::Role;
use std::process::{Command, Stdio};

use sc_app::{Application, FrameworkRef, save_application};
use sc_cli::{DbConfig, connect_catalog};
use sc_files::FileStoreDef;
use sc_test_harness::TestDb;
use serde_json::Value as Json;

const ADMIN: &str = "admin@example.com";

/// Run `feldspar <args…>`, returning (success, stdout, stderr).
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
async fn a_token_minted_from_the_cli_authenticates_until_it_is_revoked() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    sc_auth::create_user(&catalog, ADMIN, "a-long-password", Role::ADMIN).await?;
    sc_auth::create_user(
        &catalog,
        "staff@example.com",
        "a-long-password",
        Role::PUBLIC,
    )
    .await?;

    let (ok, stdout, stderr) = feldspar(&[
        "mcp-token",
        "create",
        "--label",
        "ci agent",
        "--email",
        ADMIN,
        "--expires-in-days",
        "30",
        "--allow-drop",
        "--no-allow-triggers",
        "--database-url",
        &url,
    ]);
    assert!(ok, "{stderr}");
    // stdout is the secret and nothing else, so it can be captured.
    let secret = stdout.trim();
    assert!(secret.starts_with(sc_auth::TOKEN_PREFIX), "{stdout}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(
        stderr.contains("claude mcp add --transport http feldspar"),
        "{stderr}"
    );
    assert!(stderr.contains(secret), "{stderr}");
    // The server is off by default, and the command says how to turn it on.
    assert!(stderr.contains("set-cfg mcp_enabled true"), "{stderr}");

    // The server's own authentication accepts it, as the admin, with the
    // grants the flags asked for and the defaults for the rest.
    let caller = sc_auth::authenticate_api_token(&catalog, secret).await?;
    assert_eq!(
        caller
            .user
            .extra()
            .get(sc_auth::COL_EMAIL)
            .and_then(|e| e.as_text()),
        Some(ADMIN)
    );
    let grants = &caller.token.grants;
    assert_eq!(grants["allow_drop"], Json::Bool(true));
    assert_eq!(grants["allow_triggers"], Json::Bool(false));
    assert_eq!(grants["allow_create"], Json::Bool(true));
    assert!(
        grants.get("allow_access_changes").is_none(),
        "a retired grant"
    );
    assert_eq!(grants["allow_modules"], Json::Bool(true));
    assert!(caller.token.expires_at.is_some());

    // It is listed, by id, without the secret.
    let (ok, listing, stderr) = feldspar(&["mcp-token", "list", "--json", "--database-url", &url]);
    assert!(ok, "{stderr}");
    assert!(!listing.contains(secret));
    let listed: Json = serde_json::from_str(&listing).expect("list --json is JSON");
    assert_eq!(listed[0]["label"], "ci agent");
    assert_eq!(listed[0]["state"], "live");
    let id = listed[0]["id"].as_str().expect("an id").to_owned();
    assert_eq!(id, caller.token.id.to_string());

    // Revoked, it stops working, and stays listed as revoked.
    let (ok, _, stderr) = feldspar(&["mcp-token", "revoke", &id, "--database-url", &url]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("revoked"), "{stderr}");
    assert!(
        sc_auth::authenticate_api_token(&catalog, secret)
            .await
            .is_err()
    );
    let (_, listing, _) = feldspar(&["mcp-token", "list", "--json", "--database-url", &url]);
    let listed: Json = serde_json::from_str(&listing).expect("list --json is JSON");
    assert_eq!(listed[0]["state"], "revoked");

    // A non-admin is refused before anything is minted.
    let (ok, _, stderr) = feldspar(&[
        "mcp-token",
        "create",
        "--label",
        "nope",
        "--email",
        "staff@example.com",
        "--database-url",
        &url,
    ]);
    assert!(!ok);
    assert!(stderr.contains("not an administrator"), "{stderr}");
    assert_eq!(sc_auth::list_api_tokens(&catalog).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn app_list_json_gives_each_applications_project_directory() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;

    let root = std::env::temp_dir().join(format!("sc-cli-app-list-{}", std::process::id()));
    std::fs::create_dir_all(root.join("web"))?;
    sc_catalog::save_file_store(
        &catalog,
        &FileStoreDef::local("apps", root.display().to_string()),
    )
    .await?;
    let framework = FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh");
    save_application(&catalog, &Application::new("Blog", "blog", framework)).await?;

    let (ok, stdout, stderr) = feldspar(&["app", "list", "--json", "--database-url", &url]);
    std::fs::remove_dir_all(&root).ok();
    assert!(ok, "{stderr}");
    let apps: Json = serde_json::from_str(&stdout).expect("app list --json is JSON alone");
    let blog = &apps[0];
    assert_eq!(blog["subdomain"], "blog");
    assert_eq!(blog["framework"], "code");
    assert_eq!(blog["file_store"], "apps");
    assert_eq!(blog["source_dir"], "web");
    let dir = blog["project_dir"].as_str().expect("a project directory");
    assert_eq!(
        std::path::Path::new(dir).canonicalize().ok(),
        root.join("web").canonicalize().ok(),
        "{blog}"
    );
    Ok(())
}

/// An application on a framework a **module** declares gets the directory too:
/// the command loads the modules before resolving it, as the server does. Until
/// it did, the list said "framework `astro` has no build step" about a framework
/// the running server builds, because the CLI's framework registry held only the
/// built-ins.
///
/// The module is `sc-module`'s `framework-module` fixture (its `toy` framework
/// is built from `{{ store }}/{{ project }}`), installed through
/// `feldspar module install --local` into a modules root under a throwaway
/// `XDG_DATA_HOME`. That is an `npm install` of a directory with no
/// dependencies, so it reaches no network but needs npm, and skips without it.
#[tokio::test]
async fn app_list_json_resolves_a_framework_a_module_declares() -> sc_error::Result<()> {
    let have_npm = Command::new("npm")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !have_npm {
        eprintln!("skipping: npm is not available");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;

    let root = std::env::temp_dir().join(format!("sc-cli-app-list-module-{}", std::process::id()));
    let store = root.join("store");
    std::fs::create_dir_all(store.join("todo"))?;
    let xdg = root.join("xdg");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sc-module/tests/fixtures/framework-module");
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_feldspar"))
            .args(args)
            .env("XDG_DATA_HOME", &xdg)
            .stdin(Stdio::null())
            .output()
            .expect("run the feldspar binary");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let fixture = fixture.display().to_string();
    let (ok, _, stderr) = run(&[
        "module",
        "install",
        "--local",
        &fixture,
        "--database-url",
        &url,
    ]);
    assert!(ok, "{stderr}");

    sc_catalog::save_file_store(
        &catalog,
        &FileStoreDef::local("apps", store.display().to_string()),
    )
    .await?;
    // Saved as `code`, then switched to `toy` in the row: `save_application`
    // validates against this test process's registry, which has no modules
    // loaded. The stored value is the one the admin form writes.
    let framework = FrameworkRef::new("code")
        .with("store", "apps")
        .with("output", "dist")
        .with("command", "true");
    save_application(&catalog, &Application::new("Todo", "todo", framework)).await?;
    db.execute(
        r#"UPDATE _fd_applications SET framework = '{"name": "toy", "config": {"store": "apps", "project": "todo"}}' WHERE subdomain = 'todo'"#,
    )
    .await?;

    let (ok, stdout, stderr) = run(&["app", "list", "--json", "--database-url", &url]);
    std::fs::remove_dir_all(&root).ok();
    assert!(ok, "{stderr}");
    let apps: Json = serde_json::from_str(&stdout).expect("app list --json is JSON alone");
    let todo = &apps[0];
    assert_eq!(todo["framework"], "toy", "{todo}");
    assert_eq!(todo["file_store"], "apps", "{todo}");
    assert_eq!(todo["source_dir"], "todo", "{todo}");
    assert!(todo.get("error").is_none(), "{todo}");
    assert_eq!(
        todo["project_dir"].as_str().map(std::path::Path::new),
        Some(store.join("todo").as_path()),
        "{todo}"
    );
    Ok(())
}
