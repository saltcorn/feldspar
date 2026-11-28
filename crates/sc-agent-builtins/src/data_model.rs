//! `data_model` — an application's data model: its tables and fields, its
//! queries and code libraries (TODO-app-constructor AC13.2).
//!
//! The twelve tools the App Constructor's agent builds a data model with, as a
//! trait of their own, so an agent can have them without the constructor's
//! planning tools — the module that ports the constructor gives its agent this
//! trait beside its own. Tables and fields are created and changed, never
//! dropped: dropping is an admin's deliberate act. A table with row-level
//! security keeps its access rules either way.
//!
//! Every tool is an administrator's at the console: the schema and the queries
//! are the application itself, and a run a trigger started or an application's
//! page sent has a prompt somebody else wrote, so it is offered none of them
//! and refused if it calls one.

use sc_agent::{AgentTrait, ToolsContext, TraitContext};
use sc_api::mcp::require_admin;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::coding::is_admin;

/// The tools: the schema, the application's queries and code libraries, and
/// the documentation.
pub const DATA_MODEL_TOOLS: [&str; 12] = [
    sc_api::mcp::TOOL_DESCRIBE,
    sc_api::mcp::TOOL_EDIT,
    sc_api::mcp::TOOL_DESCRIBE_CODE_API,
    sc_app::mcp::TOOL_DESCRIBE_APPS,
    sc_app::mcp::TOOL_UPDATE_APP,
    sc_app::mcp::TOOL_SAVE_QUERY,
    sc_app::mcp::TOOL_DELETE_QUERY,
    sc_app::mcp::TOOL_DESCRIBE_CODE_LIBRARIES,
    sc_app::mcp::TOOL_SAVE_CODE_LIBRARY,
    sc_app::mcp::TOOL_DELETE_CODE_LIBRARY,
    sc_docs::tools::TOOL_SEARCH_DOCS,
    sc_docs::tools::TOOL_READ_DOCS,
];

/// What the data-model tools may do: create and change, but not drop, which
/// an admin does deliberately. A table with row-level security keeps its
/// access rules either way: only a person changes those.
pub(crate) fn data_model_tool_set() -> sc_api::mcp::ToolSet {
    sc_app::mcp::tool_set(
        sc_api::schema_edit::Grants {
            create: true,
            edit: true,
            drop: false,
        },
        sc_api::mcp::Areas::all(),
    )
}

/// The data-model tools' specs, as `catalog` describes them.
pub(crate) fn data_model_specs(catalog: &sc_catalog::Catalog) -> Vec<ToolSpec> {
    data_model_tool_set()
        .specs(catalog)
        .into_iter()
        .filter(|spec| DATA_MODEL_TOOLS.contains(&spec.name.as_str()))
        .collect()
}

/// Run one of the data-model tools, with the run's caller and the console's
/// proof the schema tools take.
pub(crate) async fn data_model_call(tool: &str, args: &Json, ctx: &TraitContext<'_>) -> Result<Json> {
    let tool_context = sc_api::mcp::ToolContext {
        catalog: ctx.catalog,
        user: ctx.caller.user_ref(),
        admin: ctx.caller.console_admin().cloned(),
        triggers: ctx.triggers,
        actor: ctx.agent,
    };
    data_model_tool_set().call(tool, args, &tool_context).await
}

/// The trait.
pub struct DataModel;

#[async_trait::async_trait]
impl AgentTrait for DataModel {
    fn name(&self) -> &str {
        "data_model"
    }

    fn description(&self) -> &str {
        "Builds an application's data model: tables and fields, queries and code libraries"
    }

    fn config_spec(&self) -> Vec<sc_types::FormField> {
        Vec::new()
    }

    fn prompt(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Option<String> {
        Some(format!(
            "You build an application's data model with these tools: describe the schema first, \
             then create and change tables and fields with edit_schema, and save the \
             application's queries and code libraries. You never drop a table or a field: an \
             admin does that.\n\n{}",
            sc_docs::tools::PROMPT
        ))
    }

    /// The documentation's two, which `coding`, `docs` and `admin_copilot`
    /// offer identically.
    fn shared_tools(&self) -> &[&'static str] {
        &sc_docs::tools::TOOL_NAMES
    }

    /// Withheld from a run that is not an administrator's at the console.
    fn tools(&self, cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        if cx
            .caller
            .is_some_and(|caller| !is_admin(caller) || caller.untrusted_origin())
        {
            return Vec::new();
        }
        data_model_specs(cx.catalog)
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        require_admin(ctx.caller.role(), tool)?;
        if ctx.caller.untrusted_origin() || ctx.caller.console_admin().is_none() {
            return Err(Error::invalid(format!(
                "`{tool}` changes the application's data model, so it is only for an \
                 administrator's own runs at the console, not one a trigger started or an \
                 application's page sent"
            )));
        }
        if !DATA_MODEL_TOOLS.contains(&tool) {
            return Err(Error::invalid(format!("`data_model` has no tool `{tool}`")));
        }
        data_model_call(tool, args, ctx).await
    }
}
