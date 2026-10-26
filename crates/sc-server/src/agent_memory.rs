//! The agent memory admin API (TODO-agent-skills AS8.3): what agents' `memory`
//! traits stored, listed and deleted.
//!
//! Admin-only, and the only place every user's personal memories are read
//! together: everywhere else a personal memory is read back for its own user
//! alone (`sc_core_traits::memory::MemoryReader`).

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_core_traits::memory::{MemoryFilter, MemoryReader, delete_memory, list_memories};
use sc_error::Error;
use serde_json::{Value as Json, json};

use crate::handler::{HandlerRegistry, HandlerResponse};

/// How many memories one listing returns when the request names no limit.
const DEFAULT_LIMIT: u64 = 200;
/// The most one listing returns.
const MAX_LIMIT: u64 = 1000;

pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>) {
    reg.register("listAgentMemories", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let text = |name: &str| {
                    ctx.query_get(name)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                let limit = match text("limit") {
                    None => DEFAULT_LIMIT,
                    Some(raw) => raw
                        .parse::<u64>()
                        .map_err(|_| Error::invalid(format!("`limit` `{raw}` is not a number")))?
                        .clamp(1, MAX_LIMIT),
                };
                let mut filter = MemoryFilter::new(MemoryReader::Admin, limit);
                filter.agent = text("agent");
                filter.kind = text("kind");
                filter.search = text("search");
                filter.user =
                    match text("user") {
                        None => None,
                        Some(raw) => Some(raw.parse().map_err(|_| {
                            Error::invalid(format!("`user` `{raw}` is not a user id"))
                        })?),
                    };
                let out: Vec<Json> = list_memories(&catalog, &filter)
                    .await?
                    .iter()
                    .map(|m| m.to_json())
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register("deleteAgentMemory", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let raw = ctx.path_param("id")?;
                let id: i64 = raw
                    .parse()
                    .map_err(|_| Error::invalid(format!("`{raw}` is not a memory id")))?;
                let deleted = delete_memory(&catalog, id).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });
}
