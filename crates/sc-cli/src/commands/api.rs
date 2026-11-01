//! `feldspar build-app` and `feldspar api …`: building an application and its stored API queries.

use sc_app::{
    app_source_from_config, build_application, load_application_by_subdomain, save_application,
};
use sc_cli::DbConfig;
use sc_cli::{
    connect_catalog, connect_file_stores, connect_stored_databases, connect_stored_file_stores,
    extract_file_stores,
};
use sc_error::Result;
use sc_server::{ServerConfig, ServiceManager};

use super::serve::{Services, install_services, serving_defaults, set_public_origin, take_option};

pub(crate) async fn build_app_command(args: &[String]) -> Result<()> {
    let (subdomain, rest) = match args.split_first() {
        Some((first, rest)) if !first.starts_with('-') => (first.clone(), rest.to_vec()),
        _ => {
            return Err(sc_error::Error::config(
                "build-app requires the application's subdomain: \
                 feldspar build-app SUBDOMAIN [database flags]",
            ));
        }
    };
    let (db, rest) = DbConfig::extract(rest)?;
    let (base_domain, rest) = take_option(rest, "--base-domain")?;
    let (file_store_specs, leftover) = extract_file_stores(rest)?;
    if let Some(unknown) = leftover.first() {
        return Err(sc_error::Error::config(format!(
            "unknown build-app argument `{unknown}`"
        )));
    }

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    // A build rewrites `src/feldspar/README.md`, which names the URL the
    // application is served at — so this build has to know it, or it would
    // replace the server's answer with a placeholder.
    set_public_origin(&catalog, &db, base_domain.as_deref());
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    connect_file_stores(&catalog, &file_store_specs)?;

    let app = load_application_by_subdomain(&catalog, &subdomain)
        .await?
        .ok_or_else(|| {
            sc_error::Error::not_found(format!("no application with subdomain `{subdomain}`"))
        })?;

    eprintln!(
        "feldspar: building application `{}` ({})",
        app.name, subdomain
    );
    let source = app_source_from_config(&app.framework)?;
    // An app that exposes triggers projects `api.run<Name>()` from the live
    // trigger set, which a trigger over a module's action is in only once the
    // modules are — so the services come up as `app triggers` brings them up.
    // An app with none builds without them, as it always has.
    let triggers = match app.triggers.is_empty() {
        true => None,
        false => {
            let config = ServerConfig::from_args(serving_defaults(&db).iter())?;
            sc_log::set_log_sql(false);
            sc_dns::init()?;
            let Services { triggers, .. } =
                install_services(&catalog, &config, &ServiceManager::default()).await?;
            Some(triggers)
        }
    };
    let report = build_application(&catalog, &app, &source, triggers.as_ref()).await?;

    // The tool output is the point of running this here rather than clicking
    // Build, so it goes to stdout whole — not the tail an error message can
    // carry, and not summarised.
    if let Some(log) = &report.install_log {
        print!("{log}");
    }
    print!("{}", report.stdout);
    eprint!("{}", report.stderr);

    eprintln!(
        "feldspar: built {} file{} into {}{}",
        report.bundle.len(),
        if report.bundle.len() == 1 { "" } else { "s" },
        report.output_dir.display(),
        if report.installed {
            " (dependencies installed)"
        } else {
            ""
        }
    );
    Ok(())
}

/// `feldspar api SUBCOMMAND …` — an application's custom SQL queries from the
/// command line (§13.4).
///
/// Three subcommands rather than one, because an add-only command is a trap: the
/// first typo would need a browser to fix, which is precisely the situation this
/// command exists to avoid.
pub(crate) async fn api_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("add-query") => add_query_command(&args[1..]).await,
        Some("list-queries") => list_queries_command(&args[1..]).await,
        Some("remove-query") => remove_query_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown api subcommand `{other}`; there are add-query, list-queries \
             and remove-query"
        ))),
        None => Err(sc_error::Error::config(
            "api needs a subcommand: add-query, list-queries or remove-query",
        )),
    }
}

/// Connect the database and the file stores the way `build-app` does, and load
/// the application named by `--app`.
///
/// The file stores are connected because the command **re-emits the generated
/// client**, which is written through the app's source store — a command that
/// changed the API and left the client describing the old one would be the drift
/// §13.1 exists to prevent, introduced by the tool meant to avoid it.
pub(crate) async fn open_app(
    subdomain: &str,
    db: &DbConfig,
    file_stores: &[String],
    base_domain: Option<&str>,
) -> Result<(std::sync::Arc<sc_catalog::Catalog>, sc_app::Application)> {
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(db).await?;
    // These commands re-emit the generated directory, README included, so they
    // need the URL for the same reason a build does.
    set_public_origin(&catalog, db, base_domain);
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    connect_file_stores(&catalog, file_stores)?;
    let app = load_application_by_subdomain(&catalog, subdomain)
        .await?
        .ok_or_else(|| {
            sc_error::Error::not_found(format!("no application with subdomain `{subdomain}`"))
        })?;
    Ok((catalog, app))
}

/// Re-emit `app`'s generated client, reporting what was written.
///
/// A failure here is **reported, not fatal**: the query is already saved, and
/// exiting non-zero would say the opposite. What the message has to carry is
/// which half happened, so nobody goes looking for a client method that was
/// never written — an unreachable store or an app with no client path is a
/// configuration to fix, not a query to add again.
pub(crate) async fn reemit_client(catalog: &sc_catalog::Catalog, app: &sc_app::Application) {
    match sc_app::emit_app_client(catalog, app, None).await {
        Ok(written) if written.is_empty() => {
            eprintln!(
                "feldspar: application `{}` generates no client, so nothing was \
                 rewritten",
                app.subdomain
            );
        }
        Ok(written) => {
            eprintln!("feldspar: rewrote {}", written.join(", "));
        }
        Err(e) => {
            eprintln!(
                "feldspar: the query was saved, but the generated client could not \
                 be rewritten: {e}"
            );
        }
    }
}

/// `feldspar api add-query --app SUBDOMAIN [--api MOUNT] --name … --path …
/// [--language …] --code …`.
///
/// Validates by **preparing** — the same call the admin UI's check button and
/// every save make — so a query that will not prepare exits non-zero carrying
/// Postgres's own message, and the stored application is untouched.
pub(crate) async fn add_query_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (base_domain, rest) = take_option(rest, "--base-domain")?;
    let (file_stores, rest) = extract_file_stores(rest)?;
    let parsed = sc_cli::api::parse_add_query(&rest)?;

    let (catalog, mut app) =
        open_app(&parsed.app, &db, &file_stores, base_domain.as_deref()).await?;
    let api = sc_app::select_api(&mut app, parsed.api.as_deref(), "--api")?;
    let mut queries = sc_api::custom_queries(&api.config)?;
    if queries.iter().any(|q| q.name == parsed.query.name) {
        return Err(sc_error::Error::invalid(format!(
            "application `{}` already has a custom query named `{}`; remove it \
             first (feldspar api remove-query) or use another name",
            parsed.app, parsed.query.name
        )));
    }
    let name = parsed.query.name.clone();
    let mount = api.mount.clone();
    queries.push(parsed.query);
    sc_api::set_custom_queries(&mut api.config, &queries)?;

    // The save is the validation: it prepares every query the app declares and
    // stores the columns the database reported. A refusal leaves the stored row
    // exactly as it was, because nothing is written until every query prepares.
    let saved = save_application(&catalog, &app).await?;
    eprintln!(
        "feldspar: added `{name}` to the API at {mount} of `{}`",
        parsed.app
    );
    print_query_columns(&saved, &mount, &name)?;
    reemit_client(&catalog, &saved).await;
    Ok(())
}

/// Print what the database said the newly-saved query returns — the same answer
/// the admin UI shows, because it is the same stored value, and the shape the
/// generated client method now has.
pub(crate) fn print_query_columns(
    app: &sc_app::Application,
    mount: &str,
    name: &str,
) -> Result<()> {
    let Some(api) = app.apis.iter().find(|a| a.mount == mount) else {
        return Ok(());
    };
    let Some(query) = sc_api::custom_queries(&api.config)?
        .into_iter()
        .find(|q| q.name == name)
    else {
        return Ok(());
    };
    if query.is_code() {
        // A code body has no columns to describe: what it returns is whatever
        // the body returns, so say what was saved instead of "no columns".
        println!("{name}: a {} query", query.language.as_str());
    } else if query.columns.is_empty() {
        println!("{name}: returns no columns");
    } else {
        println!(
            "{name}: returns {}",
            query
                .columns
                .iter()
                .map(|c| format!("{} ({})", c.name, c.ty.name()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// `feldspar api list-queries --app SUBDOMAIN [--api MOUNT]`.
pub(crate) async fn list_queries_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (file_stores, rest) = extract_file_stores(rest)?;
    let parsed = sc_cli::api::parse_query_ref("list-queries", &rest)?;

    // No `--base-domain`: listing rewrites nothing, so there is no generated
    // document whose URL could go missing.
    let (_catalog, app) = open_app(&parsed.app, &db, &file_stores, None).await?;
    let mut found = 0;
    for api in &app.apis {
        if let Some(mount) = &parsed.api
            && api.mount != *mount
        {
            continue;
        }
        for query in sc_api::custom_queries(&api.config)? {
            found += 1;
            println!(
                "{} {}{}  {} (min role {})",
                query.method.as_str(),
                api.mount,
                query.path,
                query.name,
                query.min_role
            );
            if !query.description.is_empty() {
                println!("    {}", query.description);
            }
            if !query.params.is_empty() {
                println!(
                    "    parameters: {}",
                    query
                        .params
                        .iter()
                        .map(|p| format!(
                            "{}: {}{}",
                            p.name,
                            p.ty.name(),
                            if p.required { "" } else { " (optional)" }
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !query.columns.is_empty() {
                println!(
                    "    returns: {}",
                    query
                        .columns
                        .iter()
                        .map(|c| format!("{}: {}", c.name, c.ty.name()))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    if found == 0 {
        eprintln!(
            "feldspar: application `{}` has no custom SQL queries",
            parsed.app
        );
    }
    Ok(())
}

/// `feldspar api remove-query --app SUBDOMAIN [--api MOUNT] --name NAME`.
pub(crate) async fn remove_query_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (base_domain, rest) = take_option(rest, "--base-domain")?;
    let (file_stores, rest) = extract_file_stores(rest)?;
    let parsed = sc_cli::api::parse_query_ref("remove-query", &rest)?;
    let name = parsed.name.clone().ok_or_else(|| {
        sc_error::Error::config("remove-query needs --name: which query to remove")
    })?;

    let (catalog, mut app) =
        open_app(&parsed.app, &db, &file_stores, base_domain.as_deref()).await?;
    let api = sc_app::select_api(&mut app, parsed.api.as_deref(), "--api")?;
    let mount = api.mount.clone();
    let mut queries = sc_api::custom_queries(&api.config)?;
    let before = queries.len();
    queries.retain(|q| q.name != name);
    if queries.len() == before {
        // Naming what is there, because "no such query" with a list is a typo
        // fixed in one step and without it is a second command to find out.
        return Err(sc_error::Error::not_found(format!(
            "the API at {mount} of `{}` has no custom query named `{name}`; it has {}",
            parsed.app,
            if before == 0 {
                "none".to_owned()
            } else {
                sc_api::custom_queries(&api.config)?
                    .iter()
                    .map(|q| format!("`{}`", q.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )));
    }
    sc_api::set_custom_queries(&mut api.config, &queries)?;
    let saved = save_application(&catalog, &app).await?;
    eprintln!(
        "feldspar: removed `{name}` from the API at {mount} of `{}`",
        parsed.app
    );
    reemit_client(&catalog, &saved).await;
    Ok(())
}
