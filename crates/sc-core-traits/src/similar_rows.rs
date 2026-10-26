//! `similar_rows` — find the rows nearest a phrase or a question by a vector
//! field (TODO-agent-skills AS7.3; v1's "Retrieval by embedding").
//!
//! The configuration names the table holding the vectors and its vector
//! field. The text the model (or, in `every_message` mode, the user) gives is
//! embedded with **the field's own model** — the one its rows were embedded
//! with, since a distance between vectors of two models means nothing — and
//! the nearest rows are read as the run's caller through the row layer's
//! nearest read (`sc_api::vector`).
//!
//! A vector often belongs to a **chunk** of a document rather than to the
//! document: `chunks(doc → docs, text, embedding)`. With a `relation` — a key
//! field of the vector table — the trait follows it and returns the
//! documents, each once, in the order of their nearest chunk. That second read
//! is the caller's too, so a chunk the caller may see cannot leak a document
//! they may not.
//!
//! The embedding a tool call makes is charged to the run, as every non-chat
//! model call a tool makes is (AS3.5). One made for `every_message`, before
//! the model call rather than inside a tool, has no tool call to ledger it
//! under: it is logged with its cost and kept in a small cache, so the steps
//! of one turn embed the message once.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use sc_agent::{AgentTrait, RunCaller, ToolsContext, TraitCheck, TraitContext, Turn};
use sc_api::rows::RowQuery;
use sc_api::vector::{Nearest, NearestTo};
use sc_catalog::{Catalog, DataFieldKind, Table};
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_llm::ToolSpec;
use sc_types::vector::ATTR_EMBED_MODEL;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::retrieval::{
    self, CFG_DOC_FORMAT, CFG_HIDDEN_FIELDS, CFG_MODE, MODE_EVERY_MESSAGE, MODE_TOOL, Mode,
};
use crate::table::{
    CFG_FIELDS, CFG_TABLE, all_fields, arguments, config_str, configured_fields, configured_table,
    field_list, project, where_expr,
};

/// The vector field the rows are ordered nearest by.
pub const CFG_VECTOR_FIELD: &str = "vector_field";
/// A key field of the vector table leading to the document table; empty when
/// the vector table's rows are the documents.
pub const CFG_RELATION: &str = "relation";
/// How many rows (documents) at most.
pub const CFG_LIMIT: &str = "limit";
/// The rows returned when the admin sets no limit.
pub const DEFAULT_SIMILAR_ROWS: i64 = 10;
/// The tool's one argument.
pub const ARG_QUERY: &str = "phrase_or_question";

/// How many nearest chunks are read per document asked for, when a relation
/// leads to the documents: several chunks of one document are often the
/// nearest few.
const CHUNKS_PER_DOCUMENT: u64 = 5;

/// Find rows nearest some text by a vector field.
pub struct SimilarRows {
    /// Embeddings of user messages, by (table, field, text), so an
    /// `every_message` turn of several steps embeds its message once.
    cache: Mutex<lru::LruCache<(String, String, String), Vec<f32>>>,
}

impl SimilarRows {
    /// The trait, with an empty embedding cache.
    pub fn new() -> SimilarRows {
        SimilarRows {
            cache: Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(64).unwrap_or(NonZeroUsize::MIN),
            )),
        }
    }
}

impl Default for SimilarRows {
    fn default() -> Self {
        SimilarRows::new()
    }
}

/// The tool one `similar_rows` instance offers.
pub fn tool_name(table: &str) -> String {
    format!("search_similar_{table}")
}

#[async_trait::async_trait]
impl AgentTrait for SimilarRows {
    fn name(&self) -> &str {
        "similar_rows"
    }

    fn description(&self) -> &str {
        "Find the rows nearest a phrase or question by a vector field, as a tool or on every \
         user message"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table with the vector field")
                .required(),
            FormField::new(CFG_VECTOR_FIELD, BasicType::Text)
                .label("Vector field")
                .required(),
            FormField::new(CFG_RELATION, BasicType::Text)
                .label("Document relation")
                .sublabel(
                    "Optional: a key field of that table; the rows it leads to are returned \
                     instead of the vector rows",
                ),
            FormField::new(CFG_MODE, BasicType::Text)
                .label("Mode: a tool the model calls, or a search of every user message")
                .options([MODE_TOOL, MODE_EVERY_MESSAGE].map(str::to_owned))
                .default_value(MODE_TOOL),
            FormField::new(CFG_FIELDS, BasicType::Json)
                .label("Fields")
                .sublabel("The document fields the model is shown; empty for all of them")
                .default_value(Json::Array(Vec::new())),
            FormField::new(CFG_HIDDEN_FIELDS, BasicType::Json)
                .label("Hidden fields")
                .default_value(Json::Array(Vec::new())),
            FormField::new(CFG_LIMIT, BasicType::Int)
                .label("Maximum rows")
                .default_value(DEFAULT_SIMILAR_ROWS),
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
        let plan = Plan::of(check.catalog, check.config)?;
        retrieval::mode(check.config)?;
        if plan.limit == 0 {
            return Err(Error::invalid(format!("`{CFG_LIMIT}` must be at least 1")));
        }
        // Text is what the tool is given, so the field must say how to embed it.
        let field = plan
            .vectors
            .field(&plan.vector_field)
            .ok_or_else(|| Error::msg("the vector field went missing"))?;
        let has_model = field
            .base
            .attributes
            .get(ATTR_EMBED_MODEL)
            .and_then(Json::as_str)
            .is_some_and(|m| !m.trim().is_empty());
        if !has_model {
            return Err(Error::invalid(format!(
                "`{}.{}` has no embedding model (its `{ATTR_EMBED_MODEL}` attribute), so a \
                 phrase cannot be turned into one of its vectors; set the field's model",
                plan.vectors.name, plan.vector_field
            )));
        }
        for name in configured_fields(check.config)? {
            if plan.documents.field(&name).is_none() {
                return Err(Error::invalid(format!(
                    "`{}` has no field `{name}`",
                    plan.documents.name
                )));
            }
        }
        retrieval::check_hidden(&plan.documents, &retrieval::hidden_fields(check.config)?)?;
        if let Some(format) = retrieval::doc_format(check.config) {
            retrieval::check_doc_format(&format, &plan.documents, &plan.shown)?;
        }
        Ok(())
    }

    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        if matches!(retrieval::mode(config), Ok(Mode::EveryMessage)) {
            return Vec::new();
        }
        let configured = config_str(config, CFG_TABLE);
        let name = tool_name(&configured);
        let parameters = json!({
            "type": "object",
            "properties": {
                ARG_QUERY: {
                    "type": "string",
                    "description": "The phrase or question to find related rows for.",
                },
            },
            "required": [ARG_QUERY],
            "additionalProperties": false,
        });
        let Ok(plan) = Plan::of(cx.catalog, config) else {
            return vec![ToolSpec::new(
                name,
                format!("Search the `{configured}` table (which can no longer be searched)"),
                parameters,
            )];
        };
        let docs = &plan.documents;
        let mut description = format!("Search the `{}` table", docs.name);
        if !docs.description.trim().is_empty() {
            description.push_str(&format!(" ({})", docs.description.trim()));
        }
        description.push_str(&format!(
            " for the rows most related in meaning to a phrase or a question, most related \
             first, at most {}.",
            plan.limit
        ));
        if retrieval::doc_format(config).is_some() {
            description.push_str(" Each row comes back as a document of text.");
        } else {
            description.push_str(&format!(
                " Rows come back as JSON objects with the fields {}.",
                field_list(docs, &plan.shown)
            ));
        }
        vec![ToolSpec::new(name, description, parameters)]
    }

    async fn call(
        &self,
        config: &Attrs,
        _tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let args = arguments(args, &[ARG_QUERY])?;
        let text = args
            .get(ARG_QUERY)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!("`{ARG_QUERY}` should be the text to search for"))
            })?
            .to_owned();
        let plan = Plan::of(ctx.catalog, config)?;
        let field = plan
            .vectors
            .field(&plan.vector_field)
            .ok_or_else(|| Error::msg("the vector field went missing"))?;
        let embedded = sc_api::vector::embed_texts_metered(
            ctx.catalog,
            &plan.vectors,
            field,
            std::slice::from_ref(&text),
        )
        .await?;
        if let Some(delegate) = ctx.delegate {
            delegate.record_call(&embedded.call).await?;
        }
        let vector = embedded
            .vectors
            .into_iter()
            .next()
            .ok_or_else(|| Error::msg("the embedding provider returned no vector"))?;
        let rows = plan
            .nearest(ctx.catalog, vector, ctx.caller, ctx.evaluator)
            .await?;
        let format = retrieval::doc_format(config);
        Ok(retrieval::result(
            &plan.documents.name,
            &rows,
            format.as_deref(),
            &format!("{text:?}"),
        ))
    }

    async fn on_turn(&self, config: &Attrs, turn: &mut Turn<'_>) -> Result<()> {
        if retrieval::mode(config)? != Mode::EveryMessage {
            return Ok(());
        }
        let Some(catalog) = turn.catalog else {
            return Ok(());
        };
        let Some(message) = turn.user_message().map(str::to_owned) else {
            return Ok(());
        };
        let found = async {
            let plan = Plan::of(catalog, config)?;
            let vector = self.embed_cached(catalog, &plan, &message).await?;
            let rows = plan
                .nearest(catalog, vector, turn.caller, turn.evaluator)
                .await?;
            Ok::<_, Error>((plan, rows))
        }
        .await;
        let (plan, rows) = match found {
            Ok(found) => found,
            Err(e) => {
                sc_log::log_warn!(
                    "agent `{}`: finding rows of `{}` similar to the user's message failed: {e}",
                    turn.agent,
                    config_str(config, CFG_TABLE)
                );
                return Ok(());
            }
        };
        let format = retrieval::doc_format(config);
        turn.append_context(retrieval::paragraph(
            &format!(
                "Rows of the `{}` table nearest in meaning to the user's latest message, most \
                 related first. Use them if they are relevant:",
                plan.documents.name
            ),
            &rows,
            format.as_deref(),
        ));
        Ok(())
    }
}

impl SimilarRows {
    /// `text` embedded with the plan's vector field, from the cache when this
    /// text was embedded for the field before.
    async fn embed_cached(&self, catalog: &Catalog, plan: &Plan, text: &str) -> Result<Vec<f32>> {
        let key = (
            plan.vectors.name.clone(),
            plan.vector_field.clone(),
            text.to_owned(),
        );
        if let Ok(mut cache) = self.cache.lock()
            && let Some(vector) = cache.get(&key)
        {
            return Ok(vector.clone());
        }
        let field = plan
            .vectors
            .field(&plan.vector_field)
            .ok_or_else(|| Error::msg("the vector field went missing"))?;
        let vector = sc_api::vector::embed_texts(catalog, &plan.vectors, field, &[text.to_owned()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::msg("the embedding provider returned no vector"))?;
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(key, vector.clone());
        }
        Ok(vector)
    }
}

/// One configuration, resolved against the catalog.
struct Plan {
    /// The table holding the vectors.
    vectors: Table,
    /// Its vector field.
    vector_field: String,
    /// The relation's key field and the field of the document table it
    /// holds, when there is one.
    relation: Option<(String, String)>,
    /// The table whose rows are returned: the document table, or `vectors`.
    documents: Table,
    /// The document fields the model is shown, in table order.
    shown: Vec<String>,
    /// How many rows at most.
    limit: u64,
}

impl Plan {
    fn of(catalog: &Catalog, config: &Attrs) -> Result<Plan> {
        let vectors = configured_table(catalog, config)?;
        let vector_field = config_str(config, CFG_VECTOR_FIELD);
        if vector_field.is_empty() {
            return Err(Error::invalid(format!("`{CFG_VECTOR_FIELD}` is required")));
        }
        sc_api::vector::vector_field(&vectors, &vector_field)?;
        let relation_name = config_str(config, CFG_RELATION);
        let (relation, documents) = if relation_name.is_empty() {
            (None, vectors.clone())
        } else {
            let Some(field) = vectors.field(&relation_name) else {
                return Err(Error::invalid(format!(
                    "`{}` has no field `{relation_name}` to follow to the documents",
                    vectors.name
                )));
            };
            let DataFieldKind::Key {
                target_table,
                target_field,
                ..
            } = &field.kind
            else {
                return Err(Error::invalid(format!(
                    "`{}.{relation_name}` is not a key field, so it leads to no document table",
                    vectors.name
                )));
            };
            let documents = catalog.require(&target_table.0)?;
            (Some((relation_name, target_field.0.clone())), documents)
        };
        let allowed = configured_fields(config)?;
        let hidden = retrieval::hidden_fields(config)?;
        // Never a vector: a list of numbers the model can do nothing with.
        let shown: Vec<String> = all_fields(&documents)
            .into_iter()
            .filter(|name| allowed.is_empty() || allowed.contains(name))
            .filter(|name| !hidden.contains(name))
            .filter(|name| {
                !documents
                    .field(name)
                    .is_some_and(|f| matches!(f.base.type_.as_basic(), Some(BasicType::Vector(_))))
            })
            .collect();
        let limit = match config.get(CFG_LIMIT) {
            None | Some(Json::Null) => DEFAULT_SIMILAR_ROWS as u64,
            Some(Json::Number(n)) => n.as_u64().ok_or_else(|| {
                Error::invalid(format!("`{CFG_LIMIT}` should be a whole number, got {n}"))
            })?,
            Some(Json::String(s)) if s.trim().parse::<u64>().is_ok() => {
                s.trim().parse::<u64>().unwrap_or_default()
            }
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{CFG_LIMIT}` should be a number, got {other}"
                )));
            }
        };
        Ok(Plan {
            vectors,
            vector_field,
            relation,
            documents,
            shown,
            limit,
        })
    }

    /// The rows nearest `vector`, read as `caller`: the vector rows, or the
    /// documents they lead to, each once, nearest first, at most the limit.
    async fn nearest(
        &self,
        catalog: &Catalog,
        vector: Vec<f32>,
        caller: &RunCaller,
        evaluator: Option<&Arc<dyn JsEvaluator>>,
    ) -> Result<Vec<Json>> {
        let read = match self.relation {
            Some(_) => self.limit.saturating_mul(CHUNKS_PER_DOCUMENT),
            None => self.limit,
        };
        let query = RowQuery::new()
            .nearest(Nearest {
                field: self.vector_field.clone(),
                to: NearestTo::Vector(vector),
            })
            .limit(read);
        let found = sc_api::read_rows_as(
            catalog,
            &self.vectors,
            &query,
            caller.role,
            caller.user.as_ref(),
            evaluator,
        )
        .await?;
        let found: Vec<Json> = found.as_array().cloned().unwrap_or_default();
        let Some((key, target)) = &self.relation else {
            return Ok(found.iter().map(|r| project(r, &self.shown)).collect());
        };
        // The documents, in the order of their nearest chunk, each once.
        let mut ids: Vec<Json> = Vec::new();
        for row in &found {
            match row.get(key) {
                Some(id) if !id.is_null() && !ids.contains(id) => ids.push(id.clone()),
                _ => {}
            }
            if ids.len() as u64 == self.limit {
                break;
            }
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let filter = where_expr(
            &self.documents,
            &all_fields(&self.documents),
            Some(&json!({ target.as_str(): { "in": ids } })),
        )?;
        let docs = sc_api::read_rows_as(
            catalog,
            &self.documents,
            &RowQuery::new().where_(filter),
            caller.role,
            caller.user.as_ref(),
            evaluator,
        )
        .await?;
        let docs: Vec<Json> = docs.as_array().cloned().unwrap_or_default();
        Ok(ids
            .iter()
            .filter_map(|id| docs.iter().find(|d| same_key(d.get(target.as_str()), id)))
            .map(|d| project(d, &self.shown))
            .collect())
    }
}

/// Whether a document's key equals a chunk's reference to it — compared as
/// text too, since one side may read back as a number and the other as a
/// string.
fn same_key(value: Option<&Json>, id: &Json) -> bool {
    let Some(value) = value else { return false };
    let text = |v: &Json| match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    value == id || text(value) == text(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_table() {
        assert_eq!(tool_name("chunks"), "search_similar_chunks");
    }

    #[test]
    fn keys_compare_across_number_and_text() {
        assert!(same_key(Some(&json!(3)), &json!(3)));
        assert!(same_key(Some(&json!("3")), &json!(3)));
        assert!(!same_key(Some(&json!(4)), &json!(3)));
        assert!(!same_key(None, &json!(3)));
    }
}
