//! Resolving an [`Application`] into its API providers, endpoint set, and typed
//! client (design §13.2/§13.4/§13.1).
//!
//! An [`Application`] is pure data: it names the tables it may touch and the API
//! providers it enables, each on a sub-path. This module is the wiring that turns
//! that declaration into running machinery — resolving the declared tables
//! against the [`Catalog`], building one [`ApiProvider`] per [`ApiConfig`], and
//! collecting their projections into the app's single [`EndpointSet`].
//!
//! That endpoint set is what the app's **TypeScript client** is generated from
//! ([`app_client`]), by the same [`generate_client`] the admin SPA uses. The
//! admin's client is a checked-in artifact with a drift test, because its
//! endpoints are compile-time constants; an app's endpoints depend on which
//! tables it declares, so its client cannot be committed — it is emitted into the
//! app's source tree at build time (see [`crate::build::emit_client`]).

use std::sync::Arc;

use sc_action::{Trigger, TriggerDispatcher};
use sc_api::{
    ApiProvider, EndpointSet, GRAPHQL_PROVIDER, GraphqlLimits, GraphqlProvider, REST_PROVIDER,
    RestProvider, generate_client, op_name, rest_row_cap,
};
use sc_catalog::{Catalog, Table};
use sc_error::{Error, Repr, Result};
use sc_types::{FormField, validate_attrs};

use crate::application::{ApiConfig, Application};
use crate::framework::framework_serves_ui;

/// How an API provider presents itself to an admin enabling one: a human name
/// and a sentence saying what protocol they get.
///
/// The sibling of [`FrameworkInfo`](crate::FrameworkInfo), and it exists for the
/// same reason: the admin's application form offers the registered names as a
/// **list** rather than a free-text box, so `graphql` is discoverable and a typo
/// is refused at the keyboard rather than surfacing later as a mount failure on
/// a saved application.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiProviderInfo {
    /// The registry key, as stored in an [`ApiConfig`](crate::ApiConfig).
    pub name: String,
    /// A human-facing name.
    pub label: String,
    /// One sentence: what this provider serves, and what a caller does with it.
    pub description: String,
    /// The sub-path this provider is usually mounted at — what the form fills in
    /// when the admin picks it.
    pub default_mount: String,
    /// The settings this provider takes, in the same `FormField` vocabulary a
    /// framework declares its own with (§13.3).
    ///
    /// The application form renders these and posts the result into
    /// [`ApiConfig::config`](crate::ApiConfig::config), so enabling GraphQL and
    /// switching its aggregates on is one screen with no GraphQL-specific code
    /// on it — exactly the arrangement frameworks are already under.
    pub config_spec: Vec<FormField>,
    /// Whether this provider serves **custom SQL queries** — whether the
    /// application form should offer the query editor beside its settings.
    ///
    /// A flag rather than the form checking for `rest`, for the reason the
    /// settings are a declared spec: the screen renders what a provider says
    /// about itself, and the day a second provider grows custom queries the form
    /// does not have to hear about it. Custom queries are not a settings field
    /// (§13.4, decision 8), so this is how the provider declares them.
    pub supports_custom_queries: bool,
}

/// Every registered API provider with its presentation, in the order an admin
/// should be offered them.
///
/// **This is the list [`app_providers_with`] switches on**, so a provider that
/// is offered is a provider that mounts: the two cannot drift, because the
/// unknown-provider error is written from this list.
pub fn registered_api_provider_info() -> Vec<ApiProviderInfo> {
    vec![
        ApiProviderInfo {
            name: REST_PROVIDER.to_owned(),
            label: "REST".to_owned(),
            description: "A route per operation over the app's tables and exposed \
                          triggers, with a typed TypeScript client generated from it. \
                          The one to take unless you know you want the other."
                .to_owned(),
            default_mount: "/api".to_owned(),
            config_spec: sc_api::rest_config_spec(),
            supports_custom_queries: true,
        },
        ApiProviderInfo {
            name: GRAPHQL_PROVIDER.to_owned(),
            label: "GraphQL".to_owned(),
            description: "One endpoint the caller writes the shape of: nested \
                          relations and constrained child aggregates in one round \
                          trip, with the SDL served beside it. Sits alongside REST \
                          rather than replacing it."
                .to_owned(),
            default_mount: sc_api::GRAPHQL_DEFAULT_MOUNT.to_owned(),
            config_spec: sc_api::graphql_config_spec(),
            supports_custom_queries: false,
        },
    ]
}

/// The settings the API provider registered under `name` declares — the registry
/// lookup, resolving a stored [`ApiConfig`](crate::ApiConfig)'s provider name to
/// a spec without building the provider.
///
/// The sibling of [`framework_config_spec`](crate::framework_config_spec), and
/// written from [`registered_api_provider_info`] for the reason that list is
/// what [`app_providers_with`] switches on: a provider that is offered is a
/// provider that mounts, and now also a provider whose settings are validated
/// against what it actually declares.
///
/// An unknown name is a configuration error rather than a provider with no
/// settings — the same answer mounting one gives.
pub fn api_provider_config_spec(name: &str) -> Result<Vec<FormField>> {
    registered_api_provider_info()
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.config_spec)
        .ok_or_else(|| {
            Error::config(format!(
                "unknown API provider `{name}`; this server registers {}",
                registered_provider_names()
            ))
        })
}

/// Check one [`ApiConfig`](crate::ApiConfig)'s settings against its provider's
/// declared spec.
///
/// Called **on save** ([`save_application`](crate::save_application)), for the
/// reason a framework's config is: a misspelled or ill-typed setting is the
/// admin's to fix and the admin is standing in front of the form, whereas the
/// same mistake read at mount time is a limit silently serving its default —
/// which for the aggregation switch means a schema that quietly lacks the fields
/// somebody thought they had turned on.
///
/// It takes the whole [`Application`] because a REST API's **custom SQL queries**
/// are checked here too, and their rules are about the app: a query's sub-path
/// may not be one the app's own tables already answer, and its name may not be
/// one of their client methods. Whether the SQL *runs* is the database's
/// question, asked separately by [`describe_api_queries`].
pub fn validate_api_config(app: &Application, api: &ApiConfig) -> Result<()> {
    let spec = api_provider_config_spec(&api.provider)?;
    let mut settings = api.config.clone();
    // A REST API's custom SQL queries live in the same object but are not a
    // settings field (§13.4): they are a list of records each carrying a nested
    // list of parameters, so they are validated as the typed value they are and
    // then lifted out before the rest is checked against the form spec — which
    // would otherwise refuse `queries` as an unknown setting.
    if api.provider == REST_PROVIDER {
        let queries = sc_api::custom_queries(&api.config)?;
        let tables: Vec<String> = app.tables.iter().map(|t| t.0.clone()).collect();
        sc_api::validate_custom_queries(&queries, &tables)?;
        settings.remove(sc_api::REST_CFG_QUERIES);
    }
    validate_attrs(&spec, &settings).map_err(|e| {
        // Name the provider as well as the setting, rebuilt rather than wrapped
        // for the reason `check_attrs` gives in `framework.rs`: `Error`'s
        // `Invalid` renders its own prefix, and a `Context` would hide the
        // setting, which is the part the admin needs.
        if let Repr::Invalid(msg) = e.repr() {
            Error::invalid(format!("API provider `{}`: {msg}", api.provider))
        } else {
            e
        }
    })
}

/// Prepare every custom SQL query this API declares, and return the
/// configuration with each query's **result columns** as the database described
/// them (§13.4, decision 5).
///
/// Called on save, and the reason a broken query cannot be stored: preparing is
/// both the validation and the typing, so a statement that will not prepare
/// comes back as Postgres's own message while its author is still looking at it,
/// and one that will is typed by the database rather than by a declaration that
/// would go stale the first time anyone edited the SQL.
///
/// Re-describing on **every** save is what keeps the two from drifting: the
/// stored columns are never older than the stored SQL, because they are written
/// in the same statement.
///
/// An API with no custom queries — every GraphQL one, and most REST ones — is
/// returned untouched without touching the database.
pub async fn describe_api_queries(catalog: &Catalog, api: &ApiConfig) -> Result<ApiConfig> {
    if api.provider != REST_PROVIDER {
        return Ok(api.clone());
    }
    let mut queries = sc_api::custom_queries(&api.config)?;
    if queries.is_empty() {
        return Ok(api.clone());
    }
    for query in &mut queries {
        query.columns = sc_api::describe_custom_query(catalog, query).await?;
    }
    let mut api = api.clone();
    sc_api::set_custom_queries(&mut api.config, &queries)?;
    Ok(api)
}

/// Whether the API provider registered under `name` serves **custom SQL
/// queries**.
///
/// Read off [`registered_api_provider_info`]'s `supports_custom_queries` — the
/// same declaration the admin form offers the query editor on — so a provider
/// that grows custom queries is answered for here without this function hearing
/// about it.
pub fn serves_custom_queries(name: &str) -> bool {
    registered_api_provider_info()
        .iter()
        .any(|p| p.name == name && p.supports_custom_queries)
}

/// The API row of `app` that holds custom SQL queries, chosen by `mount` or, when
/// there is exactly one candidate, by there being nothing to choose.
///
/// The one answer to "which API does this query belong to", shared by everything
/// that writes one: the CLI's `feldspar api add-query`, and the `admin_copilot`
/// trait's `save_api_query` (§11.3). Two answers would be two ways for a query to
/// land somewhere its author did not mean.
///
/// Ambiguity is refused rather than resolved: an application with two such APIs
/// has two places a query could land, and picking one would be picking which
/// client method appears where. `how_to_name` is how *this* caller's user says
/// which one they mean — `--api` at the command line, the `api` argument in a
/// tool call — because the refusal is only useful if it names the thing the
/// reader can type.
///
/// A **named** API that does not serve custom queries is refused here too, rather
/// than handed back to be written to: the query would be stored under a settings
/// key that provider does not declare, and the save would refuse it as an unknown
/// setting — a true message about the wrong thing, arriving one step after the
/// mistake was made.
pub fn select_api<'a>(
    app: &'a mut Application,
    mount: Option<&str>,
    how_to_name: &str,
) -> Result<&'a mut ApiConfig> {
    let mounts: Vec<String> = app.apis.iter().map(|a| a.mount.clone()).collect();
    let serving: Vec<String> = app
        .apis
        .iter()
        .filter(|a| serves_custom_queries(&a.provider))
        .map(|a| a.mount.clone())
        .collect();
    if let Some(mount) = mount {
        let api = app
            .apis
            .iter_mut()
            .find(|a| a.mount == mount)
            .ok_or_else(|| {
                Error::config(format!(
                    "application `{}` has no API mounted at `{mount}`; it has {}",
                    app.subdomain,
                    quoted(&mounts)
                ))
            })?;
        if !serves_custom_queries(&api.provider) {
            return Err(Error::config(format!(
                "the `{}` API at `{mount}` of `{}` does not serve custom SQL \
                 queries; the ones that do are {}",
                api.provider,
                app.subdomain,
                quoted(&serving)
            )));
        }
        return Ok(api);
    }
    let candidates: Vec<usize> = app
        .apis
        .iter()
        .enumerate()
        .filter(|(_, a)| serves_custom_queries(&a.provider))
        .map(|(i, _)| i)
        .collect();
    match candidates.as_slice() {
        [only] => Ok(&mut app.apis[*only]),
        [] => Err(Error::config(format!(
            "application `{}` has no API that serves custom SQL queries; it has {}",
            app.subdomain,
            quoted(&mounts)
        ))),
        _ => Err(Error::config(format!(
            "application `{}` has more than one API that serves custom SQL \
             queries, so say which with {how_to_name}: {}",
            app.subdomain,
            quoted(
                &candidates
                    .iter()
                    .map(|i| app.apis[*i].mount.clone())
                    .collect::<Vec<_>>()
            )
        ))),
    }
}

/// `` `a`, `b` `` — or "none" for an empty list, so a message never trails off.
fn quoted(items: &[String]) -> String {
    if items.is_empty() {
        return "none".to_owned();
    }
    items
        .iter()
        .map(|i| format!("`{i}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The registered provider names, comma-separated — what an error naming what is
/// available says.
fn registered_provider_names() -> String {
    registered_api_provider_info()
        .iter()
        .map(|p| format!("`{}`", p.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Check that no API provider claims a path the app's UI needs, and that no two
/// claim the same one (§13.2/§13.4).
///
/// A provider mounted at `/` claims **every** path — that is what a root mount
/// means — so an app whose framework serves a UI would answer `GET /` from its
/// API, which has no endpoint there, and the app would be a 404 in a browser
/// with nothing to say why. That is a configuration error, and this is where it
/// is named.
///
/// It is refused only when there is a UI to lose: an app whose framework
/// declares [`serves_ui`](crate::Framework::serves_ui) `false` is an API-only
/// app, and `/` is exactly the right mount for it.
///
/// Two providers on one mount is refused whatever the framework serves, because
/// there is no arrangement in which it is what the admin meant: the router
/// resolves a request to **one** provider by longest matching mount, so the
/// loser of the tie is a whole API that is mounted, generated a client for, and
/// unreachable. REST and GraphQL coexisting on one application is the point of
/// this milestone — on `/api` and `/graphql`, not twice on `/api`.
pub fn validate_api_mounts(app: &Application) -> Result<()> {
    check_api_mounts(app, framework_serves_ui(&app.framework.name))
}

/// [`validate_api_mounts`] with the framework's answer supplied — the whole rule,
/// with the registry lookup lifted out so it is one decision on one input.
fn check_api_mounts(app: &Application, serves_ui: bool) -> Result<()> {
    for (i, api) in app.apis.iter().enumerate() {
        if let Some(other) = app.apis[..i].iter().find(|a| a.mount == api.mount) {
            return Err(Error::invalid(format!(
                "application `{}` mounts both its `{}` and `{}` APIs at `{}`; a request \
                 resolves to one provider, so the other would be unreachable — give \
                 them separate sub-paths",
                app.name, other.provider, api.provider, api.mount
            )));
        }
        if serves_ui && api.mount == "/" {
            return Err(Error::invalid(format!(
                "application `{}` mounts its `{}` API at `/`, which claims every path \
                 and would leave the `{}` framework's UI unreachable; mount the API on \
                 a sub-path such as `/api`",
                app.name, api.provider, app.framework.name
            )));
        }
    }
    Ok(())
}

/// Check that every static directory names a store the app declares, and that
/// none is mounted where an API provider would answer first (§13.2).
///
/// Two rules, one reason each, and both are checked here rather than at serve
/// time because at serve time the symptom is a 404 with nothing to say why.
///
/// **The store must be in [`Application::file_stores`].** The subset is the
/// whole truth about which stores an application touches — it is what
/// `applications_using_file_store` counts and what every other reader of the
/// app checks — so a static directory naming a store outside it would quietly
/// widen the app's reach, and (before this check) pin a store the app was never
/// granted against deletion. A typo is the same thing by accident: nothing
/// resolved the name, so a misspelled store was stored and turned up as a 404.
///
/// **The mount must not sit at or under an API provider's mount.** A request
/// resolves to a provider first (§13.2, step 1), so a directory at `/api/img`
/// behind an API at `/api` is served by the API — which has no endpoint there —
/// and never by the directory. Same shape as the API-at-`/` refusal above: the
/// admin is standing in front of the form, and this is where they can be told.
pub fn validate_static_dirs(app: &Application) -> Result<()> {
    for dir in &app.static_dirs {
        if !app.file_stores.contains(&dir.store) {
            return Err(Error::invalid(format!(
                "application `{}` serves `{}` from file store `{}`, which it does not \
                 declare access to; add the store to the application's file stores, or \
                 pick one it already has",
                app.name, dir.mount, dir.store.0
            )));
        }
        if let Some(api) = app
            .apis
            .iter()
            .find(|api| crate::application::path_under_mount(&api.mount, &dir.mount))
        {
            return Err(Error::invalid(format!(
                "application `{}` mounts its static directory at `{}`, under the `{}` API \
                 at `{}`; a request resolves to the API first, so the directory would never \
                 be served — mount it outside the API's sub-path",
                app.name, dir.mount, api.provider, api.mount
            )));
        }
    }
    Ok(())
}

/// The tables an application declares, resolved against the catalog.
///
/// An app sees only its declared subset (§13.2), so this is the *whole* of the
/// data any of its providers can reach. A declared table that is not in the
/// catalog is an error rather than a silent omission: it means the app is
/// misconfigured, and quietly serving a smaller API would hide that.
pub fn app_tables(app: &Application, cat: &Catalog) -> Result<Vec<Table>> {
    app.tables.iter().map(|id| cat.require(&id.0)).collect()
}

/// The triggers an application exposes, resolved against the live trigger set
/// (§10.2).
///
/// An app's exposed triggers are a **declared subset**, exactly like its tables,
/// and this resolves it the same way [`app_tables`] does — a declared trigger
/// that is not in the live set is an error rather than a silently missing
/// endpoint. The two ways it can be missing say different things and the live
/// set's own `require` distinguishes them: never defined (a typo, or a trigger
/// somebody deleted) versus defined but not usable (its table is gone, its action
/// came from an uninstalled plugin) — the second carries the reason, which is the
/// part the admin needs.
///
/// `dispatcher` is `None` in the contexts that only wanted endpoint shapes.
/// That is fine for an app with no exposed triggers and a **configuration error**
/// for one that has them: silently projecting an app without the endpoints its
/// own generated client calls is the failure this refuses to have.
pub fn app_triggers(
    app: &Application,
    dispatcher: Option<&Arc<TriggerDispatcher>>,
) -> Result<Vec<Trigger>> {
    if app.triggers.is_empty() {
        return Ok(Vec::new());
    }
    let Some(dispatcher) = dispatcher else {
        return Err(Error::config(format!(
            "application `{}` exposes trigger `{}`, but no trigger set is \
             available in this context",
            app.name, app.triggers[0]
        )));
    };
    let live = dispatcher.triggers()?;
    let mut resolved: Vec<Trigger> = Vec::with_capacity(app.triggers.len());
    for declared in &app.triggers {
        let trigger = live.require(&declared.0).map_err(|e| {
            Error::config(format!(
                "application `{}` exposes trigger `{declared}`: {e}",
                app.name
            ))
        })?;
        // Two triggers whose names differ only in punctuation would project one
        // endpoint name (`send_digest` and `sendDigest` are both `runSendDigest`)
        // and the second registration would be a panic in a running server. It is
        // a configuration error, so it is named here rather than survived.
        if let Some(clash) = resolved
            .iter()
            .find(|t| op_name("run", &t.name) == op_name("run", &trigger.name))
        {
            return Err(Error::config(format!(
                "application `{}` exposes both `{}` and `{declared}`, which would \
                 project the same endpoint `{}`; expose one of them",
                app.name,
                clash.name,
                op_name("run", &trigger.name)
            )));
        }
        resolved.push(trigger.clone());
    }
    Ok(resolved)
}

/// Build the API providers an application enables, each on its own sub-path
/// (design §13.4).
///
/// [`registered_api_provider_info`] is the list of names this understands; an
/// unknown one is a configuration error rather than a silently skipped API.
///
/// The mounts are checked first ([`validate_api_mounts`]), so an app saved before
/// that check existed fails to build and to mount — with the reason — rather than
/// coming up as an app that answers every request with a 404 from its API.
pub fn app_providers(app: &Application, cat: &Catalog) -> Result<Vec<Box<dyn ApiProvider>>> {
    app_providers_with(app, cat, None, None)
}

/// [`app_providers`] with the server's JavaScript evaluator and trigger
/// dispatcher injected — what a *running* mount uses, so ownership formulas'
/// reified path (§7.3) has an engine and an exposed trigger has something to run
/// on.
///
/// Both are optional because everything that only needs the endpoint *shapes* —
/// client generation, endpoint collection, tests — has neither and needs neither.
/// A provider without an evaluator fails closed if a formula actually requires
/// one; an app that declares triggers without a dispatcher is refused outright
/// ([`app_triggers`]), because there its absence changes the app's API surface
/// rather than one request's answer.
///
/// **A trigger is exposed through an API, so an app with no API resolves none.**
/// The subset is still meaningful without one — a Saltcorn UI application's views
/// may run only the triggers it declares — but there it is a bound checked when a
/// view runs a trigger, which names a missing one then, not an endpoint whose
/// absence changes a generated client. Resolving it here anyway made a Saltcorn 1
/// import fail to mount whenever one of its triggers was refused on the way in
/// (`TrimPages`, a v1 `modify_row`), taking every view down with that one column.
pub fn app_providers_with(
    app: &Application,
    cat: &Catalog,
    evaluator: Option<std::sync::Arc<dyn sc_expr::JsEvaluator>>,
    dispatcher: Option<&Arc<TriggerDispatcher>>,
) -> Result<Vec<Box<dyn ApiProvider>>> {
    validate_api_mounts(app)?;
    let tables = app_tables(app, cat)?;
    if app.apis.is_empty() {
        return Ok(Vec::new());
    }
    let triggers = app_triggers(app, dispatcher)?;
    app.apis
        .iter()
        .map(|api| match api.provider.as_str() {
            REST_PROVIDER => {
                let mut provider = RestProvider::project_with(&api.mount, &tables, &triggers)
                    // The application's own cap on a list read, from its stored
                    // provider configuration.
                    .with_row_cap(rest_row_cap(&api.config))
                    // …and its custom SQL queries, one endpoint each (§13.4).
                    .with_queries(sc_api::custom_queries(&api.config)?)
                    .map_err(|e| Error::config(format!("application `{}`: {e}", app.name)))?;
                if let Some(evaluator) = &evaluator {
                    provider = provider.with_evaluator(evaluator.clone());
                }
                if let Some(dispatcher) = dispatcher {
                    provider = provider.with_dispatcher(Arc::clone(dispatcher));
                }
                Ok(Box::new(provider) as Box<dyn ApiProvider>)
            }
            GRAPHQL_PROVIDER => {
                let mut provider = graphql_provider(app, api, &tables)?;
                if let Some(evaluator) = &evaluator {
                    provider = provider.with_evaluator(evaluator.clone());
                }
                Ok(Box::new(provider) as Box<dyn ApiProvider>)
            }
            other => Err(Error::config(format!(
                "application `{}` enables unknown API provider `{other}`; \
                 this server registers {}",
                app.name,
                registered_provider_names()
            ))),
        })
        .collect()
}

/// Project `app`'s GraphQL API from `api` (its mount **and** its configuration)
/// over `tables` — the one place a [`GraphqlProvider`] is built for an
/// application.
///
/// Shared by the mount path ([`app_providers_with`]) and the build path
/// ([`app_graphql`]) so the SDL a build writes into the app's source tree is
/// the SDL its running mount answers introspection with. Two constructions with
/// the same arguments would be the same schema *by coincidence*; one is the same
/// schema because it is the same call. That now includes the limits: the
/// aggregation switch changes which fields exist, so a build reading it from
/// somewhere else than the mount does is how an app's checked SDL and its served
/// schema come apart.
///
/// The tables are the app's declared subset, already resolved — the same value
/// the REST provider is projected from, so one application cannot expose two
/// different table subsets through its two APIs.
fn graphql_provider(
    app: &Application,
    api: &ApiConfig,
    tables: &[Table],
) -> Result<GraphqlProvider> {
    // A schema that will not build is a mount failure naming the table that
    // caused it: an application whose API is half described is worse than one
    // that refuses to come up, because the half nobody notices is the wrong one.
    let provider =
        GraphqlProvider::project_with(&api.mount, tables, GraphqlLimits::from_config(&api.config))
            .map_err(|e| {
                Error::config(format!(
                    "application `{}` cannot project its GraphQL API: {e}",
                    app.name
                ))
            })?;
    // A `File` field answers with its path and the URL the *REST* provider
    // serves the bytes at, so a GraphQL field never becomes a second download
    // path. An application with no REST provider has no such URL to give; the
    // provider's default mount is what the field then names, which is at least
    // honest about where the bytes would be served from.
    Ok(
        match app.apis.iter().find(|a| a.provider == REST_PROVIDER) {
            Some(rest) => provider.with_file_mount(&rest.mount),
            None => provider,
        },
    )
}

/// An application's GraphQL API as its **build** needs to know it: where it is
/// mounted, and the SDL of the schema served there.
///
/// Both, together, because the two generated files need one each and they have
/// to describe the same API: the client posts to `mount`, and `schema.graphql`
/// is what `gql.tada` type-checks the documents it posts against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppGraphql {
    /// The sub-path the provider is mounted at within the application.
    pub mount: String,
    /// The schema's SDL — `Schema::sdl()` of the projection that will be mounted.
    pub sdl: String,
}

/// The application's GraphQL projection, or `None` for an application that does
/// not enable the provider.
///
/// This is what the build writes into the app's source tree beside the generated
/// REST client. The SDL is the file `gql.tada` type-checks the app's own queries
/// against, so a query the schema no longer answers is a **build failure** in
/// the app's own `tsc --noEmit` rather than an error at run time.
pub fn app_graphql(app: &Application, cat: &Catalog) -> Result<Option<AppGraphql>> {
    let Some(api) = app.apis.iter().find(|a| a.provider == GRAPHQL_PROVIDER) else {
        return Ok(None);
    };
    let tables = app_tables(app, cat)?;
    let provider = graphql_provider(app, api, &tables)?;
    Ok(Some(AppGraphql {
        mount: provider.mount(),
        sdl: provider.sdl(),
    }))
}

/// The `CREATE TABLE` statements for the tables an application declares — the
/// `schema.sql` of its generated directory (§13.3).
///
/// It is there for the coding agent working in the project: "write me a custom
/// SQL query that totals invoices by month" is answerable only against real
/// column names and real types, and the alternative to shipping them is an agent
/// guessing at a schema it cannot see.
///
/// **Rendered by the driver** ([`DatabaseDriver::render_ddl`](sc_db::DatabaseDriver::render_ddl)),
/// not by a DDL writer of its own: the statement here is the one the database
/// would actually be given, so it cannot drift from what the tables are. Only the
/// app's *declared* tables appear — the same subset every provider is confined to
/// (§13.2) — so the file describes exactly what a query in this project may name,
/// and calculated fields are omitted because they are not columns.
pub fn app_schema_sql(app: &Application, cat: &Catalog) -> Result<String> {
    let driver = cat.primary();
    let mut sql = String::new();
    for table in app_tables(app, cat)? {
        let change = sc_db::SchemaChange::CreateTable {
            name: table.name.clone(),
            columns: table
                .fields
                .iter()
                .filter(|f| !f.is_calc())
                .map(sc_catalog::DataField::to_column_def)
                .collect(),
            primary_key: table.primary_key.clone(),
            unlogged: false,
        };
        sql.push_str(&driver.render_ddl(&change)?);
        sql.push_str(";\n\n");
    }
    Ok(sql)
}

/// The application's whole API surface: every enabled provider's projection,
/// collected into one [`EndpointSet`].
///
/// Two providers may not contribute the same endpoint name — the name keys both
/// the generated client's methods and server dispatch, so a collision is a
/// configuration error (e.g. two REST providers mounted over the same tables).
pub fn app_endpoints(app: &Application, cat: &Catalog) -> Result<EndpointSet> {
    app_endpoints_with(app, cat, None)
}

/// [`app_endpoints`] resolving the app's exposed triggers against the live set,
/// so the endpoints include what an app's client will be generated to call. The
/// build path passes the server's dispatcher; a caller that has none is only
/// correct for an app that exposes no triggers.
pub fn app_endpoints_with(
    app: &Application,
    cat: &Catalog,
    dispatcher: Option<&Arc<TriggerDispatcher>>,
) -> Result<EndpointSet> {
    let mut set = EndpointSet::new();
    for provider in app_providers_with(app, cat, None, dispatcher)? {
        for endpoint in provider.endpoints().iter() {
            if set.find(&endpoint.name).is_some() {
                return Err(Error::config(format!(
                    "application `{}` has two API endpoints named `{}`; \
                     each provider must project distinct operation names",
                    app.name, endpoint.name
                )));
            }
            set.register(endpoint.clone());
        }
        // The tables behind those endpoints travel with them (§13.1), so the
        // generated client can type a row rather than call it `unknown`. A table
        // two providers both project is described once: the description is of
        // the *table*, and the first projection to give one is as good as the
        // second — the endpoints it names are its own either way.
        for resource in provider.endpoints().resources() {
            if set.resource(&resource.name).is_none() {
                set.register_resource(resource.clone());
            }
        }
    }
    Ok(set)
}

/// Generate the application's TypeScript API-consumer client from its endpoint
/// set — the same [`generate_client`] machinery the admin API uses (§13.1).
pub fn app_client(app: &Application, cat: &Catalog) -> Result<String> {
    app_client_with(app, cat, None)
}

/// [`app_client`] over [`app_endpoints_with`] — the form the build path uses, so
/// an app that exposes a trigger gets a typed `runFoo(body)` in its client.
pub fn app_client_with(
    app: &Application,
    cat: &Catalog,
    dispatcher: Option<&Arc<TriggerDispatcher>>,
) -> Result<String> {
    Ok(generate_client(&app_endpoints_with(app, cat, dispatcher)?))
}

#[cfg(test)]
mod mount_tests {
    use super::*;
    use sc_catalog::FileStoreId;

    use crate::application::{ApiConfig, FrameworkRef, StaticDir};
    use crate::framework::{CODE_FRAMEWORK, framework_serves_ui};
    use crate::react::REACT_FRAMEWORK;

    fn app_with_mount(framework: &str, mount: &str) -> Application {
        Application::new("myapp", "myapp", FrameworkRef::new(framework))
            .with_api(ApiConfig::new(sc_api::REST_PROVIDER, mount))
    }

    #[test]
    fn an_api_at_the_root_is_refused_when_the_framework_serves_a_ui() {
        // The failure this prevents: the provider claims `/`, so the app's own
        // pages are answered by an API that has no endpoint there.
        let err = validate_api_mounts(&app_with_mount(REACT_FRAMEWORK, "/")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("claims every path"), "{msg}");
        assert!(msg.contains("/api"), "{msg}");
    }

    #[test]
    fn a_sub_path_mount_is_accepted() {
        validate_api_mounts(&app_with_mount(REACT_FRAMEWORK, "/api")).unwrap();
        validate_api_mounts(&app_with_mount(CODE_FRAMEWORK, "/v1/api")).unwrap();
    }

    #[test]
    fn an_app_whose_framework_serves_no_ui_may_claim_the_root() {
        // The rule is the framework's to answer: with no UI to lose, `/` is the
        // right mount for an API-only app.
        check_api_mounts(&app_with_mount("headless", "/"), false).unwrap();
    }

    #[test]
    fn every_registered_framework_serves_a_ui_and_an_unknown_one_is_assumed_to() {
        for info in crate::framework::registered_framework_info() {
            assert!(info.serves_ui, "{} should serve a UI", info.name);
            assert!(framework_serves_ui(&info.name));
        }
        // Unknown: assume there is a UI to protect — the safe direction.
        assert!(framework_serves_ui("something-else"));
    }

    /// REST at `/api` and GraphQL at `/graphql` on one application — the
    /// arrangement this milestone exists to make possible, and the only mount
    /// check it has to pass.
    #[test]
    fn rest_and_graphql_coexist_on_separate_sub_paths() {
        let app = Application::new("myapp", "myapp", FrameworkRef::new(REACT_FRAMEWORK))
            .with_api(ApiConfig::new(REST_PROVIDER, "/api"))
            .with_api(ApiConfig::new(GRAPHQL_PROVIDER, "/graphql"));
        validate_api_mounts(&app).unwrap();
    }

    #[test]
    fn two_providers_on_one_mount_are_refused_naming_both() {
        // Not a stylistic objection: the router resolves a path to one provider,
        // so the loser is a whole API that is mounted and unreachable.
        let app = Application::new("myapp", "myapp", FrameworkRef::new(REACT_FRAMEWORK))
            .with_api(ApiConfig::new(REST_PROVIDER, "/api"))
            .with_api(ApiConfig::new(GRAPHQL_PROVIDER, "/api"));
        let msg = validate_api_mounts(&app).unwrap_err().to_string();
        assert!(msg.contains(REST_PROVIDER), "{msg}");
        assert!(msg.contains(GRAPHQL_PROVIDER), "{msg}");
        assert!(msg.contains("/api"), "{msg}");
    }

    #[test]
    fn a_colliding_mount_is_refused_even_for_an_api_only_app() {
        // The `/` rule is the framework's to waive; this one is not — an
        // unreachable API is unreachable whatever the framework serves.
        let app = Application::new("myapp", "myapp", FrameworkRef::new("headless"))
            .with_api(ApiConfig::new(REST_PROVIDER, "/"))
            .with_api(ApiConfig::new(GRAPHQL_PROVIDER, "/"));
        assert!(check_api_mounts(&app, false).is_err());
    }

    /// A static directory whose store is outside the app's declared subset is
    /// refused, naming both. The subset is the whole truth about which stores an
    /// application reaches, and a typed store name was the one way round it.
    #[test]
    fn a_static_dir_outside_the_store_subset_is_refused() {
        let app = Application::new("myapp", "myapp", FrameworkRef::new(REACT_FRAMEWORK))
            .with_file_store(FileStoreId("Assets".to_owned()))
            .with_static_dir(StaticDir::new(
                "/img",
                FileStoreId("Asets".to_owned()),
                "media",
            ));
        let msg = validate_static_dirs(&app).unwrap_err().to_string();
        assert!(msg.contains("Asets"), "{msg}");
        assert!(msg.contains("myapp"), "{msg}");
    }

    #[test]
    fn a_static_dir_in_the_subset_is_accepted() {
        let app = Application::new("myapp", "myapp", FrameworkRef::new(REACT_FRAMEWORK))
            .with_file_store(FileStoreId("Assets".to_owned()))
            .with_api(ApiConfig::new(REST_PROVIDER, "/api"))
            .with_static_dir(StaticDir::new(
                "/img",
                FileStoreId("Assets".to_owned()),
                "media",
            ));
        validate_static_dirs(&app).unwrap();
    }

    /// A directory under an API's mount is never served — the request resolves
    /// to the provider first — so it is refused at save time rather than found
    /// as a 404 from an API that has no endpoint there.
    #[test]
    fn a_static_dir_under_an_api_mount_is_refused() {
        let app = Application::new("myapp", "myapp", FrameworkRef::new(REACT_FRAMEWORK))
            .with_file_store(FileStoreId("Assets".to_owned()))
            .with_api(ApiConfig::new(REST_PROVIDER, "/api"))
            .with_static_dir(StaticDir::new(
                "/api/img",
                FileStoreId("Assets".to_owned()),
                "media",
            ));
        let msg = validate_static_dirs(&app).unwrap_err().to_string();
        assert!(msg.contains("/api/img"), "{msg}");
        assert!(msg.contains(REST_PROVIDER), "{msg}");

        // The mount itself, not only what is below it.
        let same = Application::new("myapp", "myapp", FrameworkRef::new(REACT_FRAMEWORK))
            .with_file_store(FileStoreId("Assets".to_owned()))
            .with_api(ApiConfig::new(REST_PROVIDER, "/api"))
            .with_static_dir(StaticDir::new("/api", FileStoreId("Assets".to_owned()), ""));
        assert!(validate_static_dirs(&same).is_err());

        // …and an API-only app's root mount claims every directory there is.
        let rooted = Application::new("myapp", "myapp", FrameworkRef::new("headless"))
            .with_file_store(FileStoreId("Assets".to_owned()))
            .with_api(ApiConfig::new(REST_PROVIDER, "/"))
            .with_static_dir(StaticDir::new("/img", FileStoreId("Assets".to_owned()), ""));
        assert!(validate_static_dirs(&rooted).is_err());
    }

    /// Every offered provider is a provider that mounts. The list drives the
    /// admin's select, so a name in it that `app_providers_with` does not know
    /// would be a configuration error an admin was *invited* to make.
    #[test]
    fn every_offered_provider_name_is_one_the_mount_path_switches_on() {
        let names: Vec<String> = registered_api_provider_info()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert!(names.contains(&REST_PROVIDER.to_owned()), "{names:?}");
        assert!(names.contains(&GRAPHQL_PROVIDER.to_owned()), "{names:?}");
        for info in registered_api_provider_info() {
            assert!(
                matches!(info.name.as_str(), REST_PROVIDER | GRAPHQL_PROVIDER),
                "`{}` is offered but `app_providers_with` does not build it",
                info.name
            );
            assert!(info.default_mount.starts_with('/'), "{info:?}");
            assert!(
                !info.label.is_empty() && !info.description.is_empty(),
                "{info:?}"
            );
        }
        // And the error an unknown name gets names what is available.
        let listed = registered_provider_names();
        assert!(
            listed.contains(REST_PROVIDER) && listed.contains(GRAPHQL_PROVIDER),
            "{listed}"
        );
    }

    #[test]
    fn the_api_holding_a_custom_query_is_chosen_by_mount_or_by_there_being_one() {
        let mut app = Application::new("Blog", "blog", FrameworkRef::new(REACT_FRAMEWORK))
            .with_api(ApiConfig::new("rest", "/api"))
            .with_api(ApiConfig::new("graphql", "/graphql"));

        // Nothing named: the REST one, because it is the only one that serves
        // custom SQL queries.
        assert_eq!(select_api(&mut app, None, "--api").unwrap().mount, "/api");
        assert_eq!(
            select_api(&mut app, Some("/api"), "--api").unwrap().mount,
            "/api"
        );

        // Naming one that cannot hold a query is refused *here*, where the
        // mistake was made — not two steps later as an unknown setting.
        let msg = select_api(&mut app, Some("/graphql"), "--api")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("does not serve custom SQL"), "{msg}");
        assert!(msg.contains("/api"), "{msg}");

        // A mount the app does not have names the ones it does.
        let msg = select_api(&mut app, Some("/v2"), "--api")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("/api") && msg.contains("/graphql"), "{msg}");

        // Two that serve them is ambiguous, and picking one would be picking
        // which client method appears where. The refusal names how *this*
        // caller's user says which, which is the whole reason it is a parameter.
        app.apis.push(ApiConfig::new("rest", "/api2"));
        let msg = select_api(&mut app, None, "--api").unwrap_err().to_string();
        assert!(msg.contains("--api"), "{msg}");
        let msg = select_api(&mut app, None, "`application`")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("`application`"), "{msg}");

        // …and an app with none says so rather than inventing a place to put it.
        let mut none = Application::new("Blog", "blog", FrameworkRef::new(REACT_FRAMEWORK));
        assert!(select_api(&mut none, None, "--api").is_err());
    }
}
