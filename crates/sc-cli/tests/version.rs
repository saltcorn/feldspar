//! `feldspar --version`: the line Settings → System shows, on stdout, so an
//! operator on the machine can answer "which version is installed?" without
//! signing in.

use std::process::Command;

#[test]
fn version_prints_the_system_tabs_line() {
    for flag in ["--version", "-V", "version"] {
        let out = Command::new(env!("CARGO_BIN_EXE_feldspar"))
            .arg(flag)
            .output()
            .expect("run feldspar");
        assert!(out.status.success(), "{flag}: {out:?}");
        let stdout = String::from_utf8(out.stdout).expect("utf-8");
        assert_eq!(stdout.trim_end(), sc_server::version_line(), "{flag}");
        assert!(
            stdout.starts_with(&format!("feldspar {} · ", env!("CARGO_PKG_VERSION"))),
            "{stdout}"
        );
    }
}
