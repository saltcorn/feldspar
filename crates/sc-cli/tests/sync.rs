//! `feldspar sync` (TODO-app-sync S5.4), driven through the **real binary**
//! against a **real Postgres** and a bare repository on this machine: link,
//! status, commit, pull, a deploy that `--plan` only prints, `--yes` refusing a
//! plan with a blocker (an uncommitted change) and one that drops data unless
//! `--allow-destructive` says so, discard, and branches.
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git) directly"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use sc_catalog::DataField;
use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::Value;

/// Runs `feldspar sync …` against one database, with a data directory of its
/// own (the mirror, the safety backups), removed when it drops.
struct Sync {
    url: String,
    data: PathBuf,
}

impl Drop for Sync {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.data).ok();
    }
}

impl Sync {
    /// `feldspar sync <args…> --database-url URL`; (success, stdout, stderr).
    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_feldspar"))
            .arg("sync")
            .args(args)
            .args(["--database-url", &self.url])
            .env("SC_DATA_DIR", &self.data)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run the feldspar binary");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// `run`, asserting it succeeded; (stdout, stderr).
    fn ok(&self, args: &[&str]) -> (String, String) {
        let (ok, out, err) = self.run(args);
        assert!(ok, "feldspar sync {}: {out}\n{err}", args.join(" "));
        (out, err)
    }
}

/// Run git in `dir`, asserting it succeeded.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=Bea", "-c", "user.email=bea@example.com"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn int_id() -> DataField {
    DataField::plain("id", TypeRef::Basic(BasicType::Int))
        .required()
        .primary_key()
        .generated(sc_db::ColumnGenerator::Identity)
}

/// The field names of `table` as the database has them now.
async fn columns(db: &TestDb, table: &str) -> Vec<String> {
    let client = db.client().await.unwrap();
    let rows = client
        .query(
            "SELECT column_name::text FROM information_schema.columns WHERE table_name = $1 \
             ORDER BY column_name",
            &[&table],
        )
        .await
        .unwrap();
    rows.iter().map(|r| r.get::<_, String>(0)).collect()
}

#[tokio::test]
async fn sync_from_the_cli_plans_and_applies_only_when_told() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    db.on_teardown(catalog.teardown_hook());
    catalog
        .create_table(
            "tickets",
            &[
                int_id(),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                DataField::plain("notes", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    db.execute("INSERT INTO tickets (title, notes) VALUES ('Printer', 'kept')")
        .await?;
    let data = std::env::temp_dir().join(format!("sc-cli-sync-{}", uuid::Uuid::new_v4()));
    let remote = data.join("structure.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "--initial-branch=main"]);
    let remote_url = format!("file://{}", remote.display());
    let sync = Sync { url, data };

    // Not linked yet: Status says what to do.
    let (ok, _, err) = sync.run(&["status"]);
    assert!(!ok);
    assert!(err.contains("not linked"), "{err}");

    let (_, err) = sync.ok(&["link", &remote_url]);
    assert!(err.contains("linked to"), "{err}");
    let (out, _) = sync.ok(&["status"]);
    assert!(out.contains("local new        table:tickets"), "{out}");
    assert!(out.contains("deploy is refused"), "{out}");

    // An uncommitted change is a blocker, and --yes refuses a plan with one.
    let (ok, out, err) = sync.run(&["deploy", "--yes"]);
    assert!(!ok, "{out}\n{err}");
    assert!(out.contains("blocked: this instance has"), "{out}");
    assert!(err.contains("no blockers"), "{err}");

    let (_, err) = sync.ok(&["commit", "-m", "Add tickets"]);
    assert!(
        err.contains("committed ") && err.contains("table:tickets"),
        "{err}"
    );
    let (out, _) = sync.ok(&["status"]);
    assert!(out.contains("in sync"), "{out}");

    // Another instance drops `notes` and pushes.
    let work = sync.data.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["clone", "-q", &remote_url, "."]);
    let file = work.join("tables/tickets.json");
    let mut table: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    table["fields"]
        .as_array_mut()
        .unwrap()
        .retain(|f| f["name"] != "notes");
    std::fs::write(&file, sc_sync::render_json(&table)).unwrap();
    git(&work, &["commit", "-q", "-am", "Drop notes"]);
    git(&work, &["push", "-q", "origin", "main"]);

    let (_, err) = sync.ok(&["pull"]);
    assert!(err.contains(" → "), "{err}");
    let (out, _) = sync.ok(&["status"]);
    assert!(out.contains("incoming         table:tickets"), "{out}");

    // `--plan` prints the plan and writes nothing; so does a deploy without
    // `--yes`.
    for flags in [&["--plan"][..], &[][..]] {
        let mut args = vec!["deploy"];
        args.extend_from_slice(flags);
        let (out, err) = sync.ok(&args);
        assert!(out.contains("[ ] data-destructive"), "{out}");
        assert!(err.contains("nothing was applied"), "{err}");
        assert!(columns(&db, "tickets").await.contains(&"notes".to_owned()));
    }

    // `--yes` alone refuses a plan that drops data; with
    // `--allow-destructive` it is applied, after a safety backup.
    let (ok, out, err) = sync.run(&["deploy", "--yes"]);
    assert!(!ok, "{out}\n{err}");
    assert!(err.contains("--allow-destructive"), "{err}");
    assert!(columns(&db, "tickets").await.contains(&"notes".to_owned()));
    let (_, err) = sync.ok(&["deploy", "--yes", "--allow-destructive"]);
    assert!(err.contains("a backup was written first"), "{err}");
    assert!(err.contains("deployed (converged)"), "{err}");
    assert_eq!(columns(&db, "tickets").await, ["id", "title"]);
    let (out, _) = sync.ok(&["status"]);
    assert!(out.contains("in sync"), "{out}");

    // A table made here, then discarded.
    catalog.reload().await?;
    catalog.create_table("scratch", &[int_id()]).await?;
    let (out, _) = sync.ok(&["status"]);
    assert!(out.contains("local new        table:scratch"), "{out}");
    let (out, err) = sync.ok(&["discard", "table:scratch", "--plan"]);
    assert!(out.contains("scratch"), "{out}");
    assert!(err.contains("nothing was applied"), "{err}");
    sync.ok(&["discard", "table:scratch", "--yes", "--allow-destructive"]);
    let (out, _) = sync.ok(&["status"]);
    assert!(out.contains("in sync"), "{out}");
    assert!(columns(&db, "scratch").await.is_empty());

    // Branches: create one from the tip, list them, switch back.
    sync.ok(&["branch", "feature", "--create"]);
    let (out, _) = sync.ok(&["branch"]);
    assert_eq!(out, "* feature\n  main\n", "{out}");
    sync.ok(&["branch", "main"]);
    let (ok, _, err) = sync.run(&["branch", "nowhere"]);
    assert!(!ok);
    assert!(err.contains("no branch `nowhere`"), "{err}");
    Ok(())
}
