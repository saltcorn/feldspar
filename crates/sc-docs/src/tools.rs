//! The two tools, as every caller offers them: `search_docs` and `read_docs`.
//!
//! The names, the descriptions, the argument schemas and the call are here once,
//! and the callers are thin: `sc_api::mcp` wraps them as two of its
//! administrative tools (so the MCP server and `admin_copilot` offer them), and
//! `sc-core-traits`' `docs` trait as an agent trait for any other agent. A
//! model sees the same two tools whichever of them it is talking through.

use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};

use crate::{DEFAULT_HITS, Hit, MAX_HITS};

/// Search the documentation.
pub const TOOL_SEARCH_DOCS: &str = "search_docs";
/// Read a document, or one section of it.
pub const TOOL_READ_DOCS: &str = "read_docs";

/// Both tools' names, in the order they are offered.
pub const TOOL_NAMES: [&str; 2] = [TOOL_SEARCH_DOCS, TOOL_READ_DOCS];

/// What the model reads about `search_docs`.
pub fn search_description() -> &'static str {
    "Search this server's documentation — its tutorials, references and the admin \
     UI's help pages — for the sections that match a query, best first. Use it \
     **before guessing** how a feature of this server works: triggers, workflows, \
     agents, ownership formulae, the REST and GraphQL APIs, applications, \
     payments, mail and the rest are this server's own and work differently from \
     what you know from elsewhere. Search by the names you have — a tool, setting, \
     field type or function (`run_js_code`, `allow_triggers`) — or by a few words. \
     Each hit has a `doc`, a `section` and a snippet; read the section with \
     `read_docs`."
}

/// The arguments of `search_docs`.
pub fn search_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Names or a few words: `ownership formula user.id`, `save_trigger run_js_code`."
            },
            "doc": {
                "type": "string",
                "description": "Search only this document (an id from a hit or from `read_docs` with no arguments)."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_HITS,
                "description": format!("How many sections to answer; {DEFAULT_HITS} unless set.")
            }
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

/// What the model reads about `read_docs`.
pub fn read_description() -> &'static str {
    "Read this server's documentation. With no arguments, lists the documents, each \
     with a one-line summary. With `doc`, answers that document — whole if it is \
     short, as its outline of sections if not. With `doc` and `section` (an anchor \
     from a search hit or the outline), answers that section and its subsections. \
     A link inside a document (`tutorial-triggers.md#step-2`) can be passed as \
     `doc` as written."
}

/// The arguments of `read_docs`.
pub fn read_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            "doc": {
                "type": "string",
                "description": "The document's id, e.g. `tutorial-triggers` or `ownership-formula`."
            },
            "section": {
                "type": "string",
                "description": "A section's anchor, from a search hit or the document's outline."
            }
        },
        "additionalProperties": false
    })
}

/// One line for a system prompt, saying the tools are there and when to use them.
pub const PROMPT: &str = "This server's documentation is searchable with `search_docs` and \
readable with `read_docs`. When you are unsure how one of its features works — or about to \
write configuration or code against it from memory — search the documentation first.";

/// Run one of the two tools.
pub fn call(tool: &str, args: &Json) -> Result<Json> {
    match tool {
        TOOL_SEARCH_DOCS => {
            let args = arguments(args, &["query", "doc", "limit"])?;
            let query = string(&args, "query")?
                .ok_or_else(|| Error::invalid("`query` is required: what to search for"))?;
            let limit = match args.get("limit") {
                None | Some(Json::Null) => DEFAULT_HITS,
                Some(Json::Number(n)) => n
                    .as_u64()
                    .filter(|n| (1..=MAX_HITS as u64).contains(n))
                    .ok_or_else(|| {
                        Error::invalid(format!("`limit` should be between 1 and {MAX_HITS}"))
                    })? as usize,
                Some(other) => {
                    return Err(Error::invalid(format!(
                        "`limit` should be a number, got {other}"
                    )));
                }
            };
            let hits = crate::search(&query, string(&args, "doc")?.as_deref(), limit)?;
            Ok(json!({
                "hits": hits.iter().map(hit_json).collect::<Vec<_>>(),
                "note": match hits.is_empty() {
                    true => "Nothing matched. Try other words, or list the documents with \
                             `read_docs` and no arguments.",
                    false => "Read a hit with `read_docs` and its `doc` and `section`.",
                },
            }))
        }
        TOOL_READ_DOCS => {
            let args = arguments(args, &["doc", "section"])?;
            match string(&args, "doc")? {
                None => Ok(json!({ "docs": crate::list() })),
                Some(doc) => crate::read(&doc, string(&args, "section")?.as_deref()),
            }
        }
        other => Err(Error::invalid(format!(
            "`{other}` is not a documentation tool; they are `{TOOL_SEARCH_DOCS}` and \
             `{TOOL_READ_DOCS}`"
        ))),
    }
}

fn hit_json(hit: &Hit) -> Json {
    json!({
        "doc": hit.doc,
        "section": hit.section,
        "heading_path": hit.heading_path.join(" › "),
        "snippet": hit.snippet,
        "score": hit.score,
    })
}

fn arguments(args: &Json, allowed: &[&str]) -> Result<Map<String, Json>> {
    let obj = match args {
        Json::Null => Map::new(),
        Json::Object(map) => map.clone(),
        other => {
            return Err(Error::invalid(format!(
                "the arguments should be an object, got {other}"
            )));
        }
    };
    if let Some(key) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Error::invalid(format!(
            "unknown argument `{key}`; this tool takes {}",
            allowed
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(obj)
}

fn string(args: &Map<String, Json>, key: &str) -> Result<Option<String>> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a string, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_then_read_is_the_round_trip() {
        let found = call(
            TOOL_SEARCH_DOCS,
            &json!({ "query": "ownership formula", "limit": 3 }),
        )
        .unwrap();
        let hits = found["hits"].as_array().unwrap();
        assert!(!hits.is_empty() && hits.len() <= 3, "{found}");
        let first = &hits[0];
        let page = call(
            TOOL_READ_DOCS,
            &json!({ "doc": first["doc"], "section": first["section"] }),
        )
        .unwrap();
        assert!(
            page["text"].as_str().is_some_and(|t| !t.is_empty()),
            "{page}"
        );

        let listing = call(TOOL_READ_DOCS, &json!({})).unwrap();
        assert!(listing["docs"].as_array().unwrap().len() > 30);
    }

    #[test]
    fn bad_arguments_are_refused_in_words() {
        let err = call(TOOL_SEARCH_DOCS, &json!({})).unwrap_err().to_string();
        assert!(err.contains("`query` is required"), "{err}");
        let err = call(TOOL_SEARCH_DOCS, &json!({ "query": "x y", "limit": 99 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("between 1 and"), "{err}");
        let err = call(TOOL_READ_DOCS, &json!({ "path": "x" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown argument `path`"), "{err}");
    }
}
