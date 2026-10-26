//! What the two retrieval traits share (TODO-agent-skills AS7.1, AS7.3):
//! `search_table` finds rows by full-text search, `similar_rows` by a vector
//! field's distance, and both then do the same three things with what they
//! found.
//!
//! - **A mode.** `tool` offers the model a tool to call when it wants to look
//!   something up; `every_message` looks up the latest user message itself,
//!   before every model call, and puts what it found in the turn's system
//!   prompt (`AgentTrait::on_turn`, `Turn::append_system`) — v1's "Search on
//!   every user input". Not a second inference call and not a tool.
//! - **A document format.** Rows go to the model as JSON objects, or — when
//!   the admin wrote a `doc_format` — as one text per row, the template filled
//!   in from the row (`{{title}} by {{author}}`). A template names fields, and
//!   a name the rows do not carry is refused on save rather than rendered as a
//!   blank.
//! - **Hidden fields.** v1's "Hide fields": fields the model is not shown.
//!   They are still matched by a search — they are not an access rule, they
//!   are what the prompt is spared — and a field the model may not reach at
//!   all is one left out of `fields`.
//!
//! Both read **as the run's caller**, in either mode: an `every_message`
//! lookup is the read the same person's tool call would have made, so it
//! cannot put a row in the prompt that the person could not have been shown.

use sc_catalog::Table;
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::{Map, Value as Json, json};

/// How a retrieval trait is used: `tool` or `every_message`.
pub const CFG_MODE: &str = "mode";
/// The model calls a tool.
pub const MODE_TOOL: &str = "tool";
/// The latest user message is looked up before every model call.
pub const MODE_EVERY_MESSAGE: &str = "every_message";
/// Fields left out of what the model is shown: a list of names, or one
/// comma-separated string (v1's spelling).
pub const CFG_HIDDEN_FIELDS: &str = "hidden_fields";
/// A template over the row, `{{field}}` per value; empty sends JSON rows.
pub const CFG_DOC_FORMAT: &str = "doc_format";

/// The two modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A tool the model calls.
    Tool,
    /// A lookup of the latest user message before every model call.
    EveryMessage,
}

/// The configured mode; `tool` when none is set.
pub fn mode(config: &Attrs) -> Result<Mode> {
    match config.get(CFG_MODE) {
        None | Some(Json::Null) => Ok(Mode::Tool),
        Some(Json::String(s)) => match s.trim() {
            "" | MODE_TOOL => Ok(Mode::Tool),
            MODE_EVERY_MESSAGE => Ok(Mode::EveryMessage),
            other => Err(Error::invalid(format!(
                "`{CFG_MODE}` is `{MODE_TOOL}` or `{MODE_EVERY_MESSAGE}`, not `{other}`"
            ))),
        },
        Some(other) => Err(Error::invalid(format!(
            "`{CFG_MODE}` is `{MODE_TOOL}` or `{MODE_EVERY_MESSAGE}`, not {other}"
        ))),
    }
}

/// The configured hidden fields.
pub fn hidden_fields(config: &Attrs) -> Result<Vec<String>> {
    let names: Vec<String> = match config.get(CFG_HIDDEN_FIELDS) {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::String(s)) => s.split(',').map(|n| n.trim().to_owned()).collect(),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(|s| s.trim().to_owned()).ok_or_else(|| {
                    Error::invalid(format!(
                        "`{CFG_HIDDEN_FIELDS}` should be a list of field names"
                    ))
                })
            })
            .collect::<Result<_>>()?,
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{CFG_HIDDEN_FIELDS}` should be a list of field names, got {other}"
            )));
        }
    };
    Ok(names.into_iter().filter(|n| !n.is_empty()).collect())
}

/// The configured document format, if any.
pub fn doc_format(config: &Attrs) -> Option<String> {
    config
        .get(CFG_DOC_FORMAT)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The field names a template's `{{…}}` placeholders name, in order.
pub fn placeholders(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        out.push(after[..end].trim().to_owned());
        rest = &after[end + 2..];
    }
    out
}

/// Refuse a template naming a field the rows it formats will not carry.
pub fn check_doc_format(template: &str, table: &Table, fields: &[String]) -> Result<()> {
    for name in placeholders(template) {
        if !fields.contains(&name) {
            return Err(Error::invalid(format!(
                "`{CFG_DOC_FORMAT}` names `{{{{{name}}}}}`, which is not a field of `{}` the \
                 model is shown; it may name {}",
                table.name,
                fields
                    .iter()
                    .map(|f| format!("`{{{{{f}}}}}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(())
}

/// `template` filled in from `row`: text as it is, a missing value or a null
/// as nothing, anything else as its JSON.
pub fn render(template: &str, row: &Json) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        match row.get(name) {
            None | Some(Json::Null) => {}
            Some(Json::String(s)) => out.push_str(s),
            Some(other) => out.push_str(&other.to_string()),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// The tool result for `rows` of `table`: the rows, or their documents when
/// the admin set a format, or — when there are none — a sentence saying so
/// about `asked`.
pub fn result(table: &str, rows: &[Json], format: Option<&str>, asked: &str) -> Json {
    if rows.is_empty() {
        return json!({
            "table": table,
            "count": 0,
            "message": format!("No rows of `{table}` match {asked}."),
        });
    }
    let mut out = Map::new();
    out.insert("table".to_owned(), json!(table));
    out.insert("count".to_owned(), json!(rows.len()));
    match format {
        Some(template) => {
            let documents: Vec<String> = rows.iter().map(|r| render(template, r)).collect();
            out.insert("documents".to_owned(), json!(documents));
        }
        None => {
            out.insert("rows".to_owned(), json!(rows));
        }
    }
    Json::Object(out)
}

/// The paragraph an `every_message` lookup adds to the system prompt: a
/// heading, then each row on its own line (its document, or its JSON).
/// Empty when nothing was found, which `Turn::append_system` drops.
pub fn paragraph(heading: &str, rows: &[Json], format: Option<&str>) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut out = heading.to_owned();
    for row in rows {
        out.push('\n');
        match format {
            Some(template) => out.push_str(&render(template, row)),
            None => out.push_str(&row.to_string()),
        }
    }
    out
}

/// A full-text query matching **any** of the words of a user message — the
/// phrase an `every_message` search runs, or `None` when the message has no
/// word worth searching for.
///
/// A message is not a search phrase: every word of "what do we have about
/// the Roman roads?" must not have to match, so the words are joined by `or`
/// and relevance decides. Short words are dropped (they are mostly stop
/// words), as is anything a search would read as an operator.
pub fn any_word_query(message: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    for word in message.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.chars().count() < 3 || word == "or" || words.contains(&word) {
            continue;
        }
        words.push(word);
        if words.len() == MAX_MESSAGE_WORDS {
            break;
        }
    }
    (!words.is_empty()).then(|| words.join(" or "))
}

/// The most words of a message an `every_message` search looks for.
const MAX_MESSAGE_WORDS: usize = 24;

/// `visible` without the hidden names, in the same order.
pub fn shown(visible: &[String], hidden: &[String]) -> Vec<String> {
    visible
        .iter()
        .filter(|f| !hidden.contains(f))
        .cloned()
        .collect()
}

/// Refuse a hidden field that is not a field of `table`.
pub fn check_hidden(table: &Table, hidden: &[String]) -> Result<()> {
    for name in hidden {
        if table.field(name).is_none() {
            return Err(Error::invalid(format!(
                "`{CFG_HIDDEN_FIELDS}` names `{name}`, which `{}` does not have",
                table.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_is_filled_in_from_the_row() {
        let row = json!({"title": "Dune", "pages": 412, "notes": null});
        assert_eq!(
            render("{{title}} ({{ pages }} pages){{notes}}", &row),
            "Dune (412 pages)"
        );
        // An unclosed placeholder is text, not a crash.
        assert_eq!(render("{{title}} {{oops", &row), "Dune {{oops");
        assert_eq!(placeholders("{{a}} and {{ b }}"), ["a", "b"]);
    }

    #[test]
    fn the_mode_defaults_to_a_tool_and_refuses_anything_else() {
        let cfg = |v: Json| -> Attrs { json!({ "mode": v }).as_object().cloned().unwrap() };
        assert_eq!(mode(&Attrs::new()).unwrap(), Mode::Tool);
        assert_eq!(mode(&cfg(json!("tool"))).unwrap(), Mode::Tool);
        assert_eq!(
            mode(&cfg(json!("every_message"))).unwrap(),
            Mode::EveryMessage
        );
        let err = mode(&cfg(json!("sometimes"))).unwrap_err().to_string();
        assert!(
            err.contains("sometimes") && err.contains("every_message"),
            "{err}"
        );
    }

    #[test]
    fn hidden_fields_are_a_list_or_v1s_comma_separated_string() {
        let cfg =
            |v: Json| -> Attrs { json!({ "hidden_fields": v }).as_object().cloned().unwrap() };
        assert_eq!(
            hidden_fields(&cfg(json!("notes, owner ,"))).unwrap(),
            ["notes", "owner"]
        );
        assert_eq!(hidden_fields(&cfg(json!(["notes"]))).unwrap(), ["notes"]);
        assert!(hidden_fields(&cfg(json!(3))).is_err());
    }

    #[test]
    fn a_message_becomes_a_search_for_any_of_its_words() {
        assert_eq!(
            any_word_query("What do we have about the Roman roads? Roman!").as_deref(),
            Some("what or have or about or the or roman or roads")
        );
        assert_eq!(any_word_query("ok?"), None);
    }

    #[test]
    fn an_empty_result_says_what_was_asked() {
        let out = result("books", &[], None, "\"whales\"");
        assert_eq!(out["count"], json!(0));
        assert!(out["message"].as_str().unwrap().contains("whales"));
        let rows = [json!({"title": "Dune"})];
        let out = result("books", &rows, Some("Book: {{title}}"), "x");
        assert_eq!(out["documents"], json!(["Book: Dune"]));
        assert!(out.get("rows").is_none());
    }
}
