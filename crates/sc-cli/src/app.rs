//! `feldspar app list [--json]`: the stored applications, and where each one's
//! project directory is on this machine. `feldspar app triggers …` is parsed
//! here too ([`parse_triggers`]); it changes which triggers an application
//! exposes.
//!
//! The project directory is the one an external coding agent is started in
//! (`claude mcp add …`, then `claude`, there), so a script that sets up agents
//! needs it per application. It is **derived**, not stored: the framework's
//! settings name a file store and a directory inside it, and the store's
//! definition says where it is on disk. This resolves it exactly as a build
//! does — `app_source_from_config` for a framework with a build step,
//! `none_source_dir` for `none` — and asks the connected store for the local
//! path, so the answer is the directory a build would run in.
//!
//! An application whose directory cannot be resolved — a framework with no
//! source tree, a store that is not connected, a store with no local disk —
//! is still listed, with `project_dir: null` and the reason in `error`: a list
//! that left it out would read as "there is no such application".

use sc_app::Application;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

/// Parse `app list`'s flags: `--json`, or nothing for the plain listing.
pub fn parse_list(args: &[String]) -> Result<bool> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        [other, ..] => Err(Error::config(format!(
            "unknown app list argument `{other}`; it takes --json"
        ))),
    }
}

pub use sc_app::app_source_location as source_location;
pub use sc_app::store_dir as project_dir;

/// One application as `app list --json` reports it.
pub fn app_json(catalog: &Catalog, app: &Application) -> Json {
    let located = source_location(app)
        .and_then(|(store, dir)| project_dir(catalog, &store, &dir).map(|p| (store, dir, p)));
    let mut out = json!({
        "id": app.id.0,
        "name": app.name,
        "subdomain": app.subdomain,
        "framework": app.framework.name,
    });
    match located {
        Ok((store, dir, path)) => {
            out["file_store"] = json!(store);
            out["source_dir"] = json!(dir);
            out["project_dir"] = json!(path.display().to_string());
        }
        Err(e) => {
            out["project_dir"] = Json::Null;
            out["error"] = json!(e.to_string());
        }
    }
    out
}

/// What `app triggers` was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggersAction {
    /// `list`: the exposed triggers.
    List,
    /// `add NAME…`: expose these too.
    Add(Vec<String>),
    /// `remove NAME…`: stop exposing these.
    Remove(Vec<String>),
}

/// `app triggers (add|remove|list) --app SUBDOMAIN [NAME…] [--json]`, the
/// database flags already taken out.
#[derive(Debug, Clone, PartialEq)]
pub struct TriggersArgs {
    /// The application, by subdomain.
    pub app: String,
    /// What to do.
    pub action: TriggersAction,
    /// Print the answer as JSON rather than a table.
    pub json: bool,
}

/// Parse `app triggers`'s arguments. `add` and `remove` need at least one name,
/// `list` takes none.
pub fn parse_triggers(args: &[String]) -> Result<TriggersArgs> {
    parse_names("triggers", "trigger", args)
}

/// `app tables (add|remove|list) --app SUBDOMAIN [TABLE…] [--json]`: the same
/// shape as `app triggers`, over the tables the application reaches.
pub type TablesArgs = TriggersArgs;

/// Parse `app tables`'s arguments, which are `app triggers`'s with table
/// names.
pub fn parse_tables(args: &[String]) -> Result<TablesArgs> {
    parse_names("tables", "table", args)
}

/// Parse `app api-tables`'s arguments: `app tables`'s, over the connected
/// tables whose generic row API the application serves.
pub fn parse_api_tables(args: &[String]) -> Result<TablesArgs> {
    parse_names("api-tables", "table", args)
}

/// `app <section> (add|remove|list) --app SUBDOMAIN [NAME…] [--json]`.
fn parse_names(section: &str, noun: &str, args: &[String]) -> Result<TriggersArgs> {
    let usage = format!("app {section} (add|remove|list) --app SUBDOMAIN [NAME…] [--json]");
    let (verb, rest) = args
        .split_first()
        .ok_or_else(|| Error::config(format!("usage: {usage}")))?;
    let mut app = None;
    let mut json = false;
    let mut names = Vec::new();
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--app" => {
                app = Some(
                    rest.next()
                        .ok_or_else(|| Error::config("--app needs a subdomain"))?
                        .clone(),
                )
            }
            "--json" => json = true,
            flag if flag.starts_with("--") => {
                return Err(Error::config(format!(
                    "unknown app {section} argument `{flag}`; usage: {usage}"
                )));
            }
            name => names.push(name.to_owned()),
        }
    }
    let app = app.ok_or_else(|| Error::config(format!("--app is required; usage: {usage}")))?;
    let action = match (verb.as_str(), names.is_empty()) {
        ("list", true) => TriggersAction::List,
        ("list", false) => {
            return Err(Error::config(format!(
                "app {section} list takes no {noun} names"
            )));
        }
        ("add" | "remove", true) => {
            return Err(Error::config(format!(
                "app {section} {verb} needs at least one {noun} name"
            )));
        }
        ("add", false) => TriggersAction::Add(names),
        ("remove", false) => TriggersAction::Remove(names),
        (other, _) => {
            return Err(Error::config(format!(
                "unknown app {section} subcommand `{other}`; there are add, remove and list"
            )));
        }
    };
    Ok(TriggersArgs { app, action, json })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn triggers_subcommands_parse_and_refuse_what_they_cannot_do() {
        assert_eq!(
            parse_triggers(&args("add --app shop mail_send mail_test_account")).unwrap(),
            TriggersArgs {
                app: "shop".into(),
                action: TriggersAction::Add(vec!["mail_send".into(), "mail_test_account".into()]),
                json: false,
            }
        );
        let listed = parse_triggers(&args("list --json --app shop")).unwrap();
        assert_eq!((listed.action, listed.json), (TriggersAction::List, true));
        assert_eq!(
            parse_triggers(&args("remove mail_send --app shop"))
                .unwrap()
                .action,
            TriggersAction::Remove(vec!["mail_send".into()])
        );
        assert_eq!(
            parse_tables(&args(
                "add --app managed user_settings backup_status --json"
            ))
            .unwrap(),
            TriggersArgs {
                app: "managed".into(),
                action: TriggersAction::Add(vec!["user_settings".into(), "backup_status".into()]),
                json: true,
            }
        );
        let msg = parse_tables(&args("remove --app managed"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("table name"), "{msg}");
        for bad in [
            "",
            "add --app shop",
            "list --app shop mail_send",
            "add mail_send",
            "tick --app shop x",
            "add --app shop --force x",
        ] {
            assert!(parse_triggers(&args(bad)).is_err(), "{bad}");
        }
    }
}
