//! The MCP servers admin API (TODO-agent-skills AS4.1, AS4.3): the remote MCP
//! servers an agent's `mcp_server` trait calls, listed, saved, deleted, tested,
//! and their tools seen.
//!
//! Not to be confused with `mcp.rs`, which is this server *being* an MCP
//! server. Here it is a client, and the definitions are `sc-mcp`'s.
//!
//! A header's value is a secret, handled as an LLM provider's API key is: every
//! response carries the sentinel in its place ([`McpServerDef::redacted`]), and
//! a save or a test that sends the sentinel back has it replaced by the stored
//! value ([`sc_mcp::merge_headers`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_api::rows::require_object;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_mcp::{McpServerDef, McpServerDefId, McpTransport};
use serde_json::{Map, Value as Json, json};

use crate::handler::{HandlerRegistry, HandlerResponse};

/// The trait that names an MCP server in its `server` setting (AS9.1).
const MCP_TRAIT: &str = "mcp_server";

pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>) {
    reg.register("listMcpServers", {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                if catalog.get(sc_mcp::MCP_SERVERS_TABLE)?.is_none() {
                    return Ok(HandlerResponse::ok(json!([])));
                }
                let out: Vec<Json> = sc_mcp::list_mcp_servers(&catalog)
                    .await?
                    .iter()
                    .map(server_json)
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register("createMcpServer", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let def = server_from_body(McpServerDefId::new(), &ctx.body, &BTreeMap::new())?;
                let saved = sc_mcp::save_mcp_server(&catalog, &def).await?;
                Ok(HandlerResponse::ok(server_json(&saved)).with_status(201))
            }
        }
    });

    reg.register("updateMcpServer", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let existing = require_by_id(&catalog, ctx.path_param("id")?).await?;
                let mut def = server_from_body(existing.id, &ctx.body, &existing.headers)?;
                def.attributes = existing.attributes.clone();
                // A rename breaks the agents naming it, as a delete would.
                if def.name.trim() != existing.name {
                    let users = agents_using(&catalog, &existing.name).await?;
                    if !users.is_empty() {
                        return Err(Error::invalid(format!(
                            "MCP server `{}` is used by {} by its name; change those agents first",
                            existing.name,
                            users.join(", ")
                        )));
                    }
                }
                let saved = sc_mcp::save_mcp_server(&catalog, &def).await?;
                Ok(HandlerResponse::ok(server_json(&saved)))
            }
        }
    });

    reg.register("deleteMcpServer", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_id(ctx.path_param("id")?)?;
                let Some(def) = sc_mcp::load_mcp_server(&catalog, id).await? else {
                    return Ok(HandlerResponse::ok(json!({ "deleted": false })));
                };
                let users = agents_using(&catalog, &def.name).await?;
                let deleted = sc_mcp::delete_mcp_server(&catalog, id, &users).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    // A structurally wrong definition is an ordinary error (the admin's typo);
    // a server that refuses or cannot be reached is a 200 with `ok: false`.
    reg.register("testMcpServer", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let stored = match obj.get("id").and_then(Json::as_str) {
                    Some(raw) => sc_mcp::load_mcp_server(&catalog, parse_id(raw)?).await?,
                    None => None,
                };
                let stored_headers = stored
                    .as_ref()
                    .map(|s| s.headers.clone())
                    .unwrap_or_default();
                let def = server_from_body(
                    stored.as_ref().map_or_else(McpServerDefId::new, |s| s.id),
                    &ctx.body,
                    &stored_headers,
                )?;
                sc_mcp::validate_server(&def)?;
                Ok(HandlerResponse::ok(report_json(
                    sc_mcp::test_connection(&def).await,
                    &def.name,
                )))
            }
        }
    });

    reg.register("listMcpServerTools", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let def = require_by_id(&catalog, ctx.path_param("id")?).await?;
                let refresh = matches!(ctx.query_get("refresh"), Some("true" | "1"));
                let listed = if refresh {
                    sc_mcp::test_connection(&def).await
                } else {
                    sc_mcp::tools(&def)
                        .await
                        .map(|tools| sc_mcp::McpServerReport {
                            server: sc_mcp::McpServerInfo {
                                name: String::new(),
                                version: String::new(),
                                protocol_version: String::new(),
                                instructions: None,
                                tools_list_changed: false,
                            },
                            tools: tools.to_vec(),
                        })
                };
                let mut out = report_json(listed, &def.name);
                // A cached listing says nothing new about the server.
                if !refresh {
                    out["server"] = Json::Null;
                }
                Ok(HandlerResponse::ok(out))
            }
        }
    });
}

/// A definition as the API returns it, its header values redacted.
pub(crate) fn server_json(def: &McpServerDef) -> Json {
    with_headers(&def.redacted())
}

/// A definition for a **backup**, its header values included: the backup
/// carries them as it carries an LLM provider's API key.
pub(crate) fn backup_server_json(def: &McpServerDef) -> Json {
    with_headers(def)
}

fn with_headers(def: &McpServerDef) -> Json {
    json!({
        "id": def.id.0,
        "name": def.name,
        "description": def.description,
        "url": def.url,
        "transport": def.transport.as_str(),
        "headers": def
            .headers
            .iter()
            .map(|(name, value)| json!({ "name": name, "value": value }))
            .collect::<Vec<_>>(),
        "allow_private_network": def.allow_private_network,
        "timeout_secs": def.timeout_secs,
    })
}

/// Rebuild a definition from a create, update, test or restore body, the
/// sentinel in a header value resolved against `stored`.
pub(crate) fn server_from_body(
    id: McpServerDefId,
    body: &Json,
    stored: &BTreeMap<String, String>,
) -> Result<McpServerDef> {
    let obj = require_object(body)?;
    let text = |key: &str| obj.get(key).and_then(Json::as_str).unwrap_or("").to_owned();
    let mut headers = BTreeMap::new();
    for header in obj
        .get("headers")
        .and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let name = header
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            continue;
        }
        let value = header.get("value").and_then(Json::as_str).unwrap_or("");
        if headers.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(Error::invalid(format!("header `{name}` is given twice")));
        }
    }
    Ok(McpServerDef {
        id,
        name: text("name").trim().to_owned(),
        description: text("description"),
        url: text("url").trim().to_owned(),
        transport: McpTransport::parse(&text("transport"))?,
        headers: sc_mcp::merge_headers(stored, &headers)?,
        allow_private_network: obj
            .get("allow_private_network")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        timeout_secs: obj.get("timeout_secs").and_then(Json::as_u64).unwrap_or(0),
        attributes: Map::new(),
    })
}

/// A test's or a listing's answer: what was found, or the refusal.
fn report_json(outcome: Result<sc_mcp::McpServerReport>, name: &str) -> Json {
    match outcome {
        Ok(report) => json!({
            "ok": true,
            "message": format!("`{name}` offers {} tool(s)", report.tools.len()),
            "server": report.server,
            "tools": report.tools,
        }),
        Err(e) => json!({
            "ok": false,
            "message": sc_error::format_causes(&e),
            "server": Json::Null,
            "tools": [],
        }),
    }
}

/// The agents whose `mcp_server` trait names the server called `name`.
async fn agents_using(catalog: &Catalog, name: &str) -> Result<Vec<String>> {
    if catalog.get(sc_agent::AGENTS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    Ok(sc_agent::list_agents(catalog)
        .await?
        .into_iter()
        .filter(|agent| {
            agent.traits.iter().any(|t| {
                t.trait_ == MCP_TRAIT
                    && t.config.get("server").and_then(Json::as_str).map(str::trim) == Some(name)
            })
        })
        .map(|agent| format!("agent `{}`", agent.name))
        .collect())
}

async fn require_by_id(catalog: &Catalog, raw: &str) -> Result<McpServerDef> {
    let id = parse_id(raw)?;
    sc_mcp::load_mcp_server(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no MCP server with id {}", id.0)))
}

fn parse_id(raw: &str) -> Result<McpServerDefId> {
    uuid::Uuid::parse_str(raw)
        .map(McpServerDefId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid MCP server id")))
}
