//! `feldspar cmdstan status | install` (TODO "Bayesian models with Stan" §20).
//!
//! Neither command touches a database: `status` reports what
//! `sc_stan::cmdstan::discover` finds on this machine, and `install` downloads
//! and builds a CmdStan release. The install is something an operator runs on
//! purpose from a shell — the server never downloads or builds anything on its
//! own.

use std::path::PathBuf;

use sc_error::{Error, Result};
use sc_stan::cmdstan::{CmdStan, InstallEvent, Locations, Toolchain, Version, default_root};

/// A parsed `feldspar cmdstan …` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdStanArgs {
    /// `status [--cmdstan DIR]`.
    Status { cmdstan: Option<PathBuf> },
    /// `install [--version V] [--dir D] [--jobs J]`.
    Install {
        version: Option<Version>,
        dir: Option<PathBuf>,
        jobs: u32,
    },
}

const USAGE: &str = "usage: feldspar cmdstan status [--cmdstan DIR]\n       \
                     feldspar cmdstan install [--version V] [--dir D] [--jobs J]";

impl CmdStanArgs {
    pub fn parse(args: &[String]) -> Result<CmdStanArgs> {
        let sub = args.first().map(String::as_str);
        let mut parsed = match sub {
            Some("status") => CmdStanArgs::Status { cmdstan: None },
            Some("install") => CmdStanArgs::Install {
                version: None,
                dir: None,
                // One job: a CmdStan build is a C++ compile of 1–2 GB per job,
                // and running out of memory halfway is the expensive failure.
                jobs: 1,
            },
            Some(other) => {
                return Err(Error::config(format!(
                    "unknown cmdstan subcommand `{other}`; they are status and install\n{USAGE}"
                )));
            }
            None => return Err(Error::config(USAGE)),
        };
        let mut rest = args[1..].iter();
        while let Some(arg) = rest.next() {
            let mut value = |flag: &str| -> Result<String> {
                rest.next()
                    .cloned()
                    .ok_or_else(|| Error::config(format!("{flag} needs a value")))
            };
            match (&mut parsed, arg.as_str()) {
                (CmdStanArgs::Status { cmdstan }, "--cmdstan") => {
                    *cmdstan = Some(PathBuf::from(value("--cmdstan")?));
                }
                (CmdStanArgs::Install { version, .. }, "--version") => {
                    *version = Some(value("--version")?.parse()?);
                }
                (CmdStanArgs::Install { dir, .. }, "--dir") => {
                    *dir = Some(PathBuf::from(value("--dir")?));
                }
                (CmdStanArgs::Install { jobs, .. }, "--jobs") => {
                    let text = value("--jobs")?;
                    *jobs = match text.parse::<u32>() {
                        Ok(n) if n >= 1 => n,
                        _ => {
                            return Err(Error::config(format!(
                                "--jobs must be a whole number of at least 1, not `{text}`"
                            )));
                        }
                    };
                }
                (_, other) => {
                    return Err(Error::config(format!(
                        "unknown argument `{other}` for `feldspar cmdstan {}`\n{USAGE}",
                        sub.unwrap_or_default()
                    )));
                }
            }
        }
        Ok(parsed)
    }
}

/// `--dir`, else `~/.cmdstan`.
pub fn install_root(dir: Option<PathBuf>) -> Result<PathBuf> {
    dir.or_else(default_root).ok_or_else(|| {
        Error::config("no home directory to install into; pass --dir to say where CmdStan goes")
    })
}

/// What `feldspar cmdstan status` prints, and whether Stan is ready to use:
/// a CmdStan of a supported version, built, with `make` and a compiler.
pub fn status_report(found: &Result<CmdStan>, toolchain: &Toolchain) -> (String, bool) {
    let mut out = String::new();
    let mut ready = toolchain.ready();
    let tool = |path: &Option<PathBuf>| match path {
        Some(path) => path.display().to_string(),
        None => "not found".to_owned(),
    };
    match found {
        Ok(cmdstan) => {
            out.push_str(&format!(
                "CmdStan {} at {} ({})\n",
                cmdstan.version,
                cmdstan.dir.display(),
                cmdstan.source
            ));
            let built = cmdstan.built();
            ready &= built;
            out.push_str(&format!(
                "  built:  {}\n",
                match built {
                    true => format!("yes ({})", cmdstan.stanc().display()),
                    false => "no — run `make build` in that directory".to_owned(),
                }
            ));
        }
        Err(e) => {
            ready = false;
            out.push_str(&format!("{e}\n"));
        }
    }
    out.push_str(&format!("  make:   {}\n", tool(&toolchain.make)));
    out.push_str(&format!("  C++:    {}\n", tool(&toolchain.cxx)));
    for missing in toolchain.missing() {
        out.push_str(&format!("  {missing}\n"));
    }
    out.push_str(match ready {
        true => "Stan models can be compiled and fitted on this machine.\n",
        false => "Stan models cannot be fitted on this machine yet.\n",
    });
    (out, ready)
}

/// `feldspar cmdstan status`: print the report; fail (after printing) when
/// Stan is not ready, so a provisioning script can test for it.
pub fn status(cmdstan: Option<PathBuf>) -> Result<()> {
    let locations = Locations::from_env(cmdstan);
    let found = sc_stan::cmdstan::discover(&locations);
    let (report, ready) = status_report(&found, &Toolchain::find(&locations));
    print!("{report}");
    match ready {
        true => Ok(()),
        false => Err(Error::config("CmdStan is not ready")),
    }
}

/// `feldspar cmdstan install`: progress on stderr, the result on stdout.
///
/// Ctrl-C drops the install, which kills `make` and removes the half-finished
/// directory (see `sc_stan::cmdstan::install`).
pub async fn install(version: Option<Version>, dir: Option<PathBuf>, jobs: u32) -> Result<()> {
    let options = sc_stan::cmdstan::InstallOptions {
        version,
        root: install_root(dir)?,
        jobs,
    };
    let toolchain = Toolchain::find(&Locations::from_env(None));
    if !toolchain.ready() {
        return Err(Error::config(format!(
            "cannot build CmdStan here: {}",
            toolchain.missing().join("; ")
        )));
    }
    let mut last_percent = None;
    let mut on = |event: InstallEvent<'_>| match event {
        InstallEvent::Resolved { version, url, dir } => {
            eprintln!(
                "feldspar: installing CmdStan {version} into {}",
                dir.display()
            );
            eprintln!("feldspar: downloading {url}");
        }
        InstallEvent::Downloading { received, total } => {
            let mb = received as f64 / (1 << 20) as f64;
            match total {
                Some(total) if total > 0 => {
                    let percent = received * 100 / total;
                    if last_percent != Some(percent) {
                        last_percent = Some(percent);
                        eprintln!("feldspar:   {percent:>3}%  {mb:.1} MB");
                    }
                }
                _ => eprintln!("feldspar:   {mb:.1} MB"),
            }
        }
        InstallEvent::Unpacking => eprintln!("feldspar: unpacking"),
        InstallEvent::Building { jobs } => eprintln!(
            "feldspar: building with `make build -j{jobs}` (this takes a while{})",
            match jobs {
                1 => "; --jobs N builds in parallel if the machine has the memory",
                _ => "",
            }
        ),
        InstallEvent::Output(line) => eprintln!("  | {line}"),
    };
    let installed = tokio::select! {
        installed = sc_stan::cmdstan::install(&options, &mut on) => installed?,
        _ = tokio::signal::ctrl_c() => {
            return Err(Error::config("interrupted; the partial install was removed"));
        }
    };
    let cmdstan = installed.cmdstan();
    match &installed {
        sc_stan::cmdstan::Installed::Already(_) => println!(
            "CmdStan {} is already installed at {}",
            cmdstan.version,
            cmdstan.dir.display()
        ),
        sc_stan::cmdstan::Installed::Fresh(_) => println!(
            "CmdStan {} installed at {}",
            cmdstan.version,
            cmdstan.dir.display()
        ),
    }
    Ok(())
}
