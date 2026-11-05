//! The GraphQL [`ApiProvider`] (design §13.4).
//!
//! REST cannot express a *shape* a caller chooses. "For each department, the
//! number of employees earning below 50 000" is not a route; it is a question,
//! and answering it over REST means either inventing a route per question or
//! fetching every employee and counting in the browser. That is what this
//! provider is for, and it is why the milestone exists.
//!
//! It sits beside [`RestProvider`](crate::RestProvider) rather than replacing
//! it: an application enables both, on different mounts, over the same tables.
//! Two endpoints, because that is genuinely all a GraphQL API is —
//! `POST {mount}` carrying `{query, variables, operationName}`, and
//! `GET {mount}/schema.graphql` serving the SDL — so the provider participates
//! in the shared endpoint model and the TypeScript generation exactly as
//! everything else does.
//!
//! **Not a second data path.** Nothing here builds its own `SELECT`. Reads go
//! through `sc-api`'s row and ownership entry points and writes through the row
//! layer's own, so coercion, rich-type validation, `File`-field checks, table
//! events and §7.3 apply *because they are the same code* — not because this
//! provider remembered to do them.
//!
//! **One schema per application, not per role** (decision 4). Hasura compiles a
//! schema per role; we authorize at resolve time and refuse with a GraphQL error
//! naming the table. The schema describes what the *application* exposes,
//! exactly as its [`EndpointSet`](crate::EndpointSet) does — which is also why
//! both endpoints are [`Public`](crate::AuthRequirement::Public): the gate is
//! the table's, at resolve time, not the endpoint's.
//!
//! The pieces: [`names`] derives every GraphQL name (and reports what it could
//! not name), [`types`] maps a column onto the wire, [`build`] folds the tables
//! into an `async_graphql::dynamic::Schema`, [`args`] lowers a field's
//! `where`/`order_by`/`limit`/`offset` onto the row layer's own `RowQuery`,
//! [`agg`] lowers an aggregate selection onto `sc-expr`'s aggregate builders,
//! [`resolve`] is what the query fields do, [`mutate`] is what the mutation
//! fields do, [`errors`] labels a write's refusal so a form can act on it, and
//! [`limits`] is what one operation is allowed to cost — because the caller
//! writes the query here, which is the one thing REST never let them do.

mod agg;
// The `where`/`order_by` lowering. Crate-visible so the REST query string's
// tests can assert the two syntaxes lower a comparison identically.
pub(crate) mod args;
mod build;
mod client;
mod context;
mod errors;
pub mod limits;
mod loader;
mod mutate;
pub mod names;
mod resolve;
// Fixtures for this module's unit tests, and for `sc_access::filter`'s — the
// comparison vocabulary is shared, so the table it is tested against is too.
#[cfg(test)]
pub(crate) mod testing;
mod types;

use std::sync::Arc;

use async_graphql::dataloader::DataLoader;
use async_graphql::dynamic::Schema;
use async_trait::async_trait;
use sc_auth::User;
use sc_catalog::{Catalog, Table};
use sc_error::Result;
use sc_expr::JsEvaluator;
use serde_json::{Value as Json, json};

use crate::endpoint::{AuthRequirement, Endpoint, EndpointSet, HandlerRef, Method, PathSpec};
use crate::provider::{ApiProvider, ApiRequest, ApiResponse};
use crate::schema::{StructField, TypeSchema};

pub use client::{GRAPHQL_CLIENT_FILE, GRAPHQL_SCHEMA_FILE, generate_graphql_client};
pub use context::DEFAULT_FILE_MOUNT;
pub use limits::{
    CFG_AGGREGATES, DEFAULT_MAX_COMPLEXITY, DEFAULT_MAX_DEPTH, DEFAULT_ROW_CAP,
    DEFAULT_STATEMENT_BUDGET, GraphqlLimits, graphql_config_spec,
};
pub use names::SchemaNames;

/// The provider's registered name.
pub const GRAPHQL_PROVIDER: &str = "graphql";

/// The provider's default mount.
pub const DEFAULT_MOUNT: &str = "/graphql";

/// The endpoint (and generated client method) names. Fixed rather than derived
/// from a table, so they cannot collide with the REST provider's `listPosts` /
/// `login` in an application's combined endpoint set.
const QUERY_ENDPOINT: &str = "graphqlQuery";
const SCHEMA_ENDPOINT: &str = "graphqlSchema";

/// The last path segment the SDL is served at.
const SCHEMA_SEGMENT: &str = "schema.graphql";

/// A GraphQL projection of an application's tables (design §13.4).
pub struct GraphqlProvider {
    mount: String,
    endpoints: EndpointSet,
    schema: Schema,
    /// The derived names the schema was built from, shared with every request:
    /// a resolver reading a selection set has to know which relation a field
    /// name is, and this is where that was decided.
    names: Arc<SchemaNames>,
    diagnostics: Vec<String>,
    /// The engine an untranslatable ownership formula's reified path runs on
    /// (§7.3), injected by the server exactly as it is into the REST provider.
    /// Absent is not "allow": such a read fails closed.
    evaluator: Option<Arc<dyn JsEvaluator>>,
    /// What this application allows one operation to cost — depth, complexity,
    /// rows and statements. Two of the four are compiled into the schema, so a
    /// change to them rebuilds it; see [`GraphqlProvider::with_limits`].
    limits: GraphqlLimits,
    /// The REST mount a `File` field's `url` is built against — the bytes are
    /// served there, and a GraphQL field does not become a second way to them.
    file_mount: String,
}

impl GraphqlProvider {
    /// Project `tables` into a GraphQL API mounted at `mount`.
    ///
    /// The tables are the application's declared subset, already resolved
    /// against the catalog by the caller — the same arrangement
    /// [`RestProvider::project`](crate::RestProvider::project) has, and for the
    /// same reason: this crate sits below `sc-app`.
    ///
    /// A schema that cannot be built is a **mount failure** naming the table
    /// that caused it. An application whose API is half-described is worse than
    /// one that refuses to start: the half nobody notices is the half that is
    /// wrong.
    pub fn project(mount: impl Into<String>, tables: &[Table]) -> Result<GraphqlProvider> {
        GraphqlProvider::project_with(mount, tables, GraphqlLimits::default())
    }

    /// [`project`](Self::project) under an application's own [`GraphqlLimits`].
    ///
    /// The limits are taken here rather than set afterwards because two of them
    /// are properties of the **schema**: `limit_depth` and `limit_complexity`
    /// are `async-graphql` validation rules, applied to a document before any
    /// resolver runs, and a schema is built once at mount. The other two are
    /// per-request and could be set either way; they travel together because
    /// splitting "the limits" across two places is how one of them ends up
    /// configured and the other forgotten.
    pub fn project_with(
        mount: impl Into<String>,
        tables: &[Table],
        limits: GraphqlLimits,
    ) -> Result<GraphqlProvider> {
        let mount = normalize_mount(&mount.into());
        let names = SchemaNames::derive(tables);
        let schema = build::build_schema(tables, &names, limits)?;

        let mut endpoints = EndpointSet::new();
        endpoints.register(
            // `Public` because the *rows* are gated: a table's role floor and
            // its ownership formula decide at resolve time, exactly as they do
            // for a REST table with a formula. An endpoint-level gate here
            // would be a second, coarser rule that disagreed with the first.
            Endpoint::projected(
                QUERY_ENDPOINT,
                Method::Post,
                PathSpec::root().lit(&mount),
                AuthRequirement::Public,
            )
            .input(TypeSchema::struct_of([
                StructField::new("query", TypeSchema::text()),
                StructField::new("variables", TypeSchema::optional(TypeSchema::json())),
                StructField::new("operationName", TypeSchema::optional(TypeSchema::text())),
            ]))
            .output(TypeSchema::json())
            .handler(HandlerRef::named(QUERY_ENDPOINT)),
        );
        endpoints.register(
            // `Public`: introspection is on, so the SDL is not a secret being
            // kept — it describes the tables the application already exposes
            // over REST.
            Endpoint::projected(
                SCHEMA_ENDPOINT,
                Method::Get,
                PathSpec::root().lit(&mount).lit(SCHEMA_SEGMENT),
                AuthRequirement::Public,
            )
            // The SDL is text, not JSON — it is what a build feeds `gql.tada`
            // and what a developer reads.
            .binary_output()
            .handler(HandlerRef::named(SCHEMA_ENDPOINT)),
        );

        Ok(GraphqlProvider {
            mount,
            endpoints,
            schema,
            diagnostics: names.diagnostics().to_vec(),
            names: Arc::new(names),
            evaluator: None,
            limits,
            file_mount: DEFAULT_FILE_MOUNT.to_owned(),
        })
    }

    /// Inject the JavaScript evaluator an untranslatable ownership formula needs
    /// (§7.3) — the *same* engine the REST provider is given, so the two
    /// providers over one application cannot disagree about a row.
    pub fn with_evaluator(mut self, evaluator: Arc<dyn JsEvaluator>) -> GraphqlProvider {
        self.evaluator = Some(evaluator);
        self
    }

    /// Set the row cap: the bound a list field takes when the caller names no
    /// `limit`, and the ceiling one they do name is clamped to.
    ///
    /// Settable after the fact, unlike the depth and complexity bounds, because
    /// it is applied per read rather than compiled into the schema.
    pub fn with_row_cap(mut self, cap: u64) -> GraphqlProvider {
        self.limits.row_cap = cap;
        self
    }

    /// Set the statement budget: the most database statements one operation may
    /// issue. Per request, so likewise settable after the schema is built.
    pub fn with_statement_budget(mut self, statements: usize) -> GraphqlProvider {
        self.limits.statement_budget = statements;
        self
    }

    /// The limits this projection serves under.
    pub fn limits(&self) -> GraphqlLimits {
        self.limits
    }

    /// Point `File` fields' `url` at the application's REST mount. A GraphQL
    /// `File` field is a path and *that* URL; it never serves the bytes itself.
    pub fn with_file_mount(mut self, mount: impl Into<String>) -> GraphqlProvider {
        self.file_mount = normalize_mount(&mount.into());
        self
    }

    /// The schema's SDL — what `GET {mount}/schema.graphql` serves and what the
    /// build writes beside the generated client (Phase 8).
    pub fn sdl(&self) -> String {
        self.schema.sdl()
    }

    /// Everything the projection had to leave out, and why: a table or field
    /// whose name GraphQL cannot spell, or one that would have collided. Not an
    /// error — an application should not lose its whole API to one unnameable
    /// table — but not silent either.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Execute one GraphQL request body on behalf of one caller.
    ///
    /// The catalog and the caller travel as request **data** rather than as
    /// captured state, because the executor's field resolvers are `'static`
    /// closures: they are built once, at mount, and everything about *this*
    /// request has to reach them through here.
    async fn execute(&self, body: &Json, cat: &Arc<Catalog>, user: Option<&User>) -> ApiResponse {
        let Some(query) = body.get("query").and_then(Json::as_str) else {
            // A missing document is not a GraphQL error (there is no document to
            // report it against), so it is an ordinary request error.
            return ApiResponse::error(400, "a GraphQL request needs a `query` field");
        };
        let mut request = async_graphql::Request::new(query);
        if let Some(variables) = body.get("variables").filter(|v| !v.is_null()) {
            request = request.variables(async_graphql::Variables::from_json(variables.clone()));
        }
        if let Some(name) = body.get("operationName").and_then(Json::as_str) {
            request = request.operation_name(name);
        }
        let rc = context::RequestContext {
            catalog: Arc::clone(cat),
            names: Arc::clone(&self.names),
            user: user.cloned(),
            evaluator: self.evaluator.clone(),
            limits: self.limits,
            // One budget per request, because the budget is the operation's:
            // sharing it across requests would let one caller's query refuse
            // another's, and a fresh one per field would bound nothing.
            budget: Arc::new(limits::StatementBudget::new(self.limits.statement_budget)),
            file_mount: self.file_mount.clone(),
        };
        // One loader per request, holding that request's own context: a batched
        // child read is the same read as an unbatched one, by the same caller,
        // and only the shape of the statement differs.
        let loader = DataLoader::new(
            loader::ChildLoader::new(rc.clone()),
            sc_catalog::spawn_scoped,
        );
        let mut response = self.schema.execute(request.data(loader).data(rc)).await;
        limits::name_the_bound(&mut response.errors, self.limits);
        // The legacy `application/json` rule: 200 with the errors in the body.
        // `application/graphql-response+json` needs content negotiation, and
        // `ApiRequest` carries no headers to negotiate with.
        match serde_json::to_value(&response) {
            Ok(json) => ApiResponse::ok(json),
            Err(e) => ApiResponse::error(500, format!("could not serialise the response: {e}")),
        }
    }
}

#[async_trait]
impl ApiProvider for GraphqlProvider {
    fn name(&self) -> &str {
        GRAPHQL_PROVIDER
    }

    fn mount(&self) -> String {
        self.mount.clone()
    }

    fn endpoints(&self) -> &EndpointSet {
        &self.endpoints
    }

    async fn handle(
        &self,
        req: ApiRequest,
        cat: &Arc<Catalog>,
        user: Option<&User>,
    ) -> Result<ApiResponse> {
        Ok(self.route(&req, cat, user).await)
    }
}

impl GraphqlProvider {
    /// Route one request to the SDL or to the executor.
    async fn route(
        &self,
        req: &ApiRequest,
        cat: &Arc<Catalog>,
        user: Option<&User>,
    ) -> ApiResponse {
        let schema_path = format!("{}/{SCHEMA_SEGMENT}", self.mount.trim_end_matches('/'));
        if req.method == Method::Get && req.path == schema_path {
            return ApiResponse::file(self.sdl(), "text/plain; charset=utf-8");
        }
        if req.path != self.mount {
            return ApiResponse::error(404, format!("no GraphQL endpoint at `{}`", req.path));
        }
        if req.method != Method::Post {
            return ApiResponse::with_status(
                405,
                json!({ "error": "a GraphQL request is a POST" }),
            );
        }
        self.execute(&req.body, cat, user).await
    }
}

/// Normalise a mount to a single leading slash and no trailing slash — the same
/// rule the REST provider applies, so two providers' mounts are comparable.
fn normalize_mount(raw: &str) -> String {
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{id_field, plain_field, table_of};

    fn provider() -> GraphqlProvider {
        GraphqlProvider::project(
            DEFAULT_MOUNT,
            &[table_of(
                "departments",
                vec![id_field(), plain_field("name")],
            )],
        )
        .expect("projects")
    }

    #[test]
    fn the_provider_projects_exactly_two_endpoints() {
        let p = provider();
        assert_eq!(p.name(), "graphql");
        assert_eq!(p.mount(), "/graphql");
        assert_eq!(p.endpoints().len(), 2);

        let query = p.endpoints().find("graphqlQuery").expect("query endpoint");
        assert_eq!(query.method, Method::Post);
        assert_eq!(query.path.pattern(), "/graphql");
        assert_eq!(query.auth, AuthRequirement::Public);

        let sdl = p.endpoints().find("graphqlSchema").expect("sdl endpoint");
        assert_eq!(sdl.method, Method::Get);
        assert_eq!(sdl.path.pattern(), "/graphql/schema.graphql");
        assert!(sdl.binary_output);
    }

    #[test]
    fn the_endpoint_names_cannot_collide_with_the_rest_providers() {
        // An application collects every provider's endpoints into one set, and a
        // duplicate name there is a configuration error. The REST projection
        // names its operations after tables and its auth after itself; these two
        // are neither.
        let rest = crate::RestProvider::project(
            "/api",
            &[table_of(
                "departments",
                vec![id_field(), plain_field("name")],
            )],
        );
        let graphql = provider();
        for ep in graphql.endpoints().iter() {
            assert!(
                rest.endpoints().find(&ep.name).is_none(),
                "`{}` collides with the REST projection",
                ep.name
            );
        }
    }

    #[test]
    fn a_mount_is_normalised_the_way_rests_is() {
        let p = GraphqlProvider::project("graphql/", &[table_of("t", vec![id_field()])])
            .expect("projects");
        assert_eq!(p.mount(), "/graphql");
    }

    /// Run `route` on a request, on a current-thread runtime, as an anonymous
    /// caller over a catalog with no tables in it.
    fn route(p: &GraphqlProvider, req: ApiRequest) -> ApiResponse {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let cat = crate::graphql::testing::empty_catalog().await;
                p.route(&req, &cat, None).await
            })
    }

    #[test]
    fn the_sdl_endpoint_serves_the_schema_as_text() {
        let response = route(&provider(), ApiRequest::get("/graphql/schema.graphql"));
        let raw = response.raw.expect("raw body");
        assert_eq!(raw.content_type, "text/plain; charset=utf-8");
        let sdl = String::from_utf8(raw.bytes.to_vec()).expect("utf-8");
        assert!(sdl.contains("type Departments {"), "{sdl}");
    }

    #[test]
    fn a_query_without_a_document_is_a_request_error_not_a_graphql_one() {
        let response = route(
            &provider(),
            ApiRequest::new(Method::Post, "/graphql").body(json!({})),
        );
        assert_eq!(response.status, 400);
    }

    #[test]
    fn a_graphql_error_is_a_200_with_the_errors_in_the_body() {
        // The legacy `application/json` rule, which is what applies until
        // `ApiRequest` carries an `Accept` header to negotiate with.
        let response = route(
            &provider(),
            ApiRequest::new(Method::Post, "/graphql")
                .body(json!({ "query": "{ departments { nope } }" })),
        );
        assert_eq!(response.status, 200);
        let errors = response.body.get("errors").expect("errors in the body");
        assert!(format!("{errors}").contains("nope"), "{}", response.body);
    }

    #[test]
    fn a_read_resolves_against_the_live_catalog_not_the_mounted_schema() {
        // The schema is built from the application's declared tables; the *rows*
        // come from the catalog as it is when the request arrives. Here the
        // catalog has no `departments`, and the read says so rather than
        // answering from something captured at mount time.
        let response = route(
            &provider(),
            ApiRequest::new(Method::Post, "/graphql")
                .body(json!({ "query": "{ departments { id } }" })),
        );
        assert_eq!(response.status, 200);
        let errors = response.body.get("errors").expect("errors in the body");
        assert!(
            format!("{errors}").contains("departments"),
            "{}",
            response.body
        );
        assert!(response.body["data"].is_null(), "{}", response.body);
    }

    #[test]
    fn a_get_on_the_document_endpoint_is_a_405_and_a_stray_path_a_404() {
        let p = provider();
        assert_eq!(route(&p, ApiRequest::get("/graphql")).status, 405);
        assert_eq!(route(&p, ApiRequest::get("/graphql/nope")).status, 404);
    }
}
