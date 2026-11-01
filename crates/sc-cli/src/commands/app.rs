//! `feldspar app …`: listing applications and their tables and triggers.

use sc_app::save_application;
use sc_cli::DbConfig;
use sc_cli::{connect_catalog, connect_stored_file_stores, extract_file_stores};
use sc_error::Result;
use sc_server::{ServerConfig, ServiceManager};

use super::api::open_app;
use super::serve::{Services, install_services, serving_defaults, take_option};

pub(crate) async fn app_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => app_list_command(&args[1..]).await,
        Some("triggers") => app_triggers_command(&args[1..]).await,
        Some("tables") => app_tables_command(&args[1..], false).await,
        Some("api-tables") => app_tables_command(&args[1..], true).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown app subcommand `{other}`; there are `list`, `tables`, `api-tables` \
             and `triggers`"
        ))),
        None => Err(sc_error::Error::config(
            "app needs a subcommand: list, tables, api-tables or triggers",
        )),
    }
}

/// `feldspar app tables (add|remove|list) --app SUBDOMAIN [TABLE…] [--json]`:
/// the tables an application reaches, changed by the rule
/// `update_application`'s `tables` section uses (`sc_app::change_app_tables`)
/// — so a seed script names tables rather than editing
/// `_fd_applications.tables` as JSON. Idempotent: adding a connected table or
/// removing one that is not changes nothing.
///
/// `feldspar app api-tables (add|remove|list) …` is the same command over the
/// connected tables whose **generic row API** the application serves
/// (`sc_app::change_app_api_tables`) — none unless somebody adds one, and a
/// table must be connected first.
///
/// A running server serves the change after `SIGHUP`, which reloads every
/// application row.
pub(crate) async fn app_tables_command(args: &[String], api: bool) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (base_domain, rest) = take_option(rest, "--base-domain")?;
    let (file_stores, rest) = extract_file_stores(rest)?;
    let parsed = match api {
        true => sc_cli::app::parse_api_tables(&rest)?,
        false => sc_cli::app::parse_tables(&rest)?,
    };

    let (catalog, mut app) =
        open_app(&parsed.app, &db, &file_stores, base_domain.as_deref()).await?;
    sc_log::set_log_sql(false);

    let change = match parsed.action {
        sc_cli::app::TriggersAction::List => None,
        sc_cli::app::TriggersAction::Add(add) => Some(sc_app::TableChange {
            add,
            ..Default::default()
        }),
        sc_cli::app::TriggersAction::Remove(remove) => Some(sc_app::TableChange {
            remove,
            ..Default::default()
        }),
    };
    if let Some(change) = change {
        let changed = match api {
            true => sc_app::change_app_api_tables(&mut app, &change)?,
            false => sc_app::change_app_tables(&catalog, &mut app, &change)?,
        };
        if changed {
            app = save_application(&catalog, &app).await?;
            // The generated client is typed from the endpoints, which include
            // the exposed triggers — so an app exposing any is rewritten with
            // the live trigger set, as `app triggers` rewrites it, rather than
            // losing their methods.
            let triggers = match app.triggers.is_empty() {
                true => None,
                false => {
                    let config = ServerConfig::from_args(serving_defaults(&db).iter())?;
                    sc_dns::init()?;
                    let Services { triggers, .. } =
                        install_services(&catalog, &config, &ServiceManager::default()).await?;
                    Some(triggers)
                }
            };
            match sc_app::emit_app_client(&catalog, &app, triggers.as_ref()).await {
                Ok(written) if written.is_empty() => {}
                Ok(written) => eprintln!("feldspar: rewrote {}", written.join(", ")),
                Err(e) => eprintln!(
                    "feldspar: saved, but the generated client could not be rewritten: {e}"
                ),
            }
            eprintln!("feldspar: saved; a running server serves the change after SIGHUP");
        } else {
            eprintln!("feldspar: `{}` unchanged", app.subdomain);
        }
    }

    let listed = match api {
        true => &app.api_tables,
        false => &app.tables,
    };
    let tables: Vec<&str> = listed.iter().map(|t| t.as_str()).collect();
    if parsed.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&tables).unwrap_or_default()
        );
        return Ok(());
    }
    if tables.is_empty() {
        match api {
            true => println!("application `{}` serves no table's row API", app.subdomain),
            false => println!("application `{}` reaches no tables", app.subdomain),
        }
    }
    for table in tables {
        println!("{table}");
    }
    Ok(())
}

/// `feldspar app triggers (add|remove|list) --app SUBDOMAIN [NAME…] [--json]`:
/// the triggers an application exposes as `POST {mount}/actions/<name>` and
/// `api.run<Name>()`, changed by the rule `update_application`'s `triggers`
/// section uses (`sc_app::change_app_triggers`).
///
/// The services are installed as `restore` installs them, because a trigger
/// over a module's action is in the live set only once the modules are: a
/// set loaded without them would call `mail_send` not usable.
pub(crate) async fn app_triggers_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (base_domain, rest) = take_option(rest, "--base-domain")?;
    let (file_stores, rest) = extract_file_stores(rest)?;
    let parsed = sc_cli::app::parse_triggers(&rest)?;
    let config = ServerConfig::from_args(serving_defaults(&db).iter())?;

    let (catalog, mut app) =
        open_app(&parsed.app, &db, &file_stores, base_domain.as_deref()).await?;
    sc_log::set_log_sql(false);
    sc_dns::init()?;
    let Services { triggers, .. } =
        install_services(&catalog, &config, &ServiceManager::default()).await?;
    let live = triggers.triggers()?;
    let require = |name: &str| live.require(name).cloned();

    let change = match parsed.action {
        sc_cli::app::TriggersAction::List => None,
        sc_cli::app::TriggersAction::Add(add) => Some(sc_app::TriggerChange {
            add,
            ..Default::default()
        }),
        sc_cli::app::TriggersAction::Remove(remove) => Some(sc_app::TriggerChange {
            remove,
            ..Default::default()
        }),
    };
    if let Some(change) = change {
        sc_app::change_app_triggers(&mut app, &change, require)?;
        app = save_application(&catalog, &app).await?;
        match sc_app::emit_app_client(&catalog, &app, Some(&triggers)).await {
            Ok(written) if written.is_empty() => {}
            Ok(written) => eprintln!("feldspar: rewrote {}", written.join(", ")),
            Err(e) => {
                eprintln!("feldspar: saved, but the generated client could not be rewritten: {e}")
            }
        }
        eprintln!(
            "feldspar: saved; a server already running serves the change from its next start"
        );
    }

    let out = sc_app::app_triggers_json(&app, require);
    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(());
    }
    let rows = out.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("application `{}` exposes no triggers", app.subdomain);
    }
    for row in &rows {
        let field = |k: &str| match &row[k] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => "-".to_owned(),
            other => other.to_string(),
        };
        let note = match row.get("note") {
            Some(serde_json::Value::String(n)) => format!("  ({n})"),
            _ => String::new(),
        };
        println!(
            "{:<24} {:<8} {:<28} {}{note}",
            field("name"),
            field("min_role"),
            field("method"),
            field("route"),
        );
    }
    Ok(())
}

pub(crate) async fn app_list_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let as_json = sc_cli::app::parse_list(&rest)?;
    let catalog = connect_catalog(&db).await?;
    // stdout is the listing; a JSON consumer must get nothing else there.
    sc_log::set_log_sql(false);
    connect_stored_file_stores(&catalog).await?;

    let apps = sc_app::list_applications(&catalog).await?;
    let out: Vec<serde_json::Value> = apps
        .iter()
        .map(|app| sc_cli::app::app_json(&catalog, app))
        .collect();
    if as_json {
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(());
    }
    for app in &out {
        let field = |k: &str| app.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let dir = match app.get("project_dir").and_then(|v| v.as_str()) {
            Some(dir) => dir.to_owned(),
            None => format!("(no project directory: {})", field("error")),
        };
        println!(
            "{:<20} {:<10} {:<24} {}",
            field("subdomain"),
            field("framework"),
            field("name"),
            dir
        );
    }
    Ok(())
}
