//! `feldspar registry` (TODO-app-sync R5), driven through the **real binary**
//! against a **real Postgres**: a `get` then a `put` of the same JSON plans
//! nothing, `--plan` writes nothing, and `--yes` refuses a plan with a blocker
//! and one that loses data unless `--allow-destructive` says so.
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use sc_catalog::DataField;
use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value, json};

/// Runs `feldspar registry …` against one database, with a data directory of
/// its own (where a safety backup goes), removed when it drops.
struct Registry {
    url: String,
    data: PathBuf,
}

impl Drop for Registry {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.data).ok();
    }
}

impl Registry {
    /// `feldspar registry <args…> --database-url URL`, with `stdin`; (success,
    /// stdout, stderr).
    fn run(&self, args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_feldspar"))
            .arg("registry")
            .args(args)
            .args(["--database-url", &self.url])
            .env("SC_DATA_DIR", &self.data)
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

    /// `get ENTRY`, parsed.
    fn get(&self, entry: &str) -> Value {
        let (ok, out, err) = self.run(&["get", entry], None);
        assert!(ok, "get {entry}: {err}");
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
    }

    /// `put ENTRY flags…` with `value` on stdin.
    fn put(&self, entry: &str, flags: &[&str], value: &Value) -> (bool, String, String) {
        let mut args = vec!["put", entry];
        args.extend_from_slice(flags);
        self.run(&args, Some(&value.to_string()))
    }
}

/// The field called `name` in a table's JSON.
fn field<'a>(table: &'a mut Value, name: &str) -> &'a mut Value {
    table["fields"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("no field `{name}`"))
}

#[tokio::test]
async fn the_registry_from_the_cli_plans_and_applies_only_when_told() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    catalog
        .create_table(
            "tickets",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(sc_db::ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                DataField::plain("notes", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    // A ticket with no title: what makes `title` required refused.
    db.execute("INSERT INTO tickets (title, notes) VALUES (NULL, 'kept')")
        .await?;
    drop(catalog);
    let registry = Registry {
        url,
        data: std::env::temp_dir().join(format!("sc-cli-registry-{}", uuid::Uuid::new_v4())),
    };

    let (ok, listed, err) = registry.run(&["list", "table"], None);
    assert!(ok, "{err}");
    assert!(
        listed.contains("tables/tickets.json\ttable/tickets"),
        "{listed}"
    );
    let (ok, listed, err) = registry.run(&["list", "setting"], None);
    assert!(ok, "{err}");
    assert!(
        listed.contains("settings/email.json\tsetting/email\tthis instance only"),
        "{listed}"
    );

    // What `get` prints, put straight back, is a plan with nothing in it.
    let table = registry.get("table/tickets");
    let (ok, out, err) = registry.put("table/tickets", &["--yes"], &table);
    assert!(ok, "{err}");
    assert!(out.contains("(nothing to do)"), "{out}");
    assert!(err.contains("nothing to do"), "{err}");

    // `--plan` prints the plan and writes nothing; so does a put without
    // `--yes`.
    let mut described = table.clone();
    described["description"] = json!("Support tickets");
    for flags in [&["--plan"][..], &[][..]] {
        let (ok, out, err) = registry.put("tables/tickets.json", flags, &described);
        assert!(ok, "{err}");
        assert!(out.contains("[x] safe"), "{out}");
        assert!(err.contains("nothing was saved"), "{err}");
        assert_eq!(
            registry.get("table/tickets")["description"],
            table["description"]
        );
    }
    // With `--yes`, it is saved.
    let (ok, _, err) = registry.put("table/tickets", &["--yes"], &described);
    assert!(ok, "{err}");
    assert_eq!(
        registry.get("table/tickets")["description"],
        "Support tickets"
    );

    // A plan with a blocker is refused, and nothing is saved.
    let mut required = registry.get("table/tickets");
    field(&mut required, "title")["required"] = json!(true);
    let (ok, out, err) = registry.put("table/tickets", &["--yes"], &required);
    assert!(!ok, "{out}\n{err}");
    assert!(out.contains("blocked:"), "{out}");
    assert!(err.contains("no blockers"), "{err}");
    assert_eq!(
        field(&mut registry.get("table/tickets"), "title")["required"],
        false
    );

    // Dropping a field loses data: refused without --allow-destructive, and
    // done with it, after a safety backup.
    let mut dropped = registry.get("table/tickets");
    dropped["fields"]
        .as_array_mut()
        .unwrap()
        .retain(|f| f["name"] != "notes");
    let (ok, out, err) = registry.put("table/tickets", &["--yes"], &dropped);
    assert!(!ok, "{out}\n{err}");
    assert!(out.contains("[ ] data-destructive"), "{out}");
    assert!(err.contains("--allow-destructive"), "{err}");
    assert!(
        registry.get("table/tickets")["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "notes")
    );
    let (ok, _, err) = registry.put("table/tickets", &["--yes", "--allow-destructive"], &dropped);
    assert!(ok, "{err}");
    assert!(err.contains("safety backup"), "{err}");
    assert!(
        registry.get("table/tickets")["fields"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["name"] != "notes")
    );
    Ok(())
}
