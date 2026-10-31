//! `insert_row` — insert a row whose fields are computed from the event.

use sc_catalog::Authority;
use std::collections::BTreeMap;

use sc_error::Result;
use sc_expr::Operation;
use sc_types::{BasicType, FormField};
use serde_json::{Map, Value as Json};

use sc_action::{Action, ActionContext, ConfigCheck, EVENT_SCOPE, check_formula, formula_map};

use crate::rows_scope::{CFG_TABLE, CFG_VALUES, Scope, target_table, writable_field};
use sc_api::rows;

/// Insert one row into a configured table, each field a formula over the event.
///
/// The archetype of the denormalising trigger: an `insert` on `orders` writes an
/// `audit` row with `row.id` and `user.email` in it. The values are computed and
/// then handed to the **ordinary row write path**, so the target table's own type
/// coercion, rich-type attributes, `File`-field rules and (Phase 4) its own
/// triggers all apply — which is the recursion the depth limit exists for, rather
/// than a second write path that would quietly skip all of it.
pub struct InsertRow;

#[async_trait::async_trait]
impl Action for InsertRow {
    fn name(&self) -> &str {
        "insert_row"
    }

    fn description(&self) -> &str {
        "Insert a row into a table, with each field computed from the event"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_VALUES, BasicType::Json)
                .label("Field values")
                .required(),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let table = target_table(check.catalog, check.config)?;
        for (field, formula) in formula_map(check.config, CFG_VALUES)? {
            writable_field(&table, &field)?;
            // No table scope: the values are computed *from the event*, so a bare
            // `title` is refused by name and `row.title` is what was meant.
            check_formula(check.shape, EVENT_SCOPE, &formula, &format!("`{field}`"))?;
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let table = target_table(ctx.catalog, ctx.config)?;
        let values = formula_map(ctx.config, CFG_VALUES)?;
        let scope = Scope::of(ctx)?;
        // Nothing is being read, so there is no row to bind as the bare scope.
        let no_row = BTreeMap::new();
        let mut body = Map::with_capacity(values.len());
        for (field, formula) in &values {
            let value = scope
                .value(formula, &no_row, Operation::Insert, &format!("`{field}`"))
                .await?;
            body.insert(field.clone(), value);
        }
        rows::create_row_in(
            ctx.catalog,
            &table,
            &Json::Object(body),
            Authority::Caller(&scope.authority()),
            scope.executor(),
        )
        .await
    }
}
