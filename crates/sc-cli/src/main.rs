//! The feldspar binary: serve and management commands (layer 10).
//!
//! The entry point runs on the **tokio** runtime (the workspace-wide runtime
//! decision), since the server it launches is async top to bottom. It exposes
//! the `serve` command, which connects the primary database, initialises the
//! [`Catalog`](sc_catalog::Catalog), and starts the HTTP server with the admin
//! API mounted. The database connection is configured by [`DbConfig`] — flags,
//! the environment, or the environment selected with `--environment` out of the
//! `feldspar.toml` configuration file; the remaining flags configure the HTTP
//! server itself ([`ServerConfig`]). The reusable boot logic lives in the crate
//! library ([`sc_cli`]).

use std::process::ExitCode;
use std::sync::Arc;

use sc_api::admin_endpoints;
use sc_app::{
    app_source_from_config, build_application, load_application_by_subdomain, save_application,
};
use sc_auth::SessionStore;
use sc_cli::DbConfig;
use sc_cli::{
    connect_catalog, connect_file_stores, connect_stored_databases, connect_stored_file_stores,
    extract_file_stores,
};
use sc_error::Result;
use sc_server::{AppMounts, ServerConfig, ServiceManager, admin_handlers, mount_all, serve};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The **chain**, not just the outermost context: every boot step
            // wraps its failure in a sentence saying what it was doing, and
            // printing only that one ("ensuring the triggers table exists")
            // throws away the sentence that says what actually went wrong.
            eprintln!("error: {}", sc_error::format_chain(&e));
            ExitCode::FAILURE
        }
    }
}

/// Dispatch a subcommand.
async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("serve") => serve_command(&args[1..]).await,
        Some("build-app") => build_app_command(&args[1..]).await,
        Some("api") => api_command(&args[1..]).await,
        Some("get-cfg") => get_cfg_command(&args[1..]).await,
        Some("set-cfg") => set_cfg_command(&args[1..]).await,
        Some("auth") => auth_command(&args[1..]).await,
        Some("agent") => agent_command(&args[1..]).await,
        Some("i18n") => i18n_command(&args[1..]).await,
        Some("cmdstan") => cmdstan_command(&args[1..]).await,
        Some("demo") => demo_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown command `{other}`"
        ))),
        None => {
            print_usage();
            Ok(())
        }
    }
}

/// The file-store IDE's bundle, in the checkout this binary was built in.
///
/// A hard-coded path, deliberately. The IDE is not a deployment choice — it is
/// where an admin edits an application's source, reached from a button in the
/// admin UI — so there is nothing for an operator to decide and no flag to forget:
/// the default build carries the bundle's path, and one built with
/// `SC_BUILD_ADMIN=0` finds `ui/ide/dist` next to the source it was compiled from.
const IDE_BUNDLE_IN_CHECKOUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/ide/dist");

/// Where the IDE bundle is: the one built into this binary, else the checkout's.
///
/// `None` only when neither exists — a binary built with `SC_BUILD_ADMIN=0` and run
/// away from its source tree, which has no admin UI to reach the IDE from either.
fn ide_bundle_dir() -> Option<std::path::PathBuf> {
    let candidate = option_env!("SC_IDE_BUNDLE_DIR").unwrap_or(IDE_BUNDLE_IN_CHECKOUT);
    let path = std::path::PathBuf::from(candidate);
    path.join("index.html").exists().then_some(path)
}

/// The Analytics UI's bundle in the checkout this binary was built in — the
/// IDE's arrangement, for the IDE's reason: it is reached from the admin UI's
/// sidebar, so there is nothing for an operator to decide.
const ANALYTICS_BUNDLE_IN_CHECKOUT: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/analytics/dist");

/// Where the Analytics UI bundle is: the one built into this binary, else the
/// checkout's (analytics TODO A1.14).
fn analytics_bundle_dir() -> Option<std::path::PathBuf> {
    let candidate = option_env!("SC_ANALYTICS_BUNDLE_DIR").unwrap_or(ANALYTICS_BUNDLE_IN_CHECKOUT);
    let path = std::path::PathBuf::from(candidate);
    path.join("index.html").exists().then_some(path)
}

/// `feldspar serve [--database-url URL | --db-host H ...] [--bind ADDR] [...]`.
///
/// Database flags are consumed by [`DbConfig::extract`]; whatever is left over is
/// parsed as [`ServerConfig`], so a typo in either still fails loudly.
async fn serve_command(args: &[String]) -> Result<()> {
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
        option_env!("SC_PLUGINS_DIR").unwrap_or(sc_server::BUNDLED_IN_CHECKOUT),
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
    if ssl.enabled() {
        // A session cookie sent over the HTTPS this process is about to serve is
        // a cookie that should not travel over anything else. The flag stays as
        // a way to turn it on *without* TLS here (a TLS-terminating proxy in
        // front), so this only ever adds.
        config.secure_cookies = true;
    }
    // Where this process serves its applications, recorded for the project
    // generator: an app's `AGENTS.md` and `src/feldspar/README.md` name the URL
    // to open, and this is the only place that knows it (§13.2).
    if let Some(domain) = &config.base_domain {
        let port = if ssl.enabled() {
            ssl.https_port
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
    // Then any requested with `--file-store NAME=PATH`, which are ephemeral and
    // must not silently shadow a configured store of the same name.
    connect_file_stores(&catalog, &file_store_specs)?;

    // Bring the applications up. `--base-domain` is what makes them addressable
    // (an app is served at `<subdomain>.<base-domain>`), so mounting is gated on
    // it: without a base domain no request could ever reach an app, and mounting
    // one would make the router refuse to build. With one, every stored app is
    // built and mounted now — a build that fails is logged and skipped, never
    // fatal (§13.2), and can be fixed and rebuilt without a restart.
    // The JS engine ownership formulas evaluate on (§7.3): one isolate for the
    // whole server, shared by every mounted app's providers — and by the trigger
    // dispatcher, whose `only_if` formulas and action configuration are the same
    // language evaluated the same way. It also carries the **code** pool a
    // `run_js_code` body runs on, which is what `--code-workers` and
    // `--code-max-inflight` size (§10.1) — built on first use, so a deployment
    // with no code bodies pays for neither.
    let evaluator = sc_server::js_evaluator(&config);

    // Agents: the built-in trait set and the two tables an agent and its runs
    // live in (§11.2). A stored agent that does not validate is reported and
    // dropped from the live set, exactly as a trigger that does not is — the
    // rest of the server works and the admin can repair it in the UI. It comes
    // before the triggers because `run_agent` is one of the actions a trigger
    // may name (§11.5), and it needs the assembled trait set.
    // The headless browser `view_app` drives (TODO §7b), found once: on a host
    // without one the grant is refused, so say which it is before the agents
    // are validated against it.
    let browser = sc_server::detect_browser(config.browser.as_deref());
    match &browser {
        Ok(path) => eprintln!(
            "feldspar: view_app will use the browser at {}",
            path.display()
        ),
        Err(reason) => eprintln!("feldspar: view_app is unavailable: {reason}"),
    }
    let agents =
        sc_server::install_agents_on(&catalog, sc_agent::HostCapabilities { browser }).await?;

    // Models: the two tables a model and its fits live in, and the reap of any
    // fit that was running when this process last stopped (§8). A fit's registry
    // is its row, so nothing survives a restart and an instance still saying
    // `fitting` at boot is one nothing will finish — it is failed by name here,
    // before anything can read it.
    // It is also what carries the provider registry and the fits into the
    // action set below: `fit_model` needs both, so the models come up before
    // the triggers do.
    let models =
        sc_server::install_models_with(&catalog, config.model_max_rows, &config.stan).await?;
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
    let python = sc_server::python_adapter(&config);
    let triggers = sc_server::install_triggers_with_adapters(
        &catalog,
        evaluator.clone(),
        &agents,
        &models,
        [python.clone() as Arc<dyn sc_server::CodeAdapter>],
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
        &catalog,
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
        &catalog,
        &triggers,
        modules.stream_registry(),
        config.streams,
    )
    .await?;
    modules.set_streams(streams.clone());

    service.notify_status("mounting applications");
    // Held past `with_agents`, for installing the previewer below.
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
            .with_saltcorn_ui_dir(config.saltcorn_ui_dir.clone()),
    );
    if config.base_domain.is_some() {
        mount_all(&apps).await;
        // A coding run's `check` mounts its green builds as previews beside the
        // live mounts (TODO §7b). Without a base domain a preview has no host.
        view_services.set_previews(apps.clone());
    }

    // The certificate's names, now that the mounts are known: the base domain,
    // every mounted application's subdomain, and whatever else the admin listed
    // (§13.5). The three are kept apart rather than flattened because the middle
    // one is live — an application created while the server runs adds a name, and
    // `serve` hands the mount registry the certificate so that becomes a new
    // order rather than something a restart fixes. A settings mistake stops the
    // boot rather than quietly serving plain HTTP: an admin who configured TLS
    // and got HTTP would not find out from the server.
    config.tls = sc_server::TlsSettings::from_ssl(
        &ssl,
        sc_server::TlsNames::new(
            config.base_domain.clone(),
            apps.subdomains(),
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

    // And the workflow engine's (§10.3): from here a trigger whose body is a
    // workflow starts a durable run, a suspended run's timer fires, and a run a
    // crashed node was holding is picked up when its lease runs out. Started
    // here and nowhere else, for the reason the scheduler is.
    let (_workflows, _workflow_task) = sc_server::start_workflow_engine(&catalog, &triggers);

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

/// Take `flag`'s value out of `args`, returning it and what remains.
///
/// Every command that *writes* generated documentation accepts
/// `--base-domain`, and each of them parses the rest of its arguments its own
/// way, so this is pulled out ahead of that rather than added to three
/// unrelated parsers.
fn take_option(args: Vec<String>, flag: &str) -> Result<(Option<String>, Vec<String>)> {
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
fn serving_defaults(db: &DbConfig) -> Vec<String> {
    let serving = db.serving();
    let mut flags = Vec::new();
    if let Some(domain) = serving.base_domain() {
        flags.push("--base-domain".to_owned());
        flags.push(domain.to_owned());
    }
    if let Some(bind) = serving.bind() {
        flags.push("--bind".to_owned());
        flags.push(bind.to_owned());
    }
    if serving.secure_cookies() == Some(true) {
        flags.push("--secure-cookies".to_owned());
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
/// [`Environment`](sc_cli::Environment).
fn set_public_origin(catalog: &sc_catalog::Catalog, db: &DbConfig, base_domain: Option<&str>) {
    if let Some(origin) = db.serving().public_origin(base_domain) {
        catalog.set_public_origin(origin);
    }
}

/// `feldspar build-app SUBDOMAIN [database flags] [--file-store NAME=PATH]`.
///
/// Builds one application from the command line, printing the tool output as it
/// goes and failing with the bundler's own diagnostics.
///
/// The admin UI can already build an app, and this does the same work — so why
/// have it? Because when a build fails, the UI shows the *result* and this shows
/// the *run*: it is scriptable, it is what a deploy step or a CI job calls, and
/// its output goes to a terminal where it can be piped, grepped and kept. It also
/// works when the app cannot be reached in a browser at all, which is precisely
/// the state a failing build tends to leave a deployment in.
///
/// Deliberately **builds without mounting**: nothing is served by this process,
/// so running it against a live deployment's database cannot disturb what that
/// server is serving. The next build or restart there picks up the output.
/// `feldspar agent eval <suite> [...]`.
///
/// The only `agent` subcommand for now, and the reason the group exists rather
/// than a top-level `eval`: what is evaluated is the agent, and anything else
/// worth doing to one from a terminal (listing them, running one) belongs beside
/// it rather than at the top of the binary's vocabulary.
async fn agent_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("eval") => agent_eval_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown agent subcommand `{other}`; the only one is `eval`"
        ))),
        None => Err(sc_error::Error::config(
            "usage: feldspar agent eval SUITE [--model provider/model] [database flags]",
        )),
    }
}

/// Run an eval suite against the models named on the command line (§13).
///
/// **This command spends money.** Nothing else in the workspace calls a vendor,
/// which is why the harness is a command and not a test: an operator asking for
/// an eval has decided to pay for one.
async fn agent_eval_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args.to_vec())?;
    let (file_store_specs, rest) = extract_file_stores(rest)?;
    let eval = sc_cli::eval::EvalArgs::parse(&rest)?;
    let mut tasks = sc_cli::eval::load_suite(&eval.suite)?;
    if !eval.only.is_empty() {
        let wanted = eval.only.clone();
        tasks.retain(|task| wanted.contains(&task.name));
        if tasks.is_empty() {
            return Err(sc_error::Error::config(format!(
                "no task of `{}` is named {}",
                eval.suite.display(),
                wanted.join(" or ")
            )));
        }
    }

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    connect_file_stores(&catalog, &file_store_specs)?;

    // The same host the server assembles: a task's agent is the builder agent,
    // whose `view_app` grant a host without a browser cannot honour.
    let browser = sc_server::detect_browser(None);
    if let Err(reason) = &browser {
        eprintln!("feldspar: view_app is unavailable: {reason}");
    }
    let agents =
        sc_server::install_agents_on(&catalog, sc_agent::HostCapabilities { browser }).await?;
    let registry = agents.registry().clone();
    let host =
        sc_cli::eval::EvalHost::new(&catalog, &registry).with_connector(agents.providers().clone());

    eprintln!(
        "feldspar: running {} task(s) of `{}`",
        tasks.len(),
        eval.suite.display()
    );
    let report = sc_cli::eval::run_suite(&host, &eval, &tasks).await?;
    let (json, markdown) = sc_cli::eval::write_report(&eval, &report)?;
    print!("{}", report.markdown());
    eprintln!(
        "feldspar: {} of {} passed; wrote {} and {}",
        report.passed(),
        report.tasks.len(),
        json.display(),
        markdown.display()
    );
    // A suite with a failing task is a failing command: an operator running this
    // in a script should not have to parse the report to find out.
    match report.passed() == report.tasks.len() {
        true => Ok(()),
        false => Err(sc_error::Error::config(format!(
            "{} of {} eval tasks failed",
            report.tasks.len() - report.passed(),
            report.tasks.len()
        ))),
    }
}

async fn build_app_command(args: &[String]) -> Result<()> {
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
    let report = build_application(&catalog, &app, &source, None).await?;

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
async fn api_command(args: &[String]) -> Result<()> {
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
async fn open_app(
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
async fn reemit_client(catalog: &sc_catalog::Catalog, app: &sc_app::Application) {
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

/// `feldspar api add-query --app SUBDOMAIN [--api MOUNT] --name … --path … --sql …`.
///
/// Validates by **preparing** — the same call the admin UI's check button and
/// every save make — so a query that will not prepare exits non-zero carrying
/// Postgres's own message, and the stored application is untouched.
async fn add_query_command(args: &[String]) -> Result<()> {
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
fn print_query_columns(app: &sc_app::Application, mount: &str, name: &str) -> Result<()> {
    let Some(api) = app.apis.iter().find(|a| a.mount == mount) else {
        return Ok(());
    };
    let Some(query) = sc_api::custom_queries(&api.config)?
        .into_iter()
        .find(|q| q.name == name)
    else {
        return Ok(());
    };
    if query.columns.is_empty() {
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
async fn list_queries_command(args: &[String]) -> Result<()> {
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
async fn remove_query_command(args: &[String]) -> Result<()> {
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

/// `feldspar get-cfg [KEY] [database flags]` — the stored configuration values
/// (§13.5).
///
/// With a key it prints that value and nothing else, so a script can capture it:
/// `port=$(feldspar get-cfg https_port)`. With no key it prints every declared
/// setting as `key=value`, one per line, which is the answer to "what is this
/// installation actually configured to do" — the question that otherwise needs a
/// browser and a session.
///
/// **What it prints is what the server acts on**: the stored value where there is
/// one, the declared default where there is not — the same
/// [`sc_config::config_value`] the boot path reads, so the two cannot disagree.
/// A key with neither prints nothing, and says so on stderr rather than printing
/// an empty line that a caller would have to tell apart from an empty value.
///
/// It prints **nothing else**: the SQL echo is turned off for this command, so a
/// value can be captured out of it whatever Settings → Development says.
///
/// **Secrets.** A listing redacts them, because a listing is asked for by
/// somebody who wants an overview and it ends up in scrollback, in a CI log and
/// in a pasted issue. Naming the key prints it in full: that caller asked for
/// that secret, and this command already holds the database, which is more
/// authority than any of them buys.
async fn get_cfg_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::config::parse_get(&rest)?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    // This command's stdout **is** the value, so nothing else may go there.
    // The SQL echo is a stored setting meant for a *server's* stdout, which
    // nobody captures an answer out of (see `sc-log`); leaving it on here would
    // mean `port=$(feldspar get-cfg https_port)` picking up the select that
    // found the port, because of a checkbox somebody ticked days ago in another
    // process. It stays on for every other command, `set-cfg` included, where
    // stdout is not an answer.
    sc_log::set_log_sql(false);

    let Some(key) = parsed.key else {
        return print_all_config(&catalog).await;
    };
    // `config_value` refuses an undeclared key, naming the ones there are — the
    // typo is fixed from the message rather than from a second command.
    let value = sc_config::config_value(&catalog, &key).await?;
    if value.is_null() {
        eprintln!("feldspar: `{key}` is not set, and has no default");
        return Ok(());
    }
    println!("{}", sc_cli::config::render(&value));
    Ok(())
}

/// Print every declared key as `key=value`, secrets redacted.
///
/// Every *declared* key, not every stored one: a setting that is unset is still
/// a setting this installation has, and leaving it out would make the listing
/// double as a claim that the key does not exist. An unset key with no default
/// prints as `key=` — present, plainly empty.
///
/// Keys stored under a name no declaration describes are reported on **stderr**
/// afterwards. Nothing reads such a row, so it is not part of the answer; but it
/// is a setting somebody believes is in force, which is worth a sentence.
async fn print_all_config(catalog: &sc_catalog::Catalog) -> Result<()> {
    let values = sc_config::all_config(catalog).await?;
    for field in sc_config::config_spec()
        .into_iter()
        .chain(sc_config::internal_defs().iter().map(|d| d.field.clone()))
    {
        let key = field.name();
        let line = match values.get(key) {
            Some(value) if value.is_null() => String::new(),
            Some(_) if field.secret => sc_types::SECRET_SENTINEL.to_owned(),
            Some(value) => sc_cli::config::render_inline(value),
            None => String::new(),
        };
        println!("{key}={line}");
    }
    for stray in sc_config::stray_config_keys(catalog).await? {
        eprintln!(
            "feldspar: `{stray}` is stored but is not a configuration key this \
             version has, so nothing reads it"
        );
    }
    Ok(())
}

/// `feldspar set-cfg KEY [VALUE] [database flags]` — write one configuration
/// value.
///
/// The value comes from the argument when there is one and from **stdin** when
/// there is not, which is how a certificate is set without quoting a PEM block
/// into a shell:
///
/// ```text
/// feldspar set-cfg ssl_certificate < fullchain.pem
/// feldspar set-cfg https_port 8443
/// ```
///
/// A terminal has only strings, so the *declaration* decides the type
/// ([`sc_cli::config::value_for`]) and [`sc_config::set_config`] checks the
/// result against that same declaration — the one check every writer goes
/// through, so `https_port = "yes"` is refused here exactly as it is in the
/// admin UI, and nothing is written.
///
/// It does not restart anything. A stored setting is read by the server at the
/// point it needs it — the TLS settings at boot, the SMTP transport per message
/// — so what a write takes effect on, and when, is the setting's own business
/// and not this command's to guess at.
async fn set_cfg_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::config::parse_set(&rest)?;

    // The key is checked against the declarations *before* stdin is read: a typo
    // should not leave the command sitting on a terminal waiting for a value
    // nobody can store.
    let field = sc_config::definition(&parsed.key).ok_or_else(|| {
        sc_error::Error::invalid(format!(
            "no configuration key `{}`; known keys are {}",
            parsed.key,
            sc_config::known_keys().join(", ")
        ))
    })?;

    let raw = match parsed.value {
        Some(value) => value,
        None => {
            let read = std::io::read_to_string(std::io::stdin()).map_err(|e| {
                sc_error::Error::config(format!("reading the value from stdin: {e}"))
            })?;
            // One trailing newline is the pipe's, not the value's.
            sc_cli::config::strip_final_newline(&read).to_owned()
        }
    };
    let value = sc_cli::config::value_for(&field, &raw)?;
    sc_cli::config::refuse_sentinel(&parsed.key, &value)?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    sc_config::set_config(&catalog, &parsed.key, value.clone()).await?;

    // What was written, echoed back — but never a secret's value, which is the
    // one thing a terminal should not be made to hold a copy of by a command
    // that was given it on the way in.
    eprintln!(
        "feldspar: {} = {}",
        parsed.key,
        if field.secret {
            sc_types::SECRET_SENTINEL.to_owned()
        } else {
            sc_cli::config::render_inline(&value)
        }
    );
    Ok(())
}

/// `feldspar auth SUBCOMMAND …` — sessions for driving an application without a
/// browser to sign in with.
async fn auth_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("token") => auth_token_command(&args[1..]).await,
        Some(other) => Err(sc_error::Error::config(format!(
            "unknown auth subcommand `{other}`; there is `token`"
        ))),
        None => Err(sc_error::Error::config(
            "auth needs a subcommand: token (a signed-in session, written to a file)",
        )),
    }
}

/// `feldspar auth token --app SUBDOMAIN (--email EMAIL | --admin | --role NAME)
/// [--url …] [--out PATH] [--format playwright|netscape]`.
///
/// Mints a session for a user of the **running server** and writes the cookies a
/// browser would have got, so a script can screenshot the screens behind the
/// sign-in page (§13.3).
///
/// No password, because the caller already holds something stronger: the primary
/// database's credentials. What it does with them is write a one-time grant
/// (§7.2) that the server exchanges for an ordinary session — it still creates
/// no user, resets no password and forges no cookie, because the only session
/// the server accepts is one the server itself made.
async fn auth_token_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::auth::parse_token_args(&rest)?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    let app = load_application_by_subdomain(&catalog, &parsed.app)
        .await?
        .ok_or_else(|| {
            sc_error::Error::not_found(format!("no application with subdomain `{}`", parsed.app))
        })?;
    // Resolved before anything is written or any request is made, so "there is
    // no such user" arrives before "the server is unreachable" — the two are
    // fixed in different places.
    let user = sc_cli::auth::resolve_user(&catalog, &parsed.user).await?;

    // Where the application answers, and where to connect to reach it. The two
    // differ whenever a development machine has no DNS for the base domain,
    // which is most of them.
    let origin = db
        .serving()
        .public_origin(parsed.base_domain.as_deref())
        .ok_or_else(|| {
            sc_error::Error::config(
                "no base domain: an application is served at \
                 `<subdomain>.<base-domain>`, so pass --base-domain, or set \
                 `base_domain` in the feldspar.toml environment this is \
                 connecting with",
            )
        })?;
    let target = sc_cli::auth::Target {
        url: parsed
            .url
            .clone()
            .unwrap_or_else(|| origin.url_for(&app.subdomain)),
        host: origin.host_for(&app.subdomain),
        secure: origin.secure,
    };

    let (cookies, signed_in) = sc_cli::auth::session_for(&catalog, &target, &user).await?;
    let path = sc_cli::auth::out_path(parsed.out.as_deref(), parsed.format);
    sc_cli::auth::write_private(
        &path,
        &sc_cli::auth::render(&cookies, &target.host, parsed.format),
    )?;

    // What was written, for whom, and the URL to point a browser at — the three
    // things the caller's next command needs. The identity is the *server's*
    // answer, not the flag's: `--admin` and `--role` name a user this command
    // chose, and the caller should be told which one it got.
    eprintln!(
        "feldspar: session for {}{} — written to {}",
        signed_in
            .get("email")
            .and_then(|e| e.as_str())
            .unwrap_or("the selected user"),
        match signed_in.get("role").and_then(|r| r.as_i64()) {
            Some(role) => format!(" (role {role})"),
            None => String::new(),
        },
        path.display()
    );
    eprintln!("feldspar: the application is at {}", target.browser_url());
    Ok(())
}

/// `feldspar i18n extract|lint|check|translate …` (task 2.4).
///
/// The one extraction pass in `sc_i18n::extract`, with four things to do with
/// it. Only `translate` touches a database — it is the one that calls an LLM,
/// and which provider that is is a stored row — so the other three run in a
/// checkout with nothing else standing up.
async fn i18n_command(args: &[String]) -> Result<()> {
    use sc_cli::i18n::{Command, I18nArgs, find_root};

    // The database flags belong to `translate` alone. Extracting them for the
    // other three as well would mean `feldspar i18n check --root /tmp/x` in a
    // container with no `DATABASE_URL` failing on a connection it never makes.
    let translating = args.first().map(String::as_str) == Some("translate");
    let (db, rest) = match translating {
        true => {
            let (db, rest) = DbConfig::extract(args.to_vec())?;
            (Some(db), rest)
        }
        false => (None, args.to_vec()),
    };
    let parsed = I18nArgs::parse(&rest)?;
    let root = match &parsed.root {
        Some(root) => root.clone(),
        None => find_root(&std::env::current_dir().map_err(|e| {
            sc_error::Error::config(format!("reading the working directory: {e}"))
        })?)?,
    };
    match parsed.command {
        Command::Extract => i18n_extract(&root, &parsed),
        Command::Lint => i18n_lint(&root, &parsed),
        Command::Check => i18n_check(&root, &parsed),
        Command::Translate => {
            let db = db.ok_or_else(|| sc_error::Error::config("no database flags parsed"))?;
            i18n_translate(&root, &parsed, db).await
        }
    }
}

/// `feldspar cmdstan status | install` (TODO "Bayesian models with Stan" §20).
///
/// No database: `status` reports what is on this machine, and `install` is a
/// download and a build the operator asked for.
/// `feldspar demo analytics [--replace] [database flags]`: the Analytics UI's
/// demo tables (analytics TODO A1.18).
async fn demo_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::demo::DemoArgs::parse(&rest)?;
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    let report = sc_analytics::demo::demo_analytics(&catalog, parsed.replace).await?;
    for table in &report.replaced {
        println!("dropped {table}");
    }
    for (table, rows) in &report.tables {
        println!("made {table}: {rows} rows");
    }
    println!(
        "Now run `feldspar serve`, sign in, and open Analytics in the admin sidebar \
         (docs/tutorial-analytics.md)."
    );
    Ok(())
}

async fn cmdstan_command(args: &[String]) -> Result<()> {
    use sc_cli::cmdstan::{CmdStanArgs, install, status};

    match CmdStanArgs::parse(args)? {
        CmdStanArgs::Status { cmdstan } => status(cmdstan),
        CmdStanArgs::Install { version, dir, jobs } => install(version, dir, jobs).await,
    }
}

/// The trees a command reads: the bare paths when there are any, else the
/// domains'.
///
/// A bare path is what makes `feldspar i18n lint ui/admin/src` work (task 3.4)
/// and what a scaffolded application's own tree will be passed as.
fn i18n_trees(
    root: &std::path::Path,
    parsed: &sc_cli::i18n::I18nArgs,
) -> Vec<(String, std::path::PathBuf, sc_cli::i18n::Kind)> {
    if !parsed.paths.is_empty() {
        return parsed
            .paths
            .iter()
            .map(|path| {
                let full = match path.is_absolute() {
                    true => path.clone(),
                    false => root.join(path),
                };
                // A named path is source to read, and which parser to read it
                // with is decided per file — so the kind here only picks the
                // extension filter, and a tree of `.rs` gets the Rust one.
                let kind = match parsed
                    .domains
                    .iter()
                    .find(|d| d.sources.iter().any(|s| path.starts_with(s)))
                {
                    Some(domain) => domain.kind,
                    None => sc_cli::i18n::Kind::Js,
                };
                (path.to_string_lossy().into_owned(), full, kind)
            })
            .collect();
    }
    parsed
        .domains
        .iter()
        .flat_map(|domain| {
            domain
                .sources
                .iter()
                .map(move |source| (domain.name.to_owned(), root.join(source), domain.kind))
        })
        .collect()
}

/// `feldspar i18n extract` — every message, `file:line: key`, one per line.
///
/// Greppable on purpose: this is the command somebody runs to find out where a
/// sentence they saw on a screen is written.
fn i18n_extract(root: &std::path::Path, parsed: &sc_cli::i18n::I18nArgs) -> Result<()> {
    let mut problems = 0usize;
    for domain in &parsed.domains {
        let (found, _) = sc_cli::i18n::scan_domain(root, domain)?;
        for message in &found.messages {
            println!(
                "{}:{}: {}",
                message.file,
                message.line,
                display_key(&message.key)
            );
        }
        for problem in &found.problems {
            eprintln!("error: {problem}");
        }
        problems += found.problems.len();
        eprintln!(
            "feldspar: {} — {} message{} at {} call site{}",
            domain.name,
            found.keys().len(),
            plural(found.keys().len()),
            found.messages.len(),
            plural(found.messages.len())
        );
    }
    match problems {
        0 => Ok(()),
        n => Err(sc_error::Error::invalid(format!(
            "{n} call site{} could not be read",
            plural(n)
        ))),
    }
}

/// `feldspar i18n lint [PATH…]` — the literals nobody wrapped (task 2.2).
fn i18n_lint(root: &std::path::Path, parsed: &sc_cli::i18n::I18nArgs) -> Result<()> {
    let mut total = 0usize;
    for (name, dir, kind) in i18n_trees(root, parsed) {
        let (_, findings) = sc_cli::i18n::scan_tree(root, &dir, kind)?;
        for finding in &findings {
            if parsed.json {
                // One object per line, so the output streams and `jq` reads it
                // without holding the whole run.
                println!(
                    "{}",
                    serde_json::json!({
                        "file": finding.file,
                        "line": finding.line,
                        "what": match &finding.what {
                            sc_i18n::Unwrapped::JsxText => "text".to_owned(),
                            sc_i18n::Unwrapped::Attribute(name) => name.clone(),
                        },
                        "start": finding.span.0,
                        "end": finding.span.1,
                    })
                );
            } else {
                println!("{finding}");
            }
        }
        eprintln!(
            "feldspar: {name} — {} unwrapped literal{}",
            findings.len(),
            plural(findings.len())
        );
        total += findings.len();
    }
    match total {
        0 => Ok(()),
        n => Err(sc_error::Error::invalid(format!(
            "{n} unwrapped literal{}",
            plural(n)
        ))),
    }
}

/// `feldspar i18n check` — coverage per locale, and the one failure.
///
/// **Coverage is a number, not a gate.** What fails here is a translation whose
/// placeholders or plural forms do not match its key, and a call site that
/// could not be read: the first renders wrongly in front of a person, the
/// second never reaches a catalogue at all. A locale that is 40% translated is
/// a fact about a work in progress and not a broken build.
fn i18n_check(root: &std::path::Path, parsed: &sc_cli::i18n::I18nArgs) -> Result<()> {
    let mut failures = 0usize;
    for domain in &parsed.domains {
        let (found, findings) = sc_cli::i18n::scan_domain(root, domain)?;
        let keys = found.keys();
        let locales = match parsed.locales.is_empty() {
            true => sc_cli::i18n::catalogue_locales(root, domain)?,
            false => parsed.locales.clone(),
        };
        println!(
            "{}: {} message{}, {} locale{}, {} unwrapped literal{}",
            domain.name,
            keys.len(),
            plural(keys.len()),
            locales.len(),
            plural(locales.len()),
            findings.len(),
            plural(findings.len())
        );
        for problem in &found.problems {
            eprintln!("error: {problem}");
        }
        failures += found.problems.len();
        for tag in &locales {
            let locale = sc_i18n::Locale::parse(tag)?;
            let catalog = sc_cli::i18n::load_catalogue(root, domain, &locale)?;
            let coverage = sc_cli::i18n::coverage(&catalog, &keys);
            println!("{}", coverage.line());
            for (key, reason) in &coverage.mismatches {
                eprintln!(
                    "error: {}/{}.json: `{}`: {reason}",
                    domain.locales,
                    locale.as_str(),
                    display_key(key)
                );
            }
            failures += coverage.mismatches.len();
        }
    }
    match failures {
        0 => Ok(()),
        n => Err(sc_error::Error::invalid(format!(
            "{n} translation{} or call site{} must be fixed",
            plural(n),
            plural(n)
        ))),
    }
}

/// `feldspar i18n translate --domain D --locale L` — fill one catalogue through
/// the configured LLM, checking every answer (decision D9).
async fn i18n_translate(
    root: &std::path::Path,
    parsed: &sc_cli::i18n::I18nArgs,
    db: DbConfig,
) -> Result<()> {
    if parsed.locales.is_empty() {
        return Err(sc_error::Error::config(
            "feldspar i18n translate needs a target: --locale fr",
        ));
    }
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog_db = connect_catalog(&db).await?;

    // Which model. A named provider, or the only sensible default: the first
    // one configured, with its default model. The error says what to configure
    // rather than what failed.
    let provider = match &parsed.provider {
        Some(name) => sc_llm::load_llm_provider_by_name(&catalog_db, name)
            .await?
            .ok_or_else(|| sc_error::Error::not_found(format!("no LLM provider named `{name}`")))?,
        None => sc_llm::list_llm_providers(&catalog_db)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                sc_error::Error::config(
                    "no LLM provider is configured — add one in Settings → LLM providers, \
                     or name one with --provider",
                )
            })?,
    };
    let model = sc_llm::require_llm_model(&catalog_db, &provider, parsed.model.as_deref()).await?;
    let translator = sc_cli::i18n::LlmTranslator::new(sc_llm::connect_model(&provider, &model)?);
    let source = sc_i18n::Locale::source();

    for domain in &parsed.domains {
        let (found, _) = sc_cli::i18n::scan_domain(root, domain)?;
        let keys = found.keys();
        for tag in &parsed.locales {
            let locale = sc_i18n::Locale::parse(tag)?;
            let mut catalogue = sc_cli::i18n::load_catalogue(root, domain, &locale)?;
            let missing = catalogue.missing(keys.iter().map(String::as_str)).len();
            eprintln!(
                "feldspar: {} → {}: {missing} message{} to translate with {}",
                domain.name,
                locale.as_str(),
                plural(missing),
                translator.describes()
            );
            if missing == 0 {
                continue;
            }
            let report =
                sc_i18n::translate_missing(&mut catalogue, &source, &translator, &keys).await?;
            for warning in report.warnings() {
                eprintln!("warning: {warning}");
            }
            let path = sc_cli::i18n::save_catalogue(root, domain, &catalogue)?;
            eprintln!(
                "feldspar: filled {}, refused {}, unanswered {} — wrote {}",
                report.filled.len(),
                report.rejected.len(),
                report.unanswered.len(),
                path.display()
            );
        }
    }
    Ok(())
}

/// A catalogue key as a person reads it: the context, where there is one, in
/// front of the English rather than a control character between them.
fn display_key(key: &str) -> String {
    match sc_i18n::key_context(key) {
        Some(context) => format!("[{context}] {}", sc_i18n::source_text(key)),
        None => key.to_owned(),
    }
}

fn plural(n: usize) -> &'static str {
    match n {
        1 => "",
        _ => "s",
    }
}

/// Print the short usage summary.
fn print_usage() {
    eprintln!("feldspar — usage:");
    eprintln!("  feldspar serve [database flags] [server flags]");
    eprintln!("  feldspar build-app SUBDOMAIN [database flags] [--file-store NAME=PATH]");
    eprintln!("  feldspar api add-query --app SUBDOMAIN [--api MOUNT] --name NAME");
    eprintln!(
        "                        [--method GET] --path /sub/path [--min-role N] \
         [--description TEXT]"
    );
    eprintln!("                        [--param name:type[,name:type…]]… --sql TEXT|@FILE");
    eprintln!("  feldspar api list-queries --app SUBDOMAIN [--api MOUNT]");
    eprintln!("  feldspar api remove-query --app SUBDOMAIN [--api MOUNT] --name NAME");
    eprintln!("  feldspar get-cfg [KEY] [database flags]");
    eprintln!("  feldspar set-cfg KEY [VALUE] [database flags]   (no VALUE: read it from stdin)");
    eprintln!("  feldspar agent eval SUITE [--model provider/model] [--strong P/M] [--cheap P/M]");
    eprintln!("                            [--task NAME]… [--out DIR] [--keep] [database flags]");
    eprintln!(
        "  feldspar i18n extract|lint|check [--domain core|admin|builder|analytics] [--locale TAG]"
    );
    eprintln!("  feldspar i18n lint PATH…");
    eprintln!(
        "  feldspar i18n translate --domain NAME --locale TAG [--provider NAME] [--model NAME]"
    );
    eprintln!("                          [database flags]");
    eprintln!("  feldspar auth token --app SUBDOMAIN (--email EMAIL | --admin | --role NAME)");
    eprintln!("  feldspar demo analytics [--replace] [database flags]");
    eprintln!("  feldspar cmdstan status [--cmdstan DIR]");
    eprintln!("  feldspar cmdstan install [--version V] [--dir D] [--jobs J]");
    eprintln!("                      [--format playwright|netscape] [--out PATH] [--url ORIGIN]");
    eprintln!();
    eprintln!("  database (or the DATABASE_URL / PG* environment variables):");
    eprintln!("    --database-url URL   full connection string (takes precedence)");
    eprintln!("    --db-host H  --db-port N  --db-user U  --db-password P  --db-name D");
    eprintln!(
        "    --sqlite PATH        use a SQLite file as the primary database instead \
         (or FELDSPAR_SQLITE);"
    );
    eprintln!("                         it is created if it is not there");
    eprintln!();
    eprintln!("  configuration file (used for whatever the flags and environment leave unset):");
    eprintln!(
        "    --environment NAME   which [environments.NAME] section to connect with \
         (or FELDSPAR_ENV);"
    );
    eprintln!("                         naming one makes it outrank DATABASE_URL / PG*");
    eprintln!("    --config PATH        read this file instead of searching (or FELDSPAR_CONFIG)");
    for path in sc_cli::config_file::search_paths() {
        eprintln!("                         searched: {}", path.display());
    }
    eprintln!();
    eprintln!(
        "  build-app: builds one application and prints the bundler's output.

  api: adds, lists and removes an application's custom SQL queries. A query is
       validated by preparing it, so one that will not prepare is refused with
       the database's own message and nothing is stored; adding or removing one
       rewrites the application's generated client. A parameter is written
       `name:type`, or `name:type?` when the caller may leave it out. Every
       query has a minimum role, and it is **admin** unless --min-role says
       otherwise: raw SQL does not go through the row layer, so ownership
       formulae do not filter what it returns.

  get-cfg / set-cfg: the settings an admin edits in Settings, from a terminal.
       They are rows in the primary database, so these commands need the database
       and nothing else — no running server, no session. `get-cfg KEY` prints that
       value alone, ready to capture (`port=$(feldspar get-cfg https_port)`);
       `get-cfg` with no key prints every declared setting as `key=value`, one per
       line, with secrets shown as the redaction the admin UI shows — name a
       secret's key to see it in full. `set-cfg` takes the value from the command
       line, or from stdin when there is none, which is how a multi-line PEM block
       is set: `feldspar set-cfg ssl_certificate < fullchain.pem`. The value is
       checked against the key's declared type before it is written, so
       `set-cfg https_port yes` is a message rather than a stored string. Nothing
       is restarted: when a setting takes effect is the setting's own business.

  auth token: mints a session on the *running* server and writes the cookies a
       browser would have got, so a script can screenshot the screens behind the
       sign-in page. It asks no password: this command already holds the
       database, which is more authority than any password buys. Say who the
       session is for with --email EMAIL, with --admin (the first admin user) or
       with --role NAME (the first user holding that role — the error lists the
       roles when the name is not one). It forges nothing: the session is minted
       by the server, from a one-time grant, and can do exactly what that account
       can. The default file is .feldspar-session.json, Playwright's
       storageState; --format netscape writes a cookies.txt for curl instead.
       Both are written 0600 — a session file is a password.

  server:"
    );
    eprintln!("    --bind ADDR  --static-dir DIR  --session-ttl-hours N  --secure-cookies");
    eprintln!("    --base-domain DOMAIN     apps are served at <subdomain>.<domain>");
    eprintln!(
        "    --browser PATH           the headless Chromium view_app drives (default: chromium,
                             chromium-browser or google-chrome on PATH, not a snap)
    --no-browser-sandbox     start that browser with --no-sandbox
    --browser-contexts N     view_app runs that may hold a browser context at once (default 4)
    --preview-idle-minutes N unmount a run's preview after N idle minutes (default 60)"
    );
    eprintln!(
        "    --file-store NAME=PATH   connect a local directory as a named file store (repeatable)"
    );
    eprintln!(
        "    --code-workers N         V8 isolates serving run_js_code bodies (default 2)
    --code-max-inflight N    runs each of those isolates keeps resident (default 256)
    --modules-dir PATH       where modules are installed (default: the platform's
                             data directory, e.g. ~/.local/share/feldspar/modules)"
    );
    eprintln!(
        "    --python auto|off        whether this process starts its Python interpreter
                             (default auto: started by the first Python body, and
                             never in a binary built without the `python` feature)
    --python-max-inflight N  Python runs resident at once (default 32)
    --python-max-stuck N     runs that never returned before Python is refused
                             until a restart (default 8)
    --python-dir PATH        the virtual environment Python modules install into
                             (default: the platform's data directory, e.g.
                             ~/.local/share/feldspar/python)
    --python-bin PATH        the interpreter pip runs under (default python3).
                             It must be the same major.minor version as the one
                             this server was built against, or the environment
                             is refused rather than segfaulted on"
    );
    eprintln!(
        "    --model-max-rows N       rows one model dataset may select (default 200000)
    --cmdstan DIR            the CmdStan Stan models use (default: $CMDSTAN, else
                             the newest ~/.cmdstan/cmdstan-*)
    --stan-cache-dir DIR     where compiled Stan programs are kept (default: the
                             platform's data directory, e.g.
                             ~/.local/share/feldspar/stan-cache)
    --stan-max-processes N   Stan chain processes at once, across every fit
                             (default: half the CPUs, at least 1)
    --stan-max-data-values N numbers one fit's bound data may hold (default 20000000)
    --stan-max-draws-bytes N bytes of draws one fit may store (default 1000000000)
    --stan-max-draws-response N
                             numbers one draws response may carry (default 2000000)
    --stan-summary-max-elements N
                             a generated quantity larger than this is summarised
                             on demand rather than at fit time (default 1000)"
    );
    eprintln!();
    eprintln!(
        "  a feldspar.toml environment may also carry `base_domain`, `bind`,
  `secure_cookies`, `browser`, `browser_sandbox`, `cmdstan` and the `stan_*` keys, so `serve --environment NAME` needs none of those flags —
  and so a build from the command line writes the same application URL into the
  generated documentation that the server would."
    );
}
