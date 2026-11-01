//! `feldspar get-cfg` / `set-cfg`: the stored configuration values from a
//! terminal (§13.5).
//!
//! Driven through the **real binary** against a **real Postgres**, because the
//! claim is about the rows: a value this command writes is the one the server
//! reads at boot, checked by the same declaration the admin UI's Save is checked
//! against. A test that called `set_config` directly would assert nothing about
//! the command — the type a terminal's string becomes is the whole of what this
//! adds, and it happens on the way in.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;

/// Run `feldspar <args…>` with the given stdin, returning (success, stdout, stderr).
fn feldspar(args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the feldspar binary");
    // Always closed, so a command that reads stdin when it should not does not
    // hang the test suite waiting for a value that is never coming.
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

/// The `key=value` line for `key` in a `get-cfg` listing.
fn line<'a>(listing: &'a str, key: &str) -> &'a str {
    listing
        .lines()
        .find(|l| l.starts_with(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no line for `{key}` in:\n{listing}"))
}

#[tokio::test]
async fn a_value_set_from_the_cli_is_the_one_the_server_reads() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    // The tables have to exist before the command runs; `connect_catalog` is
    // what the command itself does, and it is idempotent.
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;

    // An int is stored as a number, not as the string it was typed as…
    let (ok, _, stderr) = feldspar(
        &["set-cfg", "smtp_port", "2525", "--database-url", &url],
        None,
    );
    assert!(ok, "set-cfg failed: {stderr}");
    assert_eq!(
        sc_config::stored_config(&catalog, "smtp_port").await?,
        Some(serde_json::json!(2525))
    );
    // …and a bool takes the spellings a shell script produces.
    let (ok, _, stderr) = feldspar(&["set-cfg", "log_sql", "yes", "--database-url", &url], None);
    assert!(ok, "set-cfg failed: {stderr}");
    assert_eq!(
        sc_config::stored_config(&catalog, "log_sql").await?,
        Some(serde_json::json!(true))
    );

    // What the *server* reads back is what was written: the settings the boot
    // path acts on, not a second reading of the same rows.
    assert_eq!(
        sc_config::config_value(&catalog, "smtp_port").await?,
        serde_json::json!(2525)
    );
    assert!(sc_config::development_settings(&catalog).await?.log_sql);

    // `get-cfg KEY` prints the value alone, ready for `$(…)` — no quotes around
    // a string, and nothing else on stdout. Note what was just switched on: with
    // `log_sql` set, every other command echoes its statements to stdout, and a
    // value captured out of this one would carry the select that found it.
    let (ok, stdout, stderr) = feldspar(&["get-cfg", "smtp_port", "--database-url", &url], None);
    assert!(ok, "get-cfg failed: {stderr}");
    assert_eq!(stdout, "2525\n");
    let (ok, stdout, _) = feldspar(
        &["set-cfg", "ssl_mode", "custom", "--database-url", &url],
        None,
    );
    assert!(ok);
    let (_, stdout2, _) = feldspar(&["get-cfg", "ssl_mode", "--database-url", &url], None);
    assert_eq!(stdout2, "custom\n", "{stdout}");
    Ok(())
}

/// The check is the declaration's, so a wrong type is a message rather than a
/// row — the same refusal the admin UI gives, in the place the admin can fix it.
#[tokio::test]
async fn a_value_of_the_wrong_type_is_refused_and_nothing_is_written() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;

    let (ok, _, stderr) = feldspar(
        &["set-cfg", "smtp_port", "yes", "--database-url", &url],
        None,
    );
    assert!(!ok);
    assert!(stderr.contains("smtp_port"), "{stderr}");
    assert_eq!(sc_config::stored_config(&catalog, "smtp_port").await?, None);

    // A value outside the declared options is refused by the store's own check,
    // which is what lists the options.
    let (ok, _, stderr) = feldspar(
        &["set-cfg", "ssl_mode", "sortof", "--database-url", &url],
        None,
    );
    assert!(!ok);
    assert!(stderr.contains("letsencrypt"), "{stderr}");
    assert_eq!(sc_config::stored_config(&catalog, "ssl_mode").await?, None);

    // A key nobody declared never reaches the table, and the message names the
    // keys there are — the typo is fixed from it, without a second command.
    let (ok, _, stderr) = feldspar(
        &["set-cfg", "smtp_prot", "2525", "--database-url", &url],
        None,
    );
    assert!(!ok);
    assert!(stderr.contains("smtp_prot"), "{stderr}");
    assert!(stderr.contains("smtp_port"), "{stderr}");
    // …and it is refused *before* stdin is read, so a typo does not leave the
    // command waiting on a terminal. (No stdin is given here: with the pipe
    // closed a read would end the command, but with `--database-url` pointing at
    // a live database it would also have connected first, which is what the
    // absent "database configured from" work below shows.)
    let (ok, _, stderr) = feldspar(&["set-cfg", "smtp_prot", "--database-url", &url], None);
    assert!(!ok);
    assert!(stderr.contains("known keys are"), "{stderr}");
    Ok(())
}

/// The reason `set-cfg` reads stdin at all: `ssl_certificate` is a PEM block,
/// and quoting one into a shell is not a thing anybody should have to do.
#[tokio::test]
async fn a_multi_line_value_comes_in_on_stdin_intact() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;

    let pem = "-----BEGIN CERTIFICATE-----\nMIIB\nQUJD\n-----END CERTIFICATE-----\n";
    let (ok, _, stderr) = feldspar(
        &["set-cfg", "ssl_certificate", "--database-url", &url],
        Some(pem),
    );
    assert!(ok, "set-cfg failed: {stderr}");
    // Every interior newline kept; the one the file ended with is the
    // transport's and is not stored.
    assert_eq!(
        sc_config::stored_config(&catalog, "ssl_certificate").await?,
        Some(serde_json::json!(pem.trim_end_matches('\n')))
    );
    // And it comes back out whole, so `get-cfg ssl_certificate > cert.pem` is a
    // certificate rather than a quoted escape of one.
    let (ok, stdout, _) = feldspar(
        &["get-cfg", "ssl_certificate", "--database-url", &url],
        None,
    );
    assert!(ok);
    assert_eq!(stdout, pem);

    // The listing keeps one setting per line, whatever the value looks like.
    let (ok, listing, _) = feldspar(&["get-cfg", "--database-url", &url], None);
    assert!(ok);
    assert!(
        line(&listing, "ssl_certificate").contains("\\n"),
        "a multi-line value should be one JSON line: {listing}"
    );
    Ok(())
}

#[tokio::test]
async fn the_listing_shows_every_key_and_hides_the_secrets() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;

    let (ok, _, stderr) = feldspar(
        &[
            "set-cfg",
            "smtp_password",
            "hunter2",
            "--database-url",
            &url,
        ],
        None,
    );
    assert!(ok, "set-cfg failed: {stderr}");
    // The echo of the write does not put the secret on the terminal either.
    assert!(!stderr.contains("hunter2"), "{stderr}");

    let (ok, listing, _) = feldspar(&["get-cfg", "--database-url", &url], None);
    assert!(ok);
    // Every declared key is a line, set or not…
    assert_eq!(line(&listing, "smtp_host"), "smtp_host=");
    // …a default is what the server would act on…
    assert_eq!(line(&listing, "smtp_port"), "smtp_port=587");
    assert_eq!(line(&listing, "log_sql"), "log_sql=false");
    // …and a secret is the redaction, not the key.
    assert!(!listing.contains("hunter2"), "{listing}");
    assert_eq!(
        line(&listing, "smtp_password"),
        format!("smtp_password={}", sc_types::SECRET_SENTINEL)
    );

    // Naming the secret's key prints it: that caller asked for that value, and
    // this command already holds the database.
    let (ok, stdout, _) = feldspar(&["get-cfg", "smtp_password", "--database-url", &url], None);
    assert!(ok);
    assert_eq!(stdout, "hunter2\n");

    // The redaction is not a value: pasting it back would store a password that
    // looks configured and authenticates with nothing.
    let (ok, _, stderr) = feldspar(
        &[
            "set-cfg",
            "smtp_password",
            sc_types::SECRET_SENTINEL,
            "--database-url",
            &url,
        ],
        None,
    );
    assert!(!ok, "{stderr}");
    assert_eq!(
        sc_config::stored_config(&catalog, "smtp_password").await?,
        Some(serde_json::json!("hunter2"))
    );

    // A key with no declaration is not part of the answer, but it is not
    // invisible either: it is reported, on stderr, so it can be cleared.
    sc_query_write(&catalog, "obsolete_setting").await?;
    let (ok, listing, stderr) = feldspar(&["get-cfg", "--database-url", &url], None);
    assert!(ok);
    assert!(!listing.contains("obsolete_setting"), "{listing}");
    assert!(stderr.contains("obsolete_setting"), "{stderr}");
    Ok(())
}

/// Write a row under a key no declaration describes — what a removed setting
/// leaves behind. `set_config` refuses one, which is the point, so this goes
/// through the table directly.
async fn sc_query_write(catalog: &sc_catalog::Catalog, key: &str) -> sc_error::Result<()> {
    let insert = sc_query::Insert::row(
        sc_config::CONFIG_TABLE,
        vec!["key".to_owned(), "value".to_owned()],
        vec![
            sc_query::Expr::lit(key),
            sc_query::Expr::Lit(sc_query::Value::Json(serde_json::json!(true))),
        ],
    );
    catalog
        .primary(sc_catalog::Unchecked::system("test"))
        .query(&sc_query::Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

#[test]
fn the_usage_names_both_commands() {
    let (ok, _, stderr) = feldspar(&[], None);
    assert!(ok);
    assert!(stderr.contains("feldspar get-cfg"), "{stderr}");
    assert!(stderr.contains("feldspar set-cfg"), "{stderr}");
}
