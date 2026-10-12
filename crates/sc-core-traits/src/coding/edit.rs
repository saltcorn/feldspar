//! `edit_file`: replace quoted text in one file, found by the match cascade
//! (TODO 5.6, R§3.1).
//!
//! The model sends the text it believes is there (`old_text`) and what it wants
//! instead (`new_text`), and [`matching`](super::matching) finds the one place
//! that text is, forgiving whitespace, indentation and a mistyped character, in
//! that order. What comes back is always something to act on:
//!
//! - **Success** returns the edited lines, numbered, and which step matched, so
//!   the model does not re-read the file to see what it did.
//! - **Not found** returns the most similar region, numbered, and one
//!   instruction: copy the text from these lines.
//! - **Ambiguous** names the lines of every place, and says to quote more.
//!
//! Both failures raise `EditFailed`, which the loop's escalation ladder counts.
//! Before any of that, a file the run has not read, or that changed since it was
//! read, is refused with the read tool's name. That refusal is not an edit
//! failure: nothing was tried.

use sc_agent::{Signal, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use serde_json::{Value as Json, json};

use super::change::{current, edited_region, numbered, write_tracked};
use super::matching::{self, Level, Search};
use super::read::as_text;
use super::state::{CodingState, stale_message};
use crate::files::{ARG_PATH, FileScope, open_to_write, optional_bool_arg, string_arg};
use crate::table::arguments;

/// The text to find.
const ARG_OLD: &str = "old_text";
/// What to put in its place.
const ARG_NEW: &str = "new_text";
/// Whether every occurrence is meant.
const ARG_ALL: &str = "replace_all";

/// The most edited regions shown after a `replace_all`.
const MAX_REGIONS_SHOWN: usize = 3;

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("edit_file_{}", scope.slug())
}

/// The tool this scope's edit contributes.
pub fn spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Replace text in a file that has been read in this run. `{ARG_OLD}` must match \
             exactly one place in the file (copy whole lines, with their indentation), unless \
             `{ARG_ALL}` is set. The result shows the edited lines, so there is no need to \
             read the file again."
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_PATH: {"type": "string"},
                ARG_OLD: {"type": "string", "description": "The existing text: whole lines, copied exactly from the file"},
                ARG_NEW: {"type": "string", "description": "What replaces it"},
                ARG_ALL: {"type": "boolean", "description": "Replace every occurrence instead of exactly one"},
            },
            "required": [ARG_PATH, ARG_OLD, ARG_NEW],
            "additionalProperties": false,
        }),
    )
}

/// Apply one edit, as the run's caller.
pub async fn call(scope: &FileScope, args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let args = arguments(args, &[ARG_PATH, ARG_OLD, ARG_NEW, ARG_ALL])?;
    let rel = string_arg(&args, ARG_PATH)?;
    let old = string_arg(&args, ARG_OLD)?;
    let new = string_arg(&args, ARG_NEW)?;
    let all = optional_bool_arg(&args, ARG_ALL, false)?;
    if old.is_empty() {
        return Err(Error::invalid(format!(
            "`{ARG_OLD}` is empty. To create a file, use `{}`.",
            super::write::tool_name(scope)
        )));
    }
    if old == new {
        return Err(Error::invalid(format!(
            "`{ARG_OLD}` and `{ARG_NEW}` are the same, so there is nothing to change"
        )));
    }

    let (store, path) = open_to_write(scope, ctx, &rel).await?;
    let Some(bytes) = current(store.as_ref(), &path, &rel).await? else {
        return Err(Error::invalid(format!(
            "`{rel}` does not exist. To create it, use `{}`.",
            super::write::tool_name(scope)
        )));
    };
    let text = as_text(&bytes)
        .ok_or_else(|| Error::invalid(format!("`{rel}` is a binary file and cannot be edited")))?;
    let mut state = CodingState::load(ctx.state());
    if let Err(stale) = state.check_current(&path, &bytes) {
        return Err(Error::invalid(stale_message(
            stale,
            &rel,
            &super::read::tool_name(scope),
        )));
    }

    let (edited, summary, level) = match apply(text, &old, &new, all, &rel) {
        Ok(done) => done,
        Err(message) => {
            ctx.signal(Signal::EditFailed);
            state.edit_failed();
            state.store(ctx.state());
            return Err(Error::invalid(message));
        }
    };
    state.edit_applied(Some(level));
    write_tracked(
        store.as_ref(),
        &mut state,
        &path,
        Some(&bytes),
        edited.into_bytes(),
    )
    .await?;
    state.store(ctx.state());
    Ok(Json::String(summary))
}

/// The edited text and what to tell the model, or the failure message.
///
/// Separate from the I/O so every outcome is unit-testable without a store.
fn apply(
    text: &str,
    old: &str,
    new: &str,
    all: bool,
    rel: &str,
) -> Result<(String, String, Level), String> {
    match matching::find(text, old, all) {
        Search::Found(found) => {
            let level = found[0].level;
            let (edited, ranges) = matching::replace(text, &found, new);
            let mut summary = match ranges.len() {
                1 => format!(
                    "Edited `{rel}` ({}). The lines now read:\n",
                    level.describe()
                ),
                n => format!("Edited `{rel}`: {n} places ({}). Now:\n", level.describe()),
            };
            let shown: Vec<String> = ranges
                .iter()
                .take(MAX_REGIONS_SHOWN)
                .map(|range| edited_region(&edited, range))
                .collect();
            summary.push_str(&shown.join("\n…\n"));
            if ranges.len() > MAX_REGIONS_SHOWN {
                summary.push_str(&format!(
                    "\n[{} more places not shown]",
                    ranges.len() - MAX_REGIONS_SHOWN
                ));
            }
            Ok((edited, summary, level))
        }
        Search::Ambiguous { level, lines } => Err(format!(
            "`{ARG_OLD}` matches {} places in `{rel}` ({}), starting at lines {}. {}",
            lines.len(),
            level.describe(),
            join_lines(&lines),
            match level {
                Level::Fuzzy => "Quote more surrounding lines exactly so it matches one place.",
                _ =>
                    "Quote more surrounding lines so it matches one place, or set \
                      `replace_all` to change them all.",
            }
        )),
        Search::Missing { closest } => Err(match closest {
            Some(region) => format!(
                "`{ARG_OLD}` was not found in `{rel}`. The most similar lines ({}-{}, {:.0}% \
                 similar) are:\n{}\nCopy the text to replace exactly from these lines and \
                 try again.",
                region.first,
                region.last,
                region.score * 100.0,
                numbered(text, region.first, region.last)
            ),
            None => format!(
                "`{ARG_OLD}` was not found in `{rel}`. Read the file again and copy the text \
                 to replace exactly."
            ),
        }),
    }
}

/// `1, 4 and 9`.
fn join_lines(lines: &[usize]) -> String {
    let mut words: Vec<String> = lines.iter().map(usize::to_string).collect();
    match words.len() {
        0 | 1 => words.join(""),
        _ => {
            let last = words.pop().unwrap_or_default();
            format!("{} and {last}", words.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "const a = 1;\nconst b = 2;\nconst a = 3;\n";

    #[test]
    fn a_tools_name_is_derived_from_its_scope() {
        let scope = FileScope {
            store: "src".to_owned(),
            root: String::new(),
        };
        assert_eq!(tool_name(&scope), "edit_file_src");
    }

    #[test]
    fn a_success_shows_the_edited_lines_and_the_step() {
        let (text, summary, level) =
            apply(FILE, "const b = 2;", "const b = 20;", false, "a.ts").unwrap();
        // The step is returned as well as described, because the eval harness
        // counts it (TODO §13).
        assert_eq!(level, Level::Exact);
        assert_eq!(text, "const a = 1;\nconst b = 20;\nconst a = 3;\n");
        assert_eq!(
            summary,
            "Edited `a.ts` (exact match). The lines now read:\n\
             1\tconst a = 1;\n2\tconst b = 20;\n3\tconst a = 3;"
        );
    }

    #[test]
    fn a_miss_shows_the_closest_lines_and_one_instruction() {
        let file = "function add(a, b) {\n  return a + b;\n}\n\nfunction sub(a, b) {\n  return a - b;\n}\n";
        let err = apply(
            file,
            "function sub(first, second) {\n  return first - second;\n}",
            "",
            false,
            "m.js",
        )
        .unwrap_err();
        assert!(err.contains("was not found in `m.js`"), "{err}");
        assert!(err.contains("(5-7,"), "{err}");
        assert!(err.contains("5\tfunction sub(a, b) {"), "{err}");
        assert!(err.contains("Copy the text to replace exactly"), "{err}");
    }

    #[test]
    fn an_ambiguity_names_every_place() {
        let err = apply(FILE, "const a", "let a", false, "a.ts").unwrap_err();
        assert!(err.contains("matches 2 places"), "{err}");
        assert!(err.contains("lines 1 and 3"), "{err}");
        assert!(err.contains("replace_all"), "{err}");

        let (text, summary, _) = apply(FILE, "const a", "let a", true, "a.ts").unwrap();
        assert_eq!(text, "let a = 1;\nconst b = 2;\nlet a = 3;\n");
        assert!(summary.contains("2 places"), "{summary}");
    }

    #[test]
    fn an_edit_may_delete_by_replacing_with_nothing() {
        let (text, ..) = apply(FILE, "const b = 2;\n", "", false, "a.ts").unwrap();
        assert_eq!(text, "const a = 1;\nconst a = 3;\n");
    }
}
