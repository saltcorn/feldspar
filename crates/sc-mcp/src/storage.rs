//! The `_fd_mcp_servers` table: its schema, bootstrap, and the
//! [`McpServerDef`] ⇄ row mapping (TODO-agent-skills AS4.1).
//!
//! The `_fd_llm_providers` pattern exactly: the row *is* the definition,
//! reading is strict, and the header values sit in the primary database like a
//! provider's API key — not encrypted at rest, never returned by the API.

use std::collections::BTreeMap;

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::{Value as Json, json};

use crate::def::{McpServerDef, McpServerDefId, McpTransport, validate_server};

/// Name of the MCP servers table in the primary database.
pub const MCP_SERVERS_TABLE: &str = "_fd_mcp_servers";

const COL_ID: &str = "id";
const COL_NAME: &str = "name";
const COL_DESCRIPTION: &str = "description";
const COL_URL: &str = "url";
const COL_TRANSPORT: &str = "transport";
/// The headers, as a JSON object of name to value. The values are secrets.
const COL_HEADERS: &str = "headers";
/// `allow_private_network` and `timeout_secs`, as a JSON object.
const COL_CONFIG: &str = "config";
const COL_ATTRIBUTES: &str = "attributes";

const CFG_PRIVATE_NETWORK: &str = "allow_private_network";
const CFG_TIMEOUT_SECS: &str = "timeout_secs";

fn fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_URL, text()).required(),
        DataField::plain(COL_TRANSPORT, text()).required(),
        DataField::plain(COL_HEADERS, json()).required(),
        DataField::plain(COL_CONFIG, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure `_fd_mcp_servers` exists. Idempotent; call once at startup.
pub async fn bootstrap_mcp_servers(catalog: &Catalog) -> Result<Table> {
    catalog.bootstrap_table(MCP_SERVERS_TABLE, &fields()).await
}

/// Save a definition: insert it, or update it in place when its id has a row.
/// Checked first ([`validate_server`]), and a name another server holds is
/// refused by name.
pub async fn save_mcp_server(catalog: &Catalog, def: &McpServerDef) -> Result<McpServerDef> {
    let mut def = def.clone();
    def.name = def.name.trim().to_owned();
    def.url = def.url.trim().to_owned();
    validate_server(&def)?;
    if let Some(other) = load_mcp_server_by_name(catalog, &def.name).await?
        && other.id != def.id
    {
        return Err(Error::invalid(format!(
            "MCP server name `{}` is already used",
            def.name
        )));
    }
    let columns = columns();
    let values = values(&def);
    let statement = if load_mcp_server(catalog, def.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        Statement::from(
            Update::new(MCP_SERVERS_TABLE, assignments)
                .filter(Expr::col(COL_ID).eq(Expr::lit(def.id.0))),
        )
    } else {
        Statement::from(Insert::row(
            MCP_SERVERS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        ))
    };
    run(catalog, statement).await?;
    crate::cache::forget(def.id);
    Ok(def)
}

/// The definition with this id, if any.
pub async fn load_mcp_server(
    catalog: &Catalog,
    id: McpServerDefId,
) -> Result<Option<McpServerDef>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// The definition named `name`, if any.
pub async fn load_mcp_server_by_name(
    catalog: &Catalog,
    name: &str,
) -> Result<Option<McpServerDef>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name.trim()))).await
}

/// The definition named `name`, or an error saying there is none.
pub async fn require_mcp_server(catalog: &Catalog, name: &str) -> Result<McpServerDef> {
    load_mcp_server_by_name(catalog, name)
        .await?
        .ok_or_else(|| Error::not_found(format!("no MCP server named `{}`", name.trim())))
}

/// Every definition, by name.
pub async fn list_mcp_servers(catalog: &Catalog) -> Result<Vec<McpServerDef>> {
    let mut defs: Vec<McpServerDef> = rows(catalog, Select::from(Source::table(MCP_SERVERS_TABLE)))
        .await?
        .iter()
        .map(from_row)
        .collect::<Result<_>>()?;
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(defs)
}

/// Delete a definition, returning whether there was one.
///
/// Refused while `referents` is non-empty, naming them: the agents whose
/// traits name this server, collected by a caller that can see agents, as
/// `delete_llm_provider` takes its referents.
pub async fn delete_mcp_server(
    catalog: &Catalog,
    id: McpServerDefId,
    referents: &[String],
) -> Result<bool> {
    let Some(def) = load_mcp_server(catalog, id).await? else {
        return Ok(false);
    };
    if !referents.is_empty() {
        return Err(Error::invalid(format!(
            "MCP server `{}` is still used by {}; remove those references before deleting it",
            def.name,
            referents.join(", ")
        )));
    }
    let delete = Delete::from(MCP_SERVERS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(delete)).await?;
    crate::cache::forget(id);
    Ok(true)
}

fn columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_URL,
        COL_TRANSPORT,
        COL_HEADERS,
        COL_CONFIG,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

fn values(def: &McpServerDef) -> Vec<Value> {
    let headers: serde_json::Map<String, Json> = def
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), Json::String(v.clone())))
        .collect();
    vec![
        Value::Uuid(def.id.0),
        Value::Text(def.name.clone()),
        Value::Text(def.description.clone()),
        Value::Text(def.url.clone()),
        Value::Text(def.transport.as_str().to_owned()),
        Value::Json(Json::Object(headers)),
        Value::Json(json!({
            CFG_PRIVATE_NETWORK: def.allow_private_network,
            CFG_TIMEOUT_SECS: def.timeout_secs,
        })),
        Value::Json(Json::Object(def.attributes.clone())),
    ]
}

fn from_row(row: &Row) -> Result<McpServerDef> {
    let text = |column: &str| match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        Some(Value::Null) | None if column == COL_DESCRIPTION => Ok(String::new()),
        other => Err(bad_column(column, "text", other)),
    };
    let object = |column: &str| -> Result<Attrs> {
        match row.get(column) {
            Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
            other => Err(bad_column(column, "a json object", other)),
        }
    };
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => McpServerDefId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let headers = object(COL_HEADERS)?
        .into_iter()
        .map(|(k, v)| match v {
            Json::String(s) => Ok((k, s)),
            _ => Err(Error::invalid(format!(
                "{MCP_SERVERS_TABLE}.{COL_HEADERS}: header `{k}` is not text"
            ))),
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let config = object(COL_CONFIG)?;
    Ok(McpServerDef {
        id,
        name: text(COL_NAME)?,
        description: text(COL_DESCRIPTION)?,
        url: text(COL_URL)?,
        transport: McpTransport::parse(&text(COL_TRANSPORT)?)?,
        headers,
        allow_private_network: config
            .get(CFG_PRIVATE_NETWORK)
            .and_then(Json::as_bool)
            .unwrap_or(false),
        timeout_secs: config
            .get(CFG_TIMEOUT_SECS)
            .and_then(Json::as_u64)
            .unwrap_or(0),
        attributes: object(COL_ATTRIBUTES)?,
    })
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{MCP_SERVERS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("{MCP_SERVERS_TABLE} row has no `{column}` column")),
    }
}

async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<McpServerDef>> {
    let select = Select::from(Source::table(MCP_SERVERS_TABLE))
        .filter(filter)
        .limit(1);
    rows(catalog, select)
        .await?
        .first()
        .map(from_row)
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_and_values_stay_in_step() {
        let def = McpServerDef::new("docs", "https://x/").header("X-Key", "k");
        assert_eq!(columns().len(), values(&def).len());
        assert_eq!(columns().len(), fields().len());
        assert!(MCP_SERVERS_TABLE.starts_with("_fd_"));
    }
}
