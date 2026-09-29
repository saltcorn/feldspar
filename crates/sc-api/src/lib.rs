//! Endpoint model (typed Rust values) + API providers + TypeScript consumer
//! generation (layer 8; technical design §13.1).
//!
//! This crate reifies HTTP endpoints as **data**: an [`Endpoint`] records its
//! method, typed path, request/response [`TypeSchema`], auth requirement, and a
//! handler reference — described well enough to be dispatched by the server and
//! typed for a consumer even when registered at runtime. Endpoints live in an
//! [`EndpointSet`] runtime registry; the admin API ([`admin::admin_endpoints`])
//! is a fixed set built through the *same* machinery. [`generate_client`] emits a
//! type-checked TypeScript client from any set, so the server contract and its
//! consumers cannot drift.
//!
//! [`ApiProvider`] (design §13.4) is the other half: an application enables any
//! number of providers — REST, GraphQL, gRPC, tRPC, MCP — each mounted on a
//! sub-path and each *projecting* the shared endpoint set into its protocol.
//! [`RestProvider`] is the MVP's one provider. Because a provider's projection is
//! an ordinary [`EndpointSet`], an application's typed client comes from the same
//! [`generate_client`] the admin API uses.
//!
//! [`rows`] holds the table row CRUD both the admin API's handlers and a
//! provider run, [`filter`] the comparison vocabulary every filtering syntax
//! lowers through, [`schema_edit`] the schema-changing rule the admin handlers and
//! an agent's `admin_copilot` trait both go through (§3.3, §11.3), [`auth`] the login vocabulary they share, and [`convert`] the
//! bridge from JSON to the query layer's `Value` — all kept here, below every API
//! surface, so there is one implementation rather than one per protocol.
//!
//! [`mcp`] is the same argument applied one level up: the **administrative
//! tools** that build an application's configuration half — the schema, the
//! triggers, an application's custom queries — written once here and offered
//! both to the built-in copilot agent and to the administration MCP server
//! (§13.6), rather than implemented once per caller.

pub mod auth;
// The host behind a code body's `db` (§10.1): plans in, rows out.
pub mod code_host;
pub mod convert;
// `csv`, not `bulk`: the module is named for the format it speaks. Inside it
// the crate of the same name is reached as `::csv`.
pub mod csv;
pub mod filter;
// The administrative tool surface (§13.6): one implementation of the tools that
// build an application's configuration half, shared by the built-in copilot
// agent and the administration MCP server.
pub mod mcp;
pub mod metadata_tables;
// A posterior written back into rows: the admin API's `writePosterior` and a
// code body's model handle share this one function.
pub mod models;
pub mod provided_tables;
pub mod query_string;
pub mod rows;
pub mod schema_edit;

mod admin;
mod calc_read;
mod endpoint;
mod graphql;
mod ownership;
mod provider;
mod resource;
mod rest;
mod schema;
mod typescript;
mod user_rows;

pub use admin::{ADMIN_API_PREFIX, ROW_PAGE_CAP, admin_endpoints};
pub use endpoint::{
    AuthRequirement, Endpoint, EndpointSet, HandlerRef, McpTag, Method, PathSegment, PathSpec,
    QueryParam,
};
pub use graphql::{
    CFG_AGGREGATES as GRAPHQL_CFG_AGGREGATES, DEFAULT_FILE_MOUNT as GRAPHQL_DEFAULT_FILE_MOUNT,
    DEFAULT_MAX_COMPLEXITY, DEFAULT_MAX_DEPTH, DEFAULT_MOUNT as GRAPHQL_DEFAULT_MOUNT,
    DEFAULT_ROW_CAP as GRAPHQL_DEFAULT_ROW_CAP, DEFAULT_STATEMENT_BUDGET, GRAPHQL_CLIENT_FILE,
    GRAPHQL_PROVIDER, GRAPHQL_SCHEMA_FILE, GraphqlLimits, GraphqlProvider, SchemaNames,
    generate_graphql_client, graphql_config_spec,
};
pub use ownership::{
    caller_context, caller_context_at, delete_row_as, insert_row_as, read_row_values_as,
    read_rows_as, update_row_as,
};
pub use provider::{
    ApiProvider, ApiRequest, ApiResponse, AppDirectory, AppLinks, RawBody, SessionAction,
};
pub use resource::{ResourceField, ResourceFile, ResourceModel, ResourceOps};
pub use rest::custom::{
    CFG_QUERIES as REST_CFG_QUERIES, CustomParam, CustomQuery, QueryColumn, QueryLanguage, custom_queries,
    describe_custom_query, set_custom_queries, validate_custom_queries,
};
pub use rest::password::{
    CFG_ALLOW_INVITE as REST_CFG_ALLOW_INVITE, CFG_INVITE_MIN_ROLE as REST_CFG_INVITE_MIN_ROLE,
    SET_PASSWORD_PAGE, rest_invite_min_role,
};
pub use rest::{
    AUTH_ENDPOINTS, CFG_ALLOW_SIGNUP as REST_CFG_ALLOW_SIGNUP,
    CFG_NEW_USER_ROLE as REST_CFG_NEW_USER_ROLE, DEFAULT_ROW_CAP as REST_DEFAULT_ROW_CAP,
    REST_PROVIDER, RestProvider, check_rest_config, op_name, rest_config_spec, rest_row_cap,
    rest_signup_role,
};
pub use schema::{StructField, TypeSchema, ValueType};
pub use typescript::{
    CLIENT_HELPER_FILE, StreamExport, client_helper, client_property, generate_client,
    generate_client_with_streams,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_spec_builds_pattern_and_typed_params() {
        let path = PathSpec::root()
            .lit("api/tables")
            .param("table", ValueType::Text)
            .lit("rows")
            .param("id", ValueType::Uuid);
        assert_eq!(path.pattern(), "/api/tables/{table}/rows/{id}");
        let params: Vec<_> = path.params().collect();
        assert_eq!(
            params,
            vec![("table", ValueType::Text), ("id", ValueType::Uuid)]
        );
    }

    #[test]
    fn endpoint_defaults_and_builder() {
        let ep = Endpoint::new("login", Method::Post, PathSpec::root().lit("api/login"));
        // Defaults: empty i/o, logged-in auth, handler named after the endpoint.
        assert!(ep.input.is_empty());
        assert!(ep.output.is_empty());
        assert_eq!(ep.auth, AuthRequirement::LoggedIn);
        assert_eq!(ep.handler, HandlerRef::named("login"));

        let ep = ep.auth(AuthRequirement::Public).output(TypeSchema::text());
        assert_eq!(ep.auth, AuthRequirement::Public);
        assert!(!ep.output.is_empty());
    }

    #[test]
    fn endpoint_set_is_a_runtime_registry() {
        let mut set = EndpointSet::new();
        set.register(Endpoint::new("a", Method::Get, PathSpec::root().lit("a")));
        set.register(Endpoint::new("b", Method::Get, PathSpec::root().lit("b")));
        assert_eq!(set.len(), 2);
        assert!(set.find("a").is_some());
        assert!(set.find("missing").is_none());
        // Iteration preserves registration order.
        let names: Vec<_> = set.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    #[should_panic(expected = "duplicate endpoint name")]
    fn duplicate_endpoint_names_panic() {
        let mut set = EndpointSet::new();
        set.register(Endpoint::new("dup", Method::Get, PathSpec::root().lit("x")));
        set.register(Endpoint::new(
            "dup",
            Method::Post,
            PathSpec::root().lit("y"),
        ));
    }

    #[test]
    fn admin_endpoints_use_the_same_machinery() {
        let set = admin_endpoints();
        // A representative sample of the fixed admin contract.
        assert!(set.find("login").is_some());
        assert!(set.find("createFirstUser").is_some());
        assert!(set.find("listTables").is_some());
        assert!(set.find("createRow").is_some());
        assert!(set.find("listUsers").is_some());

        // Auth is set as the design requires: bootstrap is public, admin routes gated.
        assert_eq!(set.find("login").unwrap().auth, AuthRequirement::Public);
        assert_eq!(
            set.find("listTables").unwrap().auth,
            AuthRequirement::admin()
        );
    }

    #[test]
    fn typeschema_renders_to_typescript() {
        // Optional struct field becomes `?`-optional and `| null`.
        let schema = TypeSchema::struct_of([
            StructField::new("id", TypeSchema::uuid()),
            StructField::new("tags", TypeSchema::array(TypeSchema::text())),
            StructField::new("note", TypeSchema::optional(TypeSchema::text())),
        ]);
        let client = generate_client(&EndpointSet::new().with(
            Endpoint::new("thing", Method::Post, PathSpec::root().lit("thing")).input(schema),
        ));
        assert!(client.contains("id: string"));
        assert!(client.contains("tags: Array<string>"));
        assert!(client.contains("note?: string | null"));
    }

    #[test]
    fn generated_client_has_typed_methods_and_urls() {
        let ts = generate_client(&admin_endpoints());

        // Type declarations for endpoints with a body/response.
        assert!(ts.contains("export type LoginRequest = "));
        assert!(ts.contains("export type LoginResponse = "));

        // The client interface and factory.
        assert!(ts.contains("export interface ApiClient {"));
        assert!(ts.contains("export function createClient("));

        // Path params are typed method args; the URL interpolates them.
        assert!(
            ts.contains(
                "listRows(table: string, query?: ListRowsQuery): Promise<ListRowsResponse>"
            )
        );
        assert!(ts.contains("/api/tables/${table}/rows"));

        // A void endpoint (logout has no response payload).
        assert!(ts.contains("logout(): Promise<void>"));

        // Correct HTTP verbs are emitted.
        assert!(ts.contains("method: \"POST\""));
        assert!(ts.contains("method: \"DELETE\""));
    }
}
