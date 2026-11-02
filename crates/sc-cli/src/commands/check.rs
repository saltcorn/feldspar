//! `feldspar check …`: running and listing checklists.

use super::serve::{Services, install_services, serving_defaults};
use std::sync::Arc;

use sc_api::admin_endpoints;
use sc_app::load_application_by_subdomain;
use sc_auth::SessionStore;
use sc_cli::DbConfig;
use sc_cli::{
    connect_catalog, connect_file_stores, connect_stored_databases, connect_stored_file_stores,
    extract_file_stores,
};
use sc_error::Result;
use sc_server::{ServerConfig, ServiceManager, admin_handlers, mount_all};

/// `feldspar app list [--json]` — every application, with the project directory
/// its source is in on this machine — and `feldspar app triggers …`, the
/// triggers it exposes. See `sc_cli::app`.
/// `feldspar check (run|list) [--app SUBDOMAIN] [--checklist NAME] [database flags]`
/// (TODO-checklists C1.12).
pub(crate) async fn check_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("run") => check_run_command(&args[1..]).await,
        Some("list") => check_list_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown check subcommand `{other}`; there are `run` and `list`"
        ))),
        None => Err(sc_error::Error::config(
            "check needs a subcommand: run or list",
        )),
    }
}

/// The checklists `args` selects, by `--checklist` and `--app`.
pub(crate) async fn selected_checklists(
    catalog: &sc_catalog::Catalog,
    args: &sc_cli::check::CheckArgs,
) -> Result<Vec<sc_check::Checklist>> {
    let scope = match &args.app {
        Some(subdomain) => {
            let app = load_application_by_subdomain(catalog, subdomain)
                .await?
                .ok_or_else(|| {
                    sc_error::Error::not_found(format!(
                        "no application with subdomain `{subdomain}`"
                    ))
                })?;
            sc_check::ChecklistScope::Application(app.id.0)
        }
        None => sc_check::ChecklistScope::All,
    };
    let mut checklists = sc_check::store::list_checklists(catalog, scope).await?;
    if let Some(name) = &args.checklist {
        checklists.retain(|c| c.name == *name);
        if checklists.is_empty() {
            return Err(sc_error::Error::not_found(format!(
                "no checklist called `{name}`{}",
                args.app
                    .as_ref()
                    .map(|a| format!(" in application `{a}`"))
                    .unwrap_or_default()
            )));
        }
    }
    Ok(checklists)
}

/// `feldspar check run`: run the selected checklists, print each step, and
/// exit non-zero unless every one passed.
///
/// The server's capabilities are brought up as `serve` brings them up — the
/// trigger set, and the applications mounted from their last build with the
/// router a browser's request meets — so an `http.*` step answers here what it
/// answers in the admin UI. No listener is opened.
pub(crate) async fn check_run_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (file_stores, rest) = extract_file_stores(rest)?;
    let parsed = sc_cli::check::parse_args(&rest)?;
    let mut config = ServerConfig::from_args(serving_defaults(&db).iter())?;
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    connect_file_stores(&catalog, &file_stores)?;
    let checklists = selected_checklists(&catalog, &parsed).await?;

    sc_log::set_log_sql(false);
    sc_dns::init()?;
    if config.base_domain.is_none()
        && let Some(domain) = sc_server::implied_base_domain(&config.addr, false)
    {
        config.base_domain = Some(domain.to_owned());
    }
    let Services { apps, triggers, .. } =
        install_services(&catalog, &config, &ServiceManager::default()).await?;
    let requests = if config.base_domain.is_some() {
        mount_all(&apps).await;
        let sessions = Arc::new(SessionStore::database(catalog.clone()));
        let handlers = admin_handlers(catalog.clone(), apps.clone());
        sc_server::install_app_requests(&config, &admin_endpoints(), &handlers, &sessions, &apps)?
    } else {
        None
    };

    let registry = sc_check::StepRegistry::installed();
    let env = sc_check::CheckEnv::new(&catalog, &registry)
        .with_triggers(Some(&triggers))
        .with_requests(
            requests
                .as_ref()
                .map(|r| r.as_ref() as &dyn sc_agent::AppRequester),
        );
    let mut runs = Vec::new();
    for checklist in checklists.iter().filter(|c| c.enabled) {
        let run = sc_check::run_checklist(&env, checklist, "cli").await?;
        print!("{}", sc_cli::check::format_run(&run));
        runs.push(run);
    }
    sc_cli::check::verdict(&runs)
}

/// `feldspar check list`: the checklists, with their last result.
pub(crate) async fn check_list_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::check::parse_args(&rest)?;
    let catalog = connect_catalog(&db).await?;
    for checklist in selected_checklists(&catalog, &parsed).await? {
        let steps = sc_check::store::current_steps(&catalog, checklist.id).await?;
        let last = sc_check::store::last_run(&catalog, checklist.id).await?;
        println!(
            "{}\t{} step{}\t{}{}",
            checklist.name,
            steps.len(),
            if steps.len() == 1 { "" } else { "s" },
            last.map_or_else(
                || "never run".to_owned(),
                |r| format!("{} {}", r.status.as_str(), r.finished_at.to_rfc3339())
            ),
            if checklist.enabled { "" } else { "\tdisabled" },
        );
    }
    Ok(())
}
