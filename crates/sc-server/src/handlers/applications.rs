//! Applications: CRUD, building and targets, frameworks, API providers and build hosts.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    reg.handle(
        sc_api::handler_ids::LIST_APPLICATIONS,
        typed(state, list_all_applications),
    );

    reg.handle(
        sc_api::handler_ids::CREATE_APPLICATION,
        typed(state, create_application),
    );

    reg.handle(
        sc_api::handler_ids::UPDATE_APPLICATION,
        typed(state, update_application),
    );

    reg.handle(sc_api::handler_ids::DELETE_APPLICATION, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let id = parse_app_id(ctx.path_param("id")?)?;
                // Load first so we know the subdomain to unmount — deleting the row
                // is the definition's end, unmounting is what stops it serving.
                let existing = load_application(&catalog, id).await?;
                let deleted = delete_application(&catalog, id).await?;
                // `_fd_views.application` is not a foreign key (the schema layer
                // renders no `ON DELETE`), so the views, pages and library go here.
                sc_viewpattern::delete_application_views_pages_and_library(&catalog, id).await?;
                if let Some(app) = &existing {
                    apps.unmount(&app.subdomain);
                }
                if !deleted {
                    return Err(Error::not_found(format!("no application with id {id}")));
                }
                // The agent created to build it goes with it (§13.3). An
                // application's builder has nothing to build once the application
                // is gone — it would sit in the agents list as a broken record of
                // something that no longer exists, and the admin who deleted the
                // application is the one who would have to clean it up.
                let mut body = json!({ "deleted": true });
                if let Some(app) = &existing
                    && let Some(name) = delete_builder_agent(&catalog, app).await?
                    && let Some(obj) = body.as_object_mut()
                {
                    obj.insert("agent".to_owned(), json!(name));
                }
                Ok(HandlerResponse::ok(body))
            }
        }
    });

    reg.handle(
        sc_api::handler_ids::BUILD_APPLICATION,
        typed(state, build_application),
    );

    // Deep clean: delete the installed dependencies, then build — which
    // installs them again because they are missing. The removal waits for any
    // build in progress (`sc_app::remove_dependencies`), and a build failure
    // after it is reported as Build reports one; the next build installs again
    // either way, since nothing was put back.
    reg.handle(sc_api::handler_ids::DEEP_CLEAN_APPLICATION, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let id = parse_app_id(ctx.path_param("id")?)?;
                let app = load_application(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no application with id {id}")))?;
                if sc_app::framework_factory(&app.framework.name).is_some() {
                    return Err(Error::invalid(
                        "this application has no build, so there is nothing to deep clean",
                    ));
                }
                let source = app_source_from_config(&app.framework)?;
                let removed = sc_app::remove_app_dependencies(&catalog, &source).await?;
                let marker = source
                    .build
                    .install
                    .as_ref()
                    .map(|i| i.marker.as_str())
                    .unwrap_or_default();
                let log = if removed {
                    format!("Deleted {marker}; reinstalling.\n")
                } else {
                    format!("There was no {marker} to delete; installing.\n")
                };
                build_now(&catalog, &apps, app, log, false).await
            }
        }
    });

    // Start a build of a target the application's framework offers beside its
    // web bundle, and answer at once (§13.3): a native build is minutes long, so
    // it runs as a job of its own and the admin UI polls
    // `getApplicationTargetBuild`. A target the framework does not declare is
    // refused here, before anything starts. A build already running for this
    // target answers itself — a second click joins it rather than starting a
    // second Gradle in the same project directory.
    reg.handle(sc_api::handler_ids::BUILD_APPLICATION_TARGET, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let id = parse_app_id(ctx.path_param("id")?)?;
                let target = ctx.path_param("target")?.to_owned();
                let app = load_application(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no application with id {id}")))?;
                let spec = sc_app::app_target_spec(&app, &target)?;
                let store = app_source_from_config(&app.framework)?.store.0;
                let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
                let log_path = sc_app::target_log_path(&spec, &stamp);
                // What this server cannot build goes to a build host that can,
                // as a job it asks for (`TODO-build-hosts.md` B2.2).
                if !sc_app::app_target_readiness(&app, &target)?.is_ready() {
                    let hosts = sc_app::list_build_hosts(&catalog).await?;
                    if sc_app::host_for_target(&hosts, &spec.name, chrono::Utc::now()).is_some() {
                        let _queueing = sc_app::lock_build_queue().await;
                        if let Some(open) = sc_app::latest_build_job(&catalog, id.0, &spec.name)
                            .await?
                            .filter(|j| j.state.is_open())
                        {
                            return Ok(HandlerResponse::ok(remote_job_json(&open)));
                        }
                        // A build host never generates code: the client and
                        // runtime are written here before the job is queued.
                        sc_app::prepare_remote_target(&catalog, &app, apps.triggers()).await?;
                        let job = sc_app::queue_build_job(
                            &catalog,
                            sc_app::QueuedJob {
                                application: id.0,
                                target: &spec.name,
                                label: &spec.label,
                                store: &store,
                                log_path: &log_path,
                            },
                        )
                        .await?;
                        return Ok(HandlerResponse::ok(remote_job_json(&job)));
                    }
                }
                // A machine without the toolchain is told so now, as a refusal,
                // rather than by a job that fails minutes into an install.
                sc_app::require_target_ready(&spec)?;
                // Owned, because `spec` moves into the build below.
                let (name, label) = (spec.name.clone(), spec.label.clone());
                let job = crate::target_builds::NewJob {
                    target: &name,
                    label: &label,
                    store: &store,
                    log_path: &log_path,
                };
                let triggers = apps.triggers().cloned();
                let build = {
                    let catalog = catalog.clone();
                    let log_path = log_path.clone();
                    async move {
                        let report = sc_app::build_application_target(
                            &catalog,
                            &app,
                            &spec,
                            &log_path,
                            triggers.as_ref(),
                        )
                        .await?;
                        Ok(crate::target_builds::TargetArtifact {
                            path: report.artifact,
                            size: report.size,
                            log: report.log,
                        })
                    }
                };
                let job = apps.target_builds().start(&id.to_string(), job, build);
                Ok(HandlerResponse::ok(target_job_json(&job)))
            }
        }
    });

    // Run an operation a target declares (a keystore generator): the module's
    // files written into the application's store, its settings saved on it.
    reg.handle(sc_api::handler_ids::RUN_APPLICATION_TARGET_OPERATION, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_app_id(ctx.path_param("id")?)?;
                let target = ctx.path_param("target")?.to_owned();
                let operation = ctx.path_param("operation")?.to_owned();
                let app = load_application(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no application with id {id}")))?;
                // The form's current settings, unsaved edits included; the
                // stored ones when the request sends none.
                let form_settings = match require_object(&ctx.body)?.get("config") {
                    Some(Json::Object(config)) => config.clone(),
                    None | Some(Json::Null) => app.framework.config.clone(),
                    Some(_) => return Err(Error::invalid("`config` is not an object of settings")),
                };
                let outcome = sc_app::run_target_operation(
                    &catalog,
                    &app,
                    &target,
                    &operation,
                    &form_settings,
                )
                .await?;
                // The app's settings with secrets masked, as the form shows
                // them; of those, the ones the operation set go back.
                let spec = framework_config_spec(&outcome.app.framework.name)?;
                let masked = sc_types::redact_attrs(&spec, &outcome.app.framework.config);
                let settings: sc_types::Attrs = outcome
                    .settings
                    .iter()
                    .filter_map(|name| Some((name.clone(), masked.get(name)?.clone())))
                    .collect();
                Ok(HandlerResponse::ok(json!({
                    "message": outcome.message,
                    "store": outcome.store,
                    "files": outcome.files,
                    "git_repo": outcome.git_repo,
                    "settings": Json::Object(settings),
                })))
            }
        }
    });

    // The latest build of a target: what the UI polls while one runs, and asks
    // after a reload to pick up a build that is still going.
    reg.handle(sc_api::handler_ids::GET_APPLICATION_TARGET_BUILD, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let id = parse_app_id(ctx.path_param("id")?)?;
                let target = ctx.path_param("target")?;
                // Report whichever build started last: the one on this server or
                // the one on a build host.
                let local = apps.target_builds().get(&id.to_string(), target);
                let remote = sc_app::latest_build_job(&catalog, id.0, target).await?;
                let body = match (local, remote) {
                    (Some(local), Some(remote)) if remote.created_at > local.started_at => {
                        remote_job_json(&remote)
                    }
                    (Some(local), _) => target_job_json(&local),
                    (None, Some(remote)) => remote_job_json(&remote),
                    (None, None) => {
                        return Err(Error::not_found(format!(
                            "`{target}` has not been built for this application since the \
                             server started"
                        )));
                    }
                };
                Ok(HandlerResponse::ok(body))
            }
        }
    });

    // Rewrite an application's generated code on demand, without building.
    //
    // The button beside "Build", and deliberately a different button: a build
    // runs a bundler and can take a minute, while this writes four files and
    // cannot fail on anything but the store. It is the same call the automatic
    // path makes when a table changes (decision 10), so an admin who wants it
    // now and an admin who changed a column get the same files.
    reg.handle(sc_api::handler_ids::UPDATE_APPLICATION_CLIENT, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let id = parse_app_id(ctx.path_param("id")?)?;
                let app = load_application(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no application with id {id}")))?;
                let update = update_app_client(&catalog, &app, apps.triggers()).await?;
                Ok(HandlerResponse::ok(json!({
                    "scaffolded": matches!(update, sc_app::ClientUpdate::Scaffolded(_)),
                    "files": update.files(),
                    "log": update.summary(),
                })))
            }
        }
    });

    // The admin UI's GraphQL explorer (§13.4): one operation, run against the
    // application's **mounted** GraphQL provider.
    //
    // Everything this handler does is find that provider and hand it the body.
    // The document is executed by `ApiProvider::handle` — the same entry point a
    // request to `staff.example.com/graphql` reaches — so the schema, the
    // limits, the row layer and the §7 authorization are not restated here and
    // cannot drift from what the application serves. The one thing this handler
    // chooses is *who is asking*, and it chooses `ctx.user`: the signed-in
    // admin, with their own role and their own ownership. An explorer holding
    // authority the person driving it does not have would be a way to read rows
    // through a screen that were refused through the API.
    //
    // A GraphQL endpoint answers `200` with its errors in the body, so the
    // provider's response — status and all — is passed through as it stands
    // rather than being re-judged here.
    reg.handle(sc_api::handler_ids::RUN_APPLICATION_GRAPHQL, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let id = parse_app_id(ctx.path_param("id")?)?;
                let app = load_application(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no application with id {id}")))?;
                // The *mounted* app, not the stored row: an application whose
                // record enables GraphQL but which has never been built serves
                // nothing, and "no such provider" would be a misleading way to
                // say so.
                let mounted = apps.get(&app.subdomain).ok_or_else(|| {
                    Error::invalid(format!(
                        "application `{}` is not mounted, so it serves no GraphQL yet — build it \
                         first",
                        app.name
                    ))
                })?;
                let provider = mounted
                    .providers
                    .iter()
                    .find(|p| p.name() == GRAPHQL_PROVIDER)
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "application `{}` does not enable the `{GRAPHQL_PROVIDER}` API \
                             provider",
                            app.name
                        ))
                    })?;
                let req = ApiRequest {
                    method: ApiMethod::Post,
                    // The provider routes on its own mount, so this is that
                    // mount and never a path a request chose.
                    path: provider.mount(),
                    query: Vec::new(),
                    body: ctx.body.clone(),
                    raw: None,
                    // The admin's GraphQL explorer builds no emailed links.
                    headers: Vec::new(),
                    links: None,
                    client_ip: None,
                    preview: false,
                };
                let resp = provider.handle(req, &catalog, ctx.user()).await?;
                Ok(HandlerResponse::ok(resp.body).with_status(resp.status))
            }
        }
    });

    // Server-query options are resolved here, so the UI receives a concrete list
    // and needs no query evaluator of its own (§1.6). That is what let the
    // `store` setting become a pick-list without waiting for `ui/form-runtime`.
    reg.handle(sc_api::handler_ids::LIST_FRAMEWORKS, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let mut out = Vec::new();
                // In registry order, which is itself editorial: the framework an
                // admin should take comes first (§2.2), and each carries the
                // label and sentence the picker shows — so the screen presents
                // two very different propositions while knowing neither (§2.4).
                for info in registered_framework_info() {
                    let declared = framework_config_spec(&info.name)?;
                    // Read before resolving: afterwards a store picker is just a
                    // list of names, and the form can no longer tell it is one.
                    let store_settings = file_store_settings(&declared);
                    let file_settings: Vec<Json> = sc_catalog::store_file_settings(&declared)
                        .into_iter()
                        .map(|f| json!({ "name": f.name, "extensions": f.extensions }))
                        .collect();
                    let spec = resolve_options(&catalog, declared).await?;
                    let targets: Vec<Json> = sc_app::framework_build_targets(&info.name)
                        .into_iter()
                        .map(|t| {
                            json!({
                                "name": t.name,
                                "label": t.label,
                                "options": t.options,
                                "operations": t.operations.iter().map(|o| json!({
                                    "name": o.name,
                                    "label": say(&o.label, &ctx.locale),
                                    "description": say(&o.description, &ctx.locale),
                                    "show_if": o.show_if.iter().map(|c| {
                                        json!({ "name": c.setting, "values": c.allowed })
                                    }).collect::<Vec<_>>(),
                                })).collect::<Vec<_>>(),
                            })
                        })
                        .collect();
                    out.push(json!({
                        "name": info.name,
                        "label": say(&info.label, &ctx.locale),
                        "description": say(&info.description, &ctx.locale),
                        "config_spec": spec_json(&spec, &ctx.locale),
                        "has_views": info.name == sc_viewpattern::SALTCORN_UI_FRAMEWORK,
                        "file_store_settings": store_settings,
                        "file_settings": file_settings,
                        "targets": targets,
                    }));
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    // The names an application may enable, so the form offers them rather than
    // asking the admin to remember them. This is the *same* list
    // `app_providers_with` switches on, so a name offered here is a name that
    // mounts — which is the whole point of listing them from the server.
    reg.handle(sc_api::handler_ids::LIST_API_PROVIDERS, |ctx| async move {
        let out: Vec<Json> = registered_api_provider_info()
            .into_iter()
            .map(|info| {
                json!({
                    "name": info.name,
                    "label": say(&info.label, &ctx.locale),
                    "description": say(&info.description, &ctx.locale),
                    "default_mount": info.default_mount,
                    // The provider's own settings, in the same vocabulary a
                    // framework's arrive in — so the application form renders
                    // GraphQL's aggregation switch and its four bounds without
                    // knowing that GraphQL is what it is rendering.
                    "config_spec": spec_json(&info.config_spec, &ctx.locale),
                    // …and whether it takes custom SQL queries, which are not a
                    // settings field and so have an editor of their own.
                    "supports_custom_queries": info.supports_custom_queries,
                })
            })
            .collect();
        Ok(HandlerResponse::ok(Json::Array(out)))
    });

    // Prepare one custom query and answer with the columns the database says
    // it returns — the editor's "Check" button (§13.4). A JavaScript or Python
    // query is checked for everything but its body, which only the engine can
    // read, and answers no columns: it returns what its body returns. A Python
    // one on a server that cannot run Python is refused here, with the reason.
    //
    // The same two calls a save makes, in the same order: everything decidable
    // without a database first (the name, the path, one statement, the declared
    // parameters and the used ones being the same set), then `describe`, which
    // is where Postgres's own message comes from. Deciding it twice would be two
    // answers to "is this query valid", and the one that is wrong is the one
    // nobody is reading until a caller hits it.
    reg.handle(sc_api::handler_ids::DESCRIBE_CUSTOM_QUERY, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let tables = parse_str_array(obj, "tables")?;
                let query: sc_api::CustomQuery = serde_json::from_value(ctx.body.clone())
                    .map_err(|e| Error::invalid(format!("not a custom query: {e}")))?;
                sc_api::validate_custom_queries(std::slice::from_ref(&query), &tables)?;
                if query.language == sc_api::QueryLanguage::Python {
                    let state = apps.python().map(|p| p.state());
                    if !matches!(
                        state,
                        Some(
                            sc_python::PythonState::NotInitialised
                                | sc_python::PythonState::Running { .. }
                        )
                    ) {
                        return Err(Error::invalid(
                            "this server cannot run Python, so a Python custom query \
                             would fail every call; see Development › Python for why",
                        ));
                    }
                }
                let columns = sc_api::describe_custom_query(&catalog, &query).await?;
                Ok(HandlerResponse::ok(json!({
                    "columns": columns
                        .iter()
                        .map(|c| json!({ "name": c.name, "type": c.ty }))
                        .collect::<Vec<_>>(),
                })))
            }
        }
    });

    //
    // The registry a Mac is added to as a build host. Its agent talks to
    // `/build-agent/*` (`crate::build_agent`) with the token `createBuildHost`
    // answers once; every decision about that token is `sc_app::build_hosts`'s.

    reg.handle(sc_api::handler_ids::LIST_BUILD_HOSTS, {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let now = chrono::Utc::now();
                let out: Vec<Json> = sc_app::list_build_hosts(&catalog)
                    .await?
                    .iter()
                    .map(|host| build_host_json(host, now))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::CREATE_BUILD_HOST, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let (host, token) = sc_app::add_build_host(
                    &catalog,
                    obj.str_field("name")?,
                    &string_array_field(obj, "targets")?,
                )
                .await?;
                Ok(HandlerResponse::ok(json!({
                    "host": build_host_json(&host, chrono::Utc::now()),
                    "token": token,
                }))
                .with_status(201))
            }
        }
    });

    reg.handle(sc_api::handler_ids::UPDATE_BUILD_HOST, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_build_host_id(ctx.path_param("id")?)?;
                let obj = require_object(&ctx.body)?;
                let host = sc_app::update_build_host(
                    &catalog,
                    id,
                    obj.str_field("name")?,
                    &string_array_field(obj, "targets")?,
                )
                .await?
                .ok_or_else(|| Error::not_found(format!("there is no build host `{id}`")))?;
                Ok(HandlerResponse::ok(build_host_json(
                    &host,
                    chrono::Utc::now(),
                )))
            }
        }
    });

    reg.handle(sc_api::handler_ids::DELETE_BUILD_HOST, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_build_host_id(ctx.path_param("id")?)?;
                let deleted = sc_app::delete_build_host(&catalog, id).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });
}

/// Scaffold a newly created application's project, when it is one that has a
/// project to scaffold.
///
/// `Ok(None)` means "nothing to do" — the app uses a framework that brings its
/// own project (`code`), which is not a failure and should not be reported as
/// one. An `Err` is a real scaffold failure (unreachable store, occupied
/// directory) and is surfaced *alongside* the created application rather than
/// instead of it.
async fn scaffold_new_app(
    catalog: &Catalog,
    app: &Application,
    dispatcher: Option<&Arc<sc_action::TriggerDispatcher>>,
) -> Result<Option<sc_app::ScaffoldReport>> {
    if require_scaffoldable(app).is_err() {
        return Ok(None);
    }
    scaffold_app(catalog, app, dispatcher).await.map(Some)
}

/// Rewrite a saved application's generated files, reporting a failure to the
/// operator's console and to nobody else.
///
/// Saving an application changes its API definition — the tables it declares,
/// the providers it enables, the custom queries on them — so its `src/feldspar/`
/// must follow (decision 10). It deliberately does **not** affect the response:
/// the application is stored and valid, and an unreachable store is something
/// the admin fixes and re-triggers with the update button, not a reason to tell
/// them their save failed.
async fn reemit_app_client(
    catalog: &Catalog,
    app: &Application,
    dispatcher: Option<&Arc<sc_action::TriggerDispatcher>>,
) {
    // A framework constructed from a factory (Saltcorn UI) has no source tree,
    // so there is no client to rewrite and nothing to report.
    if sc_app::framework_factory(&app.framework.name).is_some() {
        return;
    }
    if let Err(e) = sc_app::emit_app_client(catalog, app, dispatcher).await {
        eprintln!(
            "feldspar: application `{}` was saved, but its generated client could \
             not be rewritten: {e}",
            app.subdomain
        );
    }
}

/// Create the agent that builds a newly created application, returning its name.
///
/// **Which agent it is belongs to the framework** ([`framework_builder_agent`]),
/// which is where the knowledge sits: a code framework's application is a source
/// tree, so its builder is a coding agent over that tree, and a framework with no
/// source tree declares nothing. This function is only the part that needs to know
/// agents exist at all — assembling the declaration into a record and storing it —
/// because `sc-app` is a layer below `sc-agent`'s trait registry.
///
/// `Ok(None)` is "nothing to do, and that is not news": a framework that declares
/// no builder, a server assembled without agents, or an agent of that name already
/// there (a re-created application meets its own old builder, which still points
/// at the same subdomain). An `Err` is news the admin should hear — no provider is
/// connected, the first provider has no default model, or the agent did not
/// validate — and is reported *beside* the created
/// application, never instead of it: the row is saved and valid either way.
async fn create_builder_agent(
    catalog: &Catalog,
    apps: &AppMounts,
    app: &Application,
) -> Result<Option<String>> {
    let Some(spec) = framework_builder_agent(&app.framework, app) else {
        return Ok(None);
    };
    let Some(services) = apps.agents() else {
        return Ok(None);
    };
    if sc_agent::load_agent_by_name(catalog, &spec.name)
        .await?
        .is_some()
    {
        return Ok(None);
    }

    // An agent needs a provider that is connected, and a framework cannot know
    // which one a deployment has: it gets the installation's default (the
    // admin's pick, else the configuration file's, else the first by name), and
    // the admin can change it on the agent. A deployment with none is a thing to
    // say out loud rather than a silently agent-less application.
    let provider = sc_llm::default_llm_provider(catalog)
        .await?
        .ok_or_else(|| {
            Error::config(format!(
                "no LLM provider is connected, so the `{}` agent that builds this \
                 application was not created; connect a provider and create it from \
                 the Agents screen",
                spec.name
            ))
        })?;

    // And a model to call: the provider's default. A provider with none is
    // reported the same way as no provider at all, rather than saved as an agent
    // that cannot answer.
    if !list_llm_models(catalog, &provider)
        .await?
        .iter()
        .any(|m| m.is_default && m.kind == sc_llm::ModelKind::Chat)
    {
        return Err(Error::config(format!(
            "LLM provider `{}` has no default model, so the `{}` agent that builds this \
             application was not created; mark one of its models as the default and \
             create the agent from the Agents screen",
            provider.name, spec.name
        )));
    }

    // Looking at the application needs a headless browser, and `coding` refuses
    // the grant on a host without one. A server with no browser still gets a
    // builder that edits, checks and commits — it just cannot look, and the
    // admin can tick the box once a browser is installed.
    let has_browser = services.registry().host().browser.is_ok();
    let mut agent = sc_agent::Agent::new(&spec.name, &provider.name)
        .description(spec.description)
        .system_prompt(spec.system_prompt);
    for mut enabled in spec.traits {
        if !has_browser && enabled.config.contains_key(sc_app::TRAIT_CFG_MAY_VIEW_APP) {
            enabled
                .config
                .insert(sc_app::TRAIT_CFG_MAY_VIEW_APP.to_owned(), Json::Bool(false));
        }
        agent = agent
            .with_trait(sc_agent::EnabledTrait::new(enabled.trait_).configuration(enabled.config));
    }
    // The same save the Agents screen makes, so the same validation applies: a
    // builder agent this server would refuse to run is not one it quietly stores.
    sc_agent::save_agent(catalog, services.registry(), &agent).await?;
    Ok(Some(agent.name))
}

/// Delete the agent created to build `app`, returning its name if one went.
///
/// The other half of [`create_builder_agent`]: an application's builder is scoped
/// to that application and can do nothing once it is gone, so leaving it behind
/// would leave the admin who deleted the application an agent to clean up whose
/// only remaining property is that it does not work.
///
/// **It deletes that application's builder, not every agent that shares its
/// name.** The name is the derivation ([`builder_agent_name`]), but the check is
/// the trait: only an agent still carrying a `coding` trait whose `application`
/// is *this* subdomain is one. So an agent an admin created themselves under that name, or
/// re-pointed at something else, survives the deletion — a delete button on one
/// screen must not silently take an agent that is doing another job. What an
/// admin's edits to the real builder cannot buy it is survival: an agent that
/// still names this application is still this application's.
///
/// Its **runs are not deleted**, as `deleteAgent` does not delete them either: a
/// transcript is a record of what happened, and what happened does not stop
/// having happened because the application was removed.
async fn delete_builder_agent(catalog: &Catalog, app: &Application) -> Result<Option<String>> {
    // No `_fd_agents` table *means* no agent has ever been defined — the same
    // reading `Agents::load` takes — so a server without agents installed deletes
    // an application rather than failing over a table nobody made.
    if catalog.get(sc_agent::AGENTS_TABLE)?.is_none() {
        return Ok(None);
    }
    let name = builder_agent_name(app);
    let Some(agent) = sc_agent::load_agent_by_name(catalog, &name).await? else {
        return Ok(None);
    };
    let subdomain = app.subdomain.trim();
    let builds_this_app = agent.traits.iter().any(|t| {
        t.trait_ == sc_app::TRAIT_CODING
            && t.config
                .get(sc_app::TRAIT_CFG_APPLICATION)
                .and_then(Json::as_str)
                .map(str::trim)
                == Some(subdomain)
    });
    if !builds_this_app {
        return Ok(None);
    }
    sc_agent::delete_agent(catalog, agent.id).await?;
    Ok(Some(name))
}

/// Point the agent that builds `before` at `after`'s subdomain, returning its
/// (possibly new) name.
///
/// Found the way the admin finds it (`builderAgentFor`): by a `coding` trait
/// naming the old subdomain, preferring the one under the derived name. What
/// moves with the subdomain:
///
/// - **the `coding` trait's application**, which is what `check` builds and
///   `view_app` looks at — the part that stops the agent working otherwise;
/// - **the preview pane and the system prompt**, but only where they are still
///   what the framework declared for the old subdomain: an admin's own URL or
///   prompt is theirs;
/// - **the name**, but only while it is still the derived one, the new derived
///   name is free, and no application exposes it by name (a rename would leave
///   that application exposing nothing, which `updateAgent` refuses too).
async fn retarget_builder_agent(
    catalog: &Catalog,
    apps: &AppMounts,
    before: &Application,
    after: &Application,
) -> Result<Option<String>> {
    if catalog.get(sc_agent::AGENTS_TABLE)?.is_none() {
        return Ok(None);
    }
    let Some(services) = apps.agents() else {
        return Ok(None);
    };
    let old = before.subdomain.trim();
    let builds = |agent: &sc_agent::Agent| {
        agent.traits.iter().any(|t| {
            t.trait_ == sc_app::TRAIT_CODING
                && t.config
                    .get(sc_app::TRAIT_CFG_APPLICATION)
                    .and_then(Json::as_str)
                    .map(str::trim)
                    == Some(old)
        })
    };
    let old_name = builder_agent_name(before);
    let candidates: Vec<sc_agent::Agent> = sc_agent::list_agents(catalog)
        .await?
        .into_iter()
        .filter(|a| builds(a))
        .collect();
    let Some(mut agent) = candidates
        .iter()
        .find(|a| a.name == old_name)
        .or_else(|| candidates.first())
        .cloned()
    else {
        return Ok(None);
    };

    let old_pane = sc_app::preview_pane_url(before);
    for enabled in &mut agent.traits {
        if enabled.trait_ == sc_app::TRAIT_CODING
            && enabled
                .config
                .get(sc_app::TRAIT_CFG_APPLICATION)
                .and_then(Json::as_str)
                .map(str::trim)
                == Some(old)
        {
            enabled.config.insert(
                sc_app::TRAIT_CFG_APPLICATION.to_owned(),
                json!(after.subdomain.trim()),
            );
        }
        if enabled.trait_ == sc_app::TRAIT_PREVIEW_PANE
            && enabled
                .config
                .get(sc_app::TRAIT_CFG_PREVIEW_URL)
                .and_then(Json::as_str)
                .map(str::trim)
                == Some(old_pane.as_str())
        {
            enabled.config.insert(
                sc_app::TRAIT_CFG_PREVIEW_URL.to_owned(),
                json!(sc_app::preview_pane_url(after)),
            );
        }
    }
    if let (Some(was), Some(now)) = (
        framework_builder_agent(&before.framework, before),
        framework_builder_agent(&after.framework, after),
    ) && agent.system_prompt == was.system_prompt
    {
        agent.system_prompt = now.system_prompt;
    }
    let new_name = builder_agent_name(after);
    if agent.name == old_name
        && sc_agent::load_agent_by_name(catalog, &new_name)
            .await?
            .is_none()
        && sc_app::applications_exposing_agent(catalog, &old_name)
            .await?
            .is_empty()
    {
        agent.name = new_name;
    }
    sc_agent::save_agent(catalog, services.registry(), &agent).await?;
    Ok(Some(agent.name))
}

/// Parse an [`Application`] from a create/update body (matching
/// `application_input_schema`), carrying `id` — the body never sets the identity.
///
/// Missing collections default to empty and a missing `csp` to
/// [`CspPolicy::strict`], so the minimal body is `{ name, subdomain, framework }`.
/// Save-time validation (framework config against its spec) happens in
/// `save_application`, not here.
/// Let the admin frame an application whose CSP the caller left to its
/// framework (TODO "The preview pane").
///
/// The preview pane beside a builder agent's chat is an `<iframe>` on the
/// admin's origin, and an application's default policy refuses to be framed at
/// all — so a created application would arrive with a builder agent whose pane
/// is a browser error message. The widening is the base domain and nothing else
/// ([`sc_app::allow_admin_framing`]).
///
/// **Only when the caller stated no `csp` of its own.** A policy someone wrote
/// out is theirs: an admin who writes `frame-ancestors 'none'` has said what
/// they mean, and a restore is replaying a policy that was already decided.
/// Refuse a subdomain the router would give to the admin UI rather than to the
/// application: the admin UI's own, and `@` (the base domain) while the admin
/// UI is still there.
fn check_app_subdomain(apps: &AppMounts, subdomain: &str) -> Result<()> {
    let subdomain = subdomain.trim();
    let admin = apps.admin().subdomain();
    if subdomain == sc_config::ROOT_SUBDOMAIN && admin.is_none() {
        return Err(Error::invalid(
            "`@` serves the application on the base domain, which is where the admin UI is: \
             move the admin UI to a subdomain of its own first (Settings → Development → \
             Admin subdomain)",
        ));
    }
    if admin.as_deref() == Some(subdomain) {
        return Err(Error::invalid(format!(
            "`{subdomain}` is where the admin UI is served (Settings → Development → Admin \
             subdomain); choose another subdomain"
        )));
    }
    Ok(())
}

fn framable_by_the_admin(app: &mut Application, body: &Json, apps: &AppMounts) {
    let stated = body
        .get("csp")
        .is_some_and(|csp| !matches!(csp, Json::Null));
    if stated {
        return;
    }
    app.csp = sc_app::allow_admin_framing(app.csp.clone(), apps.base_domain());
}

/// Create a local file store for every framework setting of `app` that names a
/// file store and holds [`NEW_LOCAL_FILE_STORE`], and put the new store's name in
/// its place.
///
/// This is the admin UI's "Create a new local file store" choice on a new
/// application's store picker: an admin creating their first application should
/// not have to leave the form to define a store it will be the only user of. The
/// store is named after the application's subdomain — with 1, 2, … appended when
/// that name is taken — and put where the local backend's "Suggest a directory"
/// operation would put it, by running that very operation, so the two can never
/// disagree about where Saltcorn keeps a store nobody chose a directory for.
///
/// Created the way `createFileStore` creates one, except that a store that will
/// not connect is an error rather than a warning: the admin asked Saltcorn to
/// pick the directory, so a directory Saltcorn cannot create is Saltcorn's
/// problem to report now, not a broken store to find later. Anything created
/// before a failure is removed again.
async fn create_requested_file_stores(
    catalog: &Catalog,
    app: &mut sc_app::Application,
) -> Result<Vec<FileStoreDef>> {
    let settings = file_store_settings(&framework_config_spec(&app.framework.name)?);
    let requested: Vec<String> = settings
        .into_iter()
        .filter(|name| {
            app.framework.config.get(name).and_then(Json::as_str) == Some(NEW_LOCAL_FILE_STORE)
        })
        .collect();
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    let mut taken = choosable_file_stores(catalog).await?;
    let mut created: Vec<FileStoreDef> = Vec::new();
    for setting in requested {
        let name = unique_file_store_name(app.subdomain.trim(), &taken);
        match create_local_file_store(catalog, &name, &app.name).await {
            Ok(def) => {
                taken.push(def.name.clone());
                app.framework
                    .config
                    .insert(setting, Json::String(def.name.clone()));
                created.push(def);
            }
            Err(e) => {
                discard_file_stores(catalog, &created).await;
                return Err(e).context(format!(
                    "creating a new local file store `{name}` for this application"
                ));
            }
        }
    }
    Ok(created)
}

/// One local store named `name`, in its suggested directory, saved and connected.
async fn create_local_file_store(
    catalog: &Catalog,
    name: &str,
    app_name: &str,
) -> Result<FileStoreDef> {
    // Made for the application's code, so the whole store is the application's
    // and travels with the installation's structure (APP_SYNC_PROPOSAL §3.5):
    // this is the one place that intent is known for certain.
    let mut def = FileStoreDef::new(name, sc_files::LOCAL_BACKEND)
        .with_sync_contents(sc_files::SyncContents::Include);
    def.description = format!("Created for the application {app_name}");
    run_backend_operation(&mut def, sc_files::OP_SUGGEST_DIR, &sc_types::Attrs::new()).await?;
    check_file_store_saveable(catalog, &def).await?;
    create_backend_resources(&mut def).await?;
    save_file_store(catalog, &def).await?;
    if let Err(e) = connect_file_store_def(catalog, &def) {
        discard_file_stores(catalog, std::slice::from_ref(&def)).await;
        return Err(e);
    }
    Ok(def)
}

/// Undo [`create_local_file_store`] for each of `defs`: the row, the connection,
/// and the directory if it is still empty.
///
/// Best-effort, because it only ever runs while another error is on its way to
/// the admin — that error is the one worth reporting, so a failure here is
/// logged rather than put in its place.
async fn discard_file_stores(catalog: &Catalog, defs: &[FileStoreDef]) {
    for def in defs {
        if let Err(e) = delete_file_store(catalog, def.id, &[]).await {
            sc_log::log_error!(
                "feldspar: could not remove the file store `{}` created for an application \
                 that was not saved: {}",
                def.name,
                sc_error::format_chain(&e)
            );
        }
        let _ = catalog.disconnect_file_store(&def.name);
        // `remove_dir` only removes an empty directory, which is the one this
        // create made; anything with files in it is left alone.
        if let Some(dir) = def.setting(sc_files::CFG_PATH) {
            let _ = tokio::fs::remove_dir(dir).await;
        }
    }
}

/// A build host as the admin API answers it (`build_host_schema`).
fn build_host_json(host: &sc_app::BuildHost, now: chrono::DateTime<chrono::Utc>) -> Json {
    json!({
        "id": host.id,
        "name": host.name,
        "targets": host.targets,
        "created_at": host.created_at,
        "last_seen_at": host.last_seen_at,
        "online": host.is_online(now),
        "report": host.report,
    })
}

fn parse_build_host_id(raw: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid build host id")))
}

/// Build an application now and mount it, answering as `buildApplication`
/// does, with `log` ahead of the build's own — Build's whole body, and the
/// second half of Deep clean. `quick` is the dev loop's build, which leaves the
/// generated client as it is.
async fn build_now(
    catalog: &Arc<Catalog>,
    apps: &Arc<AppMounts>,
    app: Application,
    mut log: String,
    quick: bool,
) -> Result<HandlerResponse> {
    // Bring the generated code up to date first — the same call as
    // "Update client", so a project directory that was emptied is
    // scaffolded rather than handed to a bundler with nothing to
    // build. (The build rewrites the generated files again on its
    // own; that is cheap, and it is what every other build path
    // relies on.) An app with nothing to build has no client.
    let update = if !quick && sc_app::framework_factory(&app.framework.name).is_none() {
        Some(update_app_client(catalog, &app, apps.triggers()).await?)
    } else {
        None
    };
    // Build and mount live. A build failure propagates as an
    // Application error (§16) whose message is the bundler's own
    // diagnostics, and leaves any previously mounted version serving.
    let app_id = app.id;
    let report = crate::apps::build_and_mount_with(apps, app, quick).await?;
    if let Some(update @ sc_app::ClientUpdate::Scaffolded(_)) = &update {
        // Only a scaffold is news: a regeneration is part of every
        // build, and saying so each time would bury the bundler's
        // output under a line nobody needs.
        log.push_str(&update.summary());
        log.push('\n');
    }
    log.push_str(&build_log(&report));
    // The application's checklists, when it asks for them after each build
    // — not after the dev loop's save, which is many a
    // minute. The build has succeeded and is mounted whatever they say; a
    // failure to run them is a line in the log, not a failed build.
    let mut checks = Json::Null;
    if !quick {
        match crate::checklists::run_after_build(catalog, apps, app_id).await {
            Ok(None) => {}
            Ok(Some(runs)) => {
                log.push_str(&crate::checklists::after_build_log(&runs));
                checks = Json::Array(
                    runs.iter()
                        .map(|run| {
                            json!({
                                "id": run.id.to_string(),
                                "checklist": run.checklist.to_string(),
                                "checklist_name": run.checklist_name,
                                "status": run.status.as_str(),
                            })
                        })
                        .collect(),
                );
            }
            Err(e) => {
                log.push_str(&format!(
                    "\nthe checklists could not be run after the build: {}\n",
                    e.chain()
                ));
            }
        }
    }
    Ok(HandlerResponse::ok(json!({
        "built": true,
        "git_repo": report.git_repo,
        "log": log,
        "checks": checks,
    })))
}

/// The bundler's combined output, for the build log: stdout then stderr.
fn build_log(report: &sc_app::BuildReport) -> String {
    let mut log = String::new();
    // The first build of a scaffolded app is mostly the install, and an admin
    // watching a build that takes a minute needs to see why (§2.3).
    if let Some(install) = &report.install_log {
        log.push_str(install);
        if !log.is_empty() && !log.ends_with('\n') {
            log.push('\n');
        }
    }
    log.push_str(&report.stdout);
    if !report.stderr.is_empty() {
        if !log.is_empty() && !log.ends_with('\n') {
            log.push('\n');
        }
        log.push_str(&report.stderr);
    }
    log
}

/// One target build as the API answers it (`target_build_schema`).
fn target_job_json(job: &crate::target_builds::TargetJob) -> Json {
    json!({
        "target": job.target,
        "label": job.label,
        "status": job.status.as_str(),
        "store": job.store,
        "log_path": job.log_path,
        "started_at": job.started_at.to_rfc3339(),
        "finished_at": job.finished_at.map(|t| t.to_rfc3339()),
        "artifact": job.artifact.as_ref().map(|a| a.path.clone()),
        "size": job.artifact.as_ref().map(|a| a.size),
        "log": job.artifact.as_ref().map(|a| a.log.clone()),
        "error": job.error,
        "build_host": Json::Null,
    })
}

/// A build host's job in the shape of [`target_job_json`], with its host.
fn remote_job_json(job: &sc_app::BuildJob) -> Json {
    json!({
        "target": job.target,
        "label": job.label,
        "status": job.state.as_str(),
        "store": job.store,
        "log_path": job.log_path,
        "started_at": job.started_at.unwrap_or(job.created_at).to_rfc3339(),
        "finished_at": job.finished_at.map(|t| t.to_rfc3339()),
        "artifact": job.artifact,
        "size": job.size,
        "log": job.log,
        "error": job.error,
        "build_host": job.host_name,
    })
}

/// For each target of a listed application that this server can't build, check
/// whether a build host can. If one can, mark the target ready and name the host.
fn builds_on_hosts(body: &mut Json, hosts: &[sc_app::BuildHost]) {
    let now = chrono::Utc::now();
    let Some(targets) = body.get_mut("targets").and_then(Json::as_array_mut) else {
        return;
    };
    for target in targets {
        let name = target["name"].as_str().unwrap_or_default().to_owned();
        let readiness = &mut target["readiness"];
        if readiness["ready"] == json!(true) {
            continue;
        }
        if let Some(host) = sc_app::host_for_target(hosts, &name, now) {
            *readiness = json!({
                "ready": true,
                "missing": [],
                "from_environment": [],
                "build_host": host.name,
            });
        }
    }
}

/// Serve a **newly created** application without waiting for a restart.
///
/// Two shapes, one promise: creating an application is what deploys it.
///
/// - A framework that is **constructed** (Saltcorn UI) is mounted here and
///   now, synchronously — there is nothing to build, so there is nothing to
///   wait for.
/// - A framework that is **built** (`react`, `code`) has just had its project
///   scaffolded, and its first build is `npm install` plus a bundler: minutes,
///   which is not a thing to hold an HTTP response open for. So it is started
///   in the background and the application mounts when it finishes. The
///   response says `building` so the admin knows to expect it.
///
/// This is the same work the boot path does for every stored application
/// ([`mount_all`](crate::mount_all)), moved to the moment the application
/// comes into existence. Without it, a restart was the only thing that ever
/// built a newly created application, which made restarting the server look
/// like part of creating one.
///
/// Never fatal, in either shape: the row is saved and valid, and a build that
/// fails is one the admin fixes in the project and re-runs with the Build
/// button.
#[allow(
    clippy::disallowed_methods,
    reason = "tokio::spawn: detached on purpose; the work outlives the request that started it and must not run in that request's changeset"
)]
async fn serve_new_application(apps: &Arc<AppMounts>, app: &sc_app::Application, body: &mut Json) {
    if sc_app::framework_factory(&app.framework.name).is_some() {
        mount_if_constructed(apps, app, body).await;
        return;
    }
    // A framework with no resolvable source has nothing to build and nothing
    // to serve from — an application of it is a definition, not a deployment,
    // and the admin's next step is to say where its project is.
    if sc_app::app_source_from_config(&app.framework).is_err() {
        return;
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert("building".to_owned(), json!(true));
    }
    let apps = apps.clone();
    let app = app.clone();
    tokio::spawn(async move {
        let subdomain = app.subdomain.clone();
        match build_and_mount(&apps, app).await {
            Ok(_) => {
                eprintln!("feldspar: built and mounted application `{subdomain}` on creation")
            }
            // The console, and only the console: the response went out before
            // the bundler started. The admin's way back in is the Build
            // button, which reports its own failure in front of them.
            Err(e) => eprintln!(
                "feldspar: application `{subdomain}` was created, but its first build                      failed — fix the project and build it again: {e}"
            ),
        }
    });
}

/// Mount an application whose framework has nothing to build (Saltcorn UI)
/// as part of saving it: there is no Build button for it to wait for, so
/// saving is the deployment (TODO "Saltcorn UI" §1, 5.7). Not fatal — the
/// row is saved — and reported on the response as `mounted` or
/// `mount_error`.
async fn mount_if_constructed(apps: &AppMounts, app: &sc_app::Application, body: &mut Json) {
    if sc_app::framework_factory(&app.framework.name).is_none() {
        return;
    }
    let outcome = build_and_mount(apps, app.clone()).await;
    if let Some(obj) = body.as_object_mut() {
        match outcome {
            Ok(_) => {
                obj.insert("mounted".to_owned(), json!(true));
            }
            Err(e) => {
                obj.insert("mount_error".to_owned(), json!(e.causes()));
            }
        }
    }
}

/// Put an **edited** record in front of the running mount, as part of
/// saving it.
///
/// Everything an application's row says that the router reads — its static
/// directories (§13.2), its CSP, its locales, the subsets its providers are
/// projected from — is read off the `Application` the mount was made with.
/// So without this a directory added on the Applications screen changes
/// nothing a browser can see: the mount goes on resolving against the record
/// it was built with, and the framework's SPA fallback answers the new
/// mount's path with `index.html` — a 200 that is the wrong file, which is
/// worse than a 404 because it looks like it worked.
///
/// **Not a build.** An application served from a bundle keeps its bundle;
/// only the record and the providers are rebuilt, which is why this is
/// cheap enough to run on every save. A *constructed* framework (Saltcorn
/// UI) is re-mounted through its factory instead, which is what
/// [`AppMounts::refresh_mount`] does with one and costs no bundler either.
/// An application nobody has built yet is not mounted, and that is not an
/// error — saving one is an ordinary thing to do.
async fn refresh_mounted_app(apps: &AppMounts, app: &sc_app::Application, body: &mut Json) {
    let constructed = sc_app::framework_factory(&app.framework.name).is_some();
    match apps.refresh_mount(app.clone()).await {
        // `mounted` is the constructed framework's contract: for it, saving
        // is the deployment and the screen says so.
        Ok(()) if constructed => {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("mounted".to_owned(), json!(true));
            }
        }
        Ok(()) => {}
        Err(e) => {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("mount_error".to_owned(), json!(e.causes()));
            }
        }
    }
}

/// The `listApplications` endpoint.
pub(crate) async fn list_all_applications(
    state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    _body: Json,
) -> Result<HandlerResponse> {
    let catalog = state.catalog;
    let apps = list_applications(&catalog).await?;
    let hosts = sc_app::list_build_hosts(&catalog).await?;
    let claude = crate::terminal::claude_unavailable(
        &catalog,
        state.apps.claude_cli().as_deref().map(|p| p.as_path()),
        &apps,
    )
    .await;
    let out: Vec<Json> = apps
        .iter()
        .zip(claude)
        .map(|(app, unavailable)| {
            let mut body = application_api_json(app);
            builds_on_hosts(&mut body, &hosts);
            // Whether "Build with Claude" can open a session, and if not, the
            // sentence its tooltip shows.
            body["claude"] = json!({
                "available": unavailable.is_none(),
                "reason": unavailable,
            });
            body
        })
        .collect();
    Ok(HandlerResponse::ok(Json::Array(out)))
}

/// The `createApplication` endpoint.
pub(crate) async fn create_application(
    state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    body: Json,
) -> Result<HandlerResponse> {
    let HandlerState { catalog, apps } = state;
    // A create mints a fresh id; the body carries everything else.
    // What comes back from the save is what was *stored*: a custom
    // SQL query is described on the way in, so the response carries
    // the result columns the database reported rather than the empty
    // list the caller sent.
    let mut created = application_from_body(AppId::new(), &body)?;
    check_app_subdomain(&apps, &created.subdomain)?;
    framable_by_the_admin(&mut created, &body, &apps);
    // A store setting that asked for a new local store gets one
    // before the application is validated, since validation checks
    // the store exists. If the application is then refused, the
    // stores made for it go too: the admin corrects the form and
    // presses Create again, and a leftover store would make that
    // second attempt create `todo1` beside an orphaned `todo`.
    // An exposed agent must exist, and one exposed signed out must
    // be public: refused here, in front of the admin (§11.7).
    sc_app::check_exposed_agents(&catalog, &created).await?;
    let new_stores = create_requested_file_stores(&catalog, &mut created).await?;
    let app = match save_application(&catalog, &created).await {
        Ok(app) => app,
        Err(e) => {
            discard_file_stores(&catalog, &new_stores).await;
            return Err(e);
        }
    };
    // A `react` app's project is the server's to create (§2.3): this
    // is the step that removes the SSH requirement, so it happens on
    // the first save rather than waiting for an admin to ask. It is
    // deliberately *not* fatal to the create — the row is already
    // saved and valid, and an unwritable store or an occupied
    // directory is a thing the admin fixes and re-tries, not a reason
    // to lose the application they just configured.
    let mut body = application_api_json(&app);
    if !new_stores.is_empty()
        && let Some(obj) = body.as_object_mut()
    {
        let names: Vec<&str> = new_stores.iter().map(|d| d.name.as_str()).collect();
        obj.insert("created_file_stores".to_owned(), json!(names));
    }
    // The scaffolded project's generated client is typed against the
    // app's endpoints, exposed triggers included, so the scaffold
    // resolves them against the same live set a mount would.
    let scaffold = scaffold_new_app(&catalog, &app, apps.triggers()).await;
    // ...and the agent that will build it, which its framework
    // declares (§13.3). After the scaffold, deliberately: the agent is
    // pointed at the project the scaffold just wrote. Non-fatal for
    // the same reason — an application with no builder agent is one
    // the admin creates an agent for, not one that failed to exist.
    let agent = create_builder_agent(&catalog, &apps, &app).await;
    if let Some(obj) = body.as_object_mut() {
        match &scaffold {
            Ok(Some(report)) => {
                obj.insert("scaffolded".to_owned(), json!(report.summary()));
            }
            Ok(None) => {}
            Err(e) => {
                obj.insert("scaffold_error".to_owned(), json!(e.causes()));
            }
        }
        match &agent {
            Ok(Some(name)) => {
                obj.insert("agent".to_owned(), json!(name));
            }
            Ok(None) => {}
            Err(e) => {
                obj.insert("agent_error".to_owned(), json!(e.causes()));
            }
        }
    }
    serve_new_application(&apps, &app, &mut body).await;
    Ok(HandlerResponse::ok(body).with_status(201))
}

/// The `updateApplication` endpoint.
pub(crate) async fn update_application(
    state: HandlerState,
    ctx: HandlerCtx<Admin>,
    body: Json,
) -> Result<HandlerResponse> {
    let HandlerState { catalog, apps } = state;
    let id = parse_app_id(ctx.path_param("id")?)?;
    let Some(stored) = load_application(&catalog, id).await? else {
        return Err(Error::not_found(format!("no application with id {id}")));
    };
    // The id is the path's, not the body's — the row's identity is not
    // something a payload gets to reassign.
    let mut updated = application_from_body(id, &body)?;
    check_app_subdomain(&apps, &updated.subdomain)?;
    merge_framework_secrets(&stored, &mut updated);
    framable_by_the_admin(&mut updated, &body, &apps);
    sc_app::check_exposed_agents(&catalog, &updated).await?;
    let app = save_application(&catalog, &updated).await?;
    // A new subdomain is a new address for the builder too: left
    // alone, it would `check` and look at an application that is
    // no longer served there, and the admin would no longer find
    // it as this application's. Reported beside the saved
    // application rather than instead of it, as on create.
    let agent = if stored.subdomain.trim() != app.subdomain.trim() {
        retarget_builder_agent(&catalog, &apps, &stored, &app).await
    } else {
        Ok(None)
    };
    // A save is an API-definition change: a table added to the
    // subset, an endpoint's role, a custom query. The generated
    // client has to describe what the app now serves (decision 10),
    // so it is rewritten here — logged and never fatal, because the
    // application is already saved and an unreachable store is not a
    // reason to report that it was not.
    reemit_app_client(&catalog, &app, apps.triggers()).await;
    let mut body = application_api_json(&app);
    if let Some(obj) = body.as_object_mut() {
        match &agent {
            Ok(Some(name)) => {
                obj.insert("agent".to_owned(), json!(name));
            }
            Ok(None) => {}
            Err(e) => {
                obj.insert("agent_error".to_owned(), json!(e.causes()));
            }
        }
    }
    refresh_mounted_app(&apps, &app, &mut body).await;
    Ok(HandlerResponse::ok(body))
}

/// The `buildApplication` endpoint.
pub(crate) async fn build_application(
    state: HandlerState,
    ctx: HandlerCtx<Admin>,
    _body: Json,
) -> Result<HandlerResponse> {
    let HandlerState { catalog, apps } = state;
    let id = parse_app_id(ctx.path_param("id")?)?;
    let app = load_application(&catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no application with id {id}")))?;
    // `?quick=true` is the IDE's build on save.
    let quick = ctx
        .query_get("quick")
        .is_some_and(|q| q == "true" || q == "1");
    build_now(&catalog, &apps, app, String::new(), quick).await
}
