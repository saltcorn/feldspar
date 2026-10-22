//! The reified endpoint model (design §13.1).
//!
//! Endpoints are Rust **values**, not just handler functions: an [`Endpoint`]
//! records its HTTP method, its path (literal segments plus typed parameters),
//! a [`TypeSchema`] for the request body and the response value, an
//! [`AuthRequirement`], and a [`HandlerRef`] naming the code that runs it. This
//! is what lets a route be fully described (and typed for consumers) even when
//! it is registered at runtime and is not known at compile time.
//!
//! An [`EndpointSet`] is the runtime registry those values live in. The admin
//! API (see [`crate::admin`]) is a fixed set of `Endpoint` constants registered
//! through this **same** machinery, so the admin SPA consumes a generated typed
//! client exactly as an application would.

use crate::mcp::{Area, Grant};
use crate::resource::ResourceModel;
use crate::schema::{TypeSchema, ValueType};
use serde::{Deserialize, Serialize};

/// An HTTP method. A small self-contained enum so the endpoint model stays free
/// of the web framework — `sc-server` maps these onto axum's `Method`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    /// HTTP `GET`.
    Get,
    /// HTTP `POST`.
    Post,
    /// HTTP `PUT`.
    Put,
    /// HTTP `PATCH`.
    Patch,
    /// HTTP `DELETE`.
    Delete,
}

impl Method {
    /// The uppercase method token (`"GET"`, `"POST"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    /// Whether this method conventionally carries a request body.
    pub fn has_body(self) -> bool {
        matches!(self, Method::Post | Method::Put | Method::Patch)
    }
}

/// One segment of a [`PathSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSegment {
    /// A literal path segment, e.g. `tables`.
    Literal(String),
    /// A typed path parameter, e.g. `{id}` typed as a UUID.
    Param {
        /// The parameter name (the `{name}` placeholder).
        name: String,
        /// The parameter's scalar type — used to type the generated client.
        ty: ValueType,
    },
}

/// A request path: an ordered list of literal segments and typed parameters,
/// e.g. `/tables/{table}/rows` where `table` is typed. Built fluently with
/// [`PathSpec::root`] + [`lit`](PathSpec::lit) + [`param`](PathSpec::param).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathSpec {
    /// The ordered segments.
    pub segments: Vec<PathSegment>,
}

impl PathSpec {
    /// An empty path (`/`).
    pub fn root() -> PathSpec {
        PathSpec {
            segments: Vec::new(),
        }
    }

    /// Append one or more literal segments. A `/`-containing string is split, so
    /// `.lit("api/tables")` adds two segments.
    pub fn lit(mut self, s: &str) -> PathSpec {
        for part in s.split('/').filter(|p| !p.is_empty()) {
            self.segments.push(PathSegment::Literal(part.to_owned()));
        }
        self
    }

    /// Append a typed path parameter.
    pub fn param(mut self, name: &str, ty: ValueType) -> PathSpec {
        self.segments.push(PathSegment::Param {
            name: name.to_owned(),
            ty,
        });
        self
    }

    /// The typed parameters, in path order.
    pub fn params(&self) -> impl Iterator<Item = (&str, ValueType)> {
        self.segments.iter().filter_map(|seg| match seg {
            PathSegment::Param { name, ty } => Some((name.as_str(), *ty)),
            PathSegment::Literal(_) => None,
        })
    }

    /// Match a concrete request path against this spec, returning the captured
    /// parameters (by name) when it matches.
    ///
    /// Literal segments must match exactly and parameters capture one segment
    /// each, so the arity is fixed: `/posts/1` matches `/posts/{id}` but
    /// `/posts/1/comments` does not. `sc-server` routes the admin API through
    /// `matchit` instead; this exists for an [`ApiProvider`](crate::ApiProvider),
    /// which owns its own dispatch and would otherwise have to re-derive path
    /// matching from `segments`.
    pub fn match_path(&self, path: &str) -> Option<std::collections::HashMap<String, String>> {
        let actual: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if actual.len() != self.segments.len() {
            return None;
        }
        let mut params = std::collections::HashMap::new();
        for (seg, got) in self.segments.iter().zip(actual) {
            match seg {
                PathSegment::Literal(want) if want == got => {}
                PathSegment::Literal(_) => return None,
                // An empty capture would make `/posts//` match `/posts/{id}`
                // with an empty id; the filter above already drops empties, so
                // reaching here with one is impossible, but a param never
                // legitimately captures nothing.
                PathSegment::Param { .. } if got.is_empty() => return None,
                PathSegment::Param { name, .. } => {
                    params.insert(name.clone(), got.to_owned());
                }
            }
        }
        Some(params)
    }

    /// Render the `matchit`/axum route pattern, e.g. `/tables/{id}/rows`.
    pub fn pattern(&self) -> String {
        let mut out = String::from("/");
        for (i, seg) in self.segments.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            match seg {
                PathSegment::Literal(s) => out.push_str(s),
                PathSegment::Param { name, .. } => {
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                }
            }
        }
        out
    }
}

/// The authorization required to call an endpoint, enforced by the server via
/// the roles of §7. Roles run `1..=100` with **lower = more privilege**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthRequirement {
    /// Open to anyone, authenticated or not.
    Public,
    /// Any authenticated user.
    LoggedIn,
    /// A caller whose role is at least as privileged as `min_role` (i.e. a role
    /// number `<= min_role`). A caller nobody is logged in as holds the public
    /// role, so `MinRole(100)` admits everybody — see [`admits`](Self::admits).
    MinRole(u8),
}

impl AuthRequirement {
    /// Requires the administrator role (role `1`).
    pub fn admin() -> AuthRequirement {
        AuthRequirement::MinRole(sc_auth::ROLE_ADMIN)
    }

    /// Whether `user` — `None` for a caller nobody is logged in as — passes.
    ///
    /// The one statement of the rule every surface enforces. An anonymous
    /// caller is not refused a role floor for being anonymous: they hold
    /// [`ROLE_PUBLIC`](sc_auth::ROLE_PUBLIC), so they pass a floor at the
    /// public role and fail every other one.
    pub fn admits(&self, user: Option<&sc_auth::User>) -> bool {
        match self {
            AuthRequirement::Public => true,
            AuthRequirement::LoggedIn => user.is_some(),
            AuthRequirement::MinRole(min) => user.map_or(sc_auth::ROLE_PUBLIC, |u| u.role) <= *min,
        }
    }

    /// Whether this is a role floor the administrator set at the **public**
    /// role: data or an action opened to anybody at all.
    ///
    /// Such an endpoint needs no session, and so no CSRF token either — a
    /// request without one is served as the anonymous caller it then is
    /// (`sc-server`'s CSRF middleware). [`Public`](Self::Public) is *not* this:
    /// it marks the auth plumbing (`login`, `signup`, …) and the endpoints whose
    /// handler decides for itself who the caller is, and those keep the check.
    pub fn is_public_role(&self) -> bool {
        matches!(self, AuthRequirement::MinRole(min) if *min >= sc_auth::ROLE_PUBLIC)
    }
}

/// A reference to the code that runs an endpoint. The endpoint model itself is
/// framework-agnostic, so it names the handler rather than holding it; the
/// server resolves [`Named`](HandlerRef::Named) handlers through its dispatch
/// table, and custom application routes carry guest code or SQL directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandlerRef {
    /// A built-in Rust handler, resolved by name at mount time.
    Named(String),
    /// Custom developer-authored guest code (language tag + source). Stubbed for
    /// the MVP; carried so the model is complete (design §13.4).
    GuestCode {
        /// The language the source is written in.
        language: String,
        /// The source code.
        source: String,
    },
    /// An application's custom query (design §13.4) — SQL, JavaScript or
    /// Python — by name. The provider that projected it holds the definition.
    Custom(String),
}

impl HandlerRef {
    /// A named built-in handler.
    pub fn named(name: impl Into<String>) -> HandlerRef {
        HandlerRef::Named(name.into())
    }
}

/// One query-string parameter an endpoint accepts (design §13.1).
///
/// Query parameters belong in the endpoint *value* for the same reason path
/// parameters do: a consumer generated from the endpoint set has to be able to
/// express `?select=…` or a custom SQL query's arguments, and an untyped `fetch`
/// written by hand beside a generated client is exactly where drift starts.
///
/// [`repeated`](QueryParam::repeated) is the one that carries a filter
/// vocabulary: `?published=gte.2020-01-01&published=lt.2024-01-01` is two values
/// under one key, both of which mean something. That is also why
/// [`ApiRequest::query`](crate::ApiRequest) is an ordered list of pairs rather
/// than a map — a map keeps whichever arrived last and drops the rest, which for
/// a filter is rows the caller did not ask for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryParam {
    /// The parameter name — the query-string key, and the property name in the
    /// generated client's options object.
    pub name: String,
    /// The parameter's scalar type. A repeated parameter is an array *of* this.
    pub ty: ValueType,
    /// The caller must supply it. An endpoint all of whose query parameters are
    /// optional takes an optional options argument.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// The key may appear more than once, and every occurrence counts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repeated: bool,
    /// The caller supplies **keys of their own**: this is not one query-string
    /// key but a map of them, each entry becoming its own pair.
    ///
    /// What a REST filter vocabulary is (§13.4): `?published=gte.2020-01-01` is
    /// keyed by *column*, so no fixed parameter name can describe it. The
    /// parameter's [`name`](QueryParam::name) is then the property name in the
    /// generated client's options object rather than a wire key, and its
    /// [`ty`](QueryParam::ty) is the type of each value.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub map: bool,
}

impl QueryParam {
    /// An optional, single-valued parameter. Refine it with
    /// [`required`](QueryParam::required) and [`repeated`](QueryParam::repeated).
    pub fn new(name: impl Into<String>, ty: ValueType) -> QueryParam {
        QueryParam {
            name: name.into(),
            ty,
            required: false,
            repeated: false,
            map: false,
        }
    }

    /// Mark the parameter as required.
    pub fn required(mut self) -> QueryParam {
        self.required = true;
        self
    }

    /// Mark the parameter as repeatable (many values under one key).
    pub fn repeated(mut self) -> QueryParam {
        self.repeated = true;
        self
    }

    /// Mark the parameter as a map of caller-chosen keys (see
    /// [`map`](QueryParam::map)).
    pub fn map(mut self) -> QueryParam {
        self.map = true;
        self
    }
}

/// The opt-in tag that projects an endpoint as an MCP tool (design §13.6).
///
/// **Opt-in, and that is the whole design of it.** The admin `EndpointSet` is
/// upwards of a hundred endpoints and a coding agent pays for every tool in its
/// context on every turn, so the projection walks only the tagged ones. A tag is
/// therefore a decision about somebody's context window and belongs beside the
/// endpoint it is a decision about, argued for in the comment above it.
///
/// Everything else a tool needs is already in the [`Endpoint`]: the name, the
/// typed path and query parameters, and both `TypeSchema`s. What only a person
/// can supply is the three things here — the prose the model reads when it
/// chooses, and the two answers to *may this caller?*, which no signature
/// carries:
///
/// - the [`area`](McpTag::area), the half of the surface this belongs to, so an
///   `allow_triggers` that is off takes `listTriggers` out of the listing
///   exactly as it takes `save_trigger` out; and
/// - the [`grant`](McpTag::grant) it needs, so `deleteAgent` is refused by the
///   same `allow_drop` that refuses `delete_trigger`. Reading the grant off the
///   HTTP method instead would call `buildApplication` a create, which creates
///   nothing.
///
/// A tag built from a bare string — `.mcp("…")` — is a **read**: no area, no
/// grant, offered to every token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTag {
    /// What this tool does, in the words the model is given.
    pub description: String,
    /// The half of the administrative surface this belongs to, if it belongs to
    /// one — read exactly as a hand-written tool's
    /// [`AdminTool::area`](crate::mcp::AdminTool::area) is. An area that is off
    /// must mean the same thing whichever tier the tool came from, or the
    /// checkbox means two things.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<Area>,
    /// The grant this tool needs, or `None` for one that only reads.
    ///
    /// A read needs none: every one of these endpoints is already `admin()`, and
    /// the four grants are about what a caller may *change*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
    /// A second grant it needs as well, checked after [`grant`](McpTag::grant)
    /// so a refusal names the first one missing — `deleteUser` is an access
    /// change *and* a deletion, and needs both flags (TODO F1.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub also: Option<Grant>,
    /// This endpoint runs build tools, so its **failure is news about the
    /// application** rather than a refusal of the call: the projection reports it
    /// as a tool result with the tools' output and the file/line/message
    /// diagnostics parsed out of it.
    ///
    /// The same decision `sc_core_traits::build_application` already made, for
    /// the same reason — a model told only "the build failed" cannot fix
    /// anything, and a failed build is the most useful answer this tool ever
    /// returns. It is declared here rather than inferred because only a person
    /// knows whether an endpoint's error is *about the thing* or *about the
    /// request*: `deleteAgent` failing is the latter, and turning that into a
    /// cheerful result would hide it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub build_result: bool,
}

impl McpTag {
    /// A tool that only reads, described by `prose`.
    pub fn new(prose: impl Into<String>) -> McpTag {
        McpTag {
            description: prose.into(),
            area: None,
            grant: None,
            also: None,
            build_result: false,
        }
    }

    /// Put it in one half of the surface, so the area checkbox governs it.
    pub fn in_area(mut self, area: Area) -> McpTag {
        self.area = Some(area);
        self
    }

    /// Require a grant of the caller before it runs.
    pub fn needs(mut self, grant: Grant) -> McpTag {
        self.grant = Some(grant);
        self
    }

    /// Require a second grant as well as the one [`needs`](McpTag::needs) named.
    pub fn and_needs(mut self, grant: Grant) -> McpTag {
        self.also = Some(grant);
        self
    }

    /// Report this endpoint's failure as a build result rather than as a refusal
    /// (see [`build_result`](McpTag::build_result)).
    pub fn is_a_build(mut self) -> McpTag {
        self.build_result = true;
        self
    }
}

impl From<&str> for McpTag {
    fn from(prose: &str) -> McpTag {
        McpTag::new(prose)
    }
}

impl From<String> for McpTag {
    fn from(prose: String) -> McpTag {
        McpTag::new(prose)
    }
}

/// A single HTTP endpoint, described as a value (design §13.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    /// A unique operation name; also the generated TypeScript client method name.
    pub name: String,
    /// The HTTP method.
    pub method: Method,
    /// The request path (literal segments + typed parameters).
    pub path: PathSpec,
    /// The query-string parameters the endpoint accepts, in declaration order —
    /// which is the order they appear in the generated client's options object.
    /// Empty for an endpoint that takes none, and then the generated method is
    /// exactly as it was before query parameters existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<QueryParam>,
    /// The request body schema (an empty struct means "no body").
    pub input: TypeSchema,
    /// The response value schema.
    pub output: TypeSchema,
    /// The request body is raw bytes — a file upload — rather than the JSON
    /// `input` describes (§4). The generated client takes a `BodyInit` and sends
    /// it unencoded; the transport hands the provider the unparsed body.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary_input: bool,
    /// The response body is raw bytes — a file download — rather than the JSON
    /// `output` describes (§4). The generated client resolves to a `Blob`; the
    /// transport writes the provider's bytes with their own content type.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary_output: bool,
    /// The endpoint is a code query that opens a live subscription (TODO F7)
    /// rather than answering over HTTP. The generated client opens it over the
    /// application's live socket; an HTTP call is a `400`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream_output: bool,
    /// The arguments object is **open**: it carries the declared fields (the
    /// `input` struct, or the `query` parameters) *and any other key*. A
    /// JavaScript or Python custom query reads whatever arrives, so the
    /// generated method always takes the argument — typed as the declared
    /// fields `& Record<string, unknown>`, optional when none is required —
    /// rather than dropping a body nobody declared.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_args: bool,
    /// The authorization required to call it.
    pub auth: AuthRequirement,
    /// The handler that runs it.
    pub handler: HandlerRef,
    /// Projected as an MCP tool when tagged; absent for the great majority
    /// (see [`McpTag`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpTag>,
}

impl Endpoint {
    /// Start a new endpoint. Defaults: empty input/output, [`LoggedIn`](AuthRequirement::LoggedIn)
    /// auth, and a [`Named`](HandlerRef::Named) handler equal to `name`. Override
    /// with the builder setters.
    pub fn new(name: impl Into<String>, method: Method, path: PathSpec) -> Endpoint {
        let name = name.into();
        let handler = HandlerRef::Named(name.clone());
        Endpoint {
            name,
            method,
            path,
            query: Vec::new(),
            input: TypeSchema::empty(),
            output: TypeSchema::empty(),
            binary_input: false,
            binary_output: false,
            stream_output: false,
            open_args: false,
            auth: AuthRequirement::LoggedIn,
            handler,
            mcp: None,
        }
    }

    /// Declare the query-string parameters the endpoint accepts, appending to
    /// any already declared.
    pub fn query(mut self, params: impl IntoIterator<Item = QueryParam>) -> Endpoint {
        self.query.extend(params);
        self
    }

    /// Set the request body schema.
    pub fn input(mut self, schema: TypeSchema) -> Endpoint {
        self.input = schema;
        self
    }

    /// Set the response value schema.
    pub fn output(mut self, schema: TypeSchema) -> Endpoint {
        self.output = schema;
        self
    }

    /// Set the authorization requirement.
    pub fn auth(mut self, auth: AuthRequirement) -> Endpoint {
        self.auth = auth;
        self
    }

    /// Mark the request body as raw bytes (a file upload).
    pub fn binary_input(mut self) -> Endpoint {
        self.binary_input = true;
        self
    }

    /// Mark the response body as raw bytes (a file download).
    pub fn binary_output(mut self) -> Endpoint {
        self.binary_output = true;
        self
    }

    /// Mark the endpoint as opening a live subscription (see
    /// [`Endpoint::stream_output`]).
    pub fn stream_output(mut self) -> Endpoint {
        self.stream_output = true;
        self
    }

    /// Mark the arguments object as open (see [`Endpoint::open_args`]).
    pub fn open_args(mut self) -> Endpoint {
        self.open_args = true;
        self
    }

    /// Set the handler reference (defaults to `Named(name)`).
    pub fn handler(mut self, handler: HandlerRef) -> Endpoint {
        self.handler = handler;
        self
    }

    /// Offer this endpoint to the administration MCP server as a tool
    /// (design §13.6).
    ///
    /// A bare string is a **read**, offered to every token and needing no grant:
    /// `.mcp("List the …")`. Anything that changes something says so —
    /// `.mcp(McpTag::new("…").in_area(Area::Triggers).needs(Grant::Drop))` —
    /// because the area and the grant are the two questions the signature cannot
    /// answer (see [`McpTag`]).
    ///
    /// The prose is what a model has to go on when choosing, so it is a sentence
    /// about what the tool *does* rather than a label — the same contract
    /// [`ToolSpec::description`](sc_llm::ToolSpec) states.
    pub fn mcp(mut self, tag: impl Into<McpTag>) -> Endpoint {
        self.mcp = Some(tag.into());
        self
    }
}

/// A runtime registry of [`Endpoint`] values.
///
/// The set is a plain runtime value: application and custom routes are
/// registered here at runtime (they are not known at compile time), and the
/// admin API is built as fixed constants and registered through the *same*
/// machinery. `sc-server` mounts a set as JSON routes and the TypeScript
/// generator emits a typed client from it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EndpointSet {
    endpoints: Vec<Endpoint>,
    /// The tables behind those endpoints, when the projection knows of any
    /// (design §13.1). See [`crate::resource`]: this is what lets a generated
    /// client type a row rather than call it `unknown`, and it is carried here
    /// so every consumer of a set — the server, the client generator, a stored
    /// application — sees the same contract.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resources: Vec<ResourceModel>,
}

impl EndpointSet {
    /// An empty set.
    pub fn new() -> EndpointSet {
        EndpointSet {
            endpoints: Vec::new(),
            resources: Vec::new(),
        }
    }

    /// Register an endpoint, returning `&mut self` for chaining. Panics on a
    /// duplicate `name`, since names must be unique to key the client methods
    /// and the server dispatch table.
    pub fn register(&mut self, endpoint: Endpoint) -> &mut EndpointSet {
        assert!(
            !self.endpoints.iter().any(|e| e.name == endpoint.name),
            "duplicate endpoint name `{}`",
            endpoint.name
        );
        self.endpoints.push(endpoint);
        self
    }

    /// Register an endpoint, consuming and returning the set (builder style).
    pub fn with(mut self, endpoint: Endpoint) -> EndpointSet {
        self.register(endpoint);
        self
    }

    /// Iterate the registered endpoints in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &Endpoint> {
        self.endpoints.iter()
    }

    /// The number of registered endpoints.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Find an endpoint by name.
    pub fn find(&self, name: &str) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| e.name == name)
    }

    /// Find an endpoint by name, to refine its schemas after projection.
    pub(crate) fn find_mut(&mut self, name: &str) -> Option<&mut Endpoint> {
        self.endpoints.iter_mut().find(|e| e.name == name)
    }

    /// Record the table behind a group of endpoints already registered here
    /// (see [`ResourceModel`]).
    ///
    /// Panics on a duplicate `name`, and on naming an endpoint the set does not
    /// have — a model that points at nothing would generate a client method
    /// calling an endpoint the server never mounted, which is precisely the
    /// drift the generated client exists to prevent.
    pub fn register_resource(&mut self, resource: ResourceModel) -> &mut EndpointSet {
        assert!(
            !self.resources.iter().any(|r| r.name == resource.name),
            "duplicate resource `{}`",
            resource.name
        );
        for op in resource.endpoint_names() {
            assert!(
                self.endpoints.iter().any(|e| e.name == op),
                "resource `{}` names endpoint `{op}`, which is not registered",
                resource.name
            );
        }
        self.resources.push(resource);
        self
    }

    /// The tables behind these endpoints, in registration order.
    pub fn resources(&self) -> impl Iterator<Item = &ResourceModel> {
        self.resources.iter()
    }

    /// The resource named `name`, if there is one.
    pub fn resource(&self, name: &str) -> Option<&ResourceModel> {
        self.resources.iter().find(|r| r.name == name)
    }
}

impl<'a> IntoIterator for &'a EndpointSet {
    type Item = &'a Endpoint;
    type IntoIter = std::slice::Iter<'a, Endpoint>;

    fn into_iter(self) -> Self::IntoIter {
        self.endpoints.iter()
    }
}

impl FromIterator<Endpoint> for EndpointSet {
    fn from_iter<I: IntoIterator<Item = Endpoint>>(iter: I) -> EndpointSet {
        let mut set = EndpointSet::new();
        for e in iter {
            set.register(e);
        }
        set
    }
}
