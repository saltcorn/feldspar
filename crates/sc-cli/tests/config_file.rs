//! The configuration file, end to end against a **real Postgres database**: a
//! `feldspar.toml` with a `test` environment, `--environment test` on the command
//! line, and the boot path connecting to the database that file names.
//!
//! The unit tests in `sc-config-file` and `src/db.rs` cover the parsing and the
//! precedence rules. This one covers the thing neither can: that what comes out
//! of the file is actually what gets connected — and, because the fixture names
//! a *different* database from the one the ambient environment points at, that
//! naming an environment really does outrank the ambient variables rather than
//! merely claiming to.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;

/// A `feldspar.toml` on disk for the duration of one test.
struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new(name: &str, contents: &str) -> Fixture {
        let path =
            std::env::temp_dir().join(format!("sc-cli-it-{}-{name}.toml", std::process::id()));
        std::fs::write(&path, contents).expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod fixture");
        }
        Fixture(path)
    }

    fn path(&self) -> &str {
        self.0.to_str().expect("utf-8 fixture path")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn serve_boots_against_the_environment_named_on_the_command_line() -> sc_error::Result<()> {
    let db = TestDb::new().await?;

    // Three environments, as a deployment would have them. `production` and
    // `staging` point nowhere reachable on purpose: if the selection were wrong
    // in either direction this test would fail with a connection error rather
    // than quietly pass against the right database for the wrong reason.
    let file = Fixture::new(
        "envs",
        &format!(
            r#"
default_environment = "production"

[environments.production]
url = "postgres://saltcorn:secret@127.0.0.1:1/production"

[environments.staging]
url = "postgres://saltcorn:secret@127.0.0.1:1/staging"

[environments.test]
url = "{}"
"#,
            db.url()
        ),
    );

    // The command line a person would type — and the arguments that are not the
    // database's business come back out for the server parser, unchanged.
    let (cfg, rest) = DbConfig::extract([
        "--environment",
        "test",
        "--config",
        file.path(),
        "--bind",
        "127.0.0.1:3032",
    ])?;
    assert_eq!(rest, ["--bind", "127.0.0.1:3032"]);
    assert_eq!(cfg.environment(), Some("test"));
    assert!(
        cfg.source().expect("a source").contains("test"),
        "the startup line should name the environment"
    );
    assert!(
        !cfg.target().contains("secret"),
        "the target must not leak a password: {}",
        cfg.target()
    );

    // The whole boot path, on the file's parameters.
    let catalog = connect_catalog(&cfg).await?;
    assert!(catalog.get(sc_auth::USERS_TABLE)?.is_some());

    // ...in *this* database. Nothing in the environment names it — only the
    // fixture does — so a `users` table here, created by the bootstrap above, is
    // the proof that the named environment decided the connection.
    let client = db.client().await?;
    let row = client
        .query_one(
            "select to_regclass('public.users') is not null as present",
            &[],
        )
        .await
        .expect("query");
    assert!(
        row.get::<_, bool>("present"),
        "the users table should have been bootstrapped in the environment's own database"
    );

    Ok(())
}

#[tokio::test]
async fn an_environment_given_as_parts_connects_just_like_a_url() -> sc_error::Result<()> {
    let db = TestDb::new().await?;

    // The same database, described the way an operator who does not want a URL
    // in the file would describe it: as parts. They are read back out of the
    // base connection string so this works on any machine the suite runs on.
    let base: tokio_postgres::Config = db.url().parse().map_err(|e| {
        sc_error::Error::config(format!(
            "could not parse the harness connection string: {e}"
        ))
    })?;
    // A Unix-socket host, when the connection string offers one, is preferred:
    // the socket directory is a perfectly good `host` value — libpq's own
    // convention, which `tokio_postgres::Config::host` follows — and a dev box
    // whose Postgres listens only on its socket has no TCP host to fall back to.
    let host = base
        .get_hosts()
        .iter()
        .find_map(|h| match h {
            tokio_postgres::config::Host::Unix(p) => Some(p.to_string_lossy().into_owned()),
            tokio_postgres::config::Host::Tcp(_) => None,
        })
        .or_else(|| {
            base.get_hosts().iter().find_map(|h| match h {
                tokio_postgres::config::Host::Tcp(h) => Some(h.clone()),
                tokio_postgres::config::Host::Unix(_) => None,
            })
        })
        .unwrap_or_else(|| "localhost".to_owned());
    let port = base.get_ports().first().copied().unwrap_or(5432);
    let user = base.get_user().unwrap_or("postgres").to_owned();
    let password = base
        .get_password()
        .map(|p| String::from_utf8_lossy(p).into_owned());

    let mut section = format!(
        "\n[environments.test]\nhost = \"{host}\"\nport = {port}\nuser = \"{user}\"\ndatabase = \"{}\"\n",
        db.name()
    );
    if let Some(password) = password {
        section.push_str(&format!("password = \"{password}\"\n"));
    }
    let file = Fixture::new("parts", &section);

    let (cfg, _) = DbConfig::extract(["--environment", "test", "--config", file.path()])?;
    // The parts path renders a credential-free target, and it is the file's.
    assert!(
        cfg.target().ends_with(&format!("/{}", db.name())),
        "target should name the file's database: {}",
        cfg.target()
    );

    let catalog = connect_catalog(&cfg).await?;
    assert!(catalog.get(sc_auth::USERS_TABLE)?.is_some());
    Ok(())
}

/// An environment is a **deployment**, so it may also say where it is served —
/// and that is what lets a command-line build write the same application URL
/// into the generated documentation that the server would.
#[test]
fn an_environment_carries_where_its_applications_are_served() {
    let file = Fixture::new(
        "serving",
        "[environments.production]\n\
         database = \"saltcorn\"\n\
         base_domain = \"example.com\"\n\
         bind = \"0.0.0.0:443\"\n\
         secure_cookies = true\n\
         \n\
         [environments.dev]\n\
         database = \"saltcorn_dev\"\n\
         base_domain = \"localhost\"\n",
    );

    let (prod, _) = DbConfig::extract(["--environment", "production", "--config", file.path()])
        .expect("extract");
    let serving = prod.serving();
    assert_eq!(serving.base_domain(), Some("example.com"));
    assert_eq!(serving.port(), Some(443));
    assert_eq!(serving.secure_cookies(), Some(true));
    let origin = serving.public_origin(None).expect("an origin");
    // 443 is https's default, so it is left out of a URL meant to be pasted.
    assert_eq!(origin.url_for("blog"), "https://blog.example.com");
    assert_eq!(origin.host_for("blog"), "blog.example.com");

    // A section that gives only the domain still resolves: the port falls back
    // to the one `serve` binds by default, which is what a development machine
    // is running anyway.
    let (dev, _) =
        DbConfig::extract(["--environment", "dev", "--config", file.path()]).expect("extract");
    assert_eq!(
        dev.serving()
            .public_origin(None)
            .expect("an origin")
            .url_for("todo"),
        "http://todo.localhost:3032"
    );

    // A flag outranks the file, the same way every other setting does.
    assert_eq!(
        dev.serving()
            .public_origin(Some("other.test"))
            .expect("an origin")
            .url_for("todo"),
        "http://todo.other.test:3032"
    );
}

/// No base domain anywhere: a server listening on this machine only serves its
/// applications under `localhost`, as `serve` does, so a build writes that URL.
/// One reachable from elsewhere, or behind TLS, has to be told its domain.
#[test]
fn a_loopback_server_without_a_base_domain_is_served_under_localhost() {
    let file = Fixture::new(
        "implied",
        "[environments.plain]\n\
         database = \"saltcorn\"\n\
         \n\
         [environments.own_port]\n\
         database = \"saltcorn\"\n\
         bind = \"127.0.0.1:4000\"\n\
         \n\
         [environments.public]\n\
         database = \"saltcorn\"\n\
         bind = \"0.0.0.0:3032\"\n\
         \n\
         [environments.tls]\n\
         database = \"saltcorn\"\n\
         secure_cookies = true\n",
    );
    let origin = |env: &str| {
        let (db, _) =
            DbConfig::extract(["--environment", env, "--config", file.path()]).expect("extract");
        db.serving().public_origin(None).map(|o| o.url_for("todo"))
    };
    assert_eq!(
        origin("plain").as_deref(),
        Some("http://todo.localhost:3032")
    );
    assert_eq!(
        origin("own_port").as_deref(),
        Some("http://todo.localhost:4000")
    );
    assert_eq!(origin("public"), None);
    assert_eq!(origin("tls"), None);
}

/// With nothing configuring an origin, the origin is the one a default `serve`
/// answers at: it binds `127.0.0.1:3032`, so its applications are served under
/// `localhost` (see `a_loopback_server_without_a_base_domain_is_served_under_localhost`).
///
/// **Nothing here may search for a configuration file.** The two ways to reach
/// this state are "no file was found" and "the file that was found applies
/// nothing", and a bare [`DbConfig::extract`] would take the first of those from
/// the *machine the test runs on*: a developer whose own
/// `~/.config/feldspar/feldspar.toml` sets `base_domain` (which is the ordinary
/// way to run this server locally) would watch this fail for a reason that is
/// nothing to do with the code. So the state is built directly, and the
/// searching path is covered by the fixture-driven tests above.
#[test]
fn without_a_base_domain_the_default_bind_is_served_under_localhost() {
    let url = |o: Option<sc_catalog::PublicOrigin>| o.map(|o| o.url_for("todo"));
    // No file, no flag, no environment — the deployment that never had one.
    let cfg = DbConfig::from_url("postgres:///x");
    assert_eq!(
        url(cfg.serving().public_origin(None)).as_deref(),
        Some("http://todo.localhost:3032")
    );
    // ...and a flag still wins.
    assert_eq!(
        url(cfg.serving().public_origin(Some("example.com"))).as_deref(),
        Some("http://todo.example.com:3032")
    );

    // The other road to the same place: a file was found and selected nothing,
    // which is `Ok(None)` rather than an error (see `config_file::select`).
    let empty = Fixture::new("no-environments", "# nothing here\n");
    let (found, _) = DbConfig::extract(["--config", empty.path()]).expect("extract");
    assert_eq!(
        url(found.serving().public_origin(None)).as_deref(),
        Some("http://todo.localhost:3032")
    );
    assert_eq!(
        url(found.serving().public_origin(Some("example.com"))).as_deref(),
        Some("http://todo.example.com:3032")
    );
}

/// A key the file does not define is a typo, and typos in this file are errors
/// (the reader's whole design) — including in the serving half.
#[test]
fn a_misspelled_serving_key_is_refused() {
    let file = Fixture::new(
        "typo",
        "[environments.production]\nbase_domian = \"example.com\"\n",
    );
    let err = DbConfig::extract(["--environment", "production", "--config", file.path()])
        .expect_err("a misspelled key must fail");
    assert!(err.to_string().contains("base_domian"), "{err}");
}

/// An environment that names a **SQLite file** is a whole installation: the
/// boot path connects to the file, creates it if it is not there, and needs
/// nothing else in the section.
#[tokio::test]
async fn an_environment_may_name_a_sqlite_file_instead_of_a_server() -> sc_error::Result<()> {
    let dir = std::env::temp_dir().join(format!("sc-cli-sqlite-env-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let db = dir.join("laptop.sqlite");

    let file = Fixture::new(
        "sqlite",
        &format!(
            "[environments.laptop]\nsqlite = \"{}\"\n",
            db.display().to_string().replace('\\', "\\\\")
        ),
    );
    let (cfg, rest) = DbConfig::extract([
        "--environment",
        "laptop",
        "--config",
        file.path(),
        "--bind",
        "x",
    ])?;
    assert_eq!(
        rest,
        vec!["--bind", "x"],
        "only the database flags are consumed"
    );
    assert!(cfg.target().contains("laptop.sqlite"), "{}", cfg.target());

    let catalog = connect_catalog(&cfg).await?;
    assert!(db.is_file(), "the named file is the installation");
    assert!(catalog.get(sc_auth::USERS_TABLE)?.is_some());

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// A section naming a SQLite file **and** a Postgres server describes two
/// databases, and nothing can choose between them for the operator — so it is
/// refused rather than silently preferring one.
#[test]
fn an_environment_that_names_two_databases_is_refused() {
    let file = Fixture::new(
        "both",
        "[environments.production]\nsqlite = \"/tmp/a.sqlite\"\nhost = \"db.internal\"\n",
    );
    let err = DbConfig::extract(["--environment", "production", "--config", file.path()])
        .expect_err("two databases in one environment must fail");
    assert!(err.to_string().contains("SQLite"), "{err}");
    assert!(err.to_string().contains("host"), "{err}");
}

/// A `--database-url` typed on the command line outranks a `sqlite` in the
/// file: a flag always wins, and an operator who names a Postgres database on
/// the command line means to use it.
#[test]
fn a_url_flag_outranks_the_files_sqlite() {
    let file = Fixture::new(
        "outrank",
        "[environments.laptop]\nsqlite = \"/tmp/a.sqlite\"\n",
    );
    let (cfg, _) = DbConfig::extract([
        "--environment",
        "laptop",
        "--config",
        file.path(),
        "--database-url",
        "postgres://u:p@h:5432/db",
    ])
    .expect("extract");
    assert!(!cfg.target().contains("a.sqlite"), "{}", cfg.target());
    assert!(cfg.target().contains("h:5432"), "{}", cfg.target());
}
