//! `docs` — the server's documentation, for any agent.
//!
//! `search_docs` and `read_docs` ([`sc_docs::tools`]) over the tutorials,
//! references and help topics compiled into the binary. `admin_copilot` offers
//! the same two tools already, through the administrative tool set it shares
//! with the MCP server; this trait is how every other agent gets them — an
//! application's builder is created with it (`sc_app::builder_agent`), because a
//! coding agent writing against this server's API is the agent most likely to
//! write it from memory.
//!
//! Its tools have **fixed names**, so it is one per agent, and not beside an
//! `admin_copilot`, which already has them: either is refused on save as a
//! collision (§11.2), naming the tool.
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
