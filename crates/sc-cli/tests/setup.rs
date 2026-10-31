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
    Choices, Database, DbKind, Family, Host, Merged, Mode, NODE_MAJOR, OPENRC_CONF, Prompter,
    RemotePostgres, SYSTEMD_TLS_DROPIN, ServiceManager, SetupArgs, Step, browser_check_argv,
    describe, environment_section, execute, merge_config, next_steps, npm_new_enough,
    parse_os_release, plan, python_check_argv, resolve, systemd_unit, tls_setting, ubuntu_like,
};

fn args(list: &[&str]) -> SetupArgs {
    SetupArgs::parse(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        .expect("the arguments parse")
}

fn host(family: Family, services: ServiceManager) -> Host {
    Host {
        family,
        os_name: "Test Linux".into(),
        os_id: "testos".into(),
        version_id: "1".into(),
        root: false,
        sudo: Some("sudo".into()),
        services,
        login: "alice".into(),
        postgres_running: false,
        npm: Some("10.9.2".into()),
        browser: Some("/usr/bin/chromium".into()),
        ubuntu: false,
        arch: "x86_64".into(),
        service_file: None,
    }
}

/// A fresh machine: no npm and no browser.
fn bare(mut host: Host) -> Host {
    host.npm = None;
    host.browser = None;
    host
}

fn debian() -> Host {
    host(Family::Debian, ServiceManager::Systemd)
}

fn alpine() -> Host {
    host(Family::Alpine, ServiceManager::OpenRc)
}

/// `--binary` so the plan does not depend on where the test binary lives (a
/// home directory, which a production plan rightly refuses) — nor on whether
/// this machine happens to have a release installed there.
fn server_choices(extra: &[&str], host: &Host) -> Choices {
    let mut list = vec!["--server", "--binary", "/opt/feldspar/bin/feldspar"];
    list.extend_from_slice(extra);
    let mut choices = resolve(&args(&list), host, None).expect("resolve");
    choices.bundled_python = false;
    choices
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
    // The rest of the families, by ID and by the first ID_LIKE word that
    // names one.
    for (release, family) in [
        ("ID=fedora\n", Family::Fedora),
        (
            "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n",
            Family::Fedora,
        ),
        ("ID=ol\nID_LIKE=\"fedora\"\n", Family::Fedora),
        (
            "ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n",
            Family::Suse,
        ),
        ("ID=\"opensuse-leap\"\n", Family::Suse),
        ("ID=cachyos\nID_LIKE=arch\n", Family::Arch),
        ("ID=garuda\nID_LIKE=arch\n", Family::Arch),
        ("ID=gentoo\n", Family::Other),
    ] {
        assert_eq!(parse_os_release(release).0, family, "{release}");
    }
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
    let gentoo = host(Family::Other, ServiceManager::Systemd);
    assert!(plan(&server_choices(&[], &gentoo), &gentoo, None).is_err());
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

#[test]
fn npm_before_9_3_cannot_install_a_module() {
    for (version, ok) in [
        ("9.2.0", false),
        ("8.19.4", false),
        ("9.3.0", true),
        ("10.9.2\n", true),
        ("v11.0.0", true),
        ("", false),
        ("nonsense", false),
    ] {
        assert_eq!(npm_new_enough(version), ok, "{version:?}");
    }
    assert!(ubuntu_like("ID=ubuntu\nVERSION_ID=\"24.04\"\n"));
    assert!(ubuntu_like("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"));
    assert!(!ubuntu_like("ID=debian\n"));
    assert!(!ubuntu_like("ID=alpine\n"));
}

/// `--server` on a bare Debian: Node from NodeSource (keyring, source, pin),
/// apt's Chromium, and the browser started as the service account after the
/// account exists and before the service starts.
#[test]
fn server_on_a_bare_debian_installs_node_and_chromium() {
    let host = bare(debian());
    let c = server_choices(&[], &host);
    assert!(c.node && c.browser);
    let steps = plan(&c, &host, None).unwrap();
    let cmds = commands(&steps);
    let position = |needle: &str| {
        cmds.iter()
            .position(|c| c == needle)
            .unwrap_or_else(|| panic!("no `{needle}` in {cmds:#?}"))
    };
    let curl =
        position("env DEBIAN_FRONTEND=noninteractive apt-get install -y ca-certificates curl");
    let key = position(
        "curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key -o \
         /etc/apt/keyrings/nodesource.asc",
    );
    let node = position("env DEBIAN_FRONTEND=noninteractive apt-get install -y nodejs");
    let chromium = position("env DEBIAN_FRONTEND=noninteractive apt-get install -y chromium");
    let account = position("adduser --system --group --home /var/lib/feldspar feldspar");
    let enable = position("systemctl enable feldspar");
    assert!(curl < key && key < node && node < chromium && chromium < account);
    // Installed once each, and the distribution's npm never asked for.
    assert_eq!(
        cmds.iter()
            .filter(|c| c.ends_with("ca-certificates curl"))
            .count(),
        1
    );
    assert!(!cmds.iter().any(|c| c.ends_with(" npm")), "{cmds:#?}");

    assert_eq!(
        contents(written(&steps, "/etc/apt/sources.list.d/nodesource.list")),
        format!(
            "deb [signed-by=/etc/apt/keyrings/nodesource.asc] \
             https://deb.nodesource.com/node_{NODE_MAJOR}.x nodistro main\n"
        )
    );
    assert!(
        contents(written(&steps, "/etc/apt/preferences.d/nodesource"))
            .contains("Pin: origin deb.nodesource.com")
    );

    // The check: after the account, before the unit is enabled.
    let check = steps
        .iter()
        .position(|s| matches!(s, Step::CheckBrowser { .. }))
        .expect("a browser check");
    let at = |cmd: usize| {
        steps
            .iter()
            .position(|s| matches!(s, Step::Run { argv, .. } if argv.join(" ") == cmds[cmd]))
            .unwrap()
    };
    assert!(at(account) < check && check < at(enable));
    let Step::CheckBrowser {
        browser,
        user,
        config,
    } = &steps[check]
    else {
        unreachable!()
    };
    assert_eq!(
        browser, &None,
        "installed by the plan, so found on PATH afterwards"
    );
    assert_eq!(user.as_deref(), Some("feldspar"));
    assert_eq!(
        config,
        "[environments.production] in /etc/feldspar/feldspar.toml"
    );

    let mut shown = Vec::new();
    describe(&steps, &host, &mut shown);
    let shown = String::from_utf8(shown).unwrap();
    assert!(
        shown.contains("$ sudo su -s /bin/sh feldspar -c"),
        "{shown}"
    );
    assert!(shown.contains("never fatal"), "{shown}");

    // Nothing left to say about Node or the browser afterwards.
    let after = next_steps(&c, &host, true);
    assert!(
        !after.contains("npm") && !after.contains("view_app"),
        "{after}"
    );
}

/// What is already here and good enough is kept; an npm that is too old is not.
#[test]
fn what_is_already_here_is_kept_unless_too_old() {
    let host = debian();
    let steps = plan(&server_choices(&[], &host), &host, None).unwrap();
    let cmds = commands(&steps);
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("nodejs") || c.contains("chromium")),
        "{cmds:#?}"
    );
    let check = steps.iter().find_map(|s| match s {
        Step::CheckBrowser { browser, .. } => Some(browser.clone()),
        _ => None,
    });
    // The browser already here is still checked, by its path.
    assert_eq!(check, Some(Some("/usr/bin/chromium".into())));

    let mut old = debian();
    old.npm = Some("9.2.0".into());
    let steps = plan(&server_choices(&[], &old), &old, None).unwrap();
    assert!(
        commands(&steps)
            .iter()
            .any(|c| c.ends_with("apt-get install -y nodejs"))
    );
    assert!(
        steps
            .iter()
            .any(|s| matches!(s, Step::Note(n) if n.contains("npm 9.2.0 is too old")))
    );
}

/// Ubuntu's Chromium is a snap: Google Chrome from Google's repository on
/// x86_64, and on anything else a note and no check.
#[test]
fn ubuntu_gets_google_chrome_not_the_snap() {
    let mut host = bare(debian());
    host.ubuntu = true;
    let steps = plan(&server_choices(&[], &host), &host, None).unwrap();
    let cmds = commands(&steps);
    assert!(
        cmds.iter()
            .any(|c| c.ends_with("apt-get install -y google-chrome-stable"))
    );
    assert!(
        !cmds.iter().any(|c| c.ends_with("install -y chromium")),
        "{cmds:#?}"
    );
    assert!(
        contents(written(
            &steps,
            "/etc/apt/sources.list.d/google-chrome.list"
        ))
        .starts_with("deb [arch=amd64 signed-by=/etc/apt/keyrings/google-chrome.asc] ")
    );
    // The two repositories share their prerequisites.
    assert_eq!(
        cmds.iter()
            .filter(|c| c.ends_with("ca-certificates curl"))
            .count(),
        1
    );

    host.arch = "aarch64".into();
    let steps = plan(&server_choices(&[], &host), &host, None).unwrap();
    assert!(!commands(&steps).iter().any(|c| c.contains("chrome")));
    assert!(!steps.iter().any(|s| matches!(s, Step::CheckBrowser { .. })));
    assert!(
        steps
            .iter()
            .any(|s| matches!(s, Step::Note(n) if n.contains("view_app will be unavailable")))
    );
}

#[test]
fn alpine_installs_node_and_chromium_from_apk() {
    let host = bare(alpine());
    let cmds = commands(&plan(&server_choices(&[], &host), &host, None).unwrap());
    assert!(
        cmds.contains(&"apk add --no-cache nodejs npm".to_owned()),
        "{cmds:#?}"
    );
    assert!(
        cmds.contains(&"apk add --no-cache chromium".to_owned()),
        "{cmds:#?}"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("apt") || c.contains("nodesource"))
    );
}

/// `--no-node --no-browser` installs and checks nothing, and the closing notes
/// say how to have them later.
#[test]
fn node_and_the_browser_can_be_left_out() {
    let host = bare(debian());
    let c = server_choices(&["--no-node", "--no-browser"], &host);
    let steps = plan(&c, &host, None).unwrap();
    let cmds = commands(&steps);
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("nodejs") || c.contains("chromium"))
    );
    assert!(!steps.iter().any(|s| matches!(s, Step::CheckBrowser { .. })));
    let after = next_steps(&c, &host, true);
    assert!(
        after.contains("`feldspar setup --server --node` installs it"),
        "{after}"
    );
    assert!(
        after.contains("`feldspar setup --server --browser` installs one"),
        "{after}"
    );
}

/// A desktop's packages are its owner's: `--dev` installs nothing, a flag
/// installs it, and at a terminal setup asks — only about what is missing.
#[test]
fn development_installs_only_when_asked() {
    let host = bare(debian());
    let dev = resolve(&args(&["--dev"]), &host, None).unwrap();
    assert!(!dev.node && !dev.browser);
    let after = next_steps(&dev, &host, true);
    assert!(after.contains("`feldspar setup --dev --node`"), "{after}");

    let asked = resolve(&args(&["--dev", "--node", "--browser"]), &host, None).unwrap();
    let steps = plan(&asked, &host, None).unwrap();
    assert!(
        commands(&steps)
            .iter()
            .any(|c| c.ends_with("install -y nodejs"))
    );
    // A development check runs as whoever runs setup, not a service account.
    assert!(
        steps
            .iter()
            .any(|s| matches!(s, Step::CheckBrowser { user: None, .. }))
    );

    // At a terminal: yes to Node, no to the browser.
    let mut output = Vec::new();
    let c = {
        let mut p = Prompter::new(Cursor::new("\ny\nn\n"), &mut output, false);
        resolve(&args(&["--development"]), &host, Some(&mut p)).unwrap()
    };
    assert!(c.node && !c.browser);
    let asked = String::from_utf8(output).unwrap();
    assert!(asked.contains("Install Node.js") && asked.contains("Install Chromium"));

    // Nothing missing, nothing asked: the one answer is the database.
    let mut output = Vec::new();
    resolve(
        &args(&["--development"]),
        &debian(),
        Some(&mut Prompter::new(Cursor::new("\n"), &mut output, false)),
    )
    .unwrap();
    assert!(!String::from_utf8(output).unwrap().contains("Install"));
}

/// The check, carried out against stand-in browsers: one that starts, one
/// that starts only without its sandbox, and one that does not start. None of
/// them fails the setup.
#[test]
fn the_browser_check_reports_and_never_fails() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("browser");
    let fake = |name: &str, body: &str| {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let page = "echo '<html><head></head><body></body></html>'";
    let starts = fake("starts", page);
    let sandboxless = fake(
        "sandboxless",
        &format!(
            "case \"$*\" in *--no-sandbox*) {page} ;; *) echo 'No usable sandbox!' >&2; exit 1 ;; esac"
        ),
    );
    let broken = fake(
        "broken",
        "echo 'error while loading shared libraries: libnss3.so' >&2; exit 127",
    );

    let host = Host {
        sudo: None,
        ..host(Family::Other, ServiceManager::None)
    };
    let check = |browser: &Path| {
        let mut out = Vec::new();
        execute(
            &[Step::CheckBrowser {
                browser: Some(browser.to_owned()),
                user: None,
                config: "[environments.development] in feldspar.toml".into(),
            }],
            &host,
            &mut out,
        )
        .expect("a browser check never fails setup");
        String::from_utf8(out).unwrap()
    };
    assert!(check(&starts).contains("it starts\n"));
    let out = check(&sandboxless);
    assert!(
        out.contains("browser_sandbox = false  to [environments.development] in feldspar.toml"),
        "{out}"
    );
    let out = check(&broken);
    assert!(
        out.contains("it does not start") && out.contains("| error while loading shared libraries"),
        "{out}"
    );

    // The command it ran: a profile directory of its own, removed after.
    let argv = browser_check_argv(Path::new("/usr/bin/chromium"), Some("feldspar"), false);
    assert_eq!(argv[..5], ["su", "-s", "/bin/sh", "feldspar", "-c"]);
    assert!(argv[5].contains("--no-sandbox") && argv[5].contains("rm -rf \"$d\""));
    fs::remove_dir_all(&dir).ok();
}

/// A distribution of `family` with this `ID`, systemd, and nothing installed.
fn bare_on(family: Family, os_id: &str, version_id: &str) -> Host {
    Host {
        os_id: os_id.into(),
        version_id: version_id.into(),
        ..bare(host(family, ServiceManager::Systemd))
    }
}

/// `--server` on Fedora/RHEL, openSUSE and Arch: its package
/// manager, PostgreSQL's first-run setup where the package leaves it undone
/// (and only when it is), Node.js and Chromium from the distribution, and
/// `useradd` for the account.
#[test]
fn each_family_gets_its_own_packages_database_setup_and_account() {
    let useradd = "useradd --system --user-group --home-dir /var/lib/feldspar --shell \
                   /usr/sbin/nologin feldspar";
    let cases: [(Host, &[&str]); 6] = [
        (
            bare_on(Family::Fedora, "fedora", "44"),
            &[
                "dnf install -y nodejs npm",
                "dnf install -y chromium",
                "dnf install -y postgresql-server postgresql",
                "postgresql-setup --initdb",
                "systemctl enable --now postgresql",
                useradd,
            ],
        ),
        (
            bare_on(Family::Fedora, "almalinux", "10.0"),
            &["dnf install -y epel-release", "dnf install -y chromium"],
        ),
        (
            bare_on(Family::Fedora, "rhel", "10.0"),
            &[
                "dnf install -y https://dl.fedoraproject.org/pub/epel/epel-release-latest-10.noarch.rpm",
                "dnf install -y chromium",
            ],
        ),
        (
            bare_on(Family::Suse, "opensuse-tumbleweed", "20261030"),
            &[
                "zypper --non-interactive install nodejs-default npm-default",
                "zypper --non-interactive install chromium",
                "zypper --non-interactive install postgresql-server postgresql",
                "systemctl enable --now postgresql",
                useradd,
            ],
        ),
        (
            bare_on(Family::Suse, "opensuse-leap", "16.0"),
            &["zypper --non-interactive install nodejs22 npm22"],
        ),
        (
            bare_on(Family::Arch, "arch", ""),
            &[
                "pacman -Syu --needed --noconfirm nodejs npm",
                "pacman -Syu --needed --noconfirm chromium",
                "pacman -Syu --needed --noconfirm postgresql",
                "systemctl enable --now postgresql",
                useradd,
            ],
        ),
    ];
    for (host, wanted) in cases {
        let steps = plan(&server_choices(&[], &host), &host, None).expect("plan");
        let ran = commands(&steps);
        for want in wanted {
            assert!(
                ran.iter().any(|c| c == want),
                "{} {}: no `{want}` in\n{}",
                host.os_id,
                host.version_id,
                ran.join("\n")
            );
        }
        assert!(
            !ran.iter()
                .any(|c| c.contains("apt-get") || c.contains("adduser")),
            "{}: {ran:#?}",
            host.os_id
        );
        // A systemd unit, as on Debian.
        assert!(
            contents(written(&steps, "/etc/systemd/system/feldspar.service"))
                .contains("ExecStart=/opt/feldspar/bin/feldspar serve --environment production")
        );
    }

    // The first-run setups are skipped where the data directory exists.
    let unless = |host: &Host, cmd: &str| -> String {
        plan(&server_choices(&[], host), host, None)
            .unwrap()
            .into_iter()
            .find_map(|s| match s {
                Step::Run {
                    argv,
                    unless: Some(probe),
                    ..
                } if argv.join(" ").contains(cmd) => Some(probe.join(" ")),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no guarded `{cmd}`"))
    };
    assert_eq!(
        unless(&bare_on(Family::Fedora, "fedora", "44"), "postgresql-setup"),
        "test -f /var/lib/pgsql/data/PG_VERSION"
    );
    let arch = bare_on(Family::Arch, "arch", "");
    assert_eq!(
        unless(&arch, "initdb"),
        "test -f /var/lib/postgres/data/PG_VERSION"
    );
    // Arch's initdb is told peer authentication: its default is `trust`.
    assert!(
        commands(&plan(&server_choices(&[], &arch), &arch, None).unwrap())
            .iter()
            .any(|c| c.starts_with("su -s /bin/sh postgres -c") && c.contains("--auth-local=peer")),
    );
}

/// A binary with the release's bundled CPython beside it gets its HTTPS
/// certificates checked as the service account, before the service file is
/// written and read; a source build (no `python/` beside it) does not.
#[test]
fn the_bundled_python_has_its_certificates_checked_before_the_service_starts() {
    let deb = debian();
    let mut c = server_choices(&[], &deb);
    let without: Vec<Step> = plan(&c, &deb, None).unwrap();
    assert!(
        !without
            .iter()
            .any(|s| matches!(s, Step::CheckPythonTls { .. })),
        "{without:#?}"
    );

    c.bundled_python = true;
    let steps = plan(&c, &deb, None).unwrap();
    let check = steps
        .iter()
        .position(|s| matches!(s, Step::CheckPythonTls { .. }))
        .expect("a certificate check");
    assert_eq!(
        steps[check],
        Step::CheckPythonTls {
            binary: "/opt/feldspar/bin/feldspar".into(),
            user: "feldspar".into(),
            file: SYSTEMD_TLS_DROPIN.into(),
            openrc: false,
        }
    );
    let account = steps
        .iter()
        .position(|s| matches!(s, Step::Run { argv, .. } if argv[0] == "adduser"))
        .unwrap();
    let reload = steps
        .iter()
        .position(
            |s| matches!(s, Step::Run { argv, .. } if argv.join(" ") == "systemctl daemon-reload"),
        )
        .unwrap();
    assert!(account < check && check < reload, "{steps:#?}");

    // Under OpenRC the setting goes where OpenRC sources it.
    let alp = alpine();
    let mut c = server_choices(&[], &alp);
    c.bundled_python = true;
    assert!(
        plan(&c, &alp, None)
            .unwrap()
            .contains(&Step::CheckPythonTls {
                binary: "/opt/feldspar/bin/feldspar".into(),
                user: "feldspar".into(),
                file: OPENRC_CONF.into(),
                openrc: true,
            })
    );

    // What it runs: the self-check as the account, with the caller's
    // certificate settings removed and, on the retry, one bundle named.
    let argv = python_check_argv(
        Path::new("/opt/feldspar/bin/feldspar"),
        "feldspar",
        Some("/etc/pki/tls/certs/ca-bundle.crt"),
    );
    assert_eq!(argv[..5], ["su", "-s", "/bin/sh", "feldspar", "-c"]);
    assert_eq!(
        argv[5],
        "cd / && env -u SSL_CERT_FILE -u SSL_CERT_DIR \
         SSL_CERT_FILE=/etc/pki/tls/certs/ca-bundle.crt /opt/feldspar/bin/feldspar python check"
    );
    let dropin = tls_setting("/etc/ssl/ca-bundle.pem", false);
    assert!(dropin.contains("[Service]\nEnvironment=SSL_CERT_FILE=/etc/ssl/ca-bundle.pem\n"));
    assert!(
        tls_setting("/etc/ssl/ca-bundle.pem", true)
            .contains("export SSL_CERT_FILE=\"/etc/ssl/ca-bundle.pem\"")
    );
    let mut shown = Vec::new();
    describe(&steps, &deb, &mut shown);
    let shown = String::from_utf8(shown).unwrap();
    assert!(
        shown.contains("python check") && shown.contains(SYSTEMD_TLS_DROPIN),
        "{shown}"
    );
}
