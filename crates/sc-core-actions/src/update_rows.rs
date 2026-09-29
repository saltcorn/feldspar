//! `update_rows` — assign formula values to every row a predicate selects.

use sc_error::Result;
use sc_expr::{Formula, Operation};
use sc_types::{BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use sc_action::{Action, ActionContext, ConfigCheck, formula_map};

use crate::rows_scope::{
    CFG_ASSIGNMENTS, CFG_TABLE, CFG_WHERE, Scope, row_id, target_table, where_formula,
    writable_field,
};
use sc_api::rows;

/// Update the rows of a table that a `where` formula selects, each assignment a
/// formula in the target row's own scope.
///
/// Both halves are written in the **target table's** scope, so `status ===
/// "draft"` is the row being updated and `row.status` is the event's row (the
/// distinction is why the scope rule is declared rather than guessed), and
/// `count + 1` reads the value it is about to replace.
///
/// The selected rows are written **one at a time, by primary key**, through the
/// ordinary row write path — not as one bulk `UPDATE`. That is what gives each
/// affected row its own event with its own row payload (decision 2), and it is
/// why a table with no single-column primary key is refused on save rather than
/// at fire time.
pub struct UpdateRows;

#[async_trait::async_trait]
impl Action for UpdateRows {
    fn name(&self) -> &str {
        "update_rows"
    }

    fn description(&self) -> &str {
        "Update every row of a table matching a formula, with computed values"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_WHERE, BasicType::Text)
                .label("Where")
                .required(),
            FormField::new(CFG_ASSIGNMENTS, BasicType::Json)
                .label("Assignments")
                .required(),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let table = target_table(check.catalog, check.config)?;
        // Each matched row is addressed by its key, so a table without one cannot
        // be a target — said here, in front of the admin, rather than at fire
        // time in front of nobody.
        rows::single_pk(&table)?;
        check
            .formula(
                &table.name,
                &where_formula(check.config)?,
                &format!("`{CFG_WHERE}`"),
            )
            .await?;
        for (field, formula) in formula_map(check.config, CFG_ASSIGNMENTS)? {
            writable_field(&table, &field)?;
            check
                .formula(&table.name, &formula, &format!("`{field}`"))
                .await?;
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let table = target_table(ctx.catalog, ctx.config)?;
        let pk = rows::single_pk(&table)?;
        let predicate = where_formula(ctx.config)?;
        let assignments = formula_map(ctx.config, CFG_ASSIGNMENTS)?;
        let scope = Scope::of(ctx)?;

        // The assignments are evaluated against each matched row, so their
        // bindings are prefetched with it.
        let bound: Vec<&Formula> = assignments.iter().map(|(_, f)| f).collect();
        let matched = scope
            .matching_rows(&table, &predicate, &bound, Operation::Update)
            .await?;

        let authority = scope.authority();
        let mut ids = Vec::with_capacity(matched.len());
        for values in &matched {
            let mut body = Map::with_capacity(assignments.len());
            for (field, formula) in &assignments {
                let value = scope
                    .value(formula, values, Operation::Update, &format!("`{field}`"))
                    .await?;
                body.insert(field.clone(), value);
            }
            let (id, id_json) = row_id(&table, &pk, values)?;
            rows::update_row_in(
                ctx.catalog,
                &table,
                &id,
                &Json::Object(body),
                Some(&authority),
                scope.executor(),
            )
            .await?;
            ids.push(id_json);
        }
        Ok(json!({ "updated": ids.len(), "ids": ids }))
    }
}
