//! Tier 2: the tools projected from tagged endpoints (§13.6).
//!
//! A hand-written tool exists because a one-endpoint-one-tool projection is the
//! wrong shape for what it does — `edit_schema` takes an ordered operation list
//! because a schema is a set of *connected* tables. Most endpoints are not like
//! that: `listRuns` is one GET with one path parameter, and writing a tool for
//! it by hand would be transcribing a value this crate already holds.
//!
//! So [`Projection`] is that transcription, done once. An
//! [`Endpoint`] already carries the name, the typed path parameters, the typed
//! query parameters and both `TypeSchema`s; the only thing a person has to add
//! is the prose a model reads, which is what [`McpTag`] is.
//!
//! ## One arguments object out of three places
//!
//! An HTTP request puts its arguments in three places — the path, the query
//! string and the body — and a tool call has one object. So the projection
//! **merges** them and splits them back apart on the way in:
//!
//! | source | in the schema | on the way back |
//! | --- | --- | --- |
//! | path parameter | a required property | a captured path parameter |
//! | query parameter | required iff declared so | a query-string pair (or several) |
//! | body `Struct` field | its own property, required iff not `Optional` | a key of the JSON body |
//!
//! A body that is not a struct — a workflow document, an arbitrary payload —
//! cannot be spread over an object's keys, so it arrives under [`BODY_KEY`]. It
//! is deliberately the *fallback* rather than the rule: a model choosing
//! arguments does better with `label` and `expires_in_days` than with a `body`
//! it has to compose blind.
//!
//! ## Why the tool keeps the endpoint's own name
//!
//! `listRuns`, not `list_runs`, sitting in a list beside `edit_schema`. The
//! mixed casing is real and is the lesser cost: the endpoint name is what the
//! generated TypeScript client calls the method, what the docs call the
//! operation and what the audit line names, so an agent that has the repository
//! *and* this server reads one word for one thing. Renaming here would mint a
//! second name for every projected endpoint and put the translation between them
//! in the model's head.

use std::collections::HashMap;

use sc_error::{Error, Result};
use serde_json::{Map, Value as Json};

use super::json_schema::{json_schema, object_schema, scalar_schema};
use super::{Area, Grant, ToolContext, require_admin, require_grant};
use crate::endpoint::{AuthRequirement, Endpoint, PathSegment, QueryParam};
use crate::provider::ApiRequest;
use crate::schema::{TypeSchema, ValueType};
use crate::schema_edit::Grants;

/// Where a body that is not a struct arrives under.
pub const BODY_KEY: &str = "body";

/// One tagged [`Endpoint`], ready to be listed as a tool and called as a
/// request.
///
/// Borrowed from the set rather than owned, because the projection is rebuilt
/// per caller (the areas a token was granted decide which of them are offered)
/// and the endpoint set outlives every one of them.
#[derive(Debug, Clone)]
pub struct Projection {
    endpoint: Endpoint,
}

/// A tool call rendered back into the request it is (§13.6).
///
/// The path parameters travel **beside** the rendered path rather than only
/// inside it. Re-matching the path would be the tidier-looking answer and it is
/// wrong: a text path parameter may legitimately contain a `/` — `listRuns`
/// takes an agent's *name* — and a path string cannot round-trip one without an
/// encoding both ends would then have to agree about. The rendered
/// [`request`](ProjectedCall::request) is what the call *reads* as, which is
/// what a log line and a provider want; the map is what the dispatcher binds.
#[derive(Debug, Clone)]
pub struct ProjectedCall {
    /// The request this call is: method, path, query and JSON body.
    pub request: ApiRequest,
    /// The captured path parameters, by name.
    pub path_params: HashMap<String, String>,
}

impl Projection {
    /// The projection of a tagged endpoint, or `None` for one that carries no
    /// [`McpTag`](crate::McpTag) — which is nearly all of them, by design.
    pub fn of(endpoint: &Endpoint) -> Option<Projection> {
        endpoint.mcp.as_ref()?;
        Some(Projection {
            endpoint: endpoint.clone(),
        })
    }

    /// Every tagged endpoint of a set, in registration order.
    pub fn all<'a>(
        endpoints: impl IntoIterator<Item = &'a Endpoint>,
    ) -> impl Iterator<Item = Projection> {
        endpoints.into_iter().filter_map(Projection::of)
    }

    /// The endpoint behind it — what the dispatcher needs to resolve a handler
    /// and enforce an [`AuthRequirement`].
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The tool's name, which is the endpoint's (see the module docs).
    pub fn name(&self) -> &str {
        &self.endpoint.name
    }

    /// The prose the tag carries.
    pub fn description(&self) -> &str {
        // Only a tagged endpoint reaches `Projection::of`, so the tag is there.
        self.endpoint
            .mcp
            .as_ref()
            .map_or("", |tag| tag.description.as_str())
    }

    /// The half of the surface this belongs to, if any.
    pub fn area(&self) -> Option<Area> {
        self.endpoint.mcp.as_ref().and_then(|tag| tag.area)
    }

    /// The grant this tool needs of its caller, if it changes anything.
    pub fn grant(&self) -> Option<Grant> {
        self.endpoint.mcp.as_ref().and_then(|tag| tag.grant)
    }

    /// Every grant this tool needs, in the order they are checked.
    pub fn grants(&self) -> Vec<Grant> {
        self.endpoint
            .mcp
            .as_ref()
            .map(|tag| tag.grant.into_iter().chain(tag.also).collect())
            .unwrap_or_default()
    }

    /// Whether this endpoint's failure is a **build result** the model reads
    /// rather than a refusal of the call
    /// ([`McpTag::build_result`](crate::McpTag::build_result)).
    pub fn is_a_build(&self) -> bool {
        self.endpoint
            .mcp
            .as_ref()
            .is_some_and(|tag| tag.build_result)
    }

    /// The authorization the endpoint itself requires — enforced by whoever
    /// dispatches, because §13.6's whole argument is that a tool call is
    /// authorized by the code that authorizes a request.
    pub fn auth(&self) -> &AuthRequirement {
        &self.endpoint.auth
    }

    /// The JSON Schema of the merged arguments object.
    pub fn parameters(&self) -> Json {
        let mut properties = Map::new();
        let mut required = Vec::new();

        for (name, ty) in self.endpoint.path.params() {
            // A path parameter is part of the URL, so there is no such thing as
            // omitting one.
            properties.insert(name.to_owned(), scalar_schema(ty));
            required.push(Json::String(name.to_owned()));
        }

        for param in &self.endpoint.query {
            properties.insert(param.name.clone(), query_param_schema(param));
            if param.required {
                required.push(Json::String(param.name.clone()));
            }
        }

        match &self.endpoint.input {
            // No body: nothing to merge.
            input if input.is_empty() => {}
            TypeSchema::Struct(fields) => {
                for field in fields {
                    properties.insert(field.name.clone(), json_schema(&field.schema));
                    if !matches!(field.schema, TypeSchema::Optional(_)) {
                        required.push(Json::String(field.name.clone()));
                    }
                }
            }
            other => {
                properties.insert(BODY_KEY.to_owned(), json_schema(other));
                if !matches!(other, TypeSchema::Optional(_)) {
                    required.push(Json::String(BODY_KEY.to_owned()));
                }
            }
        }

        object_schema(properties, required)
    }

    /// Split one arguments object back into the request it describes.
    ///
    /// Every refusal here is read by a model rather than by a developer with a
    /// backtrace, so each names the argument and what it should have been.
    pub fn split(&self, args: &Json) -> Result<ProjectedCall> {
        let mut remaining = match args {
            // Both vendors send `null` for a tool whose arguments are all
            // optional; refusing it would fail the most ordinary call there is.
            Json::Null => Map::new(),
            Json::Object(map) => map.clone(),
            other => {
                return Err(Error::invalid(format!(
                    "the arguments to `{}` should be an object, got {other}",
                    self.name()
                )));
            }
        };

        let mut path_params = HashMap::new();
        for (name, ty) in self.endpoint.path.params() {
            let Some(value) = remaining.remove(name).filter(|v| !v.is_null()) else {
                return Err(Error::invalid(format!(
                    "`{}` needs `{name}`; it is part of the address of the thing \
                     to act on, so there is no default for it",
                    self.name()
                )));
            };
            path_params.insert(name.to_owned(), scalar_string(&value, ty, name)?);
        }

        let mut query = Vec::new();
        for param in &self.endpoint.query {
            let Some(value) = remaining.remove(&param.name).filter(|v| !v.is_null()) else {
                if param.required {
                    return Err(Error::invalid(format!(
                        "`{}` needs `{}`",
                        self.name(),
                        param.name
                    )));
                }
                continue;
            };
            push_query(&mut query, param, &value, self.name())?;
        }

        let body = match &self.endpoint.input {
            input if input.is_empty() => Json::Null,
            TypeSchema::Struct(fields) => {
                let mut object = Map::new();
                for field in fields {
                    if let Some(value) = remaining.remove(&field.name) {
                        object.insert(field.name.clone(), value);
                    }
                }
                Json::Object(object)
            }
            _ => remaining.remove(BODY_KEY).unwrap_or(Json::Null),
        };

        if !remaining.is_empty() {
            let mut unknown: Vec<&str> = remaining.keys().map(String::as_str).collect();
            unknown.sort_unstable();
            return Err(Error::invalid(format!(
                "`{}` does not take {}; it takes {}",
                self.name(),
                unknown
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                self.argument_names()
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        Ok(ProjectedCall {
            request: ApiRequest {
                method: self.endpoint.method,
                path: self.render_path(&path_params),
                query,
                body,
                // A projected tool never carries bytes: the endpoints that do
                // are tier 3 (§13.6), and `binary_input` is not a shape a JSON
                // arguments object has.
                raw: None,
                links: None,
            },
            path_params,
        })
    }

    /// Every argument name this tool accepts, in the order the schema declares
    /// them — what a refusal lists.
    pub fn argument_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .endpoint
            .path
            .params()
            .map(|(name, _)| name.to_owned())
            .collect();
        names.extend(self.endpoint.query.iter().map(|p| p.name.clone()));
        match &self.endpoint.input {
            input if input.is_empty() => {}
            TypeSchema::Struct(fields) => {
                names.extend(fields.iter().map(|f| f.name.clone()));
            }
            _ => names.push(BODY_KEY.to_owned()),
        }
        names
    }

    /// The concrete path, with the captured parameters substituted.
    fn render_path(&self, params: &HashMap<String, String>) -> String {
        let mut out = String::from("/");
        for (i, segment) in self.endpoint.path.segments.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            match segment {
                PathSegment::Literal(literal) => out.push_str(literal),
                PathSegment::Param { name, .. } => {
                    out.push_str(params.get(name).map_or("", String::as_str));
                }
            }
        }
        out
    }
}

/// The caller check every projected tool makes before it is dispatched.
///
/// Three questions, and they are different ones. [`require_admin`] is the tool
/// surface's own rule — these tools are the schema and the triggers, and an
/// `admin_copilot` reachable at role 80 must not hand a role-80 caller the table
/// editor. The endpoint's [`AuthRequirement`] is the *route's* rule, enforced
/// here for §13.6's load-bearing reason: an MCP call is authorized by the same
/// requirement an HTTP request to the same endpoint is, so a tag that reached an
/// endpoint which is not `admin()` does not quietly become an open door.
///
/// The [`Grant`] the tag declares is the third, and it is why a tag carries one
/// at all: without it a token with `allow_drop` off could not `delete_trigger`
/// and could `deleteAgent`, which would make the six flags mean two different
/// things depending on which tier a tool came from.
pub fn check_caller(projection: &Projection, ctx: &ToolContext<'_>, grants: &Grants) -> Result<()> {
    require_admin(ctx.role, projection.name())?;
    if !projection.auth().admits(ctx.user) {
        return Err(Error::invalid(format!(
            "`{}` is not available to this caller",
            projection.name()
        )));
    }
    // And the grant the tag declared, which is the third question and the one
    // this caller's administrator answered: a tool that changes something is
    // refused by the same flag that refuses the hand-written tool beside it.
    for grant in projection.grants() {
        require_grant(
            grant.allowed_by(grants),
            &format!("call `{}`", projection.name()),
            grant.key(),
        )?;
    }
    Ok(())
}

/// One query parameter's schema: the scalar, an array of it, or a map of
/// caller-chosen keys to it.
fn query_param_schema(param: &QueryParam) -> Json {
    let scalar = scalar_schema(param.ty);
    if param.repeated {
        return serde_json::json!({ "type": "array", "items": scalar });
    }
    if param.map {
        return serde_json::json!({ "type": "object", "additionalProperties": scalar });
    }
    scalar
}

/// Append the query-string pairs one argument stands for.
fn push_query(
    query: &mut Vec<(String, String)>,
    param: &QueryParam,
    value: &Json,
    tool: &str,
) -> Result<()> {
    if param.repeated {
        let Json::Array(items) = value else {
            return Err(Error::invalid(format!(
                "`{tool}`: `{}` takes a list of values, got {value}",
                param.name
            )));
        };
        for item in items {
            query.push((
                param.name.clone(),
                scalar_string(item, param.ty, &param.name)?,
            ));
        }
        return Ok(());
    }
    if param.map {
        let Json::Object(entries) = value else {
            return Err(Error::invalid(format!(
                "`{tool}`: `{}` takes an object whose keys you choose, got {value}",
                param.name
            )));
        };
        for (key, item) in entries {
            query.push((key.clone(), scalar_string(item, param.ty, key)?));
        }
        return Ok(());
    }
    query.push((
        param.name.clone(),
        scalar_string(value, param.ty, &param.name)?,
    ));
    Ok(())
}

/// One scalar argument, as the string a path segment or a query value is.
///
/// The type is checked rather than stringified blindly: a model that sent
/// `{"limit": "ten"}` should be told what `limit` is, not have `ten` reach the
/// handler's parser and come back as whatever that says.
fn scalar_string(value: &Json, ty: ValueType, name: &str) -> Result<String> {
    let wrong = |expected: &str| {
        Err(Error::invalid(format!(
            "`{name}` should be {expected}, got {value}"
        )))
    };
    match (value, ty) {
        (Json::String(s), ValueType::Bool) => match s.as_str() {
            "true" | "false" => Ok(s.clone()),
            _ => wrong("true or false"),
        },
        (Json::Bool(b), ValueType::Bool) => Ok(b.to_string()),
        (_, ValueType::Bool) => wrong("true or false"),

        (Json::Number(n), ValueType::Int | ValueType::Float) => Ok(n.to_string()),
        (_, ValueType::Int) => wrong("a whole number"),
        (_, ValueType::Float) => wrong("a number"),

        // Everything else travels as a string, exactly as it does in the
        // generated TypeScript client — a decimal keeps its precision, a
        // timestamp keeps its offset.
        (Json::String(s), _) => Ok(s.clone()),
        // A JSON scalar where a string is wanted is a near-miss worth accepting
        // rather than a turn spent on quoting: `{"version": 3}` for a text
        // parameter means what it says.
        (Json::Number(n), _) => Ok(n.to_string()),
        (Json::Bool(b), _) => Ok(b.to_string()),
        _ => wrong("a single value, not a list or an object"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{Method, PathSpec};
    use crate::schema::StructField;
    use serde_json::json;

    fn api() -> PathSpec {
        PathSpec::root().lit("api")
    }

    /// A GET with one path parameter and two optional query parameters.
    fn list_runs() -> Endpoint {
        Endpoint::new(
            "listWorkflowRuns",
            Method::Get,
            api()
                .lit("workflows")
                .param("id", ValueType::Uuid)
                .lit("runs"),
        )
        .query([
            QueryParam::new("state", ValueType::Text),
            QueryParam::new("limit", ValueType::Int),
        ])
        .auth(AuthRequirement::admin())
        .mcp("List a workflow's runs.")
    }

    /// A POST whose body is a struct with one required and one optional field.
    fn save() -> Endpoint {
        Endpoint::new("saveThing", Method::Post, api().lit("things"))
            .input(TypeSchema::struct_of([
                StructField::new("label", TypeSchema::text()),
                StructField::new("note", TypeSchema::optional(TypeSchema::text())),
            ]))
            .auth(AuthRequirement::admin())
            .mcp("Save a thing.")
    }

    #[test]
    fn an_untagged_endpoint_is_not_a_tool() {
        let plain = Endpoint::new("secretThing", Method::Get, api().lit("secret"));
        assert!(Projection::of(&plain).is_none());
        assert!(Projection::of(&list_runs()).is_some());
    }

    #[test]
    fn the_three_places_arguments_live_become_one_object() {
        let projection = Projection::of(&list_runs()).unwrap();
        let params = projection.parameters();
        assert_eq!(params["type"], json!("object"));
        // The path parameter is required because a URL has no room for an
        // absent segment; the query parameters are not.
        assert_eq!(params["required"], json!(["id"]));
        assert_eq!(
            params["properties"]["id"],
            json!({ "type": "string", "format": "uuid" })
        );
        assert_eq!(params["properties"]["limit"], json!({ "type": "integer" }));

        let projection = Projection::of(&save()).unwrap();
        let params = projection.parameters();
        assert_eq!(params["required"], json!(["label"]));
        assert_eq!(
            params["properties"]["note"],
            json!({ "type": ["string", "null"] })
        );
    }

    #[test]
    fn a_call_splits_back_into_path_query_and_body() {
        let projection = Projection::of(&list_runs()).unwrap();
        let call = projection
            .split(&json!({ "id": "11111111-2222-3333-4444-555555555555", "state": "waiting" }))
            .unwrap();
        assert_eq!(
            call.request.path,
            "/api/workflows/11111111-2222-3333-4444-555555555555/runs"
        );
        assert_eq!(
            call.path_params.get("id").map(String::as_str),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(
            call.request.query,
            vec![("state".to_owned(), "waiting".to_owned())]
        );
        // An omitted optional query parameter is omitted, not sent empty.
        assert_eq!(call.request.query.len(), 1);
        assert_eq!(call.request.body, Json::Null);

        let projection = Projection::of(&save()).unwrap();
        let call = projection.split(&json!({ "label": "one" })).unwrap();
        assert_eq!(call.request.body, json!({ "label": "one" }));
        assert!(call.request.query.is_empty());
    }

    #[test]
    fn a_missing_path_parameter_says_why_there_is_no_default() {
        let projection = Projection::of(&list_runs()).unwrap();
        let err = projection
            .split(&json!({ "state": "waiting" }))
            .unwrap_err();
        let err = err.to_string();
        assert!(err.contains("needs `id`"), "{err}");
        assert!(err.contains("address"), "{err}");
    }

    #[test]
    fn an_argument_of_the_wrong_type_is_named_with_what_it_should_be() {
        let projection = Projection::of(&list_runs()).unwrap();
        let err = projection
            .split(&json!({ "id": "x", "limit": "ten" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`limit` should be a whole number"), "{err}");
    }

    #[test]
    fn an_argument_nobody_declared_is_refused_with_the_list_that_was() {
        let projection = Projection::of(&save()).unwrap();
        let err = projection
            .split(&json!({ "label": "one", "colour": "red" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`colour`"), "{err}");
        assert!(err.contains("`label`") && err.contains("`note`"), "{err}");
    }

    #[test]
    fn a_body_that_is_not_a_struct_arrives_under_one_key() {
        // A workflow document, a GraphQL variables object: a shape no struct
        // describes cannot be spread over an object's keys.
        let endpoint = Endpoint::new("runThing", Method::Post, api().lit("run"))
            .input(TypeSchema::json())
            .mcp("Run a thing.");
        let projection = Projection::of(&endpoint).unwrap();
        assert_eq!(projection.parameters()["required"], json!([BODY_KEY]));
        let call = projection.split(&json!({ "body": { "a": 1 } })).unwrap();
        assert_eq!(call.request.body, json!({ "a": 1 }));
    }

    #[test]
    fn a_repeated_query_parameter_becomes_one_pair_per_value() {
        let endpoint = Endpoint::new("findThings", Method::Get, api().lit("things"))
            .query([QueryParam::new("tag", ValueType::Text).repeated()])
            .mcp("Find things.");
        let projection = Projection::of(&endpoint).unwrap();
        assert_eq!(
            projection.parameters()["properties"]["tag"],
            json!({ "type": "array", "items": { "type": "string" } })
        );
        let call = projection
            .split(&json!({ "tag": ["red", "blue"] }))
            .unwrap();
        assert_eq!(
            call.request.query,
            vec![
                ("tag".to_owned(), "red".to_owned()),
                ("tag".to_owned(), "blue".to_owned())
            ]
        );
    }
}
