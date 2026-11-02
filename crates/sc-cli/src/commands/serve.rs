//! `feldspar serve`: connect the database, install the services and start the HTTP server.

use std::sync::Arc;

use sc_api::admin_endpoints;
use sc_auth::SessionStore;
use sc_cli::DbConfig;
use sc_cli::{
    connect_catalog, connect_file_stores, connect_stored_databases, connect_stored_file_stores,
    extract_file_stores,
};
use sc_error::Result;
use sc_server::{AppMounts, ServerConfig, ServiceManager, admin_handlers, mount_all, serve};

/// The file-store IDE's bundle, in the checkout this binary was built in.
///
/// A hard-coded path, deliberately. The IDE is not a deployment choice — it is
/// where an admin edits an application's source, reached from a button in the
/// admin UI — so there is nothing for an operator to decide and no flag to forget:
/// the default build carries the bundle's path, and one built with
/// `SC_BUILD_ADMIN=0` finds `ui/ide/dist` next to the source it was compiled from.
pub(crate) const IDE_BUNDLE_IN_CHECKOUT: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/ide/dist");

/// Where the IDE bundle is: the one built into this binary, else the checkout's.
///
/// `None` only when neither exists — a binary built with `SC_BUILD_ADMIN=0` and run
/// away from its source tree, which has no admin UI to reach the IDE from either.
pub(crate) fn ide_bundle_dir() -> Option<std::path::PathBuf> {
    let candidate = option_env!("SC_IDE_BUNDLE_DIR").unwrap_or(IDE_BUNDLE_IN_CHECKOUT);
    let path = std::path::PathBuf::from(candidate);
    path.join("index.html").exists().then_some(path)
}

/// The Analytics UI's bundle in the checkout this binary was built in — the
/// IDE's arrangement, for the IDE's reason: it is reached from the admin UI's
/// sidebar, so there is nothing for an operator to decide.
pub(crate) const ANALYTICS_BUNDLE_IN_CHECKOUT: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/analytics/dist");

/// Where the Analytics UI bundle is: the one built into this binary, else the
/// checkout's (analytics TODO A1.14).
pub(crate) fn analytics_bundle_dir() -> Option<std::path::PathBuf> {
    let candidate = option_env!("SC_ANALYTICS_BUNDLE_DIR").unwrap_or(ANALYTICS_BUNDLE_IN_CHECKOUT);
    let path = std::path::PathBuf::from(candidate);
    path.join("index.html").exists().then_some(path)
}

/// `feldspar serve [--database-url URL | --db-host H ...] [--bind ADDR] [...]`.
///
/// Database flags are consumed by [`DbConfig::extract`]; whatever is left over is
/// parsed as [`ServerConfig`], so a typo in either still fails loudly.
pub(crate) async fn serve_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (file_store_specs, server_args) = extract_file_stores(rest)?;
    // The selected environment's serving settings come first and the command
    // line after, so a flag beats the file by simply being parsed later — the
    // same order of authority the database settings follow, expressed as
    // argument order rather than as a second merge to keep in step.
    let mut config = ServerConfig::from_args(serving_defaults(&db).iter().chain(&server_args))?;
    // When the binary was built with the admin bundle (the default — see
    // `build.rs`) and no explicit `--static-dir` was given, serve that bundle.
    if config.static_dir.is_none() {
        if let Some(dir) = option_env!("SC_ADMIN_BUNDLE_DIR") {
            config.static_dir = Some(std::path::PathBuf::from(dir));
        }
    }
    config.ide_dir = ide_bundle_dir();
    config.analytics_dir = analytics_bundle_dir();
    // Saltcorn UI's view runtime, when this binary was built with it. Unlike the
    // IDE there is no fallback to the checkout: a build with `SC_BUILD_ADMIN=0`
    // records no directory, and an application that needs one says so on mount.
    config.saltcorn_ui_dir = option_env!("SC_SALTCORN_UI_BUNDLE_DIR").map(std::path::PathBuf::from);
    // The bundled modules, from wherever this binary was packaged to look for
    // them. `build.rs` always records a path; whether the directory is there is
    // a property of the artifact, and a missing one is an empty catalog rather
    // than a failure (`sc_module::bundled`).
    // The builder, likewise only when this binary was built with it: without
    // one, its routes say so and the admin UI keeps the layout as JSON.
    config.builder_dir = option_env!("SC_BUILDER_BUNDLE_DIR").map(std::path::PathBuf::from);
    config.plugins_dir = Some(std::path::PathBuf::from(
        option_env!("SC_PLUGINS_DIR").unwrap_or(sc_module::BUNDLED_IN_CHECKOUT),
    ));

    // Which database this process is about to write to is the one fact worth
    // saying out loud before anything happens — an operator running three
    // environments off one binary should be able to see, in the log, that this
    // one is staging.
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }

    // Start this process's resolver before anything can want a name. It is not
    // glibc's: a `+crt-static` binary that calls `getaddrinfo` loads an NSS
    // module, and with it a second `libc.so.6`, which is fatal (see `sc-dns`).
    // The first name a server resolves is usually the ACME CA's, seconds after
    // the port opens, so a resolver that cannot start is worth knowing about
    // here rather than then.
    sc_dns::init()?;

    // The service manager that started this process, where one did (a
    // `Type=notify` systemd unit). The boot is the interesting part of a
    // Saltcorn start — everything below happens before the port opens — so each
    // step says what it is doing, and `systemctl status` shows it. Off a service
    // manager, every call on it does nothing.
    let service = ServiceManager::from_env();

    // Bring the data layer up before binding: connect the database, load the
    // catalog, and ensure the users table exists. A bad connection fails here
    // with a clear message rather than a server that boots then 500s.
    service.notify_status("connecting to the database");
    let catalog = connect_catalog(&db).await?;

    // How this process serves TLS is a **stored setting**, not a flag (§13.5):
    // the certificate an admin pastes and the ACME account it renews through
    // live in `_fd_config`, so every node against one database serves the same
    // thing and a renewal is not a deploy. Read here, before anything is
    // announced, because it decides the port the outside world reaches this
    // server on — which is what the public origin and the cookie's `Secure`
    // attribute are about to be built from.
    let ssl = sc_config::ssl_settings(&catalog).await?;
    if ssl.public_https() {
        // A session cookie sent over HTTPS — this process's, or the proxy's in
        // front of it (`ssl_mode = "proxy"`) — is a cookie that should not
        // travel over anything else. The flag stays as a way to turn it on
        // without either, so this only ever adds.
        config.secure_cookies = true;
    }
    // A server on this machine only, given no base domain, serves its
    // applications under `localhost` rather than not at all. Decided here,
    // after TLS: everything below (the public origin, the mounts, the
    // certificate's names) reads the base domain.
    if config.base_domain.is_none()
        && let Some(domain) = sc_server::implied_base_domain(&config.addr, ssl.public_https())
    {
        eprintln!(
            "feldspar: no base_domain is set and this server listens on {} only, so \
             applications are served at http://<subdomain>.{domain}:{}",
            config.addr.ip(),
            config.addr.port()
        );
        config.base_domain = Some(domain.to_owned());
    }
    // Where this process serves its applications, recorded for the project
    // generator: an app's `AGENTS.md` and `src/feldspar/README.md` name the URL
    // to open, and this is the only place that knows it (§13.2).
    if let Some(domain) = &config.base_domain {
        // Behind a proxy the outside world reaches the proxy's HTTPS port, not
        // the one bound here; `https_port` names it when it is not 443.
        let port = if ssl.public_https() {
            config.https_port
        } else {
            config.addr.port()
        };
        catalog.set_public_origin(
            sc_catalog::PublicOrigin::new(domain, port).secure(config.secure_cookies),
        );
    }
    // Connect the file stores configured in the admin UI. One that fails — a
    // disk unmounted since it was defined — is logged and skipped, not fatal;
    // it stays listed and editable so the admin can repoint it.
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    // Secret fields' values open only with the data directory's key: a server
    // that holds some and has lost the key stops here, naming the file
    // (TODO-imap M2.2), rather than making a new key that opens none of them.
    sc_catalog::secrets::check_key_on_start(&catalog).await?;
    // Then any requested with `--file-store NAME=PATH`, which are ephemeral and
    // must not silently shadow a configured store of the same name.
    connect_file_stores(&catalog, &file_store_specs)?;

    let Services {
        apps,
        triggers,
        view_services,
    } = install_services(&catalog, &config, &service).await?;

    // Bring the applications up. `--base-domain` is what makes them addressable
    // (an app is served at `<subdomain>.<base-domain>`), so mounting is gated on
    // it: without a base domain no request could ever reach an app, and mounting
    // one would make the router refuse to build. With one, every stored app is
    // built and mounted now — a build that fails is logged and skipped, never
    // fatal (§13.2), and can be fixed and rebuilt without a restart.
    // Where the admin UI is served (Settings → Development). Before the mounts
    // and the certificate's names, which both read it. Without a base domain
    // there is no subdomain to move to, so a stored one is reported and left
    // unused rather than stopping the boot.
    match sc_config::admin_subdomain(&catalog).await {
        Ok(Some(subdomain)) if config.base_domain.is_some() => {
            apps.set_admin_subdomain(Some(subdomain));
        }
        Ok(Some(subdomain)) => eprintln!(
            "feldspar: the admin subdomain `{subdomain}` is ignored: there is no --base-domain \
             for it to be a subdomain of"
        ),
        Ok(None) => {}
        Err(e) => eprintln!("feldspar: the admin subdomain is ignored: {e}"),
    }
    // Whether the session cookie is shared between applications, how long a
    // session lasts and whether its cookie outlives the browser (Settings →
    // Authentication), read before the first request sets one.
    match sc_config::shared_session_cookie(&catalog).await {
        Ok(shared) => apps.set_shared_session_cookie(shared),
        Err(e) => eprintln!("feldspar: the shared session cookie setting is ignored: {e}"),
    }
    match sc_config::session_policy(&catalog).await {
        Ok((hours, persistent)) => {
            apps.set_session_ttl_hours(hours);
            apps.set_persistent_session_cookie(persistent);
        }
        Err(e) => eprintln!("feldspar: the session lifetime setting is ignored: {e}"),
    }

    service.notify_status("mounting applications");
    if config.base_domain.is_some() {
        mount_all(&apps).await;
        // A coding run's `check` mounts its green builds as previews beside the
        // live mounts (TODO §7b). Without a base domain a preview has no host.
        view_services.set_previews(apps.clone());
    }
    // An agent with `may_install_modules` installs through the same path as the
    // Modules tab (`sc_agent::ModuleManager`).
    view_services.set_modules(apps.clone());

    // The certificate's names, now that the mounts are known: the base domain,
    // every mounted application's subdomain (and the admin UI's, if it has
    // moved to one), and whatever else the admin listed
    // (§13.5). The three are kept apart rather than flattened because the middle
    // one is live — an application created while the server runs adds a name, and
    // `serve` hands the mount registry the certificate so that becomes a new
    // order rather than something a restart fixes. A settings mistake stops the
    // boot rather than quietly serving plain HTTP: an admin who configured TLS
    // and got HTTP would not find out from the server.
    config.tls = sc_server::TlsSettings::from_ssl(
        &ssl,
        config.https_port,
        sc_server::TlsNames::new(
            config.base_domain.clone(),
            apps.certificate_subdomains(),
            ssl.extra_domains.clone(),
        ),
        Some(sc_config::AcmeCache::new(catalog.clone())),
    )?;

    // Everything is up — catalog, file stores, applications, triggers — and the
    // listener has not been announced yet, which is exactly what the `startup`
    // event means.
    service.notify_status("running startup triggers");
    sc_server::fire_startup(&catalog, &triggers).await;

    // The clock's turn: from here a periodic trigger fires on its own schedule,
    // and one whose run was missed while the process was down catches up — once —
    // on the first tick. Held for the lifetime of `serve`; the task ends with the
    // process.
    let (_scheduler, _scheduler_task) = sc_server::start_scheduler(&catalog, &triggers);

    // The modules' background services (an IMAP sync), for the scheduler's
    // reason: only the serving process runs them.
    if let Some(modules) = apps.modules() {
        modules.start_services().await;
    }

    // And the workflow engine's (§10.3): from here a trigger whose body is a
    // workflow starts a durable run, a suspended run's timer fires, and a run a
    // crashed node was holding is picked up when its lease runs out. Started
    // here and nowhere else, for the reason the scheduler is.
    let (_workflows, _workflow_task) = sc_server::start_workflow_engine(&catalog, &triggers);

    // And the automated backups' (Settings → Backup): each schedule writes a
    // backup to its directory when it is due, and prunes the old ones.
    let (_backups, _backup_task) = sc_backup::start_backup_scheduler(&catalog);

    // Sessions are rows, not process memory (§7.2), which is what lets a second
    // application server exist: put two of these behind a load balancer and a
    // session minted by either is a session both honour. Each keeps its own
    // bounded cache in front of the table, so the common case is still a map
    // lookup.
    let sessions = Arc::new(SessionStore::database(catalog.clone()));
    if config.tls.enabled() && config.tls.redirect_http() {
        eprintln!(
            "feldspar: listening on http://{} (redirecting to HTTPS)",
            config.addr
        );
    } else {
        eprintln!("feldspar: listening on http://{}", config.addr);
    }
    let handlers = admin_handlers(catalog, apps.clone());
    serve(config, admin_endpoints(), handlers, sessions, apps).await
}

/// What [`install_services`] assembles: the mount registry with every service
/// an application, a trigger or a restore reaches through it, and the two
/// pieces `serve` goes on to start things with.
pub(crate) struct Services {
    pub(crate) apps: Arc<AppMounts>,
    pub(crate) triggers: Arc<sc_action::TriggerDispatcher>,
    pub(crate) view_services: Arc<sc_agent::ViewServices>,
}

/// The agents, models, triggers, modules and streams over `catalog`, and the
/// mount registry that carries them — everything `serve` brings up between
/// connecting the database and mounting the applications.
///
/// One function rather than a copy in each caller, because `restore` needs the
/// same set: a backup reinstalls modules, saves agents and triggers through
/// their registries and builds the applications it restores, and a restore from
/// a terminal that assembled less than the server does would skip what the
/// restore dialog does not.
pub(crate) async fn install_services(
    catalog: &Arc<sc_catalog::Catalog>,
    config: &ServerConfig,
    service: &ServiceManager,
) -> Result<Services> {
    // The JS engine ownership formulas evaluate on (§7.3): one isolate for the
    // whole server, shared by every mounted app's providers — and by the trigger
    // dispatcher, whose `only_if` formulas and action configuration are the same
    // language evaluated the same way. It also carries the **code** pool a
    // `run_js_code` body runs on, which is what `--code-workers` and
    // `--code-max-inflight` size (§10.1) — built on first use, so a deployment
    // with no code bodies pays for neither.
    let evaluator = sc_server::js_evaluator(config);

    // Agents: the built-in trait set and the two tables an agent and its runs
    // live in (§11.2). A stored agent that does not validate is reported and
    // dropped from the live set, exactly as a trigger that does not is — the
    // rest of the server works and the admin can repair it in the UI. It comes
    // before the triggers because `run_agent` is one of the actions a trigger
    // may name (§11.5), and it needs the assembled trait set.
    // The headless browser `view_app` drives (TODO §7b), found once: on a host
    // without one the grant is refused, so say which it is before the agents
    // are validated against it.
    // A sandboxed browser is started once here, so a sandbox the host forbids
    // (a container under Docker's default seccomp profile) is a line in the
    // boot log naming the fix rather than a failure in the middle of a run.
    let browser = sc_server::detect_browser(config.browser.as_deref()).and_then(|path| {
        if config.browser_sandbox {
            sc_server::check_browser_sandbox(&path).map(|()| path)
        } else {
            Ok(path)
        }
    });
    match &browser {
        Ok(path) => eprintln!(
            "feldspar: view_app will use the browser at {}",
            path.display()
        ),
        Err(reason) => eprintln!("feldspar: view_app is unavailable: {reason}"),
    }
    let agents =
        sc_server::install_agents_on(catalog, sc_agent::HostCapabilities { browser }).await?;

    // Models: the two tables a model and its fits live in, and the reap of any
    // fit that was running when this process last stopped (§8). A fit's registry
    // is its row, so nothing survives a restart and an instance still saying
    // `fitting` at boot is one nothing will finish — it is failed by name here,
    // before anything can read it.
    // It is also what carries the provider registry and the fits into the
    // action set below: `fit_model` needs both, so the models come up before
    // the triggers do.
    let models =
        sc_server::install_models_with(catalog, config.model_max_rows, &config.stan).await?;
    // Which CmdStan Stan models will use, and how many chains may run at once
    // — or why there is none — said once, as the browser is (Stan TODO §20).
    let stan = models.stan();
    match stan.cmdstan() {
        Some(cmdstan) => eprintln!(
            "feldspar: Stan models will use CmdStan {} at {} ({}), up to {} chain \
             process(es) at once, compiling into {}",
            cmdstan.version,
            cmdstan.dir.display(),
            cmdstan.source,
            stan.budget().processes(),
            stan.compile_cache().dir().display()
        ),
        None => eprintln!(
            "feldspar: Stan models are unavailable: {}",
            stan.unavailable().unwrap_or("CmdStan was not found")
        ),
    }

    // Triggers: the built-in actions plus `run_agent`, the stored trigger set,
    // and the dispatcher installed into the catalog — after which a row write
    // raises an event. Before it, nothing observes writes, which is what keeps
    // `build-app` and every other command from firing anything. It comes before
    // the mounts because the mount registry carries the dispatcher: an app's
    // login and its errors raise events through the same router the admin API's
    // do.
    // The Python adapter goes on with them (§15): built here because the bounds
    // are this process's flags, registered whether or not this binary has an
    // interpreter linked in, and starting nothing — the interpreter is the first
    // Python body's cost, exactly as the code isolate pool is the first
    // `run_js_code` body's.
    // Built once and held: the dispatcher takes it as an adapter, and the mount
    // registry takes the runtime itself, because the diagnostics screen asks it
    // questions the adapter trait does not carry (phase 4.2).
    let python = sc_server::python_adapter(config);
    let triggers = sc_server::install_triggers_with_adapters(
        catalog,
        evaluator.clone(),
        &agents,
        &models,
        [python.clone() as Arc<dyn sc_expr::CodeAdapter>],
    )
    .await?;

    // Modules: every installed v1 plugin loaded onto the module worker pool, its actions
    // added to the registry the dispatcher just took, and the trigger set
    // reloaded against the result — which is what lets a trigger name
    // `mqtt_publish`. After the triggers because it *changes* what they were
    // validated against; a module that will not load is reported and skipped,
    // never a reason not to boot.
    service.notify_status("loading modules");
    let modules = sc_server::ModuleServices::install(
        catalog,
        &triggers,
        &agents,
        &models,
        config.modules_dir.clone(),
        config.plugins_dir.clone(),
        config.module_workers,
        // The same runtime the dispatcher took as a code adapter: one
        // interpreter per process, so a Python module and a Python body share
        // it, and one environment, which is what pip installs into.
        python.clone(),
        // Saltcorn UI's view runtime runs on the same pool, as a built-in.
        config.saltcorn_ui_dir.clone(),
    )
    .await?;

    // Streams: the `_fd_streams` table, the provider registry, and the
    // supervisor that subscribes to every enabled stream (TODO "Streams").
    // **After the triggers**, because the sink it installs fires the dispatcher
    // — an element that arrived before the triggers were up would have nothing
    // to fire — and after the modules, so a stream over a module-supplied
    // provider finds it. Started only by `serve`, for the reason the scheduler
    // is: a `build-app` that opened the same database must not connect to
    // somebody's broker.
    service.notify_status("starting streams");
    // Over the provider registry the modules just built: the built-ins plus
    // whatever a module supplies as a poll (TODO "Streams" §12). And the
    // modules are told where the streams are, so the next module change can
    // rebuild that registry and reload the supervisor against it — which is
    // what makes installing a stream provider a thing that takes effect without
    // a restart.
    let streams = sc_server::install_streams_with(
        catalog,
        &triggers,
        modules.stream_registry(),
        config.streams,
    )
    .await?;
    modules.set_streams(streams.clone());

    // Held past `with_agents`, for the caller's previewer.
    let view_services = agents.registry().view_services().clone();
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_base_domain(config.base_domain.clone())
            .with_preview_idle(config.preview_idle)
            .with_evaluator(evaluator)
            .with_triggers(triggers.clone())
            .with_agents(agents)
            .with_models(models)
            .with_streams(streams)
            .with_modules(modules)
            .with_python(python)
            .with_saltcorn_ui_dir(config.saltcorn_ui_dir.clone())
            .with_analytics_dir(config.analytics_dir.clone()),
    );
    Ok(Services {
        apps,
        triggers,
        view_services,
    })
}

/// Take `flag`'s value out of `args`, returning it and what remains.
///
/// Every command that *writes* generated documentation accepts
/// `--base-domain`, and each of them parses the rest of its arguments its own
/// way, so this is pulled out ahead of that rather than added to three
/// unrelated parsers.
pub(crate) fn take_option(args: Vec<String>, flag: &str) -> Result<(Option<String>, Vec<String>)> {
    let mut value = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if arg == flag {
            value = Some(
                it.next()
                    .ok_or_else(|| sc_error::Error::config(format!("{flag} needs a value")))?,
            );
        } else {
            rest.push(arg);
        }
    }
    Ok((value, rest))
}

/// The selected environment's serving settings, spelled as the `serve` flags
/// they mirror, to be parsed *before* the operator's own.
///
/// A `bind` that does not parse is left for [`ServerConfig::from_args`] to
/// refuse: it is about to be bound, and one parser saying so beats two
/// disagreeing about what a socket address is.
pub(crate) fn serving_defaults(db: &DbConfig) -> Vec<String> {
    let serving = db.serving();
    let mut flags = Vec::new();
    if let Some(domain) = serving.base_domain() {
        flags.push("--base-domain".to_owned());
        flags.push(domain.to_owned());
    }
    for domain in serving.extra_base_domains() {
        flags.push("--extra-base-domain".to_owned());
        flags.push(domain.to_owned());
    }
    if let Some(bind) = serving.bind() {
        flags.push("--bind".to_owned());
        flags.push(bind.to_owned());
    }
    if serving.secure_cookies() == Some(true) {
        flags.push("--secure-cookies".to_owned());
    }
    if let Some(port) = serving.https_port() {
        flags.push("--https-port".to_owned());
        flags.push(port.to_string());
    }
    if let Some(browser) = serving.browser() {
        flags.push("--browser".to_owned());
        flags.push(browser.to_owned());
    }
    if serving.browser_sandbox() == Some(false) {
        flags.push("--no-browser-sandbox".to_owned());
    }
    for (flag, value) in serving.stan_flags() {
        flags.push(flag.to_owned());
        flags.push(value);
    }
    flags
}

/// Record where this deployment serves its applications, for the generated
/// documentation a build or a definition change rewrites.
///
/// The command line's `--base-domain` outranks the configuration file's, and
/// with neither there is nothing to record: the documentation then names the
/// setting to supply instead of inventing a hostname. This is what keeps a
/// command-line build's output identical to the server's — see
/// [`Environment`](sc_config_file::Environment).
pub(crate) fn set_public_origin(
    catalog: &sc_catalog::Catalog,
    db: &DbConfig,
    base_domain: Option<&str>,
) {
    if let Some(origin) = db.serving().public_origin(base_domain) {
        catalog.set_public_origin(origin);
    }
}
