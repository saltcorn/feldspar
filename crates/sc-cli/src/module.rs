//! `feldspar module install|grant|configure|list|remove`: the Modules tab from
//! a terminal.
//!
//! A host provisioning script (or a root daemon applying an admin page's rows)
//! has no admin session, and writing `_fd_modules` by hand is not an install:
//! the package has to be in the modules root, and a running server has to
//! notice. So these commands run **the Modules tab's own path** —
//! `ModuleServices::install_module`, `update_module` and `remove_module` — in
//! this process, against the same database and the same modules root, and a
//! running server picks the change up on its next `SIGHUP`, which reloads the
//! modules whenever `_fd_modules` has changed:
//!
//! ```text
//! feldspar module install --local /opt/hosted-health --grant-read /run/health \
//!   && pkill -HUP feldspar
//! ```
//!
//! **Idempotent.** Installing a local directory whose `package.json` names the
//! version already installed from it changes nothing (unless `--force`); a
//! different version is installed over the old one, keeping the row's id, its
//! settings and its grants. Granting what is already granted, and revoking what
//! is not, change nothing either — so a provisioning script can run every line
//! on every run.
//!
//! This module is the **parsing** and the pure edits on a row; connecting,
//! installing and reloading are the binary's.

use std::path::{Path, PathBuf};

use sc_error::{Error, Result};
use sc_module::{ModuleLanguage, ModulePermissions};
use serde_json::Value as Json;

/// One permission list of a module's worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// `--read PATH`: a readable path.
    Read,
    /// `--write PATH`: a writable path.
    Write,
    /// `--net HOST`: a host it may connect to.
    Net,
    /// `--env VAR`: an environment variable it may see.
    Env,
}

impl Permission {
    /// The key the stored permission object uses.
    pub fn key(self) -> &'static str {
        match self {
            Permission::Read => sc_module::PERM_READ,
            Permission::Write => sc_module::PERM_WRITE,
            Permission::Net => sc_module::PERM_NET,
            Permission::Env => sc_module::PERM_ENV,
        }
    }

    fn list(self, permissions: &mut ModulePermissions) -> &mut Vec<String> {
        match self {
            Permission::Read => &mut permissions.read,
            Permission::Write => &mut permissions.write,
            Permission::Net => &mut permissions.net,
            Permission::Env => &mut permissions.env,
        }
    }
}

/// One grant: which list, and the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Which list.
    pub permission: Permission,
    /// The path, host or variable.
    pub value: String,
}

/// What `module` was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum ModuleCommand {
    /// `install --local DIR …`.
    Install(InstallArgs),
    /// `grant NAME (--read|--write|--net|--env) VALUE [--revoke]`.
    Grant {
        /// The module, by package name.
        name: String,
        /// What to grant or revoke.
        grant: Grant,
        /// Take it away instead.
        revoke: bool,
    },
    /// `configure NAME KEY=VALUE…`.
    Configure {
        /// The module, by package name.
        name: String,
        /// The settings to set, as typed.
        settings: Vec<(String, String)>,
    },
    /// `list [--json]`.
    List {
        /// Print JSON rather than one line per module.
        json: bool,
    },
    /// `remove NAME`.
    Remove {
        /// The module, by package name.
        name: String,
    },
}

/// `module install`'s arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct InstallArgs {
    /// `--local DIR`: the package directory.
    pub dir: PathBuf,
    /// `--python`: the directory is a Python package (pip), not an npm one.
    /// Detected from the directory when not given.
    pub python: bool,
    /// `--grant-read`/`--grant-write`/`--grant-net`/`--grant-env`, in order.
    pub grants: Vec<Grant>,
    /// `--setting KEY=VALUE`, in order. (Not `--config`, which every command
    /// already reads as the path of `feldspar.toml`.)
    pub settings: Vec<(String, String)>,
    /// `--force`: reinstall even when this version is already installed.
    pub force: bool,
    /// `--json`: print the module as JSON.
    pub json: bool,
}

const USAGE: &str = "\
feldspar module install --local DIR [--python] [--grant-read PATH]… [--grant-write PATH]…
                        [--grant-net HOST]… [--grant-env VAR]… [--setting KEY=VALUE]… [--force] [--json]
feldspar module grant NAME (--read|--write|--net|--env) VALUE [--revoke]
feldspar module configure NAME KEY=VALUE…
feldspar module list [--json]
feldspar module remove NAME";

/// Parse `module …`, the database flags (and `--modules-dir`) already taken
/// out.
pub fn parse(args: &[String]) -> Result<ModuleCommand> {
    let (verb, rest) = args
        .split_first()
        .ok_or_else(|| Error::config(format!("module needs a subcommand:\n{USAGE}")))?;
    match verb.as_str() {
        "install" => parse_install(rest).map(ModuleCommand::Install),
        "grant" => parse_grant(rest),
        "configure" => {
            let (name, rest) = name_first("configure", rest)?;
            if rest.is_empty() {
                return Err(Error::config(
                    "module configure needs at least one KEY=VALUE setting",
                ));
            }
            let settings = rest
                .iter()
                .map(|s| setting(s))
                .collect::<Result<Vec<_>>>()?;
            Ok(ModuleCommand::Configure { name, settings })
        }
        "list" => match rest {
            [] => Ok(ModuleCommand::List { json: false }),
            [flag] if flag == "--json" => Ok(ModuleCommand::List { json: true }),
            [other, ..] => Err(Error::config(format!(
                "unknown module list argument `{other}`; it takes --json"
            ))),
        },
        "remove" => {
            let (name, rest) = name_first("remove", rest)?;
            if let Some(extra) = rest.first() {
                return Err(Error::config(format!(
                    "module remove takes one module name, and got `{extra}` as well"
                )));
            }
            Ok(ModuleCommand::Remove { name })
        }
        other => Err(Error::config(format!(
            "unknown module subcommand `{other}`; usage:\n{USAGE}"
        ))),
    }
}

fn parse_install(args: &[String]) -> Result<InstallArgs> {
    let mut dir = None;
    let mut python = false;
    let mut grants = Vec::new();
    let mut settings = Vec::new();
    let mut force = false;
    let mut json = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| Error::config(format!("{flag} needs a value")))
        };
        let permission = match arg.as_str() {
            "--grant-read" => Some(Permission::Read),
            "--grant-write" => Some(Permission::Write),
            "--grant-net" => Some(Permission::Net),
            "--grant-env" => Some(Permission::Env),
            _ => None,
        };
        if let Some(permission) = permission {
            grants.push(Grant {
                permission,
                value: value(arg)?,
            });
            continue;
        }
        match arg.as_str() {
            "--local" => {
                if dir.replace(PathBuf::from(value("--local")?)).is_some() {
                    return Err(Error::config("module install takes one --local directory"));
                }
            }
            "--setting" => settings.push(setting(&value("--setting")?)?),
            "--python" => python = true,
            "--force" => force = true,
            "--json" => json = true,
            other => {
                return Err(Error::config(format!(
                    "unknown module install argument `{other}`; usage:\n{USAGE}"
                )));
            }
        }
    }
    let dir = dir.ok_or_else(|| {
        Error::config(
            "module install needs --local DIR: the package directory to install from \
             (an npm or registry install is the Modules tab's)",
        )
    })?;
    Ok(InstallArgs {
        dir,
        python,
        grants,
        settings,
        force,
        json,
    })
}

fn parse_grant(args: &[String]) -> Result<ModuleCommand> {
    let (name, rest) = name_first("grant", args)?;
    let mut grant = None;
    let mut revoke = false;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let permission = match arg.as_str() {
            "--read" => Permission::Read,
            "--write" => Permission::Write,
            "--net" => Permission::Net,
            "--env" => Permission::Env,
            "--revoke" => {
                revoke = true;
                continue;
            }
            other => {
                return Err(Error::config(format!(
                    "unknown module grant argument `{other}`; usage:\n{USAGE}"
                )));
            }
        };
        let value = it
            .next()
            .ok_or_else(|| Error::config(format!("{arg} needs a value")))?
            .clone();
        if grant.replace(Grant { permission, value }).is_some() {
            return Err(Error::config(
                "module grant changes one permission per run; run it once per entry",
            ));
        }
    }
    let grant = grant.ok_or_else(|| {
        Error::config("module grant needs one of --read PATH, --write PATH, --net HOST, --env VAR")
    })?;
    Ok(ModuleCommand::Grant {
        name,
        grant,
        revoke,
    })
}

/// The module name a subcommand starts with.
fn name_first<'a>(verb: &str, args: &'a [String]) -> Result<(String, &'a [String])> {
    match args.split_first() {
        Some((name, rest)) if !name.starts_with('-') => Ok((name.clone(), rest)),
        _ => Err(Error::config(format!(
            "module {verb} needs the module's name (its package name; `feldspar module list` \
             shows them)"
        ))),
    }
}

/// `KEY=VALUE`, split at the first `=`.
fn setting(raw: &str) -> Result<(String, String)> {
    match raw.split_once('=') {
        Some((key, value)) if !key.trim().is_empty() => {
            Ok((key.trim().to_owned(), value.to_owned()))
        }
        _ => Err(Error::config(format!(
            "`{raw}` is not a setting; write it as KEY=VALUE"
        ))),
    }
}

/// Add `grant` to `permissions`, or take it out. Returns whether anything
/// changed. The result is checked as the Modules tab's save checks it — a
/// relative path or a host with a scheme is refused, naming the entry.
pub fn apply_grant(
    permissions: &mut ModulePermissions,
    grant: &Grant,
    revoke: bool,
) -> Result<bool> {
    let mut changed = permissions.clone();
    let list = grant.permission.list(&mut changed);
    let present = list.contains(&grant.value);
    match (revoke, present) {
        (false, true) | (true, false) => return Ok(false),
        (false, false) => list.push(grant.value.clone()),
        (true, true) => list.retain(|v| v != &grant.value),
    }
    *permissions = ModulePermissions::from_json(&Json::Object(changed.to_json()))?;
    Ok(true)
}

/// Set `settings` on a module's configuration, each typed by the module's
/// own settings form when it declares the key (`min_role=80` is a number when
/// the field is an integer), and a string otherwise. Returns whether anything
/// changed.
pub fn apply_settings(
    configuration: &mut sc_types::Attrs,
    spec: &[sc_types::FormField],
    settings: &[(String, String)],
) -> Result<bool> {
    let mut changed = false;
    for (key, raw) in settings {
        let value = match spec.iter().find(|f| f.name() == key) {
            Some(field) => crate::config::value_for(field, raw)?,
            None => Json::String(raw.clone()),
        };
        crate::config::refuse_sentinel(key, &value)?;
        if configuration.get(key) != Some(&value) {
            configuration.insert(key.clone(), value);
            changed = true;
        }
    }
    Ok(changed)
}

/// Which package manager a local directory is for: npm when it has a
/// `package.json`, pip when it has a `pyproject.toml` or `setup.py`.
pub fn detect_language(dir: &Path, python: bool) -> Result<ModuleLanguage> {
    if python {
        return Ok(ModuleLanguage::Python);
    }
    if dir.join("package.json").is_file() {
        return Ok(ModuleLanguage::JavaScript);
    }
    if dir.join("pyproject.toml").is_file() || dir.join("setup.py").is_file() {
        return Ok(ModuleLanguage::Python);
    }
    Err(Error::invalid(format!(
        "{} is neither an npm package (no package.json) nor a Python one (no \
         pyproject.toml or setup.py)",
        dir.display()
    )))
}

/// The name and version an npm package directory declares.
pub fn npm_package(dir: &Path) -> Result<(String, String)> {
    let manifest = dir.join("package.json");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| Error::invalid(format!("reading {}: {e}", manifest.display())))?;
    let json: Json = serde_json::from_str(&text)
        .map_err(|e| Error::invalid(format!("reading {}: {e}", manifest.display())))?;
    let field = |key: &str| json.get(key).and_then(Json::as_str).map(str::to_owned);
    let name = field("name")
        .ok_or_else(|| Error::invalid(format!("{} names no package", manifest.display())))?;
    Ok((name, field("version").unwrap_or_else(|| "0.0.0".to_owned())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use sc_types::{BasicType, FormField};
    use serde_json::json;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn install_collects_grants_and_settings_in_order() {
        let ModuleCommand::Install(install) = parse(&args(
            "install --local /opt/health --grant-read /run/health --grant-net api.example \
             --setting min_role=80 --setting note=a=b --force",
        ))
        .unwrap() else {
            panic!("not an install");
        };
        assert_eq!(install.dir, PathBuf::from("/opt/health"));
        assert_eq!(
            install.grants,
            vec![
                Grant {
                    permission: Permission::Read,
                    value: "/run/health".into()
                },
                Grant {
                    permission: Permission::Net,
                    value: "api.example".into()
                },
            ]
        );
        assert_eq!(
            install.settings,
            vec![
                ("min_role".into(), "80".into()),
                ("note".into(), "a=b".into())
            ]
        );
        assert!(install.force && !install.json && !install.python);
    }

    #[test]
    fn what_cannot_be_done_is_refused() {
        for bad in [
            "",
            "install",
            "install --local",
            "install --local a --local b",
            "install --local a --grant-exec x",
            "install --local a --setting novalue",
            "grant",
            "grant hosted-health",
            "grant hosted-health --read",
            "grant hosted-health --read /a --write /b",
            "grant --read /a",
            "configure hosted-health",
            "configure hosted-health =x",
            "list --all",
            "remove",
            "remove a b",
            "upgrade a",
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad}");
        }
        assert_eq!(
            parse(&args("grant hosted-health --revoke --read /run/health")).unwrap(),
            ModuleCommand::Grant {
                name: "hosted-health".into(),
                grant: Grant {
                    permission: Permission::Read,
                    value: "/run/health".into()
                },
                revoke: true,
            }
        );
        assert_eq!(
            parse(&args("list --json")).unwrap(),
            ModuleCommand::List { json: true }
        );
    }

    #[test]
    fn a_grant_is_idempotent_and_checked() {
        let mut permissions = ModulePermissions::closed();
        let read = Grant {
            permission: Permission::Read,
            value: "/run/health".into(),
        };
        assert!(apply_grant(&mut permissions, &read, false).unwrap());
        assert!(!apply_grant(&mut permissions, &read, false).unwrap());
        assert_eq!(permissions.read, vec!["/run/health".to_owned()]);
        assert!(apply_grant(&mut permissions, &read, true).unwrap());
        assert!(!apply_grant(&mut permissions, &read, true).unwrap());
        assert!(permissions.read.is_empty());
        // A relative path is refused, as the Modules tab refuses it, and the
        // set is left as it was.
        let relative = Grant {
            permission: Permission::Read,
            value: "run/health".into(),
        };
        assert!(apply_grant(&mut permissions, &relative, false).is_err());
        assert!(permissions.read.is_empty());
    }

    #[test]
    fn a_setting_takes_the_type_the_module_declares() {
        let spec = vec![FormField::new("port", BasicType::Int)];
        let mut configuration = sc_types::Attrs::new();
        let settings = vec![
            ("port".to_owned(), "8080".to_owned()),
            ("min_role".to_owned(), "staff".to_owned()),
        ];
        assert!(apply_settings(&mut configuration, &spec, &settings).unwrap());
        assert_eq!(configuration["port"], json!(8080));
        assert_eq!(configuration["min_role"], json!("staff"));
        assert!(!apply_settings(&mut configuration, &spec, &settings).unwrap());
        assert!(apply_settings(&mut configuration, &spec, &[("port".into(), "x".into())]).is_err());
    }
}
