//! `feldspar backup FILE` and `feldspar restore FILE`, driven through the **real
//! binary** against two **real Postgres** databases: what one installation
//! holds, written to a file from a terminal, comes back in another — all of it
//! but the TLS settings, which the backup leaves out.

use std::path::{Path, PathBuf};
use std::process::Command;

use sc_catalog::{Catalog, DataField};
use sc_cli::{DbConfig, connect_catalog};
use sc_query::{Expr, Insert, Select, Source, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let path = std::env::temp_dir().join(format!("sc-cli-backup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
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

/// Run `feldspar <args…>`, returning (success, stdout, stderr).
fn feldspar(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .args(args)
        .output()
        .expect("run the feldspar binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The titles in `books`, sorted.
async fn titles(catalog: &Catalog) -> sc_error::Result<Vec<String>> {
    let rows = catalog
        .provider(&catalog.require("books")?)?
        .query(&Select::from(Source::table("books")))
        .await?
        .try_collect()
        .await?;
    let mut titles: Vec<String> = rows
        .iter()
        .map(|row| match row.get("title") {
            Some(Value::Text(title)) => title.clone(),
            other => panic!("a title, got {other:?}"),
        })
        .collect();
    titles.sort();
    Ok(titles)
}

#[tokio::test]
async fn a_backup_from_the_cli_restores_everything_but_tls_into_another_database()
-> sc_error::Result<()> {
    let tmp = TempDir::new();
    let file = tmp.path().join("site.zip");
    let file_arg = file.display().to_string();

    // --- an installation with a table, rows and two kinds of setting ----------
    let source_db = TestDb::new().await?;
    let source_url = source_db.url();
    let source = connect_catalog(&DbConfig::from_url(&source_url)).await?;
    source
        .create_table(
            "books",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(sc_db::ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)).required(),
            ],
        )
        .await?;
    source.reload().await?;
    for title in ["Orlando", "The Waves"] {
        source
            .provider(&source.require("books")?)?
            .write(&Statement::from(Insert::row(
                "books",
                vec!["title".into()],
                vec![Expr::lit(title)],
            )))
            .await?
            .try_collect()
            .await?;
    }
    sc_config::set_config(&source, sc_config::SMTP_HOST, "mail.example.com".into()).await?;
    sc_config::set_config(
        &source,
        sc_config::ACME_CONTACT_EMAIL,
        "ops@example.com".into(),
    )
    .await?;

    // --- backup ---------------------------------------------------------------
    let (ok, _, stderr) = feldspar(&["backup", &file_arg, "--database-url", &source_url]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("site.zip"),
        "it says what it wrote: {stderr}"
    );
    let archive = std::fs::read(&file)?;
    let (contents, _) = sc_server::inspect_backup(&archive)?;
    assert!(!contents.ssl, "the TLS settings are left out");
    assert!(contents.settings, "the other settings are in");
    let books = contents
        .tables
        .iter()
        .find(|t| t.name == "books")
        .expect("the table is in the backup");
    assert_eq!(books.count, Some(2), "with its rows");
    assert!(
        !tmp.path().join(".site.zip.partial").exists(),
        "the partial file is renamed into place"
    );

    // --- restore into an empty installation ------------------------------------
    let target_db = TestDb::new().await?;
    let target_url = target_db.url();
    let modules = tmp.path().join("modules");
    let stan_cache = tmp.path().join("stan-cache");
    let (ok, stdout, stderr) = feldspar(&[
        "restore",
        &file_arg,
        "--database-url",
        &target_url,
        "--modules-dir",
        &modules.display().to_string(),
        "--stan-cache-dir",
        &stan_cache.display().to_string(),
        "--python",
        "off",
    ]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains("books"),
        "the report names the table: {stdout}"
    );

    let target = connect_catalog(&DbConfig::from_url(&target_url)).await?;
    assert_eq!(titles(&target).await?, ["Orlando", "The Waves"]);
    assert_eq!(
        sc_config::stored_config(&target, sc_config::SMTP_HOST).await?,
        Some("mail.example.com".into())
    );
    assert_eq!(
        sc_config::stored_config(&target, sc_config::ACME_CONTACT_EMAIL).await?,
        None,
        "a TLS setting does not travel"
    );

    // --- a file that is not a backup is refused before anything is touched -----
    let junk = tmp.path().join("junk.zip");
    std::fs::write(&junk, b"not a zip")?;
    let (ok, _, stderr) = feldspar(&[
        "restore",
        &junk.display().to_string(),
        "--database-url",
        &target_url,
    ]);
    assert!(!ok, "a junk file is refused");
    assert!(stderr.starts_with("error:"), "{stderr}");

    // --- and each command says what it needs ------------------------------------
    let (ok, _, stderr) = feldspar(&["backup", "--database-url", &source_url]);
    assert!(!ok);
    assert!(stderr.contains("backup FILE"), "{stderr}");
    Ok(())
}
