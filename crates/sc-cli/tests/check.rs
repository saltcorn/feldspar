//! `feldspar check run` and `check list`, through the **real binary** against
//! a **real Postgres** (TODO-checklists C1.12, C1.17): a failing checklist
//! makes the command exit non-zero and say which step failed; a passing one
//! exits zero.

use std::process::Command;

use sc_check::{ChecklistInput, Editor, StepDraft, StepRegistry, edit};
use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;

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

#[tokio::test]
async fn check_run_exits_non_zero_on_a_failing_checklist() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    db.client()
        .await?
        .batch_execute("CREATE TABLE things (id serial primary key, name text);")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    catalog.reload().await?;
    let admin = sc_auth::create_user(&catalog, "admin@example.com", "pw", 1).await?;
    let registry = StepRegistry::builtin();
    let human = Editor::Human(admin.id);
    let mut as_admin = ChecklistInput::new("No things");
    as_admin.run_as = Some(admin.id);
    edit::create_checklist(
        &catalog,
        &registry,
        &human,
        as_admin.clone(),
        vec![
            StepDraft::new("data.expect_rows")
                .param("table", "things")
                .param("count", 0)
                .label("There are no things"),
        ],
    )
    .await?;
    as_admin.name = "Five things".to_owned();
    edit::create_checklist(
        &catalog,
        &registry,
        &human,
        as_admin,
        vec![
            StepDraft::new("data.expect_rows")
                .param("table", "things")
                .param("count", 5)
                .label("There are five things"),
        ],
    )
    .await?;

    let (ok, stdout, stderr) = feldspar(&["check", "run", "--checklist", "No things", "--database-url", &url]);
    assert!(ok, "{stdout}\n{stderr}");
    assert!(stdout.contains("PASS No things"), "{stdout}");

    let (ok, stdout, stderr) = feldspar(&["check", "run", "--database-url", &url]);
    assert!(!ok, "a failing checklist must fail the command:\n{stdout}\n{stderr}");
    assert!(stdout.contains("FAIL Five things"), "{stdout}");
    assert!(stdout.contains("expected 5 rows of `things`, found 0"), "{stdout}");
    assert!(stderr.contains("1 of 2 checklists did not pass"), "{stderr}");

    // The runs are recorded, and `check list` shows the last result.
    let (ok, stdout, stderr) = feldspar(&["check", "list", "--database-url", &url]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("Five things\t1 step\tfailed"), "{stdout}");
    assert!(stdout.contains("No things\t1 step\tpassed"), "{stdout}");

    // A name that matches nothing is an error, not a silent pass.
    let (ok, _, stderr) = feldspar(&["check", "run", "--checklist", "Nope", "--database-url", &url]);
    assert!(!ok);
    assert!(stderr.contains("no checklist called `Nope`"), "{stderr}");
    Ok(())
}
