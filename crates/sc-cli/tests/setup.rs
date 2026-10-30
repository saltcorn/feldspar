//! `feldspar setup`: the plan each kind of instance gets, the configuration
//! file it writes (and how it merges into one that is already there), the
//! questions it asks, and a development setup carried out for real.
//!
//! A production setup cannot be run under a test user — it installs packages
//! and creates accounts — so it is checked through its [`plan`], which is the
//! same list of steps `--dry-run` prints and `execute` carries out.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use sc_cli::config_file::ConfigFile;
use sc_cli::setup::{
    Choices, Database, DbKind, Family, Host, Merged, Mode, Prompter, RemotePostgres,
    ServiceManager, SetupArgs, Step, describe, environment_section, execute, merge_config,
    parse_os_release, plan, resolve, systemd_unit,
};

fn args(list: &[&str]) -> SetupArgs {
    SetupArgs::parse(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        .expect("the arguments parse")
}

fn host(family: Family, services: ServiceManager) -> Host {
    Host {
        family,
        os_name: "Test Linux".into(),
        root: false,
        sudo: Some("sudo".into()),
        services,
        login: "alice".into(),
        postgres_running: false,
        npm: true,
        service_file: None,
    }
}

fn debian() -> Host {
    host(Family::Debian, ServiceManager::Systemd)
}

fn alpine() -> Host {
    host(Family::Alpine, ServiceManager::OpenRc)
}

/// `--binary` so the plan does not depend on where the test binary lives (a
/// home directory, which a production plan rightly refuses).
fn server_choices(extra: &[&str], host: &Host) -> Choices {
    let mut list = vec!["--server", "--binary", "/opt/feldspar/bin/feldspar"];
    list.extend_from_slice(extra);
    resolve(&args(&list), host, None).expect("resolve")
}

/// Every command the plan runs, one line each, as `--dry-run` shows them.
fn commands(steps: &[Step]) -> Vec<String> {
    steps
        .iter()
        .filter_map(|s| match s {
            Step::Run { argv, .. } => Some(argv.join(" ")),
            _ => None,
        })
        .collect()
}

fn written<'a>(steps: &'a [Step], path: &str) -> &'a Step {
    steps
        .iter()
        .find(|s| matches!(s, Step::Write { path: p, .. } if p == Path::new(path)))
        .unwrap_or_else(|| panic!("the plan writes no {path}: {steps:#?}"))
}

fn contents(step: &Step) -> &str {
    match step {
        Step::Write { contents, .. } => contents,
        _ => unreachable!(),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "feldspar-setup-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn server_and_dev_are_presets_that_ask_nothing() {
    let server = args(&["--server"]);
    assert_eq!(server.mode, Some(Mode::Production));
    assert!(server.preset);
    let dev = args(&["--dev"]);
    assert_eq!(dev.mode, Some(Mode::Development));
    assert!(dev.preset);

    // The remote-database flags choose the database, whichever mode.
    assert_eq!(
        args(&["--server", "--database-url", "postgres://u:p@db/x"]).db_kind(),
        Some(DbKind::RemotePostgres)
    );
    assert_eq!(
        args(&["--dev", "--db-host=db.internal"]).db_kind(),
        Some(DbKind::RemotePostgres)
    );
    assert_eq!(
        args(&["--server", "--sqlite", "/x.sqlite"]).db_kind(),
        Some(DbKind::Sqlite)
    );
    assert_eq!(args(&["--server"]).db_kind(), None);

    for bad in [
        &["--server", "--dev"][..],
        &["--sqlite", "/a", "--database-url", "postgres://h/d"],
        &["--db", "postgres", "--db-host", "h"],
        &["--database-url", "mysql://h/d"],
        &["--no-such-flag"],
    ] {
        let list: Vec<String> = bad.iter().map(|s| (*s).to_owned()).collect();
        assert!(
            SetupArgs::parse(&list).is_err(),
            "{bad:?} should be refused"
        );
    }
}

#[test]
fn without_a_mode_and_without_a_terminal_it_says_which_flags() {
    let err = resolve(&args(&[]), &debian(), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("--dev") && err.contains("--server"), "{err}");
}

#[test]
fn os_release_decides_the_family() {
    let ubuntu = "PRETTY_NAME=\"Ubuntu 24.04 LTS\"\nID=ubuntu\nID_LIKE=debian\n";
    assert_eq!(
        parse_os_release(ubuntu),
        (Family::Debian, "Ubuntu 24.04 LTS".into())
    );
    assert_eq!(parse_os_release("ID=debian\n").0, Family::Debian);
    assert_eq!(
        parse_os_release("ID=alpine\nPRETTY_NAME=\"Alpine Linux v3.20\"\n").0,
        Family::Alpine
    );
    assert_eq!(
        parse_os_release("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n").0,
        Family::Debian
    );
    assert_eq!(parse_os_release("ID=fedora\n").0, Family::Other);
}

/// `--server` on Debian: PostgreSQL from apt, the account, the role and
/// database over the socket, `/etc/feldspar/feldspar.toml` for the service
/// account only, and a unit that is enabled and started.
#[test]
fn server_on_debian_installs_postgres_and_a_systemd_unit() {
    let host = debian();
    let c = server_choices(&["--domain", "example.com"], &host);
    assert_eq!(c.config_path, Path::new("/etc/feldspar/feldspar.toml"));
    assert_eq!(
        c.database,
        Database::LocalPostgres {
            role: "feldspar".into(),
            database: "feldspar".into()
        }
    );
    let steps = plan(&c, &host, None).unwrap();
    let cmds = commands(&steps);
    let position = |needle: &str| {
        cmds.iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("no `{needle}` in {cmds:#?}"))
    };
    let install = position("apt-get install -y postgresql postgresql-client");
    let account = position("adduser --system --group --home /var/lib/feldspar feldspar");
    let role = position("createuser feldspar");
    let enable = position("systemctl enable feldspar");
    let start = position("systemctl restart feldspar");
    assert!(install < account && account < role && role < enable && enable < start);
    assert!(cmds[role].contains("createdb -O feldspar feldspar"));

    let config = written(&steps, "/etc/feldspar/feldspar.toml");
    let Step::Write {
        mode, owner, root, ..
    } = config
    else {
        unreachable!()
    };
    assert_eq!(
        (*mode, owner.as_deref(), *root),
        (0o640, Some("root:feldspar"), true)
    );
    let parsed = ConfigFile::parse(contents(config), Path::new("feldspar.toml")).unwrap();
    assert_eq!(parsed.default_environment.as_deref(), Some("production"));
    let env = &parsed.environments["production"];
    assert_eq!(env.host.as_deref(), Some("/var/run/postgresql"));
    assert_eq!(env.user.as_deref(), Some("feldspar"));
    assert_eq!(env.database.as_deref(), Some("feldspar"));
    assert_eq!(env.base_domain.as_deref(), Some("example.com"));
    assert_eq!(env.bind.as_deref(), Some("0.0.0.0:80"));

    let unit = contents(written(&steps, "/etc/systemd/system/feldspar.service"));
    assert!(unit.contains("ExecStart=/opt/feldspar/bin/feldspar serve --environment production"));
    assert!(unit.contains("Environment=FELDSPAR_CONFIG=/etc/feldspar/feldspar.toml"));

    // Not root: the dry run shows each root command under sudo.
    let mut shown = Vec::new();
    describe(&steps, &host, &mut shown);
    let shown = String::from_utf8(shown).unwrap();
    assert!(shown.contains("$ sudo apt-get update"), "{shown}");
    assert!(shown.contains("(unless: id -u feldspar)"), "{shown}");
}

/// The unit a previous run wrote is left alone; one edited since is only
/// replaced with --force.
#[test]
fn an_existing_service_file_is_kept_or_refused_unless_forced() {
    let mut host = debian();
    let c = server_choices(&[], &host);
    host.service_file = Some(systemd_unit(&c));
    let steps = plan(&c, &host, None).unwrap();
    assert!(!steps.iter().any(|s| matches!(s, Step::Write { path, .. }
        if path == Path::new("/etc/systemd/system/feldspar.service"))));
    assert!(commands(&steps).contains(&"systemctl restart feldspar".to_owned()));

    host.service_file = Some("[Service]\nExecStart=/custom\n".into());
    let err = plan(&c, &host, None).unwrap_err().to_string();
    assert!(err.contains("--force"), "{err}");
    let forced = server_choices(&["--force"], &host);
    written(
        &plan(&forced, &host, None).unwrap(),
        "/etc/systemd/system/feldspar.service",
    );
}

/// A PostgreSQL already listening is used, not installed over.
#[test]
fn a_running_postgres_is_not_installed_again() {
    let mut host = debian();
    host.postgres_running = true;
    let steps = plan(&server_choices(&[], &host), &host, None).unwrap();
    let cmds = commands(&steps);
    assert!(!cmds.iter().any(|c| c.contains("apt-get")), "{cmds:#?}");
    assert!(cmds.iter().any(|c| c.contains("createuser feldspar")));
}

#[test]
fn server_on_alpine_uses_apk_busybox_accounts_and_openrc() {
    let host = alpine();
    let steps = plan(&server_choices(&[], &host), &host, None).unwrap();
    let cmds = commands(&steps);
    for expected in [
        "apk add --no-cache postgresql postgresql-client",
        "rc-service postgresql setup",
        "rc-service postgresql start",
        "addgroup -S feldspar",
        "adduser -S -D -H -h /var/lib/feldspar -s /sbin/nologin -G feldspar feldspar",
        "rc-update add feldspar default",
        "rc-service feldspar restart",
    ] {
        assert!(
            cmds.iter().any(|c| c == expected),
            "no `{expected}` in {cmds:#?}"
        );
    }
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("apt-get") || c.contains("systemctl"))
    );
    let script = contents(written(&steps, "/etc/init.d/feldspar"));
    assert!(script.starts_with("#!/sbin/openrc-run\n"));
    assert!(script.contains("command=\"/opt/feldspar/bin/feldspar\""));
    assert!(script.contains("command_args=\"serve --environment production\""));
    assert!(script.contains("command_user=\"feldspar:feldspar\""));
    assert!(script.contains("export FELDSPAR_CONFIG=\"/etc/feldspar/feldspar.toml\""));
}

/// A remote PostgreSQL: nothing installed, no role created, the connection in
/// the file.
#[test]
fn server_with_a_remote_postgres_installs_no_database() {
    let host = debian();
    let c = server_choices(
        &[
            "--db-host",
            "db.internal",
            "--db-port",
            "6432",
            "--db-user",
            "app",
            "--db-password",
            "s3cr\"t",
            "--db-name",
            "prod",
        ],
        &host,
    );
    let steps = plan(&c, &host, None).unwrap();
    let cmds = commands(&steps);
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("postgresql") || c.contains("createuser")),
        "{cmds:#?}"
    );
    assert!(cmds.iter().any(|c| c.starts_with("adduser --system")));
    let parsed = ConfigFile::parse(
        contents(written(&steps, "/etc/feldspar/feldspar.toml")),
        Path::new("feldspar.toml"),
    )
    .unwrap();
    let env = &parsed.environments["production"];
    assert_eq!(env.host.as_deref(), Some("db.internal"));
    assert_eq!(env.port, Some(6432));
    assert_eq!(env.user.as_deref(), Some("app"));
    // Quoted and escaped, so a password with a `"` in it survives.
    assert_eq!(env.password.as_deref(), Some("s3cr\"t"));
    assert_eq!(env.database.as_deref(), Some("prod"));

    let url = server_choices(&["--database-url", "postgres://app:pw@db/prod"], &host);
    let section = environment_section(&url);
    assert!(
        section.contains("url = \"postgres://app:pw@db/prod\""),
        "{section}"
    );

    // Parts without a role or a database are not enough to connect with.
    assert!(resolve(&args(&["--server", "--db-host", "db"]), &host, None).is_err());
}

#[test]
fn production_refuses_what_it_cannot_run() {
    // A binary in a home directory: ProtectHome hides it from the service.
    let deb = debian();
    let c = resolve(
        &args(&["--server", "--binary", "/home/alice/feldspar"]),
        &deb,
        None,
    )
    .unwrap();
    let err = plan(&c, &deb, None).unwrap_err().to_string();
    assert!(err.contains("--binary"), "{err}");
    // No service manager.
    let none = host_without_services();
    let err = plan(&server_choices(&[], &none), &none, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("systemd or OpenRC"), "{err}");
    // A distribution it does not know.
    let fedora = host(Family::Other, ServiceManager::Systemd);
    assert!(plan(&server_choices(&[], &fedora), &fedora, None).is_err());
    // Peer authentication: the local role has to be the service account.
    assert!(
        resolve(
            &args(&["--server", "--db-user", "someone"]),
            &debian(),
            None
        )
        .is_err()
    );
}

fn host_without_services() -> Host {
    host(Family::Debian, ServiceManager::None)
}

/// The generated unit is the README's §2.6 unit, line for line, when the
/// binary is where the README installs it.
#[test]
fn the_systemd_unit_agrees_with_the_readme() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    let documented = readme
        .split_once("sudo tee /etc/systemd/system/feldspar.service >/dev/null <<'UNIT'\n")
        .expect("§2.6 should write the unit with a heredoc")
        .1
        .split_once("\nUNIT\n")
        .expect("the heredoc should be terminated")
        .0;
    let mut c = server_choices(&[], &debian());
    c.binary = PathBuf::from("/usr/local/bin/feldspar");
    let unit = systemd_unit(&c);
    for line in documented.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            unit.lines().any(|u| u == line),
            "the unit is missing the README's `{line}`"
        );
    }
}

#[test]
fn an_existing_configuration_file_is_merged_into_not_overwritten() {
    let path = Path::new("/x/feldspar.toml");
    let section = "[environments.development]\nsqlite = \"/d/feldspar.sqlite\"\n";

    // Another environment — this machine's test database, say — is kept, and
    // the existing default is left alone.
    let existing =
        "default_environment = \"production\"\n\n[environments.test]\ndatabase = \"t\"\n";
    let Merged::Write { contents, note } =
        merge_config(Some(existing), path, "development", section, false).unwrap()
    else {
        panic!("should write")
    };
    let parsed = ConfigFile::parse(&contents, path).unwrap();
    assert_eq!(parsed.default_environment.as_deref(), Some("production"));
    assert!(parsed.environments.contains_key("test"));
    assert_eq!(
        parsed.environments["development"].sqlite.as_deref(),
        Some("/d/feldspar.sqlite")
    );
    assert!(note.unwrap().contains("--environment development"));

    // No default yet: this one becomes it.
    let Merged::Write { contents, .. } = merge_config(
        Some("[environments.test]\ndatabase = \"t\"\n"),
        path,
        "development",
        section,
        false,
    )
    .unwrap() else {
        panic!("should write")
    };
    assert_eq!(
        ConfigFile::parse(&contents, path)
            .unwrap()
            .default_environment
            .as_deref(),
        Some("development")
    );

    // The same section again: nothing to do, which is what makes a second run safe.
    let again = format!("{existing}\n{section}");
    assert_eq!(
        merge_config(Some(&again), path, "development", section, false).unwrap(),
        Merged::Unchanged
    );

    // A different one is refused, and replaced — sub-tables and all — with --force.
    let other = format!(
        "{existing}\n[environments.development]\nsqlite = \"/old.sqlite\"\n\n\
         [environments.development.llm_provider]\nname = \"x\"\nbackend = \"anthropic\"\n\n\
         [environments.zzz]\ndatabase = \"z\"\n"
    );
    let err = merge_config(Some(&other), path, "development", section, false).unwrap_err();
    assert!(err.to_string().contains("--force"), "{err}");
    let Merged::Write { contents, .. } =
        merge_config(Some(&other), path, "development", section, true).unwrap()
    else {
        panic!("should write")
    };
    let parsed = ConfigFile::parse(&contents, path).unwrap();
    let dev = &parsed.environments["development"];
    assert_eq!(dev.sqlite.as_deref(), Some("/d/feldspar.sqlite"));
    assert!(dev.llm_provider.is_none());
    assert!(parsed.environments.contains_key("zzz") && parsed.environments.contains_key("test"));

    // A file that does not parse is not touched.
    assert!(
        merge_config(
            Some("this is = = not toml"),
            path,
            "development",
            section,
            false
        )
        .is_err()
    );
}

/// The questions a bare `feldspar setup` asks, answered by a script: a
/// production instance against a PostgreSQL elsewhere, given by its parts.
#[test]
fn the_questions_settle_what_the_flags_leave_open() {
    let answers = "2\n3\n\ndb.internal\n\napp\nhunter2\n\nexample.com\n\n";
    let mut output = Vec::new();
    let c = {
        let mut prompter = Prompter::new(Cursor::new(answers), &mut output, false);
        resolve(
            &args(&["--binary", "/opt/feldspar/bin/feldspar"]),
            &debian(),
            Some(&mut prompter),
        )
        .unwrap()
    };
    assert_eq!(c.mode, Mode::Production);
    assert_eq!(
        c.database,
        Database::RemotePostgres(RemotePostgres {
            url: None,
            host: Some("db.internal".into()),
            port: Some(5432),
            user: Some("app".into()),
            password: Some("hunter2".into()),
            database: Some("feldspar".into()),
        })
    );
    assert_eq!(c.domain.as_deref(), Some("example.com"));
    assert_eq!(c.bind.as_deref(), Some("0.0.0.0:80"));
    let asked = String::from_utf8(output).unwrap();
    assert!(asked.contains("What kind of instance") && asked.contains("Which database"));

    // A flag is not asked about again; only the database is open here.
    let mut output = Vec::new();
    let c = {
        let mut prompter = Prompter::new(Cursor::new("\n"), &mut output, false);
        resolve(&args(&["--development"]), &debian(), Some(&mut prompter)).unwrap()
    };
    assert!(matches!(c.database, Database::Sqlite { .. }));
    let asked = String::from_utf8(output).unwrap();
    assert!(!asked.contains("What kind of instance") && asked.contains("Which database"));

    // Running out of answers is an error naming the flags, not a hang.
    let mut prompter = Prompter::new(Cursor::new(""), Vec::new(), false);
    let err = resolve(&args(&[]), &debian(), Some(&mut prompter)).unwrap_err();
    assert!(err.to_string().contains("flags"), "{err}");
}

/// `--dev`, carried out: a SQLite environment in the given configuration file,
/// which `feldspar` then selects with no flags at all.
#[test]
fn dev_setup_writes_a_configuration_the_server_reads() {
    let dir = scratch("dev");
    let config = dir.join("config/feldspar/feldspar.toml");
    let sqlite = dir.join("data/feldspar.sqlite");
    let host = Host {
        root: false,
        sudo: None,
        ..host(Family::Other, ServiceManager::None)
    };
    let c = resolve(
        &args(&[
            "--dev",
            "--config",
            config.to_str().unwrap(),
            "--sqlite",
            sqlite.to_str().unwrap(),
        ]),
        &host,
        None,
    )
    .unwrap();
    let steps = plan(&c, &host, None).unwrap();
    // Nothing a development SQLite setup does needs root.
    assert!(steps.iter().all(|s| !matches!(
        s,
        Step::Run { root: true, .. } | Step::Write { root: true, .. }
    )));
    execute(&steps, &host, &mut Vec::new()).unwrap();

    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(sqlite.parent().unwrap().is_dir());
    let parsed = ConfigFile::load(&config).unwrap().unwrap();
    assert_eq!(parsed.default_environment.as_deref(), Some("development"));
    assert_eq!(
        parsed.environments["development"].sqlite.as_deref(),
        sqlite.to_str()
    );

    // A second run finds it already done.
    let existing = fs::read_to_string(&config).unwrap();
    let again = plan(&c, &host, Some(&existing)).unwrap();
    assert!(
        !again.iter().any(|s| matches!(s, Step::Write { .. })),
        "{again:#?}"
    );

    // And the binary: `setup --dev` through the command line, then `get-cfg`
    // with no database flags reads the SQLite database it named.
    let home = dir.join("home");
    let run = |list: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_feldspar"))
            .args(list)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("SC_DATA_DIR", home.join("data"))
            .env_remove("FELDSPAR_CONFIG")
            .env_remove("FELDSPAR_ENV")
            .env_remove("DATABASE_URL")
            .env_remove("FELDSPAR_SQLITE")
            .output()
            .unwrap()
    };
    let out = run(&["setup", "--dev"]);
    assert!(
        out.status.success(),
        "setup --dev failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("feldspar serve"));
    let written = home.join(".config/feldspar/feldspar.toml");
    let parsed = ConfigFile::load(&written).unwrap().unwrap();
    let db = home.join("data/feldspar.sqlite");
    assert_eq!(
        parsed.environments["development"].sqlite.as_deref(),
        db.to_str()
    );

    let out = run(&["get-cfg", "smtp_port"]);
    assert!(
        out.status.success(),
        "get-cfg against the set-up instance failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        db.is_file(),
        "the server should have created {}",
        db.display()
    );
    fs::remove_dir_all(&dir).ok();
}
