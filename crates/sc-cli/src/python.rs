//! `feldspar python check` (TODO-portable-release R1.7).
//!
//! Starts the embedded interpreter with no database and asks it what a release
//! on a foreign distribution is most likely to get wrong: whether it found its
//! standard library, whether `ssl`, `sqlite3`, `ctypes` and `zoneinfo` load, and
//! whether a verified HTTPS request works with the host's certificates. The
//! release smoke test runs it on every distribution image, and an operator runs
//! it after an install; both read the exit status.

use std::path::PathBuf;

use sc_error::{Error, Result};
use sc_python::{PythonEnv, SelfCheck};

/// What the HTTPS check fetches by default: the index pip installs from, so a
/// pass means the one request a Python module install depends on would work.
pub const DEFAULT_CHECK_URL: &str = "https://pypi.org/simple/";

const USAGE: &str = "usage: feldspar python check [--url URL | --offline] \
                     [--python-dir PATH] [--python-bin PATH]";

/// A parsed `feldspar python …` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PythonArgs {
    /// `check [--url URL | --offline] [--python-dir PATH] [--python-bin PATH]`.
    Check {
        /// The HTTPS URL to fetch, or `None` for `--offline`.
        url: Option<String>,
        /// The environment flags `serve` takes, so the check describes the
        /// same environment the server would use.
        env: PythonEnv,
    },
}

impl PythonArgs {
    pub fn parse(args: &[String]) -> Result<PythonArgs> {
        match args.first().map(String::as_str) {
            Some("check") => {}
            Some(other) => {
                return Err(Error::config(format!(
                    "unknown python subcommand `{other}`; there is `check`\n{USAGE}"
                )));
            }
            None => return Err(Error::config(format!("python needs a subcommand\n{USAGE}"))),
        }
        let mut url = Some(DEFAULT_CHECK_URL.to_owned());
        let mut env = PythonEnv::default();
        let mut it = args[1..].iter();
        while let Some(arg) = it.next() {
            let mut value = || {
                it.next()
                    .cloned()
                    .ok_or_else(|| Error::config(format!("{arg} needs a value\n{USAGE}")))
            };
            match arg.as_str() {
                "--url" => url = Some(value()?),
                "--offline" => url = None,
                "--python-dir" => env.dir = Some(PathBuf::from(value()?)),
                "--python-bin" => env.bin = Some(PathBuf::from(value()?)),
                other => {
                    return Err(Error::config(format!(
                        "unknown python check argument `{other}`\n{USAGE}"
                    )));
                }
            }
        }
        Ok(PythonArgs::Check { url, env })
    }
}

/// The report, as the terminal shows it: the interpreter, then a line per
/// check.
pub fn format_check(found: &SelfCheck, pip: &std::path::Path) -> String {
    let mut out = format!(
        "python {} ({})\n  home        {}\n  executable  {}\n  pip runs    {}\n",
        found.version,
        if found.bundled {
            "bundled with this binary"
        } else {
            "found by CPython's own search"
        },
        found.home,
        found.executable,
        pip.display(),
    );
    for check in &found.checks {
        out.push_str(&format!(
            "{:<4} {:<15} {}\n",
            if check.ok { "ok" } else { "FAIL" },
            check.name,
            check.detail
        ));
    }
    out
}

/// `Ok` when every check passed, else the error the command exits with.
pub fn verdict(found: &SelfCheck) -> Result<()> {
    let failed: Vec<&str> = found
        .checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.name.as_str())
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "the Python self-check failed: {}",
            failed.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_python::SelfCheckItem;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn check_fetches_the_package_index_unless_told_otherwise() {
        assert_eq!(
            PythonArgs::parse(&args(&["check"])).unwrap(),
            PythonArgs::Check {
                url: Some(DEFAULT_CHECK_URL.to_owned()),
                env: PythonEnv::default()
            }
        );
        let PythonArgs::Check { url, env } =
            PythonArgs::parse(&args(&["check", "--offline", "--python-dir", "/srv/py"])).unwrap();
        assert_eq!(url, None);
        assert_eq!(env.dir, Some(PathBuf::from("/srv/py")));
        let PythonArgs::Check { url, .. } =
            PythonArgs::parse(&args(&["check", "--url", "https://example.org/"])).unwrap();
        assert_eq!(url.as_deref(), Some("https://example.org/"));
        assert!(PythonArgs::parse(&args(&[])).is_err());
        assert!(PythonArgs::parse(&args(&["doctor"])).is_err());
        assert!(PythonArgs::parse(&args(&["check", "--url"])).is_err());
        assert!(PythonArgs::parse(&args(&["check", "--verbose"])).is_err());
    }

    #[test]
    fn a_failed_check_is_printed_and_fails_the_command() {
        let found = SelfCheck {
            version: "3.13.18".to_owned(),
            home: "/opt/feldspar/python".to_owned(),
            executable: "/opt/feldspar/python/bin/python3".to_owned(),
            bundled: true,
            checks: vec![
                SelfCheckItem {
                    name: "import ssl".to_owned(),
                    ok: true,
                    detail: "OpenSSL 3.0".to_owned(),
                },
                SelfCheckItem {
                    name: "https".to_owned(),
                    ok: false,
                    detail: "URLError: certificate verify failed".to_owned(),
                },
            ],
        };
        let text = format_check(
            &found,
            std::path::Path::new("/opt/feldspar/python/bin/python3"),
        );
        assert!(
            text.starts_with("python 3.13.18 (bundled with this binary)\n"),
            "{text}"
        );
        assert!(
            text.contains("home        /opt/feldspar/python\n"),
            "{text}"
        );
        assert!(text.contains("FAIL https"), "{text}");
        assert!(text.contains("ok   import ssl"), "{text}");
        let error = verdict(&found).unwrap_err().to_string();
        assert!(error.contains("https") && !error.contains("ssl"), "{error}");
    }
}
