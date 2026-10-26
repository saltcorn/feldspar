//! `search_table` — find rows of one configured table by full-text search
//! (TODO-agent-skills AS7.1; v1's "Retrieval by full-text search").
//!
//! The tool is `search_<table>`, and it takes `phrases`: one or several
//! search phrases in the syntax of a web search engine (`"an exact phrase"`,
//! words in any order, `-excluded`, `or`). A row matching any phrase is
//! returned once, best match first, phrase by phrase. The search is the row
//! layer's (`sc_api::search`), inside the caller's own read: the table's
//! full-text index where it answers, a bounded scan where it does not, and a
//! refusal that says what to do about a big table with no index.
//!
//! Its configuration is `query_table`'s (the table, the field allow-list, the
//! row ceiling) plus the three things retrieval adds ([`crate::retrieval`]): a
//! mode, hidden fields, and a document format. In `every_message` mode the
//! trait offers no tool and instead looks up every user message itself.

use sc_agent::{AgentTrait, RunCaller, ToolsContext, TraitCheck, TraitContext, Turn};
use sc_api::rows::RowQuery;
use sc_catalog::{Catalog, Table, full_text_columns};
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_llm::ToolSpec;
use sc_query::TextSearch;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};
use std::sync::Arc;

use crate::retrieval::{
    self, CFG_DOC_FORMAT, CFG_HIDDEN_FIELDS, CFG_MODE, MODE_EVERY_MESSAGE, MODE_TOOL, Mode,
};
use crate::table::{
    CFG_FIELDS, CFG_MAX_ROWS, CFG_TABLE, arguments, check_table_config, config_str,
    configured_table, field_list, max_rows, project, visible_fields,
};

/// The ceiling when the admin sets none: fewer than `query_table`'s, because
/// a search returns documents, which are long.
pub const DEFAULT_SEARCH_ROWS: i64 = 20;

/// The tool's one argument.
pub const ARG_PHRASES: &str = "phrases";

/// Find rows of one table by full-text search.
pub struct SearchTable;

/// The tool one `search_table` instance offers.
pub fn tool_name(table: &str) -> String {
    format!("search_{table}")
}

#[async_trait::async_trait]
impl AgentTrait for SearchTable {
    fn name(&self) -> &str {
        "search_table"
    }

    fn description(&self) -> &str {
        "Find rows of one table by full-text search, as a tool or on every user message"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_MODE, BasicType::Text)
                .label("Mode: a tool the model calls, or a search of every user message")
                .options([MODE_TOOL, MODE_EVERY_MESSAGE].map(str::to_owned))
                .default_value(MODE_TOOL),
            FormField::new(CFG_FIELDS, BasicType::Json)
                .label("Fields")
                .sublabel("The fields searched and shown; empty for all of them")
                .default_value(Json::Array(Vec::new())),
            FormField::new(CFG_HIDDEN_FIELDS, BasicType::Json)
                .label("Hidden fields")
                .sublabel("Searched, but not shown to the model")
                .default_value(Json::Array(Vec::new())),
            FormField::new(CFG_MAX_ROWS, BasicType::Int)
                .label("Maximum rows")
                .default_value(DEFAULT_SEARCH_ROWS),
            FormField::new(CFG_DOC_FORMAT, BasicType::Text)
                .label("Document format")
                .sublabel(
                    "Text sent to the model for each row, with {{field}} for a value; empty \
                     sends the rows as JSON",
                )
                .multiline(),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let table = check_table_config(check.catalog, check.config, DEFAULT_SEARCH_ROWS as u64)?;
        retrieval::mode(check.config)?;
        let visible = visible_fields(&table, check.config)?;
        let hidden = retrieval::hidden_fields(check.config)?;
        retrieval::check_hidden(&table, &hidden)?;
        // Something to search: a text field the tool may reach.
        searched_columns(&table, &visible)?;
        if let Some(format) = retrieval::doc_format(check.config) {
            retrieval::check_doc_format(&format, &table, &retrieval::shown(&visible, &hidden))?;
        }
        Ok(())
    }

    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        if matches!(retrieval::mode(config), Ok(Mode::EveryMessage)) {
            return Vec::new();
        }
        let configured = config_str(config, CFG_TABLE);
        let name = tool_name(&configured);
        let Ok(table) = configured_table(cx.catalog, config) else {
            return vec![ToolSpec::new(
                name,
                format!("Search the `{configured}` table (which no longer exists)"),
                json!({ "type": "object", "properties": {} }),
            )];
        };
        let visible = visible_fields(&table, config).unwrap_or_default();
        let hidden = retrieval::hidden_fields(config).unwrap_or_default();
        let shown = retrieval::shown(&visible, &hidden);
        let ceiling =
            max_rows(config, DEFAULT_SEARCH_ROWS as u64).unwrap_or(DEFAULT_SEARCH_ROWS as u64);
        vec![ToolSpec::new(
            name,
            describe(
                &table,
                &shown,
                ceiling,
                retrieval::doc_format(config).is_some(),
            ),
            json!({
                "type": "object",
                "properties": {
                    ARG_PHRASES: {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": PHRASE_HELP,
                    },
                },
                "required": [ARG_PHRASES],
                "additionalProperties": false,
            }),
        )]
    }

    async fn call(
        &self,
        config: &Attrs,
        _tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let args = arguments(args, &[ARG_PHRASES])?;
        let phrases = phrases(args.get(ARG_PHRASES))?;
        let table = configured_table(ctx.catalog, config)?;
        let found = search(
            ctx.catalog,
            &table,
            config,
            &phrases,
            ctx.caller,
            ctx.evaluator,
        )
        .await?;
        let format = retrieval::doc_format(config);
        let asked = phrases
            .iter()
            .map(|p| format!("{p:?}"))
            .collect::<Vec<_>>()
            .join(" or ");
        let mut out = retrieval::result(&table.name, &found.rows, format.as_deref(), &asked);
        if !found.rows.is_empty() {
            out["more_rows_available"] = json!(found.more);
        }
        Ok(out)
    }

    async fn on_turn(&self, config: &Attrs, turn: &mut Turn<'_>) -> Result<()> {
        if retrieval::mode(config)? != Mode::EveryMessage {
            return Ok(());
        }
        let Some(catalog) = turn.catalog else {
            return Ok(());
        };
        let Some(phrase) = turn.user_message().and_then(retrieval::any_word_query) else {
            return Ok(());
        };
        let found = async {
            let table = configured_table(catalog, config)?;
            let found = search(
                catalog,
                &table,
                config,
                &[phrase],
                turn.caller,
                turn.evaluator,
            )
            .await?;
            Ok::<_, Error>((table, found))
        }
        .await;
        // A lookup that fails does not fail the conversation: the model
        // answers without the rows, and the log says why.
        let (table, found) = match found {
            Ok(found) => found,
            Err(e) => {
                sc_log::log_warn!(
                    "agent `{}`: searching `{}` for the user's message failed: {e}",
                    turn.agent,
                    config_str(config, CFG_TABLE)
                );
                return Ok(());
            }
        };
        let format = retrieval::doc_format(config);
        turn.append_context(retrieval::paragraph(
            &format!(
                "Rows of the `{}` table that a full-text search found for the user's latest \
                 message, best match first. Use them if they are relevant:",
                table.name
            ),
            &found.rows,
            format.as_deref(),
        ));
        Ok(())
    }
}

/// What the model is told the tool does.
fn describe(table: &Table, shown: &[String], ceiling: u64, formatted: bool) -> String {
    let mut out = format!("Search the `{}` table", table.name);
    if !table.description.trim().is_empty() {
        out.push_str(&format!(" ({})", table.description.trim()));
    }
    out.push_str(&format!(
        " by full-text search: rows matching any of the phrases, best match first, at most \
         {ceiling} per call. To search for several things, give every phrase in one call."
    ));
    if formatted {
        out.push_str(" Each row comes back as a document of text.");
    } else {
        out.push_str(&format!(
            " Rows come back as JSON objects with the fields {}.",
            field_list(table, shown)
        ));
    }
    out
}

/// How to write a phrase, told to the model where it writes one.
const PHRASE_HELP: &str = "Search phrases in the syntax of a web search engine: words \
     match in any order, \"double quotes\" match an exact phrase, a -minus excludes a word, \
     and `or` matches either side. Not SQL. A row matching any phrase is returned.";

/// The phrases argument: a non-empty list of non-blank strings.
fn phrases(value: Option<&Json>) -> Result<Vec<String>> {
    let refuse = || {
        Error::invalid(format!(
            "`{ARG_PHRASES}` should be a list of one or more search phrases"
        ))
    };
    let items = value.and_then(Json::as_array).ok_or_else(refuse)?;
    let phrases: Vec<String> = items
        .iter()
        .map(|p| p.as_str().map(|s| s.trim().to_owned()).ok_or_else(refuse))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
    if phrases.is_empty() {
        return Err(refuse());
    }
    Ok(phrases)
}

/// The text columns a search over `visible` matches — `None` for every text
/// column of the table (so the full-text index is the one used), or the text
/// columns the allow-list keeps. Refused when it keeps none.
pub(crate) fn searched_columns(table: &Table, visible: &[String]) -> Result<Option<Vec<String>>> {
    let all = full_text_columns(table);
    let kept: Vec<String> = all
        .iter()
        .filter(|c| visible.contains(c))
        .cloned()
        .collect();
    if kept.is_empty() {
        return Err(Error::invalid(format!(
            "`{}` has no text field this tool may search{}",
            table.name,
            match all.is_empty() {
                true => String::new(),
                false => format!(
                    "; add one of {} to `{CFG_FIELDS}`",
                    all.iter()
                        .map(|c| format!("`{c}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        )));
    }
    Ok((kept != all).then_some(kept))
}

/// The search for `phrase` over the text fields of `visible`.
pub(crate) fn text_search(table: &Table, visible: &[String], phrase: &str) -> Result<TextSearch> {
    let columns = searched_columns(table, visible)?;
    sc_api::search::text_search(table, phrase, columns.as_deref())
}

/// What a search found: the rows, narrowed to what the model is shown, and
/// whether there were more than the ceiling.
struct Found {
    rows: Vec<Json>,
    more: bool,
}

/// Search `table` for each of `phrases` as `caller`, merging the answers:
/// each row once, phrase by phrase, best match first, at most the ceiling.
async fn search(
    catalog: &Catalog,
    table: &Table,
    config: &Attrs,
    phrases: &[String],
    caller: &RunCaller,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
) -> Result<Found> {
    let visible = visible_fields(table, config)?;
    let hidden = retrieval::hidden_fields(config)?;
    let shown = retrieval::shown(&visible, &hidden);
    let ceiling = max_rows(config, DEFAULT_SEARCH_ROWS as u64)?;
    let pk = sc_api::rows::single_pk(table)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    let mut more = false;
    for phrase in phrases {
        let search = text_search(table, &visible, phrase)?;
        let query = RowQuery::new()
            .order_by(vec![sc_api::search::relevance(&search)])
            .searching(search)
            .limit(ceiling.saturating_add(1));
        let found = sc_api::read_rows_as(
            catalog,
            table,
            &query,
            caller.role,
            caller.user.as_ref(),
            evaluator,
        )
        .await?;
        for row in found.as_array().into_iter().flatten() {
            let key = row.get(&pk).cloned().unwrap_or(Json::Null).to_string();
            if !seen.insert(key) {
                continue;
            }
            if rows.len() as u64 == ceiling {
                more = true;
                break;
            }
            rows.push(project(row, &shown));
        }
    }
    Ok(Found { rows, more })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_table() {
        assert_eq!(tool_name("books"), "search_books");
    }

    #[test]
    fn phrases_are_a_non_empty_list_of_text() {
        assert_eq!(
            phrases(Some(&json!(["dune", "  ", "\"exact one\""]))).unwrap(),
            ["dune", "\"exact one\""]
        );
        assert!(phrases(None).is_err());
        assert!(phrases(Some(&json!([]))).is_err());
        assert!(phrases(Some(&json!(["  "]))).is_err());
        assert!(phrases(Some(&json!("dune"))).is_err());
    }
}
