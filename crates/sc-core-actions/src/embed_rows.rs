//! `embed_rows` — back-fill a computed vector field (TODO-agent-skills AS2.5).
//!
//! A vector field with an `embed_from` formula is computed on every write, but
//! the rows a table had before the field was added — or the rows written while
//! the field had no formula — have none. This computes them: every row whose
//! vector is empty, a batch per embedding request, up to `max_rows`. Run it once
//! from the trigger screen after adding the field, or on a schedule to catch up
//! rows written while the embedding provider was down.

use sc_action::{Action, ActionContext, ConfigCheck};
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::rows_scope::{CFG_TABLE, target_table};

/// The vector field to back-fill.
pub const CFG_FIELD: &str = "field";
/// The most rows one run embeds; blank for every empty one.
pub const CFG_MAX_ROWS: &str = "max_rows";

/// Compute the embedding of every row of a table whose vector field is empty.
pub struct EmbedRows;

fn field(config: &Attrs) -> Result<String> {
    config
        .get(CFG_FIELD)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("`{CFG_FIELD}` is required")))
}

fn max_rows(config: &Attrs) -> Result<Option<u64>> {
    match config.get(CFG_MAX_ROWS) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Json::Number(n)) if n.as_u64().is_some() => Ok(n.as_u64()),
        Some(Json::String(s)) => {
            s.trim().parse::<u64>().map(Some).map_err(|_| {
                Error::invalid(format!("`{CFG_MAX_ROWS}` is a whole number, not `{s}`"))
            })
        }
        Some(other) => Err(Error::invalid(format!(
            "`{CFG_MAX_ROWS}` is a whole number, not `{other}`"
        ))),
    }
}

#[async_trait::async_trait]
impl Action for EmbedRows {
    fn name(&self) -> &str {
        "embed_rows"
    }

    fn description(&self) -> &str {
        "Compute the embedding of every row whose vector field is still empty"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_FIELD, BasicType::Text)
                .label("Vector field (one with an embed_from formula)")
                .required(),
            FormField::new(CFG_MAX_ROWS, BasicType::Int)
                .label("Most rows to embed in one run (blank for all)"),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let table = target_table(check.catalog, check.config)?;
        let name = field(check.config)?;
        let (vector, _) = sc_api::vector::vector_field(&table, &name)?;
        if vector
            .base
            .attributes
            .get(sc_types::vector::ATTR_EMBED_FROM)
            .and_then(Json::as_str)
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(Error::invalid(format!(
                "`{}.{name}` has no `embed_from` formula, so there is nothing to compute it from",
                table.name
            )));
        }
        sc_api::rows::single_pk(&table)?;
        max_rows(check.config)?;
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let named = |e: Error| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger));
        let table = target_table(ctx.catalog, ctx.config).map_err(named)?;
        let name = field(ctx.config).map_err(named)?;
        let max = max_rows(ctx.config).map_err(named)?;
        let report = sc_api::vector::embed_rows(ctx.catalog, &table, &name, max)
            .await
            .map_err(named)?;
        Ok(json!({ "embedded": report.embedded, "skipped": report.skipped }))
    }
}
