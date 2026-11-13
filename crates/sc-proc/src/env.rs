//! The environment every child starts from: [`BASE_ENV`] and
//! [`BASE_ENV_PREFIXES`], taken from the server's own, and nothing else.

use std::ffi::OsString;

/// The variables a child keeps from the server's environment, when they are
/// set: what a program needs to find its tools, its home and temporary
/// directories, its locale, and the network the operator configured (proxies
/// and certificate bundles — an `npm install` behind a corporate proxy needs
/// both). Not the server's database URL, its passwords, its keys or any
/// `FELDSPAR_*` setting.
pub const BASE_ENV: [&str; 39] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    // Windows: without these, much of what a program links fails to start.
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// Families of variables a child keeps: the locale's.
pub const BASE_ENV_PREFIXES: [&str; 1] = ["LC_"];

/// Whether the server's variable `name` is one a child keeps.
fn kept(name: &str) -> bool {
    let matches = |a: &str, b: &str| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    BASE_ENV.iter().any(|kept| matches(kept, name))
        || BASE_ENV_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// [`BASE_ENV`] and [`BASE_ENV_PREFIXES`] as the server has them now.
pub(crate) fn base() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| name.to_str().is_some_and(kept))
        .collect()
}

/// The server's variables whose names start with `prefix` and that
/// [`EnvName`](crate::EnvName) would accept.
pub(crate) fn with_prefix(prefix: &str) -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(|n| n.starts_with(prefix) && crate::EnvName::new(n).is_ok())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_keeps_the_locale_and_the_path_and_nothing_secret() {
        assert!(kept("PATH"));
        assert!(kept("LC_ALL"));
        assert!(kept("https_proxy"));
        for secret in [
            "DATABASE_URL",
            "PGPASSWORD",
            "FELDSPAR_BACKUP_PASSWORD",
            "AWS_SECRET_ACCESS_KEY",
            "ANTHROPIC_API_KEY",
            "LD_PRELOAD",
            "NODE_OPTIONS",
        ] {
            assert!(!kept(secret), "{secret}");
        }
    }
}
