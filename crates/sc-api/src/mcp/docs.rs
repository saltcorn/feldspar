//! `search_docs` and `read_docs` — the server's documentation, as two of the
//! administrative tools.
//!
//! The tools are `sc-docs`' ([`sc_docs::tools`]): its names, its descriptions,
//! its call. They are here so that every caller of this surface — the MCP
//! server and `admin_copilot` — offers them, and always, like
//! `describe_code_api`: reading documentation grants nothing, and an agent that
//! may configure a feature must be able to read how it works.

use sc_catalog::Catalog;
use sc_docs::tools::{self, TOOL_READ_DOCS, TOOL_SEARCH_DOCS};
use sc_error::Result;
use serde_json::Value as Json;

use crate::schema_edit::Grants;

use super::{AdminTool, ToolContext};

/// Search the documentation.
pub(super) struct SearchDocs;

/// Read a document, or one section of it.
pub(super) struct ReadDocs;

#[async_trait::async_trait]
impl AdminTool for SearchDocs {
    fn name(&self) -> &'static str {
        TOOL_SEARCH_DOCS
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        tools::search_description().to_owned()
    }

    fn parameters(&self) -> Json {
        tools::search_parameters()
    }

    async fn call(&self, _ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        tools::call(TOOL_SEARCH_DOCS, args)
    }
}

#[async_trait::async_trait]
impl AdminTool for ReadDocs {
    fn name(&self) -> &'static str {
        TOOL_READ_DOCS
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        tools::read_description().to_owned()
    }

    fn parameters(&self) -> Json {
        tools::read_parameters()
    }

    async fn call(&self, _ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        tools::call(TOOL_READ_DOCS, args)
    }
}
