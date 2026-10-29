//! `docs` — the server's documentation, for any agent.
//!
//! `search_docs` and `read_docs` ([`sc_docs::tools`]) over the tutorials,
//! references and help topics compiled into the binary. `admin_copilot` offers
//! the same two tools already, through the administrative tool set it shares
//! with the MCP server, and `coding` offers them beside its file tools; this
//! trait is how every other agent gets them — a support agent answering
//! questions about the installation, say.
//!
//! `coding` offers the same two tools too, so an agent with a `coding` trait
//! does not need this one. The tools are **shared** ([`AgentTrait::shared_tools`]):
//! beside a `coding` or an `admin_copilot` they are one tool, not a collision.
//!
//! No configuration: what it reaches is documentation, which is the same for
//! every caller and grants nothing.

use sc_agent::{AgentTrait, ToolsContext, TraitContext};
use sc_docs::tools::{self, TOOL_READ_DOCS, TOOL_SEARCH_DOCS};
use sc_error::Result;
use sc_llm::ToolSpec;
use sc_types::{Attrs, FormField};
use serde_json::Value as Json;

/// Search and read the server's documentation.
pub struct Docs;

#[async_trait::async_trait]
impl AgentTrait for Docs {
    fn name(&self) -> &str {
        "docs"
    }

    fn description(&self) -> &str {
        "Search and read this server's documentation: its tutorials, references and \
         help pages"
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                TOOL_SEARCH_DOCS,
                tools::search_description(),
                tools::search_parameters(),
            ),
            ToolSpec::new(
                TOOL_READ_DOCS,
                tools::read_description(),
                tools::read_parameters(),
            ),
        ]
    }

    fn shared_tools(&self) -> &[&'static str] {
        &sc_docs::tools::TOOL_NAMES
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        args: &Json,
        _ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        tools::call(tool, args)
    }

    fn prompt(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Option<String> {
        Some(tools::PROMPT.to_owned())
    }
}
