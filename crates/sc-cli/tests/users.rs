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
