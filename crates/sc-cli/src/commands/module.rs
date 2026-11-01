//! `feldspar module …`: installing and inspecting modules.

use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::Result;
use sc_server::{ServerConfig, ServiceManager};

use super::serve::{Services, install_services, serving_defaults, take_option};

/// `feldspar module install|grant|configure|list|remove` — the Modules tab
/// without an admin session. See `sc_cli::module`.
///
/// The module machinery is brought up in this process as `restore` brings it
/// up — over the same database and the same modules root (`--modules-dir`, as
/// the server was started with) — and every change goes through the Modules
/// tab's own `ModuleServices` methods. Nothing here reaches a running server:
/// its next `SIGHUP` sees that `_fd_modules` changed and reloads the modules.
pub(crate) async fn module_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (modules_dir, rest) = take_option(rest, "--modules-dir")?;
    let parsed = sc_cli::module::parse(&rest)?;
    let mut flags = serving_defaults(&db);
    if let Some(dir) = modules_dir {
        flags.push("--modules-dir".to_owned());
        flags.push(dir);
    }
    let config = ServerConfig::from_args(flags.iter())?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    // `list --json` is an answer on stdout.
    sc_log::set_log_sql(false);
    sc_dns::init()?;
    let Services { apps, .. } =
        install_services(&catalog, &config, &ServiceManager::default()).await?;
    let modules = apps
        .modules()
        .cloned()
        .ok_or_else(|| sc_error::Error::msg("the module services did not start"))?;
    let named = |name: String| {
        let catalog = catalog.clone();
        async move {
            sc_module::load_module_by_name(&catalog, &name)
                .await?
                .ok_or_else(|| {
                    sc_error::Error::not_found(format!(
                        "no module `{name}` is installed; `feldspar module list` shows the ones \
                         that are"
                    ))
                })
        }
    };

    use sc_cli::module::ModuleCommand;
    match parsed {
        ModuleCommand::Install(install) => module_install(&catalog, &modules, install).await,
        ModuleCommand::Grant {
            name,
            grant,
            revoke,
        } => {
            let mut module = named(name).await?;
            if sc_cli::module::apply_grant(&mut module.permissions, &grant, revoke)? {
                modules.update_module(&module).await?;
                eprintln!(
                    "feldspar: {} `{}` {} {} — a running server picks it up on SIGHUP",
                    if revoke { "revoked" } else { "granted" },
                    module.name,
                    grant.permission.key(),
                    grant.value
                );
            } else {
                eprintln!("feldspar: `{}` unchanged", module.name);
            }
            module_report(&modules, &module.name, false)
        }
        ModuleCommand::Configure { name, settings } => {
            let mut module = named(name).await?;
            let spec = modules
                .modules()
                .get(&module.name)
                .map(|loaded| loaded.config_spec.clone())
                .unwrap_or_default();
            if sc_cli::module::apply_settings(&mut module.configuration, &spec, &settings)? {
                modules.update_module(&module).await?;
                eprintln!(
                    "feldspar: configured `{}` — a running server picks it up on SIGHUP",
                    module.name
                );
            } else {
                eprintln!("feldspar: `{}` unchanged", module.name);
            }
            module_report(&modules, &module.name, false)
        }
        ModuleCommand::List { json } => {
            let set = modules.modules();
            if json {
                let out: Vec<serde_json::Value> =
                    set.modules().iter().map(loaded_module_json).collect();
                println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
                return Ok(());
            }
            for loaded in set.modules() {
                let m = &loaded.module;
                println!(
                    "{}  {}  {}  {}{}",
                    m.name,
                    m.version.as_deref().unwrap_or("-"),
                    if loaded.is_loaded() {
                        "loaded"
                    } else {
                        "not loaded"
                    },
                    m.location,
                    match loaded.issues.is_empty() {
                        true => String::new(),
                        false => format!("  ({})", loaded.issues.join("; ")),
                    }
                );
            }
            Ok(())
        }
        ModuleCommand::Remove { name } => {
            let Some(module) = sc_module::load_module_by_name(&catalog, &name).await? else {
                // Idempotent: removing what is not installed is what was asked.
                eprintln!("feldspar: no module `{name}` is installed; nothing to remove");
                return Ok(());
            };
            modules.remove_module(&module).await?;
            eprintln!("feldspar: removed `{name}` — a running server drops it on SIGHUP");
            Ok(())
        }
    }
}

/// `module install --local DIR …`: install (or upgrade) and load, then the
/// grants and settings — skipping the package manager when this version from
/// this directory is already installed.
pub(crate) async fn module_install(
    catalog: &sc_catalog::Catalog,
    modules: &sc_server::ModuleServices,
    install: sc_cli::module::InstallArgs,
) -> Result<()> {
    let dir = std::fs::canonicalize(&install.dir).map_err(|e| {
        sc_error::Error::invalid(format!(
            "no directory at {} to install a module from: {e}",
            install.dir.display()
        ))
    })?;
    let language = sc_cli::module::detect_language(&dir, install.python)?;
    let location = dir.display().to_string();
    let grant_all = |module: &mut sc_module::Module| -> Result<bool> {
        let mut changed = false;
        for grant in &install.grants {
            changed |= sc_cli::module::apply_grant(&mut module.permissions, grant, false)?;
        }
        Ok(changed)
    };

    // The same version from the same directory, already on the disk: there is
    // nothing for npm to do. (pip is asked every time; it is idempotent itself.)
    let current = match language {
        sc_module::ModuleLanguage::JavaScript if !install.force => {
            let (name, version) = sc_cli::module::npm_package(&dir)?;
            sc_module::load_module_by_name(catalog, &name)
                .await?
                .filter(|m| {
                    m.version.as_deref() == Some(version.as_str())
                        && m.location == location
                        && modules.installer().is_installed(&name)
                })
        }
        _ => None,
    };
    let module = match current {
        Some(mut module) => {
            let spec = modules
                .modules()
                .get(&module.name)
                .map(|loaded| loaded.config_spec.clone())
                .unwrap_or_default();
            let granted = grant_all(&mut module)?;
            let configured = sc_cli::module::apply_settings(
                &mut module.configuration,
                &spec,
                &install.settings,
            )?;
            if granted || configured {
                modules.update_module(&module).await?;
                eprintln!(
                    "feldspar: `{}` {} is already installed; updated its {}",
                    module.name,
                    module.version.as_deref().unwrap_or(""),
                    match (granted, configured) {
                        (true, true) => "permissions and settings",
                        (true, false) => "permissions",
                        _ => "settings",
                    }
                );
            } else {
                eprintln!(
                    "feldspar: `{}` {} is already installed from {location}; nothing changed",
                    module.name,
                    module.version.as_deref().unwrap_or("")
                );
            }
            module
        }
        None => {
            let mut module = modules
                .install_module(
                    language,
                    sc_module::ModuleSource::Local,
                    &location,
                    |module| grant_all(module).map(|_| ()),
                )
                .await?;
            // The settings form is the loaded package's, so settings are typed
            // against it once it has loaded.
            if !install.settings.is_empty() {
                let spec = modules
                    .modules()
                    .get(&module.name)
                    .map(|loaded| loaded.config_spec.clone())
                    .unwrap_or_default();
                if sc_cli::module::apply_settings(
                    &mut module.configuration,
                    &spec,
                    &install.settings,
                )? {
                    modules.update_module(&module).await?;
                }
            }
            eprintln!(
                "feldspar: installed `{}` {} from {location} — a running server loads it on \
                 SIGHUP",
                module.name,
                module.version.as_deref().unwrap_or("")
            );
            module
        }
    };
    module_report(modules, &module.name, install.json)
}

/// Say how a module stands after a change — its permissions and whether it
/// loaded — and fail when it did not load, so a script stops there.
pub(crate) fn module_report(
    modules: &sc_server::ModuleServices,
    name: &str,
    json: bool,
) -> Result<()> {
    let set = modules.modules();
    let loaded = set
        .get(name)
        .ok_or_else(|| sc_error::Error::msg(format!("`{name}` is not in the loaded set")))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&loaded_module_json(loaded)).unwrap_or_default()
        );
    }
    let sentences = loaded.module.permissions.sentences();
    eprintln!(
        "feldspar: `{name}` {}",
        match sentences.is_empty() {
            true => "may reach nothing outside its own package".to_owned(),
            false => sentences.join("; "),
        }
    );
    for issue in &loaded.issues {
        eprintln!("feldspar: `{name}`: {issue}");
    }
    match loaded.is_loaded() {
        true => Ok(()),
        false => Err(sc_error::Error::invalid(format!(
            "`{name}` is installed but did not load: {}",
            loaded.issues.join("; ")
        ))),
    }
}

/// One module as `module list --json` reports it: the row (secrets redacted),
/// whether it loaded, what is wrong with it, and the routes it serves.
pub(crate) fn loaded_module_json(loaded: &sc_module::LoadedModule) -> serde_json::Value {
    let m = &loaded.module;
    serde_json::json!({
        "id": m.id.0,
        "name": m.name,
        "language": m.language.as_str(),
        "source": m.source.as_str(),
        "location": m.location,
        "version": m.version,
        "configuration": serde_json::Value::Object(sc_module::redacted_configuration(loaded)),
        "permissions": serde_json::Value::Object(m.permissions.to_json()),
        "loaded": loaded.is_loaded(),
        "issues": loaded.issues,
        "routes": loaded
            .manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .routes
                    .iter()
                    .map(|route| sc_module::route_path(&m.name, &route.url))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    })
}
