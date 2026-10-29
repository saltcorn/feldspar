//! The **documentation agents read**: the tutorials and references in `docs/`
//! and the admin UI's help topics, as one library of documents an agent can
//! search and read a section at a time.
//!
//! A model asked to "escalate a task when its priority goes high" can write a
//! trigger that looks like every trigger it has seen elsewhere, and be wrong in
//! the ways this server is not like elsewhere. The tutorials say how it is, step
//! by step, and they were not reachable from a tool: an agent read them only if
//! a person pasted them in. Now [`tools`] offers two tools over them —
//! `search_docs` and `read_docs` — to every caller that builds: the
//! administration MCP server and `admin_copilot` through `sc_api::mcp`'s tool
//! set, and any other agent through the `docs` trait.
//!
//! ## One kind of document
//!
//! A help topic is **just another document**: `ownership-formula` sits beside
//! `tutorial-ownership` in the listing, is found by the same search and read by
//! the same tool. It is a template filled in with what a screen is about
//! (`sc-help`), so here it is rendered with an **empty** context — the general
//! text the popup shows on a screen with no table — and given its title as a
//! heading, which is all that makes it look like a file in `docs/`. A model has
//! no use for knowing which of the two a page came from.
//!
//! ## Compiled in
//!
//! The documents are `include_str!`ed, so the documentation an agent reads is
//! the documentation of the binary it is talking to, and a single-file install
//! has its documentation with it. Which files are in is [`FILES`], and which are
//! deliberately not is [`EXCLUDED`]; a test fails on a file in `docs/` that is in
//! neither, so a new document is a decision rather than an omission. Planning
//! notes, design documents and the security review are out: they are about how
//! the server is built rather than how to build with it, they go stale in ways a
//! tutorial is tested not to, and the last of them is a list of weaknesses.
//!
//! ## Search
//!
//! BM25 over **sections** — a heading and the text it owns — with the headings
//! above it counted again, because a heading is what a section is about. Terms
//! are code identifiers as often as words ([`text::terms`]), and there is no
//! embedding model: an agent searches by the names it has been shown, and a
//! lexical index finds a name exactly where a vector one finds things like it.
//! The index is built on first use, once per process.

mod text;
pub mod tools;

use std::collections::HashMap;
use std::sync::OnceLock;

use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

pub use text::slug;

/// The files of `docs/` agents read, by id — the file name without `.md`.
pub const FILES: &[(&str, &str)] = &[
    ("AGG_EXPRS", include_str!("../../../docs/AGG_EXPRS.md")),
    ("CHECKLISTS", include_str!("../../../docs/CHECKLISTS.md")),
    ("GRAPHQL_API", include_str!("../../../docs/GRAPHQL_API.md")),
    ("I18N", include_str!("../../../docs/I18N.md")),
    ("OPERATIONS", include_str!("../../../docs/OPERATIONS.md")),
    ("ROW_HISTORY", include_str!("../../../docs/ROW_HISTORY.md")),
    (
        "tutorial-agents",
        include_str!("../../../docs/tutorial-agents.md"),
    ),
    (
        "tutorial-analytics",
        include_str!("../../../docs/tutorial-analytics.md"),
    ),
    (
        "tutorial-astro",
        include_str!("../../../docs/tutorial-astro.md"),
    ),
    (
        "tutorial-code-framework",
        include_str!("../../../docs/tutorial-code-framework.md"),
    ),
    (
        "tutorial-code-library",
        include_str!("../../../docs/tutorial-code-library.md"),
    ),
    (
        "tutorial-constraints",
        include_str!("../../../docs/tutorial-constraints.md"),
    ),
    (
        "tutorial-file-fields",
        include_str!("../../../docs/tutorial-file-fields.md"),
    ),
    (
        "tutorial-graphql",
        include_str!("../../../docs/tutorial-graphql.md"),
    ),
    (
        "tutorial-history",
        include_str!("../../../docs/tutorial-history.md"),
    ),
    (
        "tutorial-i18n",
        include_str!("../../../docs/tutorial-i18n.md"),
    ),
    (
        "tutorial-live",
        include_str!("../../../docs/tutorial-live.md"),
    ),
    (
        "tutorial-mail",
        include_str!("../../../docs/tutorial-mail.md"),
    ),
    (
        "tutorial-mcp",
        include_str!("../../../docs/tutorial-mcp.md"),
    ),
    (
        "tutorial-models",
        include_str!("../../../docs/tutorial-models.md"),
    ),
    (
        "tutorial-modules",
        include_str!("../../../docs/tutorial-modules.md"),
    ),
    (
        "tutorial-ownership",
        include_str!("../../../docs/tutorial-ownership.md"),
    ),
    (
        "tutorial-payments",
        include_str!("../../../docs/tutorial-payments.md"),
    ),
    (
        "tutorial-payments-credits",
        include_str!("../../../docs/tutorial-payments-credits.md"),
    ),
    (
        "tutorial-payments-saas",
        include_str!("../../../docs/tutorial-payments-saas.md"),
    ),
    (
        "tutorial-python",
        include_str!("../../../docs/tutorial-python.md"),
    ),
    (
        "tutorial-react-todo",
        include_str!("../../../docs/tutorial-react-todo.md"),
    ),
    (
        "tutorial-rest-queries",
        include_str!("../../../docs/tutorial-rest-queries.md"),
    ),
    (
        "tutorial-saltcorn-ui",
        include_str!("../../../docs/tutorial-saltcorn-ui.md"),
    ),
    (
        "tutorial-stan",
        include_str!("../../../docs/tutorial-stan.md"),
    ),
    (
        "tutorial-streams",
        include_str!("../../../docs/tutorial-streams.md"),
    ),
    (
        "tutorial-table-providers",
        include_str!("../../../docs/tutorial-table-providers.md"),
    ),
    (
        "tutorial-triggers",
        include_str!("../../../docs/tutorial-triggers.md"),
    ),
    (
        "tutorial-workflows",
        include_str!("../../../docs/tutorial-workflows.md"),
    ),
];

/// The files of `docs/` agents do not read, by name; a name ending in `*` is a
/// prefix.
pub const EXCLUDED: &[&str] = &[
    "AGENT_EVAL.md",
    "BUILD_ALPINE_MUSL.md",
    "GOALS.md",
    "Saltcorn1_description.md",
    "SECURITY_REVIEW_*",
    "TECHNICAL_DESIGN.md",
    "TODO-*",
    "analytics-ui-goals.md",
    "saltcorn-coding-agent-design*",
];

/// How much of a document one `read_docs` answers, in bytes.
pub const READ_CAP: usize = 12_000;

/// How many hits a search answers unless asked for another number.
pub const DEFAULT_HITS: usize = 5;

/// The most hits a search answers.
pub const MAX_HITS: usize = 10;

/// How long a hit's snippet is, in bytes.
const SNIPPET: usize = 300;

/// One document: a file of `docs/` or a help topic, which are the same thing
/// from here on.
#[derive(Debug)]
pub struct Doc {
    /// What `read_docs` names it by.
    pub id: String,
    /// Its first heading.
    pub title: String,
    /// Its first paragraph, shortened.
    pub summary: String,
    /// The markdown.
    pub text: String,
    sections: Vec<text::Section>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The document.
    pub doc: String,
    /// The section's anchor, for `read_docs`.
    pub section: String,
    /// The headings it sits under, ending with its own.
    pub heading_path: Vec<String>,
    /// The paragraph of it that matched best, shortened.
    pub snippet: String,
    /// BM25.
    pub score: f64,
}

/// One section in the index: which document, which section, and its length in
/// terms.
struct Entry {
    doc: usize,
    section: usize,
    len: u32,
}

/// The documents and the index over their sections.
struct Library {
    docs: Vec<Doc>,
    entries: Vec<Entry>,
    postings: HashMap<String, Vec<(u32, u32)>>,
    avg_len: f64,
}

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| Library::build(sources()))
}

/// Every document as `(id, markdown)`: the files, then the help topics.
fn sources() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = FILES
        .iter()
        .map(|(id, text)| ((*id).to_owned(), (*text).to_owned()))
        .collect();
    for topic in sc_help::TOPICS {
        // A topic that does not render is a bug `sc-help`'s own tests catch;
        // here it would only cost the library one page, not the whole of it.
        if let Ok(body) = sc_help::markdown(topic.id, &json!({})) {
            out.push((topic.id.to_owned(), format!("# {}\n\n{body}", topic.title)));
        }
    }
    out
}

impl Library {
    fn build(sources: Vec<(String, String)>) -> Library {
        let mut docs = Vec::with_capacity(sources.len());
        let mut entries = Vec::new();
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        for (id, mut text) in sources {
            if !text.trim_start().starts_with('#') {
                text = format!("# {id}\n\n{text}");
            }
            let sections = text::sections(&text);
            let title = sections.first().map_or(id.clone(), |s| s.heading.clone());
            let doc_index = docs.len();
            for (i, section) in sections.iter().enumerate() {
                let mut counts: HashMap<String, u32> = HashMap::new();
                let owned = &text[section.start..section.own_end];
                let boost = section.path.join(" ");
                let mut len = 0u32;
                for (_, term) in text::terms(owned).into_iter().chain(text::terms(&boost)) {
                    *counts.entry(term).or_default() += 1;
                    len += 1;
                }
                let entry = entries.len() as u32;
                for (term, tf) in counts {
                    postings.entry(term).or_default().push((entry, tf));
                }
                entries.push(Entry {
                    doc: doc_index,
                    section: i,
                    len,
                });
            }
            docs.push(Doc {
                summary: summary(&text, sections.first()),
                id,
                title,
                text,
                sections,
            });
        }
        let total: u64 = entries.iter().map(|e| u64::from(e.len)).sum();
        let avg_len = total as f64 / entries.len().max(1) as f64;
        Library {
            docs,
            entries,
            postings,
            avg_len,
        }
    }

    fn doc(&self, name: &str) -> Result<(&Doc, Option<String>)> {
        let (name, anchor) = match name.split_once('#') {
            Some((name, anchor)) => (name, Some(anchor.to_owned())),
            None => (name, None),
        };
        let name = name.trim();
        let name = name.strip_prefix("help:").unwrap_or(name);
        let name = name.strip_prefix("./").unwrap_or(name);
        let name = name.strip_prefix("docs/").unwrap_or(name);
        let name = name.strip_suffix(".md").unwrap_or(name);
        match self.docs.iter().find(|d| d.id.eq_ignore_ascii_case(name)) {
            Some(doc) => Ok((doc, anchor.filter(|a| !a.is_empty()))),
            None => Err(Error::not_found(format!(
                "there is no document `{name}`. The documents are: {}. Or search them \
                 with `search_docs`.",
                self.docs
                    .iter()
                    .map(|d| format!("`{}`", d.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    fn search(&self, query: &str, within: Option<usize>, limit: usize) -> Vec<Hit> {
        let mut wanted: Vec<String> = text::terms(query).into_iter().map(|(_, t)| t).collect();
        wanted.sort();
        wanted.dedup();
        let n = self.entries.len() as f64;
        let (k1, b) = (1.2, 0.75);
        let mut scores: HashMap<u32, f64> = HashMap::new();
        for term in &wanted {
            let Some(list) = self.postings.get(term) else {
                continue;
            };
            let idf = (1.0 + (n - list.len() as f64 + 0.5) / (list.len() as f64 + 0.5)).ln();
            for &(entry, tf) in list {
                let e = &self.entries[entry as usize];
                if within.is_some_and(|d| d != e.doc) {
                    continue;
                }
                let tf = f64::from(tf);
                let norm = 1.0 - b + b * f64::from(e.len) / self.avg_len;
                *scores.entry(entry).or_default() += idf * tf * (k1 + 1.0) / (tf + k1 * norm);
            }
        }
        let mut ranked: Vec<(u32, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked
            .into_iter()
            .take(limit)
            .map(|(entry, score)| {
                let e = &self.entries[entry as usize];
                let doc = &self.docs[e.doc];
                let section = &doc.sections[e.section];
                Hit {
                    doc: doc.id.clone(),
                    section: section.id.clone(),
                    heading_path: section.path.clone(),
                    snippet: snippet(&doc.text[section.body..section.own_end], &wanted)
                        .unwrap_or_else(|| section.heading.clone()),
                    score: (score * 100.0).round() / 100.0,
                }
            })
            .collect()
    }
}

/// The first paragraph under a document's title that is prose, shortened.
fn summary(text: &str, title: Option<&text::Section>) -> String {
    let body = title.map_or(text, |t| &text[t.body..]);
    let paragraph = body
        .split("\n\n")
        .map(str::trim)
        .find(|p| {
            !p.is_empty()
                && !p.starts_with(['#', '|', '`', '>', '-', '*', '<', '!', '~'])
                && !p.starts_with(char::is_numeric)
        })
        .unwrap_or("");
    shorten(&text::collapse(paragraph), 240)
}

/// `text` cut at a word boundary before `max` bytes, with an ellipsis.
fn shorten(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let cut = text::floor_boundary(text, max);
    let cut = text[..cut].rfind(' ').unwrap_or(cut);
    format!("{}…", &text[..cut])
}

/// The paragraph of `body` with the most of `wanted` in it, shortened to a
/// window around its first match. `None` when the section has no text of its own.
fn snippet(body: &str, wanted: &[String]) -> Option<String> {
    let best = body
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let found = text::terms(p);
            let distinct = wanted
                .iter()
                .filter(|w| found.iter().any(|(_, t)| t == *w))
                .count();
            let first = found
                .iter()
                .find(|(_, t)| wanted.contains(t))
                .map(|(at, _)| *at);
            (distinct, first, p)
        })
        // The first of the best, not the last: `max_by_key` keeps the last.
        .fold(
            None::<(usize, Option<usize>, &str)>,
            |best, next| match best {
                Some(b) if b.0 >= next.0 => Some(b),
                _ => Some(next),
            },
        )?;
    let (_, first, paragraph) = best;
    let from = first.map_or(0, |at| {
        let back = text::floor_boundary(paragraph, at.saturating_sub(SNIPPET / 3));
        // Start on a word.
        match paragraph[..back].rfind(char::is_whitespace) {
            Some(space) if back > 0 => space + 1,
            _ => back,
        }
    });
    let window = text::collapse(&paragraph[from..]);
    let lead = if from > 0 { "…" } else { "" };
    Some(format!("{lead}{}", shorten(&window, SNIPPET)))
}

/// Every document: `{ doc, title, summary }`, in the listing's order.
pub fn list() -> Json {
    Json::Array(
        library()
            .docs
            .iter()
            .map(|d| json!({ "doc": d.id, "title": d.title, "summary": d.summary }))
            .collect(),
    )
}

/// The sections best matching `query`, best first; within one document when
/// `within` names one.
pub fn search(query: &str, within: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
    if text::terms(query).is_empty() {
        return Err(Error::invalid(
            "the query has no words to search for; name the feature, setting, tool or \
             function you want to read about",
        ));
    }
    let lib = library();
    let within = match within {
        Some(name) => {
            let (doc, _) = lib.doc(name)?;
            lib.docs.iter().position(|d| std::ptr::eq(d, doc))
        }
        None => None,
    };
    Ok(lib.search(query, within, limit.clamp(1, MAX_HITS)))
}

/// Read a document, or one section of it.
///
/// `doc` may carry the section as an anchor (`tutorial-triggers.md#step-2`),
/// which is how the documents link to each other. Without a section, a document
/// that fits in [`READ_CAP`] is answered whole and a longer one as its outline.
/// A section that does not fit is answered as the text it owns before its first
/// subsection, and the subsections to read next.
pub fn read(doc: &str, section: Option<&str>) -> Result<Json> {
    read_in(library(), doc, section)
}

fn read_in(lib: &Library, doc: &str, section: Option<&str>) -> Result<Json> {
    let (doc, anchor) = lib.doc(doc)?;
    let section = section
        .map(|s| s.trim().trim_start_matches('#').to_owned())
        .filter(|s| !s.is_empty())
        .or(anchor);
    let Some(wanted) = section else {
        if doc.text.len() <= READ_CAP {
            return Ok(json!({ "doc": doc.id, "title": doc.title, "text": doc.text }));
        }
        return Ok(json!({
            "doc": doc.id,
            "title": doc.title,
            "summary": doc.summary,
            "chars": doc.text.len(),
            "outline": doc.sections.iter().skip(1).map(|s| json!({
                "section": s.id,
                "heading": s.heading,
                "level": s.level,
                "chars": s.end - s.start,
            })).collect::<Vec<_>>(),
            "note": format!(
                "This document is longer than one read. Read a section of it with \
                 `read_docs` and `section`, or find the part you need with `search_docs` \
                 and `doc: \"{}\"`.",
                doc.id
            ),
        }));
    };
    let found = doc
        .sections
        .iter()
        .position(|s| s.id == wanted)
        .or_else(|| {
            let slugged = slug(&wanted);
            doc.sections.iter().position(|s| s.id == slugged)
        })
        .ok_or_else(|| {
            Error::not_found(format!(
                "`{}` has no section `{wanted}`; its sections are {}",
                doc.id,
                doc.sections
                    .iter()
                    .map(|s| format!("`{}`", s.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
    let s = &doc.sections[found];
    let spans = &doc.text[s.start..s.end];
    let mut out = json!({
        "doc": doc.id,
        "section": s.id,
        "heading_path": s.path,
    });
    if spans.len() <= READ_CAP {
        out["text"] = Json::from(spans);
        return Ok(out);
    }
    let owned = &doc.text[s.start..s.own_end];
    let (shown, cut) = if owned.len() <= READ_CAP {
        (owned, false)
    } else {
        let at = text::floor_boundary(owned, READ_CAP);
        (&owned[..owned[..at].rfind('\n').unwrap_or(at)], true)
    };
    let children: Vec<Json> = doc.sections[found + 1..]
        .iter()
        .take_while(|c| c.start < s.end)
        .filter(|c| c.path.len() == s.path.len() + 1)
        .map(|c| json!({ "section": c.id, "heading": c.heading, "chars": c.end - c.start }))
        .collect();
    out["text"] = Json::from(shown);
    out["truncated"] = Json::from(true);
    out["subsections"] = Json::from(children);
    out["note"] = Json::from(if cut {
        format!(
            "Cut at {} of this section's {} characters. Search within the document \
             (`search_docs` with `doc: \"{}\"`) for the part you need.",
            shown.len(),
            spans.len(),
            doc.id
        )
    } else {
        "This section is longer than one read: above is its text before its first \
         subsection. Read the subsections one at a time."
            .to_owned()
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_in_docs_is_included_or_excluded_on_purpose() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs");
        let excluded = |name: &str| {
            EXCLUDED.iter().any(|e| match e.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == *e,
            })
        };
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            let included = FILES.iter().any(|(id, _)| *id == stem);
            assert!(
                included != excluded(&name),
                "docs/{name} must be in exactly one of sc-docs' FILES and EXCLUDED"
            );
        }
    }

    #[test]
    fn a_help_topic_is_a_document_like_any_other() {
        let listing = list();
        let ids: Vec<&str> = listing
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["doc"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"tutorial-ownership"), "{ids:?}");
        assert!(ids.contains(&"ownership-formula"), "{ids:?}");
        // Ids are unique, so a name reads one document.
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());

        let page = read("ownership-formula", None).unwrap();
        assert_eq!(page["title"], "Ownership formula");
        let text = page["text"].as_str().unwrap();
        assert!(text.starts_with("# Ownership formula"), "{text}");
        // Rendered with no screen behind it: no template syntax left over.
        assert!(!text.contains("{%") && !text.contains("{{"), "{text}");
        // The popup's own link form reads it too.
        assert_eq!(read("help:ownership-formula", None).unwrap(), page);
    }

    #[test]
    fn search_finds_a_section_by_its_words_and_by_an_identifier() {
        let hits = search("ownership formula", None, 5).unwrap();
        assert!(
            hits.iter()
                .any(|h| h.doc == "tutorial-ownership" || h.doc == "ownership-formula"),
            "{hits:?}"
        );
        let hits = search("allow_triggers", None, 10).unwrap();
        assert!(hits.iter().any(|h| h.doc == "tutorial-mcp"), "{hits:?}");
        // Within one document, every hit is from it.
        let hits = search("trigger", Some("tutorial-triggers.md"), 10).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.doc == "tutorial-triggers"));
        for h in &hits {
            assert!(h.snippet.len() <= SNIPPET + 8, "{}", h.snippet);
            // Every hit reads.
            read(&h.doc, Some(&h.section)).unwrap();
        }
        assert!(search("the of and", None, 5).is_err());
    }

    #[test]
    fn a_long_document_reads_as_its_outline_and_then_by_section() {
        let outline = read("tutorial-mcp", None).unwrap();
        assert!(outline.get("text").is_none(), "{outline}");
        let first = outline["outline"][0]["section"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(first, "step-1--turn-it-on");
        let page = read("tutorial-mcp.md#step-1--turn-it-on", None).unwrap();
        let text = page["text"].as_str().unwrap();
        assert!(text.starts_with("## Step 1 — Turn it on"), "{text}");
        assert!(!text.contains("## Step 2"), "{text}");
        // A heading's text finds its section as well as its anchor does.
        assert_eq!(
            read("tutorial-mcp", Some("Step 1 — Turn it on")).unwrap(),
            page
        );
        let err = read("tutorial-mcp", Some("no-such"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("step-2--mint-a-token"), "{err}");
        let err = read("TECHNICAL_DESIGN", None).unwrap_err().to_string();
        assert!(err.contains("tutorial-triggers"), "{err}");
    }

    #[test]
    fn a_section_too_long_to_read_points_at_its_subsections() {
        let lib = Library::build(vec![(
            "big".to_owned(),
            format!(
                "# Big\n\nintro\n\n## Part\n\nlead\n\n### A\n\n{}\n\n### B\n\nb\n",
                "word ".repeat(READ_CAP / 4)
            ),
        )]);
        assert_eq!(lib.docs[0].summary, "intro");
        let page = read_in(&lib, "big", Some("part")).unwrap();
        assert_eq!(page["truncated"], true);
        assert_eq!(page["text"], "## Part\n\nlead\n\n");
        let subs: Vec<&str> = page["subsections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["section"].as_str().unwrap())
            .collect();
        assert_eq!(subs, ["a", "b"]);
        // One that owns too much is cut on a line, and says so.
        let page = read_in(&lib, "big", Some("a")).unwrap();
        assert!(page["text"].as_str().unwrap().len() <= READ_CAP);
        assert!(page["note"].as_str().unwrap().contains("Cut at"), "{page}");
        assert_eq!(lib.search("word", None, 1)[0].section, "a");
    }
}
