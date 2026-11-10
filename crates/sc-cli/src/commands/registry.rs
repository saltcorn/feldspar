//! `feldspar registry` (APP_SYNC_PROPOSAL §7.3): the registry editor at the
//! terminal — every configuration value of the installation, one JSON file of
//! the instance tree at a time.
//!
//! ```text
//! feldspar registry list [KIND]
//! feldspar registry get ENTRY
//! feldspar registry put ENTRY [--plan] [--yes] [--allow-destructive] < value.json
//! feldspar registry delete ENTRY… [--plan] [--yes] [--allow-destructive]
//! ```
//!
//! An ENTRY is its path in the tree (`tables/tickets.json`) or a kind and a name
//! (`table/tickets`, `setting/email`, `view/APP/NAME`). A save is the editor's:
//! a one-entity deploy against live, planned, classified and recorded in the
//! ledger as an `edit`. `put` and `delete` print the plan and apply it only with
//! `--yes`, which refuses a plan with blockers, and one that loses data unless
//! `--allow-destructive` says so — which also lifts a `destructive: never`
//! policy, as it does for `feldspar sync deploy`.

use std::io::Read;
use std::sync::Arc;

use sc_cli::{DbConfig, connect_catalog, connect_stored_databases, connect_stored_file_stores};
use sc_error::{Error, Result};
use sc_server::{AppMounts, ServerConfig, ServiceManager};
use sc_sync::registry::{self, Edit, EditBy};
use sc_sync::{Confirm, Deployer, Executor, OpClass, Plan, Policy, Source};

use super::serve::{Services, install_services, serving_defaults};

/// The operator saving through the registry at the CLI.
const REGISTRY: sc_catalog::Operator = sc_catalog::Operator::at_cli("registry");

const USAGE: &str = "feldspar registry list [KIND] | get ENTRY | \
     put ENTRY [--plan] [--yes] [--allow-destructive] (the JSON on stdin) | \
     delete ENTRY… [--plan] [--yes] [--allow-destructive]";

/// What a `put` or `delete` was told to do with its plan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Apply {
    plan_only: bool,
    yes: bool,
    allow_destructive: bool,
}

impl Apply {
    /// Take the flags out of `args`.
    fn extract(args: Vec<String>) -> (Apply, Vec<String>) {
        let mut out = Apply::default();
        let mut rest = Vec::new();
        for arg in args {
            match arg.as_str() {
                "--plan" => out.plan_only = true,
                "--yes" | "-y" => out.yes = true,
                "--allow-destructive" => out.allow_destructive = true,
                _ => rest.push(arg),
            }
        }
        (out, rest)
    }
}

pub(crate) async fn registry_command(args: &[String]) -> Result<()> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(Error::config(format!("usage: {USAGE}")));
    };
    let (db, rest) = DbConfig::extract(rest.to_vec())?;
    match sub.as_str() {
        "list" => {
            let catalog = connect_catalog(&db).await?;
            sc_log::set_log_sql(false);
            let kind = match rest.as_slice() {
                [] => None,
                [kind] => Some(sc_sync::Kind::parse(kind).ok_or_else(|| {
                    Error::config(format!("`{kind}` is not a kind of registry entry"))
                })?),
                _ => return Err(Error::config(format!("usage: {USAGE}"))),
            };
            let apps = Arc::new(AppMounts::new(catalog.clone()));
            let export = deployer(&catalog, &apps)?.instance_export().await?;
            for entry in registry::list(&export) {
                if kind.is_some_and(|k| k != entry.kind) {
                    continue;
                }
                let name = match &entry.application {
                    Some(app) => format!("{app}/{}", entry.name),
                    None => entry.name.clone(),
                };
                let mark = match &entry.instance_only {
                    Some(sc_sync::InstanceOnly::Whole) => "\tthis instance only",
                    Some(sc_sync::InstanceOnly::Keys { .. }) => "\tsome values this instance only",
                    None => "",
                };
                println!("{}\t{}/{name}{mark}", entry.path, entry.kind.as_str());
            }
            Ok(())
        }
        "get" => {
            let [address] = rest.as_slice() else {
                return Err(Error::config("usage: feldspar registry get ENTRY"));
            };
            let catalog = connect_catalog(&db).await?;
            sc_log::set_log_sql(false);
            let apps = Arc::new(AppMounts::new(catalog.clone()));
            let export = deployer(&catalog, &apps)?.instance_export().await?;
            let path = registry::resolve(&registry::list(&export), address)?;
            let entity = registry::get(&export, &path)
                .ok_or_else(|| Error::not_found(format!("there is no `{path}` in the registry")))?;
            print!(
                "{}",
                String::from_utf8_lossy(&sc_sync::render_json(&entity.value))
            );
            Ok(())
        }
        "put" => {
            let (apply, rest) = Apply::extract(rest);
            let [address] = rest.as_slice() else {
                return Err(Error::config(
                    "usage: feldspar registry put ENTRY [--plan] [--yes] [--allow-destructive] \
                     < value.json",
                ));
            };
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| Error::file(format!("reading the JSON from stdin: {e}")))?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| Error::config(format!("the JSON on stdin does not parse: {e}")))?;
            save(&db, std::slice::from_ref(address), Some(value), apply).await
        }
        "delete" => {
            let (apply, rest) = Apply::extract(rest);
            if rest.is_empty() {
                return Err(Error::config(
                    "usage: feldspar registry delete ENTRY… [--plan] [--yes] [--allow-destructive]",
                ));
            }
            save(&db, &rest, None, apply).await
        }
        other => Err(Error::config(format!(
            "unknown registry command `{other}`; usage: {USAGE}"
        ))),
    }
}

/// Plan, print and — with `--yes` — apply a save of each of `addresses`
/// (one, for a put): `value`, or a delete.
async fn save(
    db: &DbConfig,
    addresses: &[String],
    value: Option<serde_json::Value>,
    apply: Apply,
) -> Result<()> {
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let config = ServerConfig::from_args(serving_defaults(db).iter())?;
    sc_dns::init()?;
    let catalog = connect_catalog(db).await?;
    sc_log::set_log_sql(false);
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    // The services a server holds: a trigger is saved through the dispatcher
    // and an agent through its registry, as the admin UI saves them.
    let Services { apps, .. } =
        install_services(&catalog, &config, &ServiceManager::default()).await?;
    let deployer = deployer(&catalog, &apps)?;
    for address in addresses {
        let export = deployer.instance_export().await?;
        let path = registry::resolve(&registry::list(&export), address)?;
        let version = export.tree.json(&path).map(registry::version);
        let edit = Edit {
            path: path.clone(),
            value: value.clone(),
            version,
        };
        let planned = deployer.plan_edit(&edit).await?;
        print_plan(&path, &planned.plan);
        if planned.plan.is_empty() && planned.plan.blockers.is_empty() {
            eprintln!("feldspar: nothing to do: `{path}` is already that");
            continue;
        }
        if apply.plan_only {
            eprintln!("feldspar: --plan: nothing was saved");
            continue;
        }
        if !apply.yes {
            eprintln!("feldspar: nothing was saved; pass --yes to apply this plan");
            continue;
        }
        let confirm = confirmation(&planned.plan, apply)?;
        let edit = Edit {
            value: planned.value,
            ..edit
        };
        let by = EditBy {
            actor: Some("operator".to_owned()),
            source: Source::Cli,
            policy: Policy::default(),
        };
        let receipt = deployer.edit(&edit, &confirm, &by).await?;
        for backup in &receipt.receipt.backups {
            eprintln!("feldspar: a safety backup was written first: {backup}");
        }
        for note in &receipt.receipt.applied.notes {
            eprintln!("note: {note}");
        }
        for change in &receipt.changes {
            eprintln!(
                "feldspar: stored differently from what was given at {}: {} → {}",
                change.path,
                change
                    .before
                    .as_ref()
                    .map_or("—".to_owned(), |v| v.to_string()),
                change
                    .after
                    .as_ref()
                    .map_or("—".to_owned(), |v| v.to_string()),
            );
        }
        if let Some(failed) = &receipt.receipt.applied.failed {
            return Err(Error::invalid(format!(
                "`{path}`: {} failed: {}",
                failed.summary, failed.message
            )));
        }
        eprintln!(
            "feldspar: {} `{path}` ({}); restart any server running on this database to load it",
            if receipt.path.is_some() {
                "saved"
            } else {
                "deleted"
            },
            receipt.receipt.result.as_str()
        );
    }
    Ok(())
}

/// What `--yes` confirms: every operation as it is offered, and the ones that
/// lose data only with `--allow-destructive`. A plan with a blocker is refused.
fn confirmation(plan: &Plan, apply: Apply) -> Result<Confirm> {
    if !plan.blockers.is_empty() {
        let reasons: Vec<&str> = plan.blockers.iter().map(|b| b.message.as_str()).collect();
        return Err(Error::invalid(format!(
            "--yes applies only a plan with no blockers; nothing was saved: {}",
            reasons.join("; ")
        )));
    }
    let destructive: Vec<&sc_sync::PlannedOp> = plan
        .operations
        .iter()
        .filter(|op| op.class == OpClass::DataDestructive)
        .collect();
    if !destructive.is_empty() && !apply.allow_destructive {
        return Err(Error::invalid(format!(
            "the plan loses data ({}); nothing was saved. Pass --allow-destructive to include \
             it — a safety backup is written first",
            destructive
                .iter()
                .map(|op| op.summary.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    let mut confirm = Confirm::defaults(plan);
    for op in destructive {
        confirm = confirm.tick(&op.id);
    }
    confirm.allow_destructive = apply.allow_destructive;
    Ok(confirm)
}

/// The plan, one operation a line, then what blocks it.
fn print_plan(path: &str, plan: &Plan) {
    println!("plan for {path}:");
    if plan.operations.is_empty() {
        println!("  (nothing to do)");
    }
    for op in &plan.operations {
        println!(
            "  [{}] {:<16} {}",
            if op.ticked { "x" } else { " " },
            class_name(op.class),
            op.summary
        );
    }
    for blocker in &plan.blockers {
        println!("  blocked: {}", blocker.message);
    }
}

fn class_name(class: OpClass) -> &'static str {
    match class {
        OpClass::Safe => "safe",
        OpClass::MetadataDelete => "metadata delete",
        OpClass::CanFail => "can fail",
        OpClass::DataDestructive => "data-destructive",
        OpClass::Unsupported => "unsupported",
    }
}

fn deployer<'a>(
    catalog: &'a sc_catalog::Catalog,
    apps: &'a Arc<AppMounts>,
) -> Result<Deployer<'a>> {
    Ok(Deployer::new(Executor {
        catalog,
        host: apps.as_ref(),
        editor: sc_catalog::Editor::Operator(REGISTRY),
        data_dir: sc_files::data_dir()?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flags_come_out_of_the_arguments() {
        let (apply, rest) = Apply::extract(
            ["table/tickets", "--plan", "--allow-destructive"]
                .map(String::from)
                .to_vec(),
        );
        assert_eq!(
            apply,
            Apply {
                plan_only: true,
                yes: false,
                allow_destructive: true
            }
        );
        assert_eq!(rest, ["table/tickets"]);
    }
}
