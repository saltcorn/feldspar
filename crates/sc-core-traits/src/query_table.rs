//! `query_table` — read one configured table (§11.3).
//!
//! The first trait, and the one that sets the pattern the rest follow. Three
//! properties are load-bearing:
//!
//! - **The tool's description and JSON schema are generated from the table's own
//!   fields.** A model told "call `query_books` with a `where` object" and left
//!   to guess the column names will guess, and a guess that misses is a wasted
//!   turn the person watching pays for. So the schema *is* the table: the
//!   filterable fields are enumerated, each with its type, and the ordering key
//!   is an enum of exactly the fields that can be ordered by.
//! - **The read goes through `sc_api::read_rows_as`**, which is the same §7.3
//!   rule the REST API applies. The caller is the run's, so an agent chatting
//!   with a user below the table's read floor sees exactly the rows that user's
//!   ownership formula grants and no others. This is not a check this crate
//!   performs — it is a check this crate is unable to skip.
//! - **The configuration bounds the tool, not the other way round.** The field
//!   allow-list is what the model may see, filter on and order by; `max_rows` is
//!   a ceiling the `limit` argument is clamped to rather than a default it can
//!   raise. An agent cannot widen its own grant by asking nicely.
//!
//! Two things it grew for v1's Table skill (TODO-agent-skills AS7.2, AS7.4):
//!
//! - **`search`**, offered only when the table has a full-text search
//!   constraint: a phrase matched against the text fields the tool may see,
//!   inside the same read, and ordering by relevance unless `order_by` says
//!   otherwise. Offered only then because a search the index does not answer
//!   is a scan, which the row layer refuses past a few thousand rows — a tool
//!   that worked on the admin's test data and failed on the real table would be
//!   worse than none.
//! - **`show_results`**: the rows go to the **person** as a display part
//!   (AS6.3), through a named view where one is set, and the model is told how
//!   many there were and their keys, not what they say. The rows were for the
//!   person; repeating them through the model costs tokens twice and paraphrases
//!   the data.

use sc_agent::{AgentTrait, DisplayPart, MAX_DISPLAY_ROWS, ToolsContext, TraitCheck, TraitContext};
use sc_catalog::{Table, full_text_constraint};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_query::{Expr, OrderBy};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use sc_api::rows::RowQuery;

use crate::table::{
    ARG_WHERE, CFG_FIELDS, CFG_MAX_ROWS, CFG_TABLE, WHERE_HELP, arguments, check_table_config,
    config_str, configured_table, field_list, max_rows, project, queryable_field, queryable_fields,
    visible_fields, where_expr, where_schema,
};

/// The ceiling when the admin sets none.
///
/// Small on purpose. A tool result becomes context the model pays for on every
/// subsequent turn, so an unbounded read is not merely slow — it is a
/// conversation that gets more expensive and less coherent with each answer. An
/// admin who wants a thousand rows says so.
pub const DEFAULT_MAX_ROWS: i64 = 50;

/// Whether the rows go to the person rather than to the model (AS7.4).
pub const CFG_SHOW_RESULTS: &str = "show_results";
/// The view the shown rows are rendered through, by name; empty for a plain
/// table.
pub const CFG_RESULTS_VIEW: &str = "results_view";

/// Read rows of one configured table.
pub struct QueryTable;

/// The tool one `query_table` instance offers, derived from its table.
///
/// Public because it is the answer to "what will this be called?", which the
/// admin UI wants before the agent is saved and the collision check
/// (§11.2) wants at the moment of saving.
pub fn tool_name(table: &str) -> String {
    format!("query_{table}")
}

#[async_trait::async_trait]
impl AgentTrait for QueryTable {
    fn name(&self) -> &str {
        "query_table"
    }

    fn description(&self) -> &str {
        "Read rows of one table, filtered, ordered and bounded"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_FIELDS, BasicType::Json)
                .label("Fields")
                .default_value(Json::Array(Vec::new())),
            FormField::new(CFG_MAX_ROWS, BasicType::Int)
                .label("Maximum rows")
                .default_value(DEFAULT_MAX_ROWS),
            FormField::new(CFG_SHOW_RESULTS, BasicType::Bool)
                .label("Show the rows to the user instead of the model")
                .default_value(false),
            FormField::new(CFG_RESULTS_VIEW, BasicType::Text)
                .label("View to show them through")
                .sublabel("Optional: the name of a view over this table")
                .show_if(CFG_SHOW_RESULTS, vec![Json::Bool(true)]),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        check_table_config(check.catalog, check.config, DEFAULT_MAX_ROWS as u64)?;
        shows_results(check.config)?;
        Ok(())
    }

    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let catalog = cx.catalog;
        let configured = config_str(config, CFG_TABLE);
        let name = tool_name(&configured);
        // The table is gone, or the configuration never validated. Keep the tool
        // under the name the admin's configuration gives it — the collision check
        // and the admin UI both ask this question of agents that do not validate
        // — and describe what can still be described.
        let Ok(table) = configured_table(catalog, config) else {
            return vec![ToolSpec::new(
                name,
                format!("Read rows of the `{configured}` table (which no longer exists)"),
                json!({ "type": "object", "properties": {} }),
            )];
        };
        let visible = visible_fields(&table, config).unwrap_or_default();
        let ceiling = max_rows(config, DEFAULT_MAX_ROWS as u64).unwrap_or(DEFAULT_MAX_ROWS as u64);
        let searchable = searchable(&table, &visible);
        let shown = shows_results(config).unwrap_or(false);
        vec![ToolSpec::new(
            name,
            describe(&table, &visible, ceiling, searchable, shown),
            parameters(&table, &visible, ceiling, searchable),
        )]
    }

    async fn call(
        &self,
        config: &Attrs,
        _tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let table = configured_table(ctx.catalog, config)?;
        let visible = visible_fields(&table, config)?;
        let ceiling = max_rows(config, DEFAULT_MAX_ROWS as u64)?;
        let args = arguments(args, &ARGUMENTS)?;

        let filter = where_expr(&table, &visible, args.get(ARG_WHERE))?;
        let mut order = order_by(&table, &visible, &args)?;
        let limit = requested_limit(&args, ceiling)?;
        let search = match args.get(ARG_SEARCH).filter(|v| !v.is_null()) {
            None => None,
            Some(phrase) => Some(search_for(&table, &visible, phrase)?),
        };

        // One more row than asked for, so "there are more" can be reported
        // rather than guessed at. A model that cannot tell a complete answer
        // from a truncated one will state the truncated one as fact.
        let mut query = RowQuery::new().where_(filter);
        if let Some(search) = search {
            // Best match first, unless the model asked for another order.
            if order.is_empty() {
                order.push(sc_api::search::relevance(&search));
            }
            query = query.searching(search);
        }
        let query = query.order_by(order).limit(limit.saturating_add(1));
        let found = sc_api::read_rows_as(
            ctx.catalog,
            &table,
            &query,
            ctx.caller.role,
            ctx.caller.user.as_ref(),
            ctx.evaluator,
        )
        .await?;

        let mut rows: Vec<Json> = found.as_array().cloned().unwrap_or_default();
        let more = rows.len() as u64 > limit;
        rows.truncate(limit as usize);
        let rows: Vec<Json> = rows.iter().map(|row| project(row, &visible)).collect();
        if shows_results(config)? {
            return show(&table, config, &visible, rows, more, ctx);
        }
        Ok(json!({
            "table": table.name,
            "rows": rows,
            "count": rows.len(),
            "more_rows_available": more,
        }))
    }
}

// --- the tool, described from the table -------------------------------------

/// What the model is told this tool does — the table, what it returns, how much
/// of it, and every field by name and type.
///
/// This is the whole of what the model has to go on when it decides whether to
/// call and what to ask for, so it is prose with the schema in it rather than a
/// label. Listing the fields here as well as in the parameter schema is
/// deliberate: the description is the part a model reads when *choosing* a tool,
/// and "does this table even have an author?" is the question it is choosing on.
fn describe(
    table: &Table,
    visible: &[String],
    ceiling: u64,
    searchable: bool,
    shown: bool,
) -> String {
    let mut out = format!("Read rows of the `{}` table", table.name);
    if !table.description.trim().is_empty() {
        out.push_str(&format!(" ({})", table.description.trim()));
    }
    out.push_str(&format!(
        ". Returns matching rows as JSON objects, at most {ceiling} per call, \
         and says whether more were available.\n\nFields: {}.",
        field_list(table, visible)
    ));
    if visible.len() < table.fields.len() {
        out.push_str(" Other fields of this table are not available to you.");
    }
    out.push_str("\n\n");
    out.push_str(WHERE_HELP);
    if searchable {
        out.push_str(
            "\n\n`search` matches a phrase against the text fields, in the syntax of a web \
             search engine, best match first unless `order_by` is given.",
        );
    }
    if shown {
        out.push_str(
            "\n\nThe rows are shown to the user directly, and you are told only how many \
             there were and their keys: do not repeat or describe them.",
        );
    }
    out
}

/// The tool's JSON Schema, generated from the table.
fn parameters(table: &Table, visible: &[String], ceiling: u64, searchable: bool) -> Json {
    // Only the stored fields: `where` and `order_by` become SQL, and a
    // calculated field has no column to put in it.
    let names: Vec<&str> = queryable_fields(table, visible)
        .iter()
        .map(|f| f.base.name.as_str())
        .collect();
    let mut where_ = where_schema(table, visible);
    if let Some(obj) = where_.as_object_mut() {
        obj.insert(
            "description".to_owned(),
            json!(
                "Which rows: field name → an exact value, or an object with one \
                 of the operator keys. Omit it for every row."
            ),
        );
    }
    let mut schema = json!({
        "type": "object",
        "properties": {
            ARG_WHERE: where_,
            ARG_ORDER_BY: {
                "type": "string",
                "description": "Sort the rows by this field.",
                "enum": names,
            },
            ARG_DESCENDING: {
                "type": "boolean",
                "description":
                    "Sort from the largest down instead of the smallest up. \
                     Only meaningful with `order_by`.",
            },
            ARG_LIMIT: {
                "type": "integer",
                "description": format!(
                    "At most this many rows. The ceiling — and the default — is {ceiling}."
                ),
                "minimum": 1,
                "maximum": ceiling,
            },
        },
        "additionalProperties": false,
    });
    if searchable {
        schema["properties"][ARG_SEARCH] = json!({
            "type": "string",
            "description":
                "Only rows whose text fields match this phrase: words match in any order, \
                 \"double quotes\" an exact phrase, a -minus excludes a word, `or` either side. \
                 Not SQL.",
        });
    }
    schema
}

/// Whether the tool offers `search`: the table has a full-text constraint, and
/// the tool may see one of its text fields.
fn searchable(table: &Table, visible: &[String]) -> bool {
    full_text_constraint(table).is_some()
        && crate::search_table::searched_columns(table, visible).is_ok()
}

/// The search a `search` argument asks for, refused by name where the tool
/// does not offer one.
fn search_for(table: &Table, visible: &[String], phrase: &Json) -> Result<sc_query::TextSearch> {
    if full_text_constraint(table).is_none() {
        return Err(Error::invalid(format!(
            "`{ARG_SEARCH}` is not an argument of this tool: `{}` has no full-text search index",
            table.name
        )));
    }
    let phrase = phrase.as_str().ok_or_else(|| {
        Error::invalid(format!("`{ARG_SEARCH}` should be a phrase, got {phrase}"))
    })?;
    crate::search_table::text_search(table, visible, phrase)
}

/// Whether the configuration shows the rows to the person.
fn shows_results(config: &Attrs) -> Result<bool> {
    match config.get(CFG_SHOW_RESULTS) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => Ok(*b),
        Some(other) => Err(Error::invalid(format!(
            "`{CFG_SHOW_RESULTS}` should be true or false, got {other}"
        ))),
    }
}

/// Show `rows` to the person and tell the model what was shown (AS7.4).
fn show(
    table: &Table,
    config: &Attrs,
    visible: &[String],
    mut rows: Vec<Json>,
    more: bool,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let pk = sc_api::rows::single_pk(table)?;
    let keys: Vec<Json> = rows
        .iter()
        .map(|r| r.get(&pk).cloned().unwrap_or(Json::Null))
        .collect();
    rows.truncate(MAX_DISPLAY_ROWS);
    let count = keys.len();
    let view = config_str(config, CFG_RESULTS_VIEW);
    ctx.show(DisplayPart::Rows {
        title: Some(format!("{} ({count})", table.name)),
        view: (!view.is_empty()).then_some(view),
        columns: visible.to_vec(),
        rows,
    })?;
    Ok(json!({
        "table": table.name,
        "count": count,
        "more_rows_available": more,
        "shown_to_user": true,
        "keys": { pk: keys },
        "note": format!(
            "The {count} rows were shown to the user. Do not repeat or describe them."
        ),
    }))
}

// --- the tool's arguments ---------------------------------------------------

/// The field to order by.
const ARG_ORDER_BY: &str = "order_by";
/// Whether that ordering descends.
const ARG_DESCENDING: &str = "descending";
/// How many rows at most.
const ARG_LIMIT: &str = "limit";
/// A full-text search phrase (AS7.2).
const ARG_SEARCH: &str = "search";

/// Every argument this tool takes — so one it does not is refused **by name**.
/// `search` is in the list even where the schema does not offer it, so that a
/// model guessing it is told why it is not available rather than that it is
/// unknown.
const ARGUMENTS: [&str; 5] = [
    ARG_WHERE,
    ARG_ORDER_BY,
    ARG_DESCENDING,
    ARG_LIMIT,
    ARG_SEARCH,
];

/// The `limit`, clamped to the configured ceiling.
///
/// Clamped rather than refused: a model that asked for 500 from a tool bounded
/// at 50 has made a recoverable mistake, and 50 rows plus `more_rows_available`
/// tells it exactly that. A `limit` of zero or a negative one is not a smaller
/// request, it is a malformed one.
fn requested_limit(args: &Map<String, Json>, ceiling: u64) -> Result<u64> {
    match args.get(ARG_LIMIT) {
        None | Some(Json::Null) => Ok(ceiling),
        Some(Json::Number(n)) => match n.as_i64() {
            Some(n) if n >= 1 => Ok((n as u64).min(ceiling)),
            _ => Err(Error::invalid(format!(
                "`{ARG_LIMIT}` should be a whole number of at least 1, got {n}"
            ))),
        },
        Some(other) => Err(Error::invalid(format!(
            "`{ARG_LIMIT}` should be a number, got {other}"
        ))),
    }
}

/// The `ORDER BY`, from `order_by` plus `descending`.
fn order_by(table: &Table, visible: &[String], args: &Map<String, Json>) -> Result<Vec<OrderBy>> {
    let Some(key) = args.get(ARG_ORDER_BY).filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let key = key.as_str().ok_or_else(|| {
        Error::invalid(format!(
            "`{ARG_ORDER_BY}` should be a field name, got {key}"
        ))
    })?;
    // Ordering happens in the database, so a calculated field — which has no
    // column — cannot be ordered by any more than it can be filtered on.
    let field = queryable_field(table, visible, key, ARG_ORDER_BY)?;
    let descending = match args.get(ARG_DESCENDING) {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_DESCENDING}` should be true or false, got {other}"
            )));
        }
    };
    let col = Expr::col(field.base.name.clone());
    Ok(vec![match descending {
        true => OrderBy::desc(col),
        false => OrderBy::asc(col),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_table() {
        assert_eq!(tool_name("books"), "query_books");
    }

    #[test]
    fn the_limit_is_clamped_to_the_ceiling_rather_than_refused() {
        let args = |json: Json| json.as_object().cloned().unwrap();
        assert_eq!(requested_limit(&args(json!({})), 50).unwrap(), 50);
        assert_eq!(requested_limit(&args(json!({"limit": 5})), 50).unwrap(), 5);
        assert_eq!(
            requested_limit(&args(json!({"limit": 500})), 50).unwrap(),
            50
        );
        // Zero and a negative are malformed, not small.
        assert!(requested_limit(&args(json!({"limit": 0})), 50).is_err());
        assert!(requested_limit(&args(json!({"limit": -3})), 50).is_err());
        assert!(requested_limit(&args(json!({"limit": "5"})), 50).is_err());
    }
}
