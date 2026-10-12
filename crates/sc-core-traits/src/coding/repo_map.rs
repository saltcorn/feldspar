//! `repo_map` — what in this project matters here (TODO 7.5, §11).
//!
//! The map `sc-repomap` builds, over the scope's files as the run's caller may
//! see them: the definitions that matter most for what the agent is working on,
//! as `path` headers with `line│ signature` rows, fitted to a token budget.
//! Offered in every mode, since it only reads.
//!
//! **The focus** is what the model names (paths in the scope, and identifiers).
//! Where it names none, it is the files the run has read or changed so far. The
//! session header's map is focused on what the request mentions.
//!
//! **Tags are cached per store**, by content hash, for the life of the process:
//! a second map of an unchanged tree parses nothing.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use sc_agent::TraitContext;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_files::{DEFAULT_EXCLUDED_DIRS, walk_store};
use sc_llm::ToolSpec;
use sc_repomap::{Focus, Language, MAX_PARSED_BYTES, SourceFile, TagCache};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::state::CodingState;
use crate::files::{FileScope, config_count};
use crate::table::arguments;

/// The budget of the map in the session header, and of a `repo_map` call that
/// names none. `0` leaves the map out of the header.
pub const CFG_REPO_MAP_TOKENS: &str = "repo_map_tokens";

/// [`CFG_REPO_MAP_TOKENS`] when the admin sets none.
pub const DEFAULT_REPO_MAP_TOKENS: u64 = sc_repomap::DEFAULT_TOKENS as u64;

/// The most tokens one `repo_map` call may ask for.
pub const MAX_REPO_MAP_TOKENS: u64 = 8192;

const ARG_FOCUS: &str = "focus";
const ARG_TOKENS: &str = "tokens";

/// The tag caches, one per store.
static CACHES: LazyLock<Mutex<HashMap<String, Arc<TagCache>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_for(store: &str) -> Arc<TagCache> {
    let mut caches = CACHES.lock().unwrap_or_else(|e| e.into_inner());
    caches.entry(store.to_owned()).or_default().clone()
}

/// The tool one configured scope offers.
pub fn tool_name(scope: &FileScope) -> String {
    format!("repo_map_{}", scope.slug())
}

/// The tool.
pub fn spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        tool_name(scope),
        "A map of the project's most relevant definitions (functions, components, types), \
         ranked by how they relate to the focus, as `line│ signature` rows under each file's \
         path. Use it to get oriented and to find which files to read, before reading them."
            .to_owned(),
        json!({
            "type": "object",
            "properties": {
                ARG_FOCUS: {
                    "type": "array", "items": {"type": "string"},
                    "description": "File paths and identifiers to rank around (default: the files read or \
                                    changed in this run)"
                },
                ARG_TOKENS: {
                    "type": "integer", "minimum": 64, "maximum": MAX_REPO_MAP_TOKENS,
                    "description": "How large a map to return, in tokens"
                },
            },
            "additionalProperties": false,
        }),
    )
}

/// The configured budget. `0` is allowed: no map in the header.
pub fn configured_tokens(config: &Attrs) -> Result<u64> {
    match config.get(CFG_REPO_MAP_TOKENS) {
        Some(Json::Number(n)) if n.as_u64() == Some(0) => Ok(0),
        _ => config_count(config, CFG_REPO_MAP_TOKENS, DEFAULT_REPO_MAP_TOKENS),
    }
}

/// The scope's files, tagged: every file the caller may see, outside the
/// excluded directories, with paths relative to the scope. Returns whether the
/// walk stopped at its limit.
pub async fn source_files(
    scope: &FileScope,
    catalog: &Catalog,
    caller: &sc_files::FileCaller,
) -> Result<(Vec<SourceFile>, bool)> {
    let (store, floor) = scope.connect(catalog).await?;
    let root = scope.resolve("")?;
    let walked = walk_store(store.as_ref(), floor, caller, &root, &DEFAULT_EXCLUDED_DIRS).await?;
    let cache = cache_for(&scope.store);
    let mut files = Vec::new();
    for entry in walked.entries.into_iter().filter(|e| !e.is_dir) {
        let rel = scope.relative(&entry.path);
        let parsed = Language::for_path(&rel).is_some()
            && entry
                .size
                .is_none_or(|size| size <= MAX_PARSED_BYTES as u64);
        let tags = match parsed {
            true => match store.read(&entry.path).await {
                Ok(bytes) => cache.tags(&rel, &bytes),
                // A file that vanished or cannot be read mid-walk is listed by
                // name, like any other the map cannot parse.
                Err(_) => Arc::default(),
            },
            false => Arc::default(),
        };
        files.push(SourceFile { path: rel, tags });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((files, walked.truncated))
}

/// The focus `terms` make over `files`.
pub fn focus_of<'a>(files: &[SourceFile], terms: impl IntoIterator<Item = &'a str>) -> Focus {
    let known: BTreeSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    Focus::from_terms(terms, &known)
}

/// The words of free text that could name a file or an identifier.
pub fn terms_of(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || ",;:()[]{}<>\"'`!?".contains(c))
        .map(|word| word.trim_end_matches('.'))
        .filter(|word| word.chars().count() >= 3)
}

/// Build the map, as the run's caller.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let args = arguments(args, &[ARG_FOCUS, ARG_TOKENS])?;
    let tokens = match args.get(ARG_TOKENS) {
        None | Some(Json::Null) => configured_tokens(config)?.max(64),
        Some(value) => value
            .as_u64()
            .filter(|n| *n >= 1)
            .ok_or_else(|| Error::invalid(format!("`{ARG_TOKENS}` should be a positive number")))?,
    }
    .min(MAX_REPO_MAP_TOKENS);
    let named: Vec<String> = match args.get(ARG_FOCUS) {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::invalid(format!("`{ARG_FOCUS}` should be a list of strings"))
                })
            })
            .collect::<Result<_>>()?,
        Some(_) => {
            return Err(Error::invalid(format!(
                "`{ARG_FOCUS}` should be a list of strings"
            )));
        }
    };

    let (files, truncated) =
        source_files(scope, ctx.catalog, &crate::files::file_caller(ctx.caller)).await?;
    let focus = match named.is_empty() {
        false => focus_of(&files, named.iter().map(String::as_str)),
        true => {
            // What the run has read or changed, as the scope names them.
            let state = CodingState::load(ctx.trait_state);
            let in_play: Vec<String> = state
                .seen
                .keys()
                .map(String::as_str)
                .chain(state.ledger.paths())
                .map(|path| scope.relative(path))
                .collect();
            focus_of(&files, in_play.iter().map(String::as_str))
        }
    };
    Ok(Json::String(render(
        scope,
        &files,
        &focus,
        tokens as usize,
        truncated,
    )))
}

/// The map with a line saying what it is of.
pub fn render(
    scope: &FileScope,
    files: &[SourceFile],
    focus: &Focus,
    tokens: usize,
    truncated: bool,
) -> String {
    if files.is_empty() {
        return format!("{} has no files.", capitalised(&scope.label()));
    }
    let map = sc_repomap::repo_map(files, focus, tokens);
    let shown = map.lines().filter(|l| !l.starts_with(' ')).count();
    let mut head = format!(
        "Repo map of {}: {shown} of {} files",
        scope.label(),
        files.len()
    );
    if !focus.files.is_empty() || !focus.names.is_empty() {
        let named: Vec<&str> = focus
            .files
            .iter()
            .chain(&focus.names)
            .map(String::as_str)
            .collect();
        head.push_str(&format!(", focused on {}", named.join(", ")));
    }
    head.push('.');
    if truncated {
        head.push_str(" The walk stopped at its limit, so some files are missing.");
    }
    format!("{head}\n{map}")
}

fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The session header's map (TODO §9): focused on what the brief mentions, at
/// the configured budget, over files the header has already walked. `None`
/// when the budget is `0`.
pub fn header(
    scope: &FileScope,
    config: &Attrs,
    files: &[SourceFile],
    truncated: bool,
    brief: &str,
) -> Option<String> {
    let tokens = configured_tokens(config).ok().filter(|t| *t > 0)?;
    let focus = focus_of(files, terms_of(brief));
    Some(render(scope, files, &focus, tokens as usize, truncated))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_brief_names_files_and_identifiers() {
        let words: Vec<&str> =
            terms_of("Fix `useTasks` in src/App.tsx, then (maybe) the api.ts.").collect();
        assert_eq!(
            words,
            [
                "Fix",
                "useTasks",
                "src/App.tsx",
                "then",
                "maybe",
                "the",
                "api.ts"
            ]
        );
    }
}
