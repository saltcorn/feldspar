//! `feldspar sync` (APP_SYNC_PROPOSAL §7.3): source control for the
//! installation's structure at the terminal — what Source control does in the
//! admin UI.
//!
//! ```text
//! feldspar sync link URL [--branch B] [--subdirectory DIR] | unlink
//! feldspar sync status
//! feldspar sync pull
//! feldspar sync commit -m MSG [KEY…] [--keep-mine KEY]…
//! feldspar sync discard KEY… [--plan] [--yes] [--allow-destructive]
//! feldspar sync deploy [--plan] [--yes] [--allow-destructive] [--full-backup]
//! feldspar sync branch [NAME] [--create]
//! ```
//!
//! A KEY is an entity as Status prints it (`table:tickets`,
//! `view:helpdesk/list`). `deploy` and `discard` print the plan and apply it
//! only with `--yes`, which refuses a plan with blockers, and one that loses
//! data unless `--allow-destructive` says so. `--allow-destructive` is also
//! what lifts a `destructive: never` policy, which only the operator on the
//! server can do.

use std::sync::Arc;

use sc_cli::{DbConfig, connect_catalog, connect_stored_databases, connect_stored_file_stores};
use sc_error::{Error, Result};
use sc_server::{AppMounts, ServerConfig, ServiceManager};
use sc_sync::{
    Class, CommitRequest, Deployer, Executor, Plan, Receipt, Source, SourceControl, SyncLink,
};

use super::registry::{Apply, confirmation, print_plan};
use super::serve::{Services, install_services, serving_defaults};

/// The operator deploying, committing or discarding at the CLI.
const SYNC: sc_catalog::Operator = sc_catalog::Operator::at_cli("sync");

/// Who the ledger names for what is done here.
const ACTOR: &str = "operator";

const USAGE: &str = "feldspar sync link URL [--branch B] [--subdirectory DIR] | unlink | status | \
     pull | commit -m MSG [KEY…] [--keep-mine KEY]… | \
     discard KEY… [--plan] [--yes] [--allow-destructive] | \
     deploy [--plan] [--yes] [--allow-destructive] [--full-backup] | branch [NAME] [--create]";

#[allow(clippy::too_many_lines)]
pub(crate) async fn sync_command(args: &[String]) -> Result<()> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(Error::config(format!("usage: {USAGE}")));
    };
    let (db, rest) = DbConfig::extract(rest.to_vec())?;
    match sub.as_str() {
        "link" => {
            let (flags, rest) = take_values(rest, &["--branch", "--subdirectory"])?;
            let [url] = rest.as_slice() else {
                return Err(Error::config(
                    "usage: feldspar sync link URL [--branch B] [--subdirectory DIR]",
                ));
            };
            let catalog = connect_catalog(&db).await?;
            sc_log::set_log_sql(false);
            let apps = Arc::new(AppMounts::new(catalog.clone()));
            let sc = source(&catalog, &apps)?;
            let mut link = SyncLink::new(
                url.clone(),
                flags[0]
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "main".to_owned()),
            );
            link.subdirectory = flags[1].first().cloned();
            // The credential stored here (a key generated in Source control),
            // and the policy the link has, are kept.
            link.credential = sc.credential().await?;
            if let Some(previous) = sc.link().await? {
                link.policy = previous.policy;
            }
            let linked = sc.set_link(link).await?;
            eprintln!(
                "feldspar: linked to {url}, at {}{}",
                linked.tip.as_deref().map_or("no commit yet", short),
                if linked.base_taken {
                    "; the base is the branch's tip"
                } else {
                    ""
                }
            );
            Ok(())
        }
        "unlink" => {
            no_more(&rest, "feldspar sync unlink")?;
            let catalog = connect_catalog(&db).await?;
            let apps = Arc::new(AppMounts::new(catalog.clone()));
            source(&catalog, &apps)?.unlink().await?;
            eprintln!("feldspar: unlinked; the structure here is unchanged");
            Ok(())
        }
        "status" => {
            no_more(&rest, "feldspar sync status")?;
            let catalog = connect_catalog(&db).await?;
            sc_log::set_log_sql(false);
            let apps = Arc::new(AppMounts::new(catalog.clone()));
            let linked = source(&catalog, &apps)?.status().await?;
            println!(
                "linked to {} ({}), at {}",
                linked.link.url,
                linked.link.branch,
                linked.tip.as_deref().map_or("no commit yet", short)
            );
            let shown: Vec<&sc_sync::StatusEntry> = linked.status.shown().collect();
            if shown.is_empty() {
                println!("  in sync");
            }
            for entry in &shown {
                println!("  {:<16} {}", class_name(entry.class), entry.key);
            }
            for warning in &linked.warnings {
                println!("  warning: {warning}");
            }
            let blockers = linked.status.deploy_blockers().len();
            if blockers > 0 {
                println!(
                    "deploy is refused until the {blockers} local change{} {} committed or discarded",
                    if blockers == 1 { "" } else { "s" },
                    if blockers == 1 { "is" } else { "are" },
                );
            }
            Ok(())
        }
        "pull" => {
            no_more(&rest, "feldspar sync pull")?;
            let catalog = connect_catalog(&db).await?;
            let apps = Arc::new(AppMounts::new(catalog.clone()));
            let pulled = source(&catalog, &apps)?.pull().await?;
            eprintln!(
                "feldspar: {} → {}{}",
                pulled.before.as_deref().map_or("no commit", short),
                pulled.tip.as_deref().map_or("no commit", short),
                if pulled.forced {
                    " (the remote was force-pushed; the branch was reset)"
                } else {
                    ""
                }
            );
            Ok(())
        }
        "commit" => {
            let (flags, keys) = take_values(rest, &["-m", "--keep-mine"])?;
            let Some(message) = flags[0].last().cloned() else {
                return Err(Error::config(
                    "usage: feldspar sync commit -m MSG [KEY…] [--keep-mine KEY]…",
                ));
            };
            let catalog = connect_catalog(&db).await?;
            sc_log::set_log_sql(false);
            let apps = Arc::new(AppMounts::new(catalog.clone()));
            let receipt = source(&catalog, &apps)?
                .commit(&CommitRequest {
                    message,
                    keys: (!keys.is_empty()).then_some(keys),
                    keep_mine: flags[1].iter().cloned().collect(),
                    actor: Some(ACTOR.to_owned()),
                    source: Some(Source::Cli),
                })
                .await?;
            for warning in &receipt.warnings {
                eprintln!("warning: {warning}");
            }
            eprintln!(
                "feldspar: committed {} to {} as {}",
                receipt.entities.join(", "),
                receipt.branch,
                short(&receipt.commit)
            );
            Ok(())
        }
        "discard" => {
            let (apply, keys) = Apply::extract(rest);
            if keys.is_empty() {
                return Err(Error::config(
                    "usage: feldspar sync discard KEY… [--plan] [--yes] [--allow-destructive]",
                ));
            }
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            let (catalog, apps) = served(&db).await?;
            let sc = source(&catalog, &apps)?;
            let plan = sc.plan_discard(&keys).await?;
            print_plan("the discard", &plan);
            let Some(confirm) = confirmed(&plan, apply)? else {
                return Ok(());
            };
            let receipt = sc
                .discard(&keys, &confirm, Some(ACTOR.to_owned()), Source::Cli)
                .await?;
            report("discarded", &receipt)
        }
        "deploy" => {
            let full_backup = rest.iter().any(|a| a == "--full-backup");
            let rest: Vec<String> = rest.into_iter().filter(|a| a != "--full-backup").collect();
            let (apply, rest) = Apply::extract(rest);
            no_more(
                &rest,
                "feldspar sync deploy [--plan] [--yes] [--allow-destructive] [--full-backup]",
            )?;
            let (catalog, apps) = served(&db).await?;
            let sc = source(&catalog, &apps)?;
            let planned = sc.plan_deploy(Some(ACTOR.to_owned()), Source::Cli).await?;
            print_plan(
                &format!(
                    "deploying {}",
                    planned.commit.as_deref().map_or("no commit", short)
                ),
                &planned.plan,
            );
            let Some(mut confirm) = confirmed(&planned.plan, apply)? else {
                return Ok(());
            };
            confirm.full_backup = full_backup;
            let receipt = sc
                .deploy(
                    planned.commit.as_deref(),
                    &confirm,
                    Some(ACTOR.to_owned()),
                    Source::Cli,
                )
                .await?;
            report("deployed", &receipt)
        }
        "branch" => {
            let create = rest.iter().any(|a| a == "--create");
            let rest: Vec<&String> = rest.iter().filter(|a| *a != "--create").collect();
            let catalog = connect_catalog(&db).await?;
            sc_log::set_log_sql(false);
            let apps = Arc::new(AppMounts::new(catalog.clone()));
            let sc = source(&catalog, &apps)?;
            match rest.as_slice() {
                [] if !create => {
                    let current = sc.link().await?.map(|l| l.branch);
                    for branch in sc.branches().await? {
                        let mark = if current.as_deref() == Some(branch.as_str()) {
                            "*"
                        } else {
                            " "
                        };
                        println!("{mark} {branch}");
                    }
                }
                [name] => {
                    let pulled = match create {
                        true => sc.create_branch(name).await?,
                        false => sc.switch_branch(name).await?,
                    };
                    eprintln!(
                        "feldspar: {} `{name}`, at {}; local changes stay local changes",
                        if create {
                            "created and switched to"
                        } else {
                            "switched to"
                        },
                        pulled.tip.as_deref().map_or("no commit yet", short)
                    );
                }
                _ => {
                    return Err(Error::config(
                        "usage: feldspar sync branch [NAME] [--create]",
                    ));
                }
            }
            Ok(())
        }
        other => Err(Error::config(format!(
            "unknown sync command `{other}`; usage: {USAGE}"
        ))),
    }
}

/// Source control over `catalog`, as the operator.
fn source<'a>(
    catalog: &'a sc_catalog::Catalog,
    apps: &'a Arc<AppMounts>,
) -> Result<SourceControl<'a>> {
    Ok(SourceControl::new(Deployer::new(Executor {
        catalog,
        host: apps.as_ref(),
        editor: sc_catalog::Editor::Operator(SYNC),
        data_dir: sc_files::data_dir()?,
    })))
}

/// The catalog with the services a server holds, as a deploy or a discard
/// needs: a trigger is saved through the dispatcher and an agent through its
/// registry, as the admin UI saves them.
async fn served(db: &DbConfig) -> Result<(Arc<sc_catalog::Catalog>, Arc<AppMounts>)> {
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let config = ServerConfig::from_args(serving_defaults(db).iter())?;
    sc_dns::init()?;
    let catalog = connect_catalog(db).await?;
    sc_log::set_log_sql(false);
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    let Services { apps, .. } =
        install_services(&catalog, &config, &ServiceManager::default()).await?;
    Ok((catalog, apps))
}

/// What `--plan`, `--yes` and neither do with a printed plan: the confirmation
/// to apply, or `None` when nothing is to be applied.
fn confirmed(plan: &Plan, apply: Apply) -> Result<Option<sc_sync::Confirm>> {
    if plan.is_empty() && plan.blockers.is_empty() {
        eprintln!("feldspar: nothing to do");
        return Ok(None);
    }
    if apply.plan_only {
        eprintln!("feldspar: --plan: nothing was applied");
        return Ok(None);
    }
    if !apply.yes {
        eprintln!("feldspar: nothing was applied; pass --yes to apply this plan");
        return Ok(None);
    }
    confirmation(plan, apply).map(Some)
}

/// The receipt, on stderr; a failed step is an error.
fn report(done: &str, receipt: &Receipt) -> Result<()> {
    for backup in &receipt.backups {
        eprintln!("feldspar: a backup was written first: {backup}");
    }
    for deferred in &receipt.applied.deferred {
        eprintln!("feldspar: deferred, as not ticked: {deferred}");
    }
    for note in &receipt.applied.notes {
        eprintln!("note: {note}");
    }
    for drift in &receipt.drift {
        eprintln!("drift: {drift}");
    }
    if let Some(failed) = &receipt.applied.failed {
        return Err(Error::invalid(format!(
            "{} failed: {}; what was applied before it is listed in the ledger ({})",
            failed.summary, failed.message, receipt.id
        )));
    }
    eprintln!(
        "feldspar: {done} ({}); restart any server running on this database to load it",
        receipt.result.as_str()
    );
    Ok(())
}

/// Take each of `names`' values out of `args` (`--branch main`), in order;
/// the rest stay.
fn take_values(args: Vec<String>, names: &[&str]) -> Result<(Vec<Vec<String>>, Vec<String>)> {
    let mut values = vec![Vec::new(); names.len()];
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match names.iter().position(|n| *n == arg) {
            Some(i) => values[i].push(
                args.next()
                    .ok_or_else(|| Error::config(format!("`{arg}` needs a value")))?,
            ),
            None => rest.push(arg),
        }
    }
    Ok((values, rest))
}

fn no_more(rest: &[String], usage: &str) -> Result<()> {
    match rest.first() {
        None => Ok(()),
        Some(extra) => Err(Error::config(format!(
            "unexpected `{extra}`; usage: {usage}"
        ))),
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(10)]
}

fn class_name(class: Class) -> &'static str {
    match class {
        Class::InSync => "in sync",
        Class::Incoming | Class::IncomingNew => "incoming",
        Class::IncomingDelete => "incoming delete",
        Class::LocalChange => "local change",
        Class::LocalDelete => "local delete",
        Class::LocalNew => "local new",
        Class::PinMoved => "pin moved",
        Class::Conflict => "conflict",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_come_out_of_the_arguments_in_order() {
        let (values, rest) = take_values(
            [
                "-m",
                "First",
                "table:a",
                "--keep-mine",
                "view:x/y",
                "table:b",
            ]
            .map(String::from)
            .to_vec(),
            &["-m", "--keep-mine"],
        )
        .unwrap();
        assert_eq!(
            values,
            [vec!["First".to_owned()], vec!["view:x/y".to_owned()]]
        );
        assert_eq!(rest, ["table:a", "table:b"]);
        assert!(take_values(vec!["-m".into()], &["-m"]).is_err());
    }
}
