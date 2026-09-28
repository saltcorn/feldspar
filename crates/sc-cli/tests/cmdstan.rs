//! `feldspar cmdstan status | install`: the arguments and the status report.
//!
//! Discovery and the installer themselves are tested in `sc-stan`; the
//! download and the build are exercised by running the install on a real
//! machine (TODO Phase 0.4), not here.

use std::path::PathBuf;

use sc_cli::cmdstan::{CmdStanArgs, install_root, status_report};
use sc_stan::cmdstan::{CmdStan, Source, Toolchain};

fn args(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

#[test]
fn status_and_install_parse_their_own_flags() {
    assert_eq!(
        CmdStanArgs::parse(&args("status")).unwrap(),
        CmdStanArgs::Status { cmdstan: None }
    );
    assert_eq!(
        CmdStanArgs::parse(&args("status --cmdstan /opt/cmdstan-2.40.0")).unwrap(),
        CmdStanArgs::Status {
            cmdstan: Some(PathBuf::from("/opt/cmdstan-2.40.0"))
        }
    );
    // One job unless asked: the build's memory is the reason.
    assert_eq!(
        CmdStanArgs::parse(&args("install")).unwrap(),
        CmdStanArgs::Install {
            version: None,
            dir: None,
            jobs: 1
        }
    );
    assert_eq!(
        CmdStanArgs::parse(&args("install --version v2.40 --dir /x --jobs 4")).unwrap(),
        CmdStanArgs::Install {
            version: Some("2.40.0".parse().unwrap()),
            dir: Some(PathBuf::from("/x")),
            jobs: 4
        }
    );

    for (line, needle) in [
        ("", "usage"),
        ("frobnicate", "unknown cmdstan subcommand"),
        ("status --jobs 2", "unknown argument `--jobs`"),
        ("install --cmdstan /x", "unknown argument `--cmdstan`"),
        ("install --jobs 0", "at least 1"),
        ("install --jobs lots", "at least 1"),
        ("install --version two", "not a CmdStan version"),
        ("install --dir", "needs a value"),
    ] {
        let err = CmdStanArgs::parse(&args(line)).unwrap_err().to_string();
        assert!(err.contains(needle), "{line:?}: {err}");
    }
}

#[test]
fn install_goes_to_the_named_directory_else_home() {
    assert_eq!(
        install_root(Some(PathBuf::from("/x"))).unwrap(),
        PathBuf::from("/x")
    );
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        assert_eq!(
            install_root(None).unwrap(),
            PathBuf::from(home).join(".cmdstan")
        );
    }
}

#[test]
fn the_status_report_says_what_is_missing() {
    let dir = std::env::temp_dir().join(format!("sc-cli-cmdstan-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("makefile"), "CMDSTAN_VERSION := 2.40.0\n").unwrap();
    let cmdstan = CmdStan::at(&dir, Source::Flag).unwrap();
    let tools = Toolchain {
        make: Some(PathBuf::from("/usr/bin/make")),
        cxx_name: "g++".into(),
        cxx: Some(PathBuf::from("/usr/bin/g++")),
    };

    // Found, but `make build` never ran.
    let (report, ready) = status_report(&Ok(cmdstan.clone()), &tools);
    assert!(!ready);
    assert!(
        report.contains("CmdStan 2.40.0 at") && report.contains("from --cmdstan"),
        "{report}"
    );
    assert!(report.contains("built:  no"), "{report}");

    // Built, with both tools: ready.
    for product in cmdstan.build_products() {
        std::fs::write(product, "").unwrap();
    }
    let (report, ready) = status_report(&Ok(cmdstan.clone()), &tools);
    assert!(ready, "{report}");
    assert!(
        report.contains("built:  yes") && report.contains("/usr/bin/g++"),
        "{report}"
    );

    // Built, but no compiler: not ready, and it says so.
    let no_cxx = Toolchain {
        cxx: None,
        ..tools.clone()
    };
    let (report, ready) = status_report(&Ok(cmdstan), &no_cxx);
    assert!(!ready);
    assert!(report.contains("no C++ compiler"), "{report}");

    // Not found: discovery's own sentence is the report.
    let absent = sc_stan::cmdstan::discover(&sc_stan::cmdstan::Locations {
        root: Some(dir.join("nothing-here")),
        ..Default::default()
    });
    let (report, ready) = status_report(&absent, &tools);
    assert!(!ready);
    assert!(report.contains("feldspar cmdstan install"), "{report}");
    std::fs::remove_dir_all(&dir).unwrap();
}
