//! The commands a hosting daemon sets up a server with, without the admin UI:
//! `module …`, `mcp-token create --routes-only`, `set-cfg --json` / `--unset`,
//! `get-cfg --json`, `send-email` and `app tables`.
//!
//! Driven through the **real binary** against a **real Postgres**: what each
//! claims is about the rows another process — the running server — reads.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sc_app::{Application, FrameworkRef, save_application};
use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::{TestDb, TestSmtp};
use serde_json::{Value as Json, json};

/// Run `feldspar <args…>` with `stdin`, returning (success, stdout, stderr).
fn feldspar(args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the feldspar binary");
    {
        let mut pipe = child.stdin.take().expect("stdin");
        if let Some(text) = stdin {
            pipe.write_all(text.as_bytes()).expect("write stdin");
        }
    }
    let out = child.wait_with_output().expect("wait for feldspar");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn have_npm() -> bool {
    Command::new("npm")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("sc-cli-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A v1-shaped module with one setting and one route, at `version`.
fn write_module(dir: &Path, version: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        json!({
            "name": "cli-health",
            "version": version,
            "main": "index.js",
            "private": true,
            "license": "MIT",
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("index.js"),
        r#"module.exports = {
  sc_plugin_api_version: 1,
  configuration_workflow: () => ({
    steps: [{ name: "Access", form: { fields: [
      { name: "min_role", label: "Minimum role", type: "String" },
    ] } }],
  }),
  routes: (cfg) => [
    { url: "/heartbeat", callback: (req, res) => res.json({ ok: true, min_role: cfg.min_role || null }) },
  ],
};
"#,
    )
    .unwrap();
}

/// `module list --json`'s entry for `cli-health`, if there is one.
fn listed(url: &str, modules_dir: &str) -> Option<Json> {
    let (ok, stdout, stderr) = feldspar(
        &[
            "module",
            "list",
            "--json",
            "--modules-dir",
            modules_dir,
            "--database-url",
            url,
        ],
        None,
    );
    assert!(ok, "{stderr}");
    let list: Json = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    list.as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == json!("cli-health"))
        .cloned()
}

#[tokio::test]
async fn a_module_is_installed_granted_configured_and_removed_from_the_cli() -> sc_error::Result<()>
{
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let url = db.url();
    connect_catalog(&DbConfig::from_url(&url)).await?;
    let scratch = TempDir::new("module");
    let package = scratch.path().join("cli-health");
    let modules_dir = scratch.path().join("modules");
    let modules_dir = modules_dir.to_str().unwrap();
    write_module(&package, "0.1.0");
    let package = package.to_str().unwrap();
    let module = |args: &[&str]| {
        let mut all = vec!["module"];
        all.extend_from_slice(args);
        all.extend_from_slice(&["--modules-dir", modules_dir, "--database-url", &url]);
        feldspar(&all, None)
    };

    // Installed, granted and configured in one run, and loaded.
    let (ok, _, stderr) = module(&[
        "install",
        "--local",
        package,
        "--grant-read",
        "/run/feldspar-health",
        "--setting",
        "min_role=staff",
    ]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("installed `cli-health` 0.1.0"), "{stderr}");
    let entry = listed(&url, modules_dir).expect("listed after the install");
    assert_eq!(entry["version"], json!("0.1.0"));
    assert_eq!(entry["loaded"], json!(true), "{entry}");
    assert_eq!(
        entry["permissions"]["read"],
        json!(["/run/feldspar-health"])
    );
    assert_eq!(entry["configuration"]["min_role"], json!("staff"));
    assert_eq!(
        entry["routes"],
        json!(["/_feldspar/routes/cli-health/heartbeat"])
    );
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    let installed_at = |row: &sc_module::Module| row.attributes.get("installed_at").cloned();
    let first = sc_module::load_module_by_name(&catalog, "cli-health")
        .await?
        .unwrap();

    // The same version again: idempotent — npm is not run and the row is
    // untouched, so a running server's SIGHUP has nothing to reload.
    let (ok, _, stderr) = module(&[
        "install",
        "--local",
        package,
        "--grant-read",
        "/run/feldspar-health",
    ]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("nothing changed"), "{stderr}");
    let again = sc_module::load_module_by_name(&catalog, "cli-health")
        .await?
        .unwrap();
    assert_eq!(installed_at(&again), installed_at(&first));

    // A newer version is installed over it, keeping the id, the grant and the
    // setting.
    write_module(Path::new(package), "0.2.0");
    let (ok, _, stderr) = module(&["install", "--local", package]);
    assert!(ok, "{stderr}");
    let entry = listed(&url, modules_dir).unwrap();
    assert_eq!(entry["version"], json!("0.2.0"));
    assert_eq!(entry["id"], json!(first.id.0));
    assert_eq!(
        entry["permissions"]["read"],
        json!(["/run/feldspar-health"])
    );
    assert_eq!(entry["configuration"]["min_role"], json!("staff"));

    // Grants: added once, unchanged the second time, revoked; a relative path
    // refused as the Modules tab refuses it.
    let (ok, _, stderr) = module(&["grant", "cli-health", "--net", "api.example.com"]);
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = module(&["grant", "cli-health", "--net", "api.example.com"]);
    assert!(ok && stderr.contains("unchanged"), "{stderr}");
    assert_eq!(
        listed(&url, modules_dir).unwrap()["permissions"]["net"],
        json!(["api.example.com"])
    );
    let (ok, _, stderr) = module(&[
        "grant",
        "cli-health",
        "--read",
        "/run/feldspar-health",
        "--revoke",
    ]);
    assert!(ok, "{stderr}");
    assert_eq!(
        listed(&url, modules_dir).unwrap()["permissions"]["read"],
        json!([])
    );
    let (ok, _, _) = module(&["grant", "cli-health", "--read", "relative/path"]);
    assert!(!ok);

    // A setting.
    let (ok, _, stderr) = module(&["configure", "cli-health", "min_role=admin"]);
    assert!(ok, "{stderr}");
    assert_eq!(
        listed(&url, modules_dir).unwrap()["configuration"]["min_role"],
        json!("admin")
    );

    // A module that is not installed is named.
    let (ok, _, stderr) = module(&["configure", "no-such-module", "a=b"]);
    assert!(!ok && stderr.contains("no-such-module"), "{stderr}");

    // Removed — the row and the package — and removing it again is no error.
    let (ok, _, stderr) = module(&["remove", "cli-health"]);
    assert!(ok, "{stderr}");
    assert!(listed(&url, modules_dir).is_none());
    assert!(
        !Path::new(modules_dir)
            .join("node_modules/cli-health/package.json")
            .exists()
    );
    let (ok, _, stderr) = module(&["remove", "cli-health"]);
    assert!(ok && stderr.contains("nothing to remove"), "{stderr}");
    Ok(())
}

#[tokio::test]
async fn a_routes_only_token_reaches_module_routes_as_a_non_admin() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    sc_auth::create_user(&catalog, "admin@example.com", "a-long-password", 1).await?;
    sc_auth::save_role(&catalog, &sc_auth::Role::new(80, "Monitor")).await?;
    sc_auth::create_user(&catalog, "monitor@example.com", "a-long-password", 80).await?;

    // An ordinary token for a non-admin is refused at the mint, as before…
    let (ok, _, stderr) = feldspar(
        &[
            "mcp-token",
            "create",
            "--label",
            "monitors",
            "--email",
            "monitor@example.com",
            "--database-url",
            &url,
        ],
        None,
    );
    assert!(!ok, "{stderr}");

    // …and a routes-only one is minted.
    let (ok, stdout, stderr) = feldspar(
        &[
            "mcp-token",
            "create",
            "--label",
            "monitors",
            "--email",
            "monitor@example.com",
            "--routes-only",
            "--database-url",
            &url,
        ],
        None,
    );
    assert!(ok, "{stderr}");
    let secret = stdout.trim();
    assert!(secret.starts_with(sc_auth::TOKEN_PREFIX), "{stdout}");
    assert!(stderr.contains("/_feldspar/routes"), "{stderr}");
    assert!(!stderr.contains("claude mcp add"), "{stderr}");

    // A module route accepts it, as the role-80 user; the MCP server does not.
    let caller = sc_auth::authenticate_route_token(&catalog, secret).await?;
    assert_eq!(caller.user.role, 80);
    let refused = sc_auth::authenticate_api_token(&catalog, secret)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("module routes"), "{refused}");

    // An ordinary admin token still reaches both.
    let (ok, stdout, stderr) = feldspar(
        &[
            "mcp-token",
            "create",
            "--label",
            "agent",
            "--admin",
            "--database-url",
            &url,
        ],
        None,
    );
    assert!(ok, "{stderr}");
    let admin = stdout.trim();
    sc_auth::authenticate_api_token(&catalog, admin).await?;
    sc_auth::authenticate_route_token(&catalog, admin).await?;

    // And the list says what the routes-only one is.
    let (ok, stdout, _) = feldspar(&["mcp-token", "list", "--database-url", &url], None);
    assert!(ok);
    assert!(stdout.contains("routes_only"), "{stdout}");
    Ok(())
}

#[tokio::test]
async fn settings_are_written_together_unset_and_read_back_as_json() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;

    // Several keys in one run, typed by their declarations.
    let settings = json!({
        "smtp_host": "mail.example.com",
        "smtp_port": "2525",
        "smtp_username": "alerts",
        "smtp_password": "hunter2",
        "email_from": "alerts@example.com",
    });
    let (ok, _, stderr) = feldspar(
        &["set-cfg", "--json", "--database-url", &url],
        Some(&settings.to_string()),
    );
    assert!(ok, "{stderr}");
    assert!(
        !stderr.contains("hunter2"),
        "a secret is not echoed: {stderr}"
    );
    assert_eq!(
        sc_config::stored_config(&catalog, "smtp_port").await?,
        Some(json!(2525))
    );

    // One bad value refuses the whole document: nothing of it is written.
    let (ok, _, stderr) = feldspar(
        &["set-cfg", "--json", "--database-url", &url],
        Some(r#"{"smtp_host": "other.example.com", "smtp_port": "many"}"#),
    );
    assert!(!ok);
    assert!(stderr.contains("smtp_port"), "{stderr}");
    assert_eq!(
        sc_config::stored_config(&catalog, "smtp_host").await?,
        Some(json!("mail.example.com"))
    );

    // Read back as JSON: the values, the secret redacted, an unset key null.
    let (ok, stdout, stderr) = feldspar(&["get-cfg", "--json", "--database-url", &url], None);
    assert!(ok, "{stderr}");
    let all: Json = serde_json::from_str(&stdout).unwrap();
    assert_eq!(all["smtp_host"], json!("mail.example.com"));
    assert_eq!(all["smtp_port"], json!(2525));
    assert_eq!(all["smtp_password"], json!(sc_types::SECRET_SENTINEL));
    assert_eq!(all["smtp_certificate_name"], Json::Null);
    let (ok, stdout, _) = feldspar(
        &["get-cfg", "smtp_port", "--json", "--database-url", &url],
        None,
    );
    assert!(ok);
    assert_eq!(stdout.trim(), "2525");

    // Unset: the row goes, and the key reads as its default again.
    let (ok, _, stderr) = feldspar(
        &[
            "set-cfg",
            "--unset",
            "smtp_username",
            "--database-url",
            &url,
        ],
        None,
    );
    assert!(ok && stderr.contains("unset"), "{stderr}");
    assert_eq!(
        sc_config::stored_config(&catalog, "smtp_username").await?,
        None
    );
    let (ok, _, stderr) = feldspar(
        &[
            "set-cfg",
            "--unset",
            "smtp_username",
            "--database-url",
            &url,
        ],
        None,
    );
    assert!(ok && stderr.contains("nothing changed"), "{stderr}");
    let (ok, _, _) = feldspar(
        &[
            "set-cfg",
            "--unset",
            "smtp_nonsense",
            "--database-url",
            &url,
        ],
        None,
    );
    assert!(!ok);
    Ok(())
}

// Multi-threaded: the binary is waited on synchronously, and the SMTP listener
// is a task on this runtime that has to keep answering it meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_test_message_goes_through_the_servers_own_sender() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    connect_catalog(&DbConfig::from_url(&url)).await?;

    // Nothing configured: said, and nothing sent.
    let (ok, _, stderr) = feldspar(
        &[
            "send-email",
            "--to",
            "ops@example.com",
            "--subject",
            "x",
            "--database-url",
            &url,
        ],
        Some("body"),
    );
    assert!(!ok && stderr.contains("smtp_host"), "{stderr}");

    let server = TestSmtp::start().await?;
    let settings = json!({
        "smtp_host": "127.0.0.1",
        "smtp_port": server.port(),
        "smtp_security": "none",
        "email_from": "Backups <backups@example.com>",
    });
    let (ok, _, stderr) = feldspar(
        &["set-cfg", "--json", "--database-url", &url],
        Some(&settings.to_string()),
    );
    assert!(ok, "{stderr}");

    let (ok, _, stderr) = feldspar(
        &[
            "send-email",
            "--to",
            "ops@example.com",
            "--subject",
            "Backup check",
            "--database-url",
            &url,
        ],
        Some("All backups ran.\n"),
    );
    assert!(ok, "{stderr}");
    let received = server.message().await?;
    let transcript = received.transcript();
    assert!(
        transcript.contains("MAIL FROM:<backups@example.com>"),
        "{transcript}"
    );
    assert!(
        transcript.contains("RCPT TO:<ops@example.com>"),
        "{transcript}"
    );
    assert!(
        received.body.contains("Subject: Backup check"),
        "{}",
        received.body
    );
    assert!(
        received.body.contains("All backups ran."),
        "{}",
        received.body
    );
    Ok(())
}

#[tokio::test]
async fn tables_are_connected_to_an_application_by_name() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    let id = sc_catalog::DataField::plain(
        "id",
        sc_types::TypeRef::from_sql_type("bigint generated by default as identity"),
    )
    .required()
    .primary_key();
    catalog.create_table("user_settings", &[id]).await?;
    let scratch = TempDir::new("tables");
    sc_catalog::save_file_store(
        &catalog,
        &sc_files::FileStoreDef::local("apps", scratch.path().display().to_string()),
    )
    .await?;
    save_application(
        &catalog,
        &Application::new(
            "Managed",
            "managed",
            FrameworkRef::new("none").with("store", "apps"),
        ),
    )
    .await?;

    let tables = |args: &[&str]| {
        let mut all = vec!["app", "tables"];
        all.extend_from_slice(args);
        all.extend_from_slice(&["--app", "managed", "--json", "--database-url", &url]);
        let (ok, stdout, stderr) = feldspar(&all, None);
        (ok, serde_json::from_str::<Json>(&stdout).ok(), stderr)
    };

    let (ok, listed, stderr) = tables(&["add", "user_settings"]);
    assert!(ok, "{stderr}");
    assert_eq!(listed, Some(json!(["user_settings"])));
    // Idempotent.
    let (ok, listed, stderr) = tables(&["add", "user_settings"]);
    assert!(ok && stderr.contains("unchanged"), "{stderr}");
    assert_eq!(listed, Some(json!(["user_settings"])));
    // A table that does not exist is refused, and nothing changes.
    let (ok, _, stderr) = tables(&["add", "no_such_table"]);
    assert!(!ok && stderr.contains("no_such_table"), "{stderr}");
    let (_, listed, _) = tables(&["list"]);
    assert_eq!(listed, Some(json!(["user_settings"])));

    let (ok, listed, stderr) = tables(&["remove", "user_settings"]);
    assert!(ok, "{stderr}");
    assert_eq!(listed, Some(json!([])));
    let stored = sc_app::load_application_by_subdomain(&catalog, "managed")
        .await?
        .unwrap();
    assert!(stored.tables.is_empty());
    Ok(())
}
