//! `delete_rows` — delete every row a predicate selects.

use sc_error::Result;
use sc_expr::Operation;
use sc_types::{BasicType, FormField};
use serde_json::{Value as Json, json};

use sc_action::{Action, ActionContext, ConfigCheck};

use crate::rows_scope::{CFG_TABLE, CFG_WHERE, Scope, row_id, target_table, where_formula};
use sc_api::rows;

/// Delete the rows of a table that a `where` formula selects.
///
/// Same predicate semantics and same one-row-at-a-time write path as
/// [`UpdateRows`](super::UpdateRows) — so each deleted row raises its own delete
/// event carrying the row as it was — and the same save-time refusal of a table
/// with no single-column primary key.
///
/// The predicate is **required**. `delete_rows` with an omitted `where` would be
/// "delete everything", and an emptied table is not something a missing setting
/// should be able to cause; an admin who means it writes `true`.
pub struct DeleteRows;

#[async_trait::async_trait]
impl Action for DeleteRows {
    fn name(&self) -> &str {
        "delete_rows"
    }

    fn description(&self) -> &str {
        "Delete every row of a table matching a formula"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_WHERE, BasicType::Text)
                .label("Where")
                .required(),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let table = target_table(check.catalog, check.config)?;
        rows::single_pk(&table)?;
        check
            .formula(
                &table.name,
                &where_formula(check.config)?,
                &format!("`{CFG_WHERE}`"),
            )
            .await
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let table = target_table(ctx.catalog, ctx.config)?;
        let pk = rows::single_pk(&table)?;
        let predicate = where_formula(ctx.config)?;
        let scope = Scope::of(ctx)?;
        let matched = scope
            .matching_rows(&table, &predicate, &[], Operation::Delete)
            .await?;

        let authority = scope.authority();
        let mut ids = Vec::with_capacity(matched.len());
        for values in &matched {
            let (id, id_json) = row_id(&table, &pk, values)?;
            rows::delete_row_in(ctx.catalog, &table, &id, Some(&authority), scope.executor())
                .await?;
            ids.push(id_json);
        }
        Ok(json!({ "deleted": ids.len(), "ids": ids }))
    }
}
