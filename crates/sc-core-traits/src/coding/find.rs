//! `find_files`: which files and directories in the scope match a glob, newest
//! first (TODO 5.2). It replaces `list_files`.
//!
//! One directory at a time was the wrong shape for finding code: a model looking
//! for "the components" listed `src`, then `src/components`, then a directory
//! inside that, a turn each. A glob over the whole tree finds them in one call,
//! and with no pattern this is still a listing, just a recursive one.
//!
//! - **Newest first.** The file the person was just working on, or the model
//!   just wrote, is the one most likely to matter next.
//! - **Capped, with a hint.** Past the cap the result says how many there were
//!   and how to narrow the search, instead of returning ten thousand paths.
//! - **The same skips and the same access rule as `search_files`.** §9's rule
//!   filters every directory, and `node_modules`, `dist` and the rest of
//!   [`DEFAULT_EXCLUDED_DIRS`] are not descended into, unless `dir` names one.

use sc_agent::TraitContext;
use sc_error::Result;
use sc_files::{DEFAULT_EXCLUDED_DIRS, DEFAULT_MAX_RESULTS, Entry, glob_matches, walk_store};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::search::CFG_MAX_RESULTS;
use crate::files::{FileScope, config_count, optional_string_arg};
use crate::table::arguments;

/// The glob.
const ARG_PATTERN: &str = "pattern";
/// Where to look.
const ARG_DIR: &str = "dir";

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("find_files_{}", scope.slug())
}

/// The tool this scope's file finder contributes.
pub fn spec(scope: &FileScope, config: &Attrs) -> ToolSpec {
    let ceiling =
        config_count(config, CFG_MAX_RESULTS, DEFAULT_MAX_RESULTS as u64).unwrap_or(u64::MAX);
    ToolSpec::new(
        tool_name(scope),
        format!(
            "List files and directories matching a glob, most recently changed first, at most \
             {ceiling}. Directories end in `/`. Dependency and build directories \
             (node_modules, .git, dist, build) are skipped. Use this to find where something \
             lives by name; use the search tool to find it by content."
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_PATTERN: {
                    "type": "string",
                    "description": "`*.tsx` matches file names anywhere; `src/**/*.ts` matches \
                                    paths; `{a,b}` gives alternatives. Omit to list everything.",
                },
                ARG_DIR: {"type": "string", "description": "Start here (default: the root)"},
            },
            "additionalProperties": false,
        }),
    )
}

/// Find matching entries, as the run's caller.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let ceiling = config_count(config, CFG_MAX_RESULTS, DEFAULT_MAX_RESULTS as u64)? as usize;
    let args = arguments(args, &[ARG_PATTERN, ARG_DIR])?;
    let pattern = optional_string_arg(&args, ARG_PATTERN)?;
    let rel_dir = optional_string_arg(&args, ARG_DIR)?;

    let (store, floor) = scope.connect(ctx.catalog).await?;
    let dir = scope.resolve(&rel_dir)?;
    let caller = crate::files::file_caller(ctx.caller);
    sc_files::check_access(store.as_ref(), floor, &dir, &caller).await?;
    let walked = walk_store(store.as_ref(), floor, &caller, &dir, &DEFAULT_EXCLUDED_DIRS).await?;

    let prefix = match dir.is_empty() {
        true => String::new(),
        false => format!("{dir}/"),
    };
    let matched: Vec<Entry> = walked
        .entries
        .into_iter()
        .filter(|entry| {
            let under_dir = entry.path.strip_prefix(&prefix).unwrap_or(&entry.path);
            glob_matches(&pattern, under_dir, &entry.name)
        })
        .collect();
    Ok(Json::String(render(
        scope,
        matched,
        walked.truncated,
        ceiling,
        &pattern,
        &rel_dir,
    )))
}

/// The matches as one path per line, newest first, with the cap's hint.
fn render(
    scope: &FileScope,
    mut matched: Vec<Entry>,
    walk_stopped: bool,
    ceiling: usize,
    pattern: &str,
    dir: &str,
) -> String {
    let what = match (pattern.trim().is_empty(), dir.trim().is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!(" in `{dir}`"),
        (false, true) => format!(" matching `{pattern}`"),
        (false, false) => format!(" matching `{pattern}` in `{dir}`"),
    };
    if matched.is_empty() {
        return format!("No files{what}.");
    }
    // Newest first; entries with no modification time last; then by path, so
    // the order is stable.
    matched.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let total = matched.len();
    let mut out: Vec<String> = matched
        .iter()
        .take(ceiling)
        .map(|entry| {
            let path = scope.relative(&entry.path);
            match entry.is_dir {
                true => format!("{path}/"),
                false => path,
            }
        })
        .collect();
    if total > ceiling {
        out.push(format!(
            "[{ceiling} of {total} entries{what} shown. Narrow the search with `{ARG_PATTERN}` \
             or `{ARG_DIR}`.]"
        ));
    }
    if walk_stopped {
        out.push(format!(
            "[The search stopped after {} entries; results are incomplete.]",
            sc_files::MAX_FILES_SCANNED
        ));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, dir: bool, modified: &str) -> Entry {
        let name = path.rsplit('/').next().unwrap_or(path);
        let mut e = match dir {
            true => Entry::dir(name, path),
            false => Entry::file(name, path, Some(1)),
        };
        e.modified = Some(modified.to_owned());
        e
    }

    fn scope() -> FileScope {
        FileScope {
            store: "src".to_owned(),
            root: "web".to_owned(),
        }
    }

    #[test]
    fn a_tools_name_is_derived_from_its_scope() {
        assert_eq!(tool_name(&scope()), "find_files_src_web");
    }

    #[test]
    fn matches_are_newest_first_with_directories_marked() {
        let found = vec![
            entry("web/src/old.ts", false, "2026-01-01T00:00:00.000Z"),
            entry("web/src", true, "2026-03-01T00:00:00.000Z"),
            entry("web/src/new.ts", false, "2026-02-01T00:00:00.000Z"),
        ];
        assert_eq!(
            render(&scope(), found, false, 100, "", ""),
            "src/\nsrc/new.ts\nsrc/old.ts"
        );
    }

    #[test]
    fn past_the_cap_it_says_how_many_and_how_to_narrow() {
        let found: Vec<Entry> = (0..5)
            .map(|i| entry(&format!("web/f{i}.ts"), false, "2026-01-01T00:00:00.000Z"))
            .collect();
        let out = render(&scope(), found, false, 2, "*.ts", "");
        assert_eq!(
            out,
            "f0.ts\nf1.ts\n[2 of 5 entries matching `*.ts` shown. Narrow the search with \
             `pattern` or `dir`.]"
        );
        assert_eq!(
            render(&scope(), Vec::new(), false, 2, "*.vue", "src"),
            "No files matching `*.vue` in `src`."
        );
    }
}
