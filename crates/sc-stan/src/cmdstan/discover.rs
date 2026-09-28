//! Finding a CmdStan, and the tools compiling a Stan program needs besides.
//!
//! The order of authority (TODO §20) is the operator's word first — the
//! `--cmdstan` flag, then `$CMDSTAN` — and the convention last: the newest
//! `cmdstan-*` directory under `~/.cmdstan`, which is where cmdstanpy's
//! `install_cmdstan` and our own `feldspar cmdstan install` both put one.
//!
//! **A named directory that is wrong is an error, not a fall-through.** An
//! operator who passed `--cmdstan /opt/cmdstan-2.36.0` and mistyped it wants to
//! be told, not to have a different CmdStan picked up from their home
//! directory — that would be a fit that runs, on a version nobody chose.
//!
//! Everything read from the process environment is gathered into [`Locations`]
//! first, so the rules can be tested against a fake directory and a fake
//! `PATH` without touching the real ones.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use sc_error::{Error, Result};

use super::version::{MIN_VERSION, Version};

/// Where a CmdStan was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--cmdstan <dir>`.
    Flag,
    /// `$CMDSTAN`.
    Env,
    /// The newest `cmdstan-*` under the default root (`~/.cmdstan`).
    Default,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Flag => "from --cmdstan",
            Source::Env => "from $CMDSTAN",
            Source::Default => "the newest under ~/.cmdstan",
        })
    }
}

/// Everything discovery reads from outside: the flag, and the four environment
/// variables it consults.
#[derive(Debug, Clone, Default)]
pub struct Locations {
    /// `--cmdstan <dir>`.
    pub flag: Option<PathBuf>,
    /// `$CMDSTAN`.
    pub env: Option<PathBuf>,
    /// The directory holding `cmdstan-*` installs: `~/.cmdstan`, or `None` on a
    /// machine with no home directory.
    pub root: Option<PathBuf>,
    /// `$PATH`, searched for `make` and the compiler.
    pub path: Option<OsString>,
    /// `$CXX`, which CmdStan's makefiles honour, so we look for that compiler
    /// rather than guessing another.
    pub cxx: Option<OsString>,
}

impl Locations {
    /// This process's environment, with `flag` as the `--cmdstan` value.
    pub fn from_env(flag: Option<PathBuf>) -> Self {
        Locations {
            flag,
            env: env_var("CMDSTAN").map(PathBuf::from),
            root: default_root(),
            path: env_var("PATH"),
            cxx: env_var("CXX"),
        }
    }
}

/// `~/.cmdstan`: where installs go by default, and where discovery looks last.
pub fn default_root() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        env_var("USERPROFILE")
    } else {
        env_var("HOME")
    }?;
    Some(PathBuf::from(home).join(".cmdstan"))
}

/// Read an environment variable, treating empty as absent.
fn env_var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|v| !v.is_empty())
}

/// A CmdStan directory of a supported version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdStan {
    pub dir: PathBuf,
    pub version: Version,
    pub source: Source,
}

impl CmdStan {
    /// The CmdStan at `dir`, which the operator named through `source`.
    ///
    /// Refused if `dir` is missing, is not a CmdStan directory, or is older
    /// than [`MIN_VERSION`] — each by name.
    pub fn at(dir: &Path, source: Source) -> Result<CmdStan> {
        let what = || format!("{} ({source})", dir.display());
        if !dir.is_dir() {
            return Err(Error::config(format!(
                "CmdStan directory {} does not exist",
                what()
            )));
        }
        let version = Version::of_dir(dir).ok_or_else(|| {
            Error::config(format!(
                "{} is not a CmdStan directory: it has no makefile naming CMDSTAN_VERSION",
                what()
            ))
        })?;
        let found = CmdStan {
            dir: dir.to_path_buf(),
            version,
            source,
        };
        found.check_version()?;
        Ok(found)
    }

    fn check_version(&self) -> Result<()> {
        match self.version.supported() {
            true => Ok(()),
            false => Err(Error::config(format!(
                "CmdStan {} at {} ({}) is too old: {}.{} or newer is required. Install a \
                 newer one with `feldspar cmdstan install`",
                self.version,
                self.dir.display(),
                self.source,
                MIN_VERSION.major,
                MIN_VERSION.minor
            ))),
        }
    }

    /// `bin/stanc`, the Stan-to-C++ compiler `make build` puts in place.
    pub fn stanc(&self) -> PathBuf {
        self.dir.join("bin").join(exe("stanc"))
    }

    /// What `make build` leaves in `bin/` — `stanc` and the three tools
    /// built after it — each of which must exist.
    pub fn build_products(&self) -> Vec<PathBuf> {
        BUILD_PRODUCTS
            .iter()
            .map(|tool| self.dir.join("bin").join(exe(tool)))
            .collect()
    }

    /// Whether `make build` has finished here. Without it the first model
    /// compile would spend minutes building CmdStan's own libraries before
    /// failing on something else.
    ///
    /// `stanc` alone is not the answer: it is fetched *first*, so an
    /// interrupted build has one. The tools are built last, after the main
    /// object and the precompiled header.
    pub fn built(&self) -> bool {
        self.build_products().iter().all(|p| p.is_file())
    }
}

/// The files in `bin/` a finished `make build` has made (CmdStan's `build`
/// target: `bin/stanc … bin/stansummary bin/print bin/diagnose`).
const BUILD_PRODUCTS: [&str; 4] = ["stanc", "stansummary", "print", "diagnose"];

/// `name` with the platform's executable suffix.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Find the CmdStan this process should use (see the module documentation for
/// the order).
pub fn discover(locations: &Locations) -> Result<CmdStan> {
    if let Some(dir) = &locations.flag {
        return CmdStan::at(dir, Source::Flag);
    }
    if let Some(dir) = &locations.env {
        return CmdStan::at(dir, Source::Env);
    }
    let Some(root) = &locations.root else {
        return Err(Error::not_found(
            "no CmdStan on this machine: $CMDSTAN is not set and there is no home directory to \
             look for ~/.cmdstan in. Pass --cmdstan <dir> or set $CMDSTAN",
        ));
    };
    let installs = installs_under(root);
    // The newest *supported* install, preferring one that is built: an
    // unfinished 2.40 beside a working 2.37 should not make Stan unavailable.
    let best = installs
        .iter()
        .filter(|found| found.version.supported())
        .max_by(|a, b| (a.built(), &a.version).cmp(&(b.built(), &b.version)));
    if let Some(found) = best {
        return Ok(found.clone());
    }
    match installs.iter().max_by(|a, b| a.version.cmp(&b.version)) {
        Some(newest) => newest.check_version().map(|()| newest.clone()),
        None => Err(Error::not_found(format!(
            "no CmdStan on this machine: $CMDSTAN is not set and {} has no cmdstan-* directory. \
             Install one with `feldspar cmdstan install`, or pass --cmdstan <dir>",
            root.display()
        ))),
    }
}

/// Every `cmdstan-*` directory under `root` with a readable version, of any
/// version. A missing or unreadable `root` has none.
fn installs_under(root: &Path) -> Vec<CmdStan> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("cmdstan-"))
        .filter_map(|entry| {
            let dir = entry.path();
            let version = Version::of_dir(&dir)?;
            Some(CmdStan {
                dir,
                version,
                source: Source::Default,
            })
        })
        .collect()
}

/// `make` and a C++ compiler, as found on `PATH`.
///
/// CmdStan cannot compile a model without both, and neither is something it
/// brings: a CmdStan that is found and built on a machine whose compiler has
/// since been removed is not a working Stan, and `feldspar cmdstan status` says
/// which half is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    pub make: Option<PathBuf>,
    /// The compiler that was looked for: `$CXX`, or the first of the usual
    /// names found (the last tried when none is).
    pub cxx_name: String,
    pub cxx: Option<PathBuf>,
}

impl Toolchain {
    /// Look for `make` and the compiler on `locations.path`.
    pub fn find(locations: &Locations) -> Toolchain {
        let path = locations.path.as_deref();
        let make = which(OsStr::new("make"), path);
        // `$CXX` may carry a wrapper and flags (`ccache g++ -std=…`); the
        // program is its first word.
        let wanted: Vec<OsString> = match &locations.cxx {
            Some(cxx) => cxx
                .to_string_lossy()
                .split_whitespace()
                .next()
                .map(|first| vec![OsString::from(first)])
                .unwrap_or_default(),
            None if cfg!(target_os = "macos") => vec!["clang++".into(), "g++".into()],
            None => vec!["g++".into(), "clang++".into(), "c++".into()],
        };
        let found = wanted
            .iter()
            .find_map(|name| which(name, path).map(|p| (name, p)));
        let (cxx_name, cxx) = match found {
            Some((name, p)) => (name.to_string_lossy().into_owned(), Some(p)),
            None => (
                wanted
                    .last()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "g++".to_owned()),
                None,
            ),
        };
        Toolchain {
            make,
            cxx_name,
            cxx,
        }
    }

    /// What is missing, one sentence each; empty when both were found.
    pub fn missing(&self) -> Vec<String> {
        let mut missing = Vec::new();
        if self.make.is_none() {
            missing.push("`make` is not on the PATH".to_owned());
        }
        if self.cxx.is_none() {
            missing.push(format!(
                "no C++ compiler was found on the PATH (looked for `{}`; set $CXX to use \
                 another)",
                self.cxx_name
            ));
        }
        missing
    }

    pub fn ready(&self) -> bool {
        self.make.is_some() && self.cxx.is_some()
    }
}

/// `name` resolved the way a shell would: a name with a path separator is
/// taken as a path, anything else is searched for on `path`.
fn which(name: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    let as_path = Path::new(name);
    if as_path.components().count() > 1 {
        return is_executable(as_path).then(|| as_path.to_path_buf());
    }
    let mut file = name.to_os_string();
    file.push(std::env::consts::EXE_SUFFIX);
    std::env::split_paths(path?)
        .map(|dir| dir.join(&file))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "sc-stan-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fake CmdStan: the makefile line discovery reads, and what `make
    /// build` leaves in `bin/` when `built`.
    fn fake_cmdstan(dir: &Path, version: &str, built: bool) {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(
            dir.join("makefile"),
            format!("# CmdStan makefile\nCMDSTAN_VERSION := {version}\n"),
        )
        .unwrap();
        if built {
            for tool in BUILD_PRODUCTS {
                std::fs::write(dir.join("bin").join(exe(tool)), "").unwrap();
            }
        }
    }

    /// An executable file called `name` in `dir`.
    fn fake_tool(dir: &Path, name: &str) {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn root_only(root: &Path) -> Locations {
        Locations {
            root: Some(root.to_path_buf()),
            ..Locations::default()
        }
    }

    #[test]
    fn the_newest_built_install_under_the_root_is_found() {
        let root = Scratch::new("root");
        fake_cmdstan(&root.0.join("cmdstan-2.34.1"), "2.34.1", true);
        fake_cmdstan(&root.0.join("cmdstan-2.36.0"), "2.36.0", true);
        // Newer, but never built: a working 2.36 beats it.
        fake_cmdstan(&root.0.join("cmdstan-2.37.0"), "2.37.0", false);
        // Not a CmdStan at all, and not named like one: both ignored.
        std::fs::create_dir_all(root.0.join("cmdstan-9.9.9")).unwrap();
        fake_cmdstan(&root.0.join("other"), "3.0.0", true);

        let found = discover(&root_only(&root.0)).unwrap();
        assert_eq!(found.version.to_string(), "2.36.0");
        assert_eq!(found.dir, root.0.join("cmdstan-2.36.0"));
        assert_eq!(found.source, Source::Default);
        assert!(found.built());
    }

    #[test]
    fn an_unbuilt_install_is_found_when_it_is_the_only_one() {
        let root = Scratch::new("unbuilt");
        let dir = root.0.join("cmdstan-2.37.0");
        fake_cmdstan(&dir, "2.37.0", false);
        // `stanc` is the first thing `make build` puts in place, so an
        // interrupted build has it; that is still not a built CmdStan.
        std::fs::write(dir.join("bin").join(exe("stanc")), "").unwrap();
        let found = discover(&root_only(&root.0)).unwrap();
        assert_eq!(found.version.to_string(), "2.37.0");
        assert!(!found.built());
    }

    #[test]
    fn the_flag_outranks_the_environment_which_outranks_the_root() {
        let scratch = Scratch::new("order");
        let (flag, env, root) = (
            scratch.0.join("flag"),
            scratch.0.join("env"),
            scratch.0.join("root"),
        );
        fake_cmdstan(&flag, "2.34.0", true);
        fake_cmdstan(&env, "2.35.0", true);
        fake_cmdstan(&root.join("cmdstan-2.36.0"), "2.36.0", true);
        let mut locations = Locations {
            flag: Some(flag.clone()),
            env: Some(env.clone()),
            root: Some(root),
            ..Locations::default()
        };
        let found = discover(&locations).unwrap();
        assert_eq!((found.dir, found.source), (flag, Source::Flag));
        locations.flag = None;
        let found = discover(&locations).unwrap();
        assert_eq!((found.dir, found.source), (env, Source::Env));
        locations.env = None;
        assert_eq!(discover(&locations).unwrap().source, Source::Default);
    }

    #[test]
    fn a_named_directory_that_is_wrong_is_refused_rather_than_skipped() {
        let scratch = Scratch::new("wrong");
        let root = scratch.0.join("root");
        fake_cmdstan(&root.join("cmdstan-2.36.0"), "2.36.0", true);
        let missing = scratch.0.join("typo");
        let not_cmdstan = scratch.0.join("empty");
        std::fs::create_dir_all(&not_cmdstan).unwrap();

        let err = discover(&Locations {
            flag: Some(missing.clone()),
            root: Some(root.clone()),
            ..Locations::default()
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("does not exist") && err.contains("typo"),
            "{err}"
        );

        let err = discover(&Locations {
            env: Some(not_cmdstan),
            root: Some(root),
            ..Locations::default()
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("not a CmdStan directory") && err.contains("$CMDSTAN"),
            "{err}"
        );
    }

    #[test]
    fn a_version_older_than_the_minimum_is_refused_by_name() {
        let scratch = Scratch::new("old");
        let old = scratch.0.join("cmdstan-2.32.2");
        fake_cmdstan(&old, "2.32.2", true);

        let err = discover(&Locations {
            flag: Some(old),
            ..Locations::default()
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("2.32.2") && err.contains("too old") && err.contains("2.33"),
            "{err}"
        );

        // Under the root, an old install is refused only when there is nothing
        // newer to use instead.
        let err = discover(&root_only(&scratch.0)).unwrap_err().to_string();
        assert!(err.contains("2.32.2") && err.contains("too old"), "{err}");
        fake_cmdstan(&scratch.0.join("cmdstan-2.33.0"), "2.33.0", false);
        assert_eq!(
            discover(&root_only(&scratch.0))
                .unwrap()
                .version
                .to_string(),
            "2.33.0"
        );
    }

    #[test]
    fn nothing_found_says_where_it_looked_and_what_to_do() {
        let root = Scratch::new("empty");
        let err = discover(&root_only(&root.0)).unwrap_err().to_string();
        assert!(
            err.contains("not found") && err.contains("feldspar cmdstan install"),
            "{err}"
        );
        assert!(err.contains(&root.0.display().to_string()), "{err}");
        // A root that does not exist at all is the same answer.
        let err = discover(&root_only(&root.0.join("nope")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no cmdstan-* directory"), "{err}");
        let err = discover(&Locations::default()).unwrap_err().to_string();
        assert!(err.contains("no home directory"), "{err}");
    }

    #[test]
    fn the_toolchain_is_make_and_a_compiler_on_the_path() {
        let bin = Scratch::new("bin");
        let mut locations = Locations {
            path: Some(bin.0.clone().into_os_string()),
            ..Locations::default()
        };

        let none = Toolchain::find(&locations);
        assert!(!none.ready());
        assert_eq!(none.missing().len(), 2, "{:?}", none.missing());

        // `make` alone: the compiler is what is missing, by name.
        fake_tool(&bin.0, "make");
        let no_compiler = Toolchain::find(&locations);
        assert_eq!(no_compiler.make, Some(bin.0.join("make")));
        assert!(!no_compiler.ready());
        let missing = no_compiler.missing();
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("C++ compiler"), "{missing:?}");

        fake_tool(&bin.0, "clang++");
        let ready = Toolchain::find(&locations);
        assert!(ready.ready(), "{:?}", ready.missing());
        assert_eq!(ready.cxx, Some(bin.0.join("clang++")));
        assert_eq!(ready.cxx_name, "clang++");

        // `$CXX` is the compiler CmdStan will use, so it is the one looked for —
        // not whichever of the usual names happens to exist.
        locations.cxx = Some("ccache my-g++ -O2".into());
        let named = Toolchain::find(&locations);
        assert_eq!(named.cxx, None);
        assert!(
            named.missing()[0].contains("`ccache`"),
            "{:?}",
            named.missing()
        );
        locations.cxx = Some("my-g++".into());
        fake_tool(&bin.0, "my-g++");
        assert_eq!(Toolchain::find(&locations).cxx, Some(bin.0.join("my-g++")));
    }
}
