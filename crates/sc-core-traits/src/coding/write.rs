//! `write_file`: create a file in the configured scope, or replace one whole
//! (TODO 5.4).
//!
//! Offered only under the [`Coding`](super::Coding) trait's edit grant. It
//! **replaces the whole file**, so it is guarded like an edit: an existing file
//! the run has not read (or written) is refused, naming the read tool, and so is
//! one that changed since the run last saw it. A model that has not read a file
//! writes the file it remembers, not the file that is there.

use sc_agent::TraitContext;
use sc_error::{Error, Result};
use sc_llm::{EditFormat, ToolSpec};
use serde_json::{Value as Json, json};

use super::change::{current, write_tracked};
use super::state::{CodingState, stale_message};
use crate::files::{ARG_PATH, FileScope, open_to_write, string_arg};
use crate::table::arguments;

/// The file's new contents.
const ARG_CONTENT: &str = "content";

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("write_file_{}", scope.slug())
}

/// The tool this scope's whole-file write contributes. Under `whole_file` it is
/// the only way to change a file, and its description says so.
pub fn spec(scope: &FileScope, format: EditFormat) -> ToolSpec {
    let how = match format {
        EditFormat::WholeFile => "This is the only way to change a file.",
        _ => "To change part of a file, use the edit tool instead.",
    };
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Create a new file, or replace the whole content of an existing one. An existing \
             file must have been read in this run, and not changed since. {how}"
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_PATH: {"type": "string"},
                ARG_CONTENT: {"type": "string", "description": "The complete new content of the file"},
            },
            "required": [ARG_PATH, ARG_CONTENT],
            "additionalProperties": false,
        }),
    )
}

/// Write one file whole, as the run's caller.
pub async fn call(scope: &FileScope, args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let args = arguments(args, &[ARG_PATH, ARG_CONTENT])?;
    let rel = string_arg(&args, ARG_PATH)?;
    let content = string_arg(&args, ARG_CONTENT)?;

    let (store, path) = open_to_write(scope, ctx, &rel).await?;
    let before = current(store.as_ref(), &path, &rel).await?;
    let mut state = CodingState::load(ctx.state());
    if let Some(before) = &before
        && let Err(stale) = state.check_current(&path, before)
    {
        return Err(Error::invalid(stale_message(
            stale,
            &rel,
            &super::read::tool_name(scope),
        )));
    }

    let lines = content.lines().count();
    write_tracked(
        store.as_ref(),
        &mut state,
        &path,
        before.as_deref(),
        content.into_bytes(),
    )
    .await?;
    state.store(ctx.state());
    Ok(Json::String(format!(
        "{} `{rel}` ({lines} lines).",
        match before {
            None => "Created",
            Some(_) => "Replaced",
        }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_scope() {
        let scope = FileScope {
            store: "src".to_owned(),
            root: String::new(),
        };
        assert_eq!(tool_name(&scope), "write_file_src");
    }

    #[test]
    fn under_whole_file_the_description_says_it_is_the_only_way() {
        let scope = FileScope {
            store: "src".to_owned(),
            root: String::new(),
        };
        assert!(
            spec(&scope, EditFormat::WholeFile)
                .description
                .contains("only way to change a file")
        );
        assert!(
            !spec(&scope, EditFormat::StrReplace)
                .description
                .contains("only way")
        );
    }
}
