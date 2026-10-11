//! `feldspar add-user` and `feldspar modify-user`, driven through the **real
//! binary** against a **real Postgres**: an account created or re-passworded
//! from a terminal is one `authenticate` — the sign-in form's check — accepts.

use std::io::Write;
use std::process::{Command, Stdio};

use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;

const ADMIN: &str = "admin@example.com";

/// Run `feldspar <args…>` with `stdin` piped in, returning (success, stderr).
fn feldspar(args: &[&str], stdin: &str) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the feldspar binary");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for feldspar");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

async fn signs_in(catalog: &sc_catalog::Catalog, email: &str, password: &str) -> bool {
    sc_auth::authenticate(catalog, email, password)
        .await
        .expect("authenticate")
        .is_some()
}

#[tokio::test]
async fn a_forgotten_admin_password_is_reset_from_the_cli() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;

    // add-user with the password as a flag value, the role by name.
    let (ok, stderr) = feldspar(
        &[
            "add-user",
            ADMIN,
            "--role",
            "admin",
            "--password",
            "first-password",
            "--database-url",
            &url,
        ],
        "",
    );
    assert!(ok, "{stderr}");
    let user = sc_auth::load_user_by_email(&catalog, ADMIN)
        .await?
        .expect("created");
    assert_eq!(user.role, sc_auth::ROLE_ADMIN);
    assert!(signs_in(&catalog, ADMIN, "first-password").await);

    // add-user with no password reads it (stdin is not a terminal here); the
    // role by number.
    let (ok, stderr) = feldspar(
        &[
            "add-user",
            "staff@example.com",
            "--role",
            "100",
            "--database-url",
            &url,
        ],
        "piped-password\n",
    );
    assert!(ok, "{stderr}");
    assert!(signs_in(&catalog, "staff@example.com", "piped-password").await);

    // A second account with the same email is refused before any prompt.
    let (ok, stderr) = feldspar(
        &["add-user", ADMIN, "--role", "admin", "--database-url", &url],
        "x\n",
    );
    assert!(!ok);
    assert!(stderr.contains("already a user"), "{stderr}");

    // An unknown role lists the ones there are.
    let (ok, stderr) = feldspar(
        &[
            "add-user",
            "x@example.com",
            "--role",
            "wizard",
            "--password",
            "pw",
            "--database-url",
            &url,
        ],
        "",
    );
    assert!(!ok);
    assert!(stderr.contains("no role named `wizard`"), "{stderr}");

    // modify-user --password with no value asks for it.
    let (ok, stderr) = feldspar(
        &["modify-user", ADMIN, "--password", "--database-url", &url],
        "remembered-now\n",
    );
    assert!(ok, "{stderr}");
    assert!(signs_in(&catalog, ADMIN, "remembered-now").await);
    assert!(!signs_in(&catalog, ADMIN, "first-password").await);

    // modify-user --role, and a password given as a value, together.
    let (ok, stderr) = feldspar(
        &[
            "modify-user",
            "staff@example.com",
            "--role",
            "admin",
            "--password",
            "promoted",
            "--database-url",
            &url,
        ],
        "",
    );
    assert!(ok, "{stderr}");
    let staff = sc_auth::load_user_by_email(&catalog, "staff@example.com")
        .await?
        .expect("exists");
    assert_eq!(staff.role, sc_auth::ROLE_ADMIN);
    assert!(signs_in(&catalog, "staff@example.com", "promoted").await);

    // An empty piped password is refused, and nothing changes.
    let (ok, stderr) = feldspar(
        &["modify-user", ADMIN, "--password", "--database-url", &url],
        "\n",
    );
    assert!(!ok);
    assert!(stderr.contains("must not be blank"), "{stderr}");
    assert!(signs_in(&catalog, ADMIN, "remembered-now").await);

    // An unknown user is named.
    let (ok, stderr) = feldspar(
        &[
            "modify-user",
            "nobody@example.com",
            "--role",
            "admin",
            "--database-url",
            &url,
        ],
        "",
    );
    assert!(!ok);
    assert!(stderr.contains("no user with the email"), "{stderr}");
    Ok(())
}

/// Run `feldspar <args…>`, returning (success, stdout, stderr).
fn feldspar_out(args: &[&str]) -> (bool, String, String) {
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

/// TODO F1.8: roles, disabling and deleting accounts from a terminal, over
/// `sc-auth`'s own functions — and the last enabled admin survives all of it.
#[tokio::test]
async fn roles_and_accounts_are_managed_from_the_cli() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let url = db.url();
    let catalog = connect_catalog(&DbConfig::from_url(&url)).await?;
    let with_db = |args: &[&str]| -> Vec<String> {
        let mut out: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        out.extend(["--database-url".to_owned(), url.clone()]);
        out
    };
    let run = |args: &[&str]| {
        let args = with_db(args);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        feldspar_out(&refs)
    };

    // role add, then list: the new role is there by name and level.
    let (ok, _, stderr) = run(&[
        "role",
        "add",
        "Editor",
        "--level",
        "40",
        "--description",
        "Edits",
    ]);
    assert!(ok, "{stderr}");
    let (ok, stdout, stderr) = run(&["role", "list", "--json"]);
    assert!(ok, "{stderr}");
    let roles: serde_json::Value = serde_json::from_str(&stdout).expect("JSON on stdout");
    assert!(
        roles
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["role"] == 40 && r["name"] == "Editor" && r["description"] == "Edits"),
        "{roles}"
    );
    // A clash is refused naming it.
    let (ok, _, stderr) = run(&["role", "add", "Other", "--level", "40"]);
    assert!(
        !ok && stderr.contains("already used by `Editor`"),
        "{stderr}"
    );

    for (email, role) in [(ADMIN, "admin"), ("ed@example.com", "Editor")] {
        let (ok, _, stderr) = run(&["add-user", email, "--role", role, "--password", "pw-1"]);
        assert!(ok, "{stderr}");
    }

    // A role a user holds is not deleted out from under them.
    let (ok, _, stderr) = run(&["role", "delete", "Editor"]);
    assert!(!ok && stderr.contains("held by 1 user"), "{stderr}");

    // --disabled stops the account signing in; --enabled gives it back.
    let (ok, _, stderr) = run(&["modify-user", "ed@example.com", "--disabled"]);
    assert!(ok, "{stderr}");
    assert!(!signs_in(&catalog, "ed@example.com", "pw-1").await);
    let (ok, _, stderr) = run(&["modify-user", "ed@example.com", "--enabled"]);
    assert!(ok, "{stderr}");
    assert!(signs_in(&catalog, "ed@example.com", "pw-1").await);

    // The last enabled admin cannot be disabled or deleted.
    let (ok, _, stderr) = run(&["modify-user", ADMIN, "--disabled"]);
    assert!(!ok && stderr.contains("last enabled admin"), "{stderr}");
    let (ok, _, stderr) = run(&["delete-user", ADMIN]);
    assert!(!ok && stderr.contains("last enabled admin"), "{stderr}");
    assert!(signs_in(&catalog, ADMIN, "pw-1").await);

    // delete-user, then the role is free to go.
    let (ok, _, stderr) = run(&["delete-user", "ed@example.com"]);
    assert!(ok, "{stderr}");
    assert!(
        sc_auth::load_user_by_email(&catalog, "ed@example.com")
            .await?
            .is_none()
    );
    let (ok, _, stderr) = run(&["delete-user", "ed@example.com"]);
    assert!(!ok && stderr.contains("no user"), "{stderr}");
    let (ok, _, stderr) = run(&["role", "delete", "Editor"]);
    assert!(ok, "{stderr}");
    assert!(sc_auth::load_role(&catalog, 40).await?.is_none());
    // A built-in role stays.
    let (ok, _, stderr) = run(&["role", "delete", "admin"]);
    assert!(!ok && stderr.contains("built in"), "{stderr}");
    Ok(())
}
