//! `search_files`: grep the configured scope, server-side (§11.3, TODO 5.3).
//!
//! The search itself is [`sc_files::search_store`], which is also what the
//! admin API's `searchFiles` endpoint (and therefore the IDE's find-in-files)
//! runs, so what a person finds in the editor is what the model finds.
//!
//! The result is **grep's own shape**, `path:line: text`, with `path-line- text`
//! for context lines and `--` between groups. Every model has read a great deal
//! of grep output, and the shape costs a fraction of the tokens the same hits
//! cost as JSON. Past the cap it says so and says how to narrow the query,
//! rather than letting the model believe it has seen every hit.

use std::collections::BTreeSet;

use sc_agent::TraitContext;
use sc_error::{Error, Result};
use sc_files::{DEFAULT_MAX_RESULTS, MAX_LINE_CHARS, SearchQuery, search_store};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Map, Value as Json, json};

use crate::files::{FileScope, config_count, optional_bool_arg, optional_string_arg, string_arg};
use crate::table::arguments;

/// The ceiling on matches one call may return (also `find_files`' ceiling on
/// entries).
pub const CFG_MAX_RESULTS: &str = "max_results";

/// The most context lines either side of a match.
pub const MAX_CONTEXT_LINES: u64 = 10;

/// What to look for.
const ARG_PATTERN: &str = "pattern";
/// Whether that is a regular expression.
const ARG_REGEX: &str = "regex";
/// Whether case matters.
const ARG_CASE: &str = "case_sensitive";
/// Which files to look in.
const ARG_GLOB: &str = "glob";
/// Where to start.
const ARG_DIR: &str = "dir";
/// Lines of context either side.
const ARG_CONTEXT: &str = "context";

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("search_files_{}", scope.slug())
}

/// The tool this scope's search contributes.
pub fn spec(scope: &FileScope, config: &Attrs) -> ToolSpec {
    let ceiling =
        config_count(config, CFG_MAX_RESULTS, DEFAULT_MAX_RESULTS as u64).unwrap_or(u64::MAX);
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Search the contents of the project's text files and return matching lines as \
             `path:line: text`, at most {ceiling}. The pattern is literal text unless `regex` \
             is set, and case-insensitive unless `case_sensitive` is. Dependency and build \
             directories (node_modules, .git, dist, build) are skipped. Narrow a broad search \
             with `glob` or `dir`."
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_PATTERN: {"type": "string", "description": "Text to find (literal unless `regex` is true)"},
                ARG_REGEX: {"type": "boolean", "description": "Treat the pattern as a regular expression"},
                ARG_CASE: {"type": "boolean", "description": "Match case exactly (default false)"},
                ARG_GLOB: {"type": "string", "description": "Only files matching this glob, e.g. `*.tsx`"},
                ARG_DIR: {"type": "string", "description": "Only files under this directory"},
                ARG_CONTEXT: {
                    "type": "integer", "minimum": 0, "maximum": MAX_CONTEXT_LINES,
                    "description": "Lines of context to show around each match",
                },
            },
            "required": [ARG_PATTERN],
            "additionalProperties": false,
        }),
    )
}

/// Run one search, at the run's caller's role.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let ceiling = config_count(config, CFG_MAX_RESULTS, DEFAULT_MAX_RESULTS as u64)?;
    let args = arguments(
        args,
        &[
            ARG_PATTERN,
            ARG_REGEX,
            ARG_CASE,
            ARG_GLOB,
            ARG_DIR,
            ARG_CONTEXT,
        ],
    )?;

    let dir = optional_string_arg(&args, ARG_DIR)?;
    let glob = optional_string_arg(&args, ARG_GLOB)?;
    let context = context_arg(&args)?;
    let query = SearchQuery {
        pattern: string_arg(&args, ARG_PATTERN)?,
        regex: optional_bool_arg(&args, ARG_REGEX, false)?,
        case_sensitive: optional_bool_arg(&args, ARG_CASE, false)?,
        whole_word: false,
        glob: (!glob.trim().is_empty()).then_some(glob),
        dir: scope.resolve(&dir)?,
        max_results: ceiling as usize,
        ..SearchQuery::literal("")
    };

    let (store, floor) = scope.connect(ctx.catalog).await?;
    // The caller's own role, so a search cannot report a line out of a file
    // the caller could not have opened.
    let found = search_store(
        store.as_ref(),
        floor,
        &crate::files::file_caller(ctx.caller),
        &query,
    )
    .await?;
    if found.hits.is_empty() {
        return Ok(Json::String(format!(
            "No matches for `{}` in {} files.",
            query.pattern, found.files_scanned
        )));
    }

    // The matched lines, per file in the order the walk found them.
    let mut files: Vec<(String, BTreeSet<usize>)> = Vec::new();
    for hit in &found.hits {
        let line = hit.line as usize;
        match files.last_mut() {
            Some((path, lines)) if *path == hit.path => {
                lines.insert(line);
            }
            _ => files.push((hit.path.clone(), BTreeSet::from([line]))),
        }
    }

    let mut out = Vec::new();
    for (path, lines) in &files {
        let rel = scope.relative(path);
        match context {
            0 => {
                // The hit already carries its (capped) line.
                for hit in found.hits.iter().filter(|h| &h.path == path) {
                    let entry = format!("{rel}:{}: {}", hit.line, hit.text);
                    if out.last() != Some(&entry) {
                        out.push(entry);
                    }
                }
            }
            n => {
                // The search opened this file already, as this caller.
                let bytes = store.read(path).await?;
                let text = String::from_utf8_lossy(&bytes);
                with_context(&mut out, &rel, &text, lines, n);
            }
        }
    }
    if found.truncated {
        out.push(format!(
            "[Stopped at {} matches; there are more. Narrow the query with `{ARG_GLOB}`, \
             `{ARG_DIR}` or a more specific pattern.]",
            found.hits.len()
        ));
    }
    Ok(Json::String(out.join("\n")))
}

/// The context argument, bounded.
fn context_arg(args: &Map<String, Json>) -> Result<usize> {
    match args.get(ARG_CONTEXT) {
        None | Some(Json::Null) => Ok(0),
        Some(Json::Number(n)) => match n.as_u64() {
            Some(n) => Ok(n.min(MAX_CONTEXT_LINES) as usize),
            None => Err(Error::invalid(format!(
                "`{ARG_CONTEXT}` should be a whole number, got {n}"
            ))),
        },
        Some(other) => Err(Error::invalid(format!(
            "`{ARG_CONTEXT}` should be a number, got {other}"
        ))),
    }
}

/// grep -C: the matched lines of one file with `n` lines either side, groups
/// that touch merged and separate groups divided by `--`.
fn with_context(out: &mut Vec<String>, rel: &str, text: &str, matched: &BTreeSet<usize>, n: usize) {
    let lines: Vec<&str> = text.lines().collect();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &line in matched {
        let (first, last) = (line.saturating_sub(n).max(1), (line + n).min(lines.len()));
        match groups.last_mut() {
            Some((_, end)) if first <= *end + 1 => *end = (*end).max(last),
            _ => groups.push((first, last)),
        }
    }
    for (first, last) in groups {
        if !out.is_empty() {
            out.push("--".to_owned());
        }
        for number in first..=last {
            let text = cap(lines.get(number - 1).copied().unwrap_or(""));
            out.push(match matched.contains(&number) {
                true => format!("{rel}:{number}: {text}"),
                false => format!("{rel}-{number}- {text}"),
            });
        }
    }
}

/// A line capped as search hits are.
fn cap(line: &str) -> &str {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        None => line,
        Some((end, _)) => &line[..end],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_scope() {
        let scope = FileScope {
            store: "src".to_owned(),
            root: "web".to_owned(),
        };
        assert_eq!(tool_name(&scope), "search_files_src_web");
    }

    #[test]
    fn context_groups_merge_when_they_touch_and_are_divided_when_they_do_not() {
        let text: String = (1..=12).map(|n| format!("l{n}\n")).collect();
        let mut out = Vec::new();
        with_context(&mut out, "a.ts", &text, &BTreeSet::from([2, 4, 10]), 1);
        assert_eq!(
            out,
            [
                "a.ts-1- l1",
                "a.ts:2: l2",
                "a.ts-3- l3",
                "a.ts:4: l4",
                "a.ts-5- l5",
                "--",
                "a.ts-9- l9",
                "a.ts:10: l10",
                "a.ts-11- l11",
            ]
        );
    }
}
