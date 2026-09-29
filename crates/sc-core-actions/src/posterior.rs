//! A posterior's two actions: `write_posterior`, which writes a variable's
//! summary into rows, and `fit_model`, which refits a model (Stan TODO §16,
//! 7.4–7.5) — together what makes "refit and write back every night" two steps
//! of one workflow.
//!
//! The write-back itself is [`write_posterior`], shared with the admin API's
//! `writePosterior`: one path from a summary to rows, through the row layer, so
//! a write-back is validated, ownership-checked and fires the target table's
//! own triggers whichever way it was asked for.

use std::sync::Arc;
use std::time::Duration;

use sc_action::{Action, ActionContext, ConfigCheck};
use sc_api::rows;
use sc_auth::ROLE_ADMIN;
use sc_catalog::{CallerContext, Catalog, Table};
use sc_error::{Error, Result};
use sc_model::{
    FitStarter, Model, ModelInstance, ModelRegistry, PosteriorView, PosteriorWrite, WriteMode,
    fitted_cleanly, instance_coordinates, plan_write, summarise_variable,
};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::rows_scope::writable_field;

/// The model, by name.
pub const CFG_MODEL: &str = "model";
/// A particular fit, by id — empty for the model's active one.
pub const CFG_INSTANCE: &str = "instance";
/// The variable written back.
pub const CFG_VARIABLE: &str = "variable";
/// `update` or `insert`.
pub const CFG_MODE: &str = "mode";
/// Statistic → field.
pub const CFG_STATISTICS: &str = "statistics";
/// The table an insert writes into.
pub const CFG_TABLE: &str = "table";
/// The coordinates an insert writes.
pub const CFG_COORDINATES: &str = "coordinates";
/// The field an insert writes the instance id into.
pub const CFG_INSTANCE_FIELD: &str = "instance_field";
/// Only these elements.
pub const CFG_ELEMENTS: &str = "elements";

/// Statistics that may go into an integer field, rounded: the effective sample
/// sizes, which are counts of draws.
const COUNT_STATISTICS: [&str; 2] = ["ess_bulk", "ess_tail"];

/// Write `write` of `instance` (a fit of `model`) into rows, as `authority`,
/// through `executor` — **the** write-back, for the action and the API alike.
///
/// Update mode writes the rows of the table the variable's one axis is about
/// (the rows dimension's dataset's table), matched by key; insert mode makes a
/// row per element in `write.table`. Every target field is checked before any
/// row is written: statistics go into a float field (or an integer one, for
/// an effective sample size).
pub async fn write_posterior(
    catalog: &Catalog,
    model: &Model,
    instance: &ModelInstance,
    write: &PosteriorWrite,
    authority: &CallerContext,
    executor: &rows::Executor,
) -> Result<Json> {
    if instance.model != model.id {
        return Err(Error::invalid(format!(
            "fit {} is not a fit of model `{}`",
            instance.id, model.name
        )));
    }
    let view = PosteriorView::of(instance, &write.variable)?;
    let selection = sc_model::Selection::from_json(write.elements.as_ref())?;
    let summary = summarise_variable(catalog, instance, &write.variable, &selection).await?;
    let plan = plan_write(
        &view,
        &summary,
        write,
        &instance_coordinates(instance)?,
        instance.id,
    )?;
    let table = match plan.mode {
        WriteMode::Update => {
            let dataset = plan.dataset.as_deref().unwrap_or_default();
            catalog.require(dataset_table(model, dataset)?)?
        }
        WriteMode::Insert => catalog.require(plan.table.as_deref().unwrap_or_default())?,
    };
    check_targets(&table, write)?;
    let mut written = 0usize;
    for row in &plan.rows {
        let values = dates_as_days(&table, write, row.values.clone())?;
        let body = Json::Object(round_counts(&table, write, values));
        match (&plan.mode, &row.key) {
            (WriteMode::Update, Some(key)) => {
                rows::update_row_in(catalog, &table, key, &body, Some(authority), executor)
                    .await
                    .map_err(|e| {
                        Error::invalid(format!(
                            "writing `{}` into row `{key}` of `{}`: {e}",
                            write.variable, table.name
                        ))
                    })?;
            }
            _ => {
                rows::create_row_in(catalog, &table, &body, Some(authority), executor)
                    .await
                    .map_err(|e| {
                        Error::invalid(format!(
                            "inserting `{}` into `{}`: {e}",
                            write.variable, table.name
                        ))
                    })?;
            }
        }
        written += 1;
    }
    Ok(json!({
        "variable": write.variable,
        "mode": write.mode,
        "table": table.name,
        "instance": instance.id.to_string(),
        "written": written,
    }))
}

/// The table of `model`'s dataset called `name` — `main` or a related one's.
fn dataset_table<'m>(model: &'m Model, name: &str) -> Result<&'m str> {
    if name == sc_model::MAIN_DATASET {
        return Ok(model.table());
    }
    model
        .related
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.dataset.table.as_str())
        .ok_or_else(|| {
            Error::invalid(format!(
                "model `{}` no longer has the dataset `{name}` this fit's dimension is over",
                model.name
            ))
        })
}

/// Every target field exists, is writable, and can hold what goes into it.
fn check_targets(table: &Table, write: &PosteriorWrite) -> Result<()> {
    for (field, statistic) in write.fields() {
        writable_field(table, field)?;
        let Some(statistic) = statistic else {
            continue;
        };
        let ty = table
            .field(field)
            .and_then(|f| f.base.type_.as_basic().cloned());
        let fits = match ty {
            None => true,
            Some(BasicType::Float | BasicType::Decimal) => true,
            Some(BasicType::Int) => COUNT_STATISTICS.contains(&statistic),
            Some(_) => false,
        };
        if !fits {
            return Err(Error::invalid(format!(
                "`{field}` of `{}` is {}, and `{statistic}` is a number: write it into a float \
                 field{}",
                table.name,
                ty.map_or_else(|| "not a number".to_owned(), |t| t.name().to_owned()),
                if COUNT_STATISTICS.contains(&statistic) {
                    " (or an integer one)"
                } else {
                    ""
                }
            )));
        }
    }
    Ok(())
}

/// An effective sample size written into an integer field is rounded down.
fn round_counts(table: &Table, write: &PosteriorWrite, mut values: Attrs) -> Attrs {
    for (statistic, field) in &write.statistics {
        let int = table
            .field(field)
            .and_then(|f| f.base.type_.as_basic().cloned())
            == Some(BasicType::Int);
        if int && COUNT_STATISTICS.contains(&statistic.as_str()) {
            if let Some(x) = values.get(field).and_then(Json::as_f64) {
                values.insert(field.clone(), Json::from(x.floor() as i64));
            }
        }
    }
    values
}

/// A time grid's coordinate is an instant (`2025-05-01T00:00:00Z`), which a
/// `date` field refuses. Written into one, it is its day — when it is a
/// midnight, as every step of a grid of days, weeks, months or years is. An
/// hour's instant would lose its hour, so that is refused, naming a timestamp
/// field as the way out.
fn dates_as_days(table: &Table, write: &PosteriorWrite, mut values: Attrs) -> Result<Attrs> {
    for coordinate in &write.coordinates {
        let field = &coordinate.field;
        let date = table
            .field(field)
            .and_then(|f| f.base.type_.as_basic().cloned())
            == Some(BasicType::Date);
        let Some(Json::String(instant)) = values.get(field).filter(|_| date) else {
            continue;
        };
        let Some((day, time)) = instant.split_once('T') else {
            continue;
        };
        if !matches!(time, "00:00:00Z" | "00:00:00.000Z" | "00:00:00+00:00") {
            return Err(Error::invalid(format!(
                "`{field}` of `{}` is a date, and `{instant}` is not a midnight: write this                  axis into a timestamp field",
                table.name
            )));
        }
        let day = day.to_owned();
        values.insert(field.clone(), Json::from(day));
    }
    Ok(values)
}

/// A required, non-empty configuration string.
fn configured(config: &Attrs, key: &str) -> Result<String> {
    optional(config, key).ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

/// An optional configuration string, empty treated as absent.
fn optional(config: &Attrs, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The fit a configuration names: its `instance`, or the model's active one.
async fn configured_fit(catalog: &Catalog, config: &Attrs, model: &Model) -> Result<ModelInstance> {
    match optional(config, CFG_INSTANCE) {
        Some(raw) => {
            let id = uuid::Uuid::parse_str(&raw)
                .map(sc_model::InstanceId)
                .map_err(|_| {
                    Error::invalid(format!("`{CFG_INSTANCE}`: `{raw}` is not a fit id"))
                })?;
            let instance = sc_model::require_model_instance(catalog, id).await?;
            if instance.model != model.id {
                return Err(Error::invalid(format!(
                    "fit {id} is not a fit of model `{}`",
                    model.name
                )));
            }
            Ok(instance)
        }
        None => sc_model::active_model_instance(catalog, model.id)
            .await?
            .ok_or_else(|| {
                Error::invalid(format!(
                    "model `{}` has no active fit, so there is nothing to write back: fit it and \
                     mark the fit active",
                    model.name
                ))
            }),
    }
}

/// The write-back an action's configuration describes.
fn configured_write(config: &Attrs) -> Result<PosteriorWrite> {
    let mut write = serde_json::Map::new();
    write.insert(
        "variable".to_owned(),
        Json::from(configured(config, CFG_VARIABLE)?),
    );
    write.insert(
        "mode".to_owned(),
        Json::from(optional(config, CFG_MODE).unwrap_or_else(|| "update".to_owned())),
    );
    for key in [CFG_STATISTICS, CFG_COORDINATES, CFG_ELEMENTS] {
        if let Some(value) = config.get(key).filter(|v| !v.is_null()) {
            write.insert(key.to_owned(), value.clone());
        }
    }
    for key in [CFG_TABLE, CFG_INSTANCE_FIELD] {
        if let Some(value) = optional(config, key) {
            write.insert(key.to_owned(), Json::from(value));
        }
    }
    if !write.contains_key(CFG_STATISTICS) {
        write.insert(CFG_STATISTICS.to_owned(), json!({}));
    }
    PosteriorWrite::from_json(&Json::Object(write))
}

/// Write a fitted posterior's summary into rows (§16).
pub struct WritePosterior;

#[async_trait::async_trait]
impl Action for WritePosterior {
    fn name(&self) -> &str {
        "write_posterior"
    }

    fn description(&self) -> &str {
        "Write a fitted model's posterior summary of one variable into rows: into the rows it \
         is about, or as new rows of another table"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_MODEL, BasicType::Text)
                .label("Model")
                .server_query(sc_model::MODELS_QUERY)
                .required(),
            FormField::new(CFG_INSTANCE, BasicType::Text).label("Fit (defaults to the active one)"),
            FormField::new(CFG_VARIABLE, BasicType::Text)
                .label("Variable")
                .required(),
            FormField::new(CFG_MODE, BasicType::Text)
                .label("Mode")
                .options(["update", "insert"])
                .default_value("update"),
            FormField::new(CFG_STATISTICS, BasicType::Json)
                .label("Statistic → field (mean, sd, q5, q50, q95, …)")
                .required(),
            FormField::new(CFG_TABLE, BasicType::Text).label("Table (insert)"),
            FormField::new(CFG_COORDINATES, BasicType::Json).label("Coordinates → fields (insert)"),
            FormField::new(CFG_INSTANCE_FIELD, BasicType::Text)
                .label("Field for the fit's id (insert)"),
            FormField::new(CFG_ELEMENTS, BasicType::Json).label("Only these elements"),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let name = configured(check.config, CFG_MODEL)?;
        let model = sc_model::load_model_by_name(check.catalog, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no model named `{name}`")))?;
        let write = configured_write(check.config)?;
        if let Some(raw) = optional(check.config, CFG_INSTANCE) {
            let id = uuid::Uuid::parse_str(&raw)
                .map(sc_model::InstanceId)
                .map_err(|_| {
                    Error::invalid(format!("`{CFG_INSTANCE}`: `{raw}` is not a fit id"))
                })?;
            let instance = sc_model::require_model_instance(check.catalog, id).await?;
            if instance.model != model.id {
                return Err(Error::invalid(format!(
                    "fit {id} is not a fit of model `{name}`"
                )));
            }
        }
        // The targets, where the table is known without a fit: an insert's own,
        // or an update's through the fit that would answer now. An update of a
        // model not yet fitted is checked when it runs.
        match write.mode {
            WriteMode::Insert => {
                let table = check
                    .catalog
                    .require(write.table.as_deref().unwrap_or_default())?;
                check_targets(&table, &write)?;
            }
            WriteMode::Update => {
                if let Ok(instance) = configured_fit(check.catalog, check.config, &model).await {
                    let view = PosteriorView::of(&instance, &write.variable)?;
                    let dimension = view
                        .axes
                        .first()
                        .and_then(|a| a.dimension.clone())
                        .and_then(|d| {
                            instance_coordinates(&instance)
                                .ok()?
                                .dimension(&d)
                                .map(|d| d.dataset.clone())
                        });
                    if let Some(dataset) = dimension {
                        if let Ok(table) = dataset_table(&model, &dataset) {
                            check_targets(&check.catalog.require(table)?, &write)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let named = |e: Error| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger));
        let name = configured(ctx.config, CFG_MODEL).map_err(named)?;
        let model = sc_model::require_model(ctx.catalog, &name)
            .await
            .map_err(named)?;
        let instance = configured_fit(ctx.catalog, ctx.config, &model)
            .await
            .map_err(named)?;
        let write = configured_write(ctx.config).map_err(named)?;
        // The trigger's own authority, carrying the firing chain — as
        // `predict_row` writes — and the step's transaction when there is one.
        let authority =
            CallerContext::new(ROLE_ADMIN, ctx.event.user.clone()).chained(ctx.chain.clone());
        write_posterior(
            ctx.catalog,
            &model,
            &instance,
            &write,
            &authority,
            &rows::Executor::of(ctx.transaction()),
        )
        .await
        .map_err(named)
    }
}

/// Whether `fit_model` waits for the fit by default.
const DEFAULT_WAIT: bool = true;

/// How often a waiting `fit_model` reads its instance's row back.
const WAIT_POLL: Duration = Duration::from_millis(250);

/// The configured `fit_model` settings.
const CFG_ACTIVATE: &str = "activate";
const CFG_WAIT: &str = "wait";
const CFG_NAME: &str = "name";

/// Start a fit of a named model from a trigger or a workflow (carried from the
/// predictive-models milestone; Stan TODO 7.5).
///
/// By default it **waits** for the fit and answers how it ended, so the next
/// step of a workflow — a `write_posterior` of the model's active fit — sees
/// the new one. With `activate`, a fit that finished without warnings is made
/// the model's active fit; one with warnings is kept and left inactive, for
/// the admin to read. Not waiting starts the fit and answers at once, and the
/// job activates it when it finishes.
pub struct FitModel {
    providers: Arc<ModelRegistry>,
    fits: Arc<dyn FitStarter>,
}

impl FitModel {
    /// The action, validating against `providers` and starting fits through
    /// `fits`.
    pub fn new(providers: Arc<ModelRegistry>, fits: Arc<dyn FitStarter>) -> FitModel {
        FitModel { providers, fits }
    }
}

/// A boolean setting, `default` when unset.
fn flag(config: &Attrs, key: &str, default: bool) -> Result<bool> {
    match config.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(Json::Bool(b)) => Ok(*b),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` must be true or false, not `{other}`"
        ))),
    }
}

#[async_trait::async_trait]
impl Action for FitModel {
    fn name(&self) -> &str {
        "fit_model"
    }

    fn description(&self) -> &str {
        "Fit a model again, and optionally make the new fit active when it has no warnings"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_MODEL, BasicType::Text)
                .label("Model")
                .server_query(sc_model::MODELS_QUERY)
                .required(),
            FormField::new(CFG_ACTIVATE, BasicType::Bool)
                .label("Make the new fit active when it has no warnings")
                .default_value(false),
            FormField::new(CFG_WAIT, BasicType::Bool)
                .label("Wait for the fit to finish")
                .default_value(DEFAULT_WAIT),
            FormField::new(CFG_NAME, BasicType::Text).label("Name of the new fit"),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let name = configured(check.config, CFG_MODEL)?;
        sc_model::load_model_by_name(check.catalog, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no model named `{name}`")))?;
        flag(check.config, CFG_ACTIVATE, false)?;
        flag(check.config, CFG_WAIT, DEFAULT_WAIT)?;
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let named = |e: Error| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger));
        let name = configured(ctx.config, CFG_MODEL).map_err(named)?;
        let activate = flag(ctx.config, CFG_ACTIVATE, false).map_err(named)?;
        let wait = flag(ctx.config, CFG_WAIT, DEFAULT_WAIT).map_err(named)?;
        let model = sc_model::require_model(ctx.catalog, &name)
            .await
            .map_err(named)?;
        // Checked before the row exists, as the Fit button checks.
        sc_model::validate_model(ctx.catalog, &self.providers, &model, None)
            .await
            .map_err(named)?;
        let mut instance = ModelInstance::starting(model.id);
        instance.name = optional(ctx.config, CFG_NAME)
            .unwrap_or_else(|| format!("fitted by trigger `{}`", ctx.trigger));
        let started = self
            .fits
            .start_fit(&model, instance, activate && !wait)
            .await
            .map_err(named)?;
        if !wait {
            return Ok(json!({
                "instance": started.id.to_string(),
                "status": started.status.as_str(),
            }));
        }
        let mut finished = loop {
            let row = sc_model::require_model_instance(ctx.catalog, started.id)
                .await
                .map_err(named)?;
            if row.status != sc_model::FitStatus::Fitting {
                break row;
            }
            tokio::time::sleep(WAIT_POLL).await;
        };
        if activate && fitted_cleanly(&finished) {
            finished.active = true;
            sc_model::save_model_instance(ctx.catalog, &finished)
                .await
                .map_err(named)?;
        }
        Ok(json!({
            "instance": finished.id.to_string(),
            "status": finished.status.as_str(),
            "active": finished.active,
            "error": finished.error(),
            "warnings": finished
                .attributes
                .get(sc_model::ATTR_WARNINGS)
                .cloned()
                .unwrap_or_else(|| json!([])),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, Json)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn an_actions_configuration_is_a_write_back() {
        let write = configured_write(&config(&[
            (CFG_VARIABLE, json!("alpha")),
            (CFG_STATISTICS, json!({ "mean": "alpha_mean" })),
        ]))
        .unwrap();
        assert_eq!(write.mode, WriteMode::Update);
        assert_eq!(write.statistics["mean"], "alpha_mean");

        let write = configured_write(&config(&[
            (CFG_VARIABLE, json!("y_future")),
            (CFG_MODE, json!("insert")),
            (CFG_TABLE, json!("forecasts")),
            (CFG_STATISTICS, json!({ "mean": "mean", "q5": "lower" })),
            (
                CFG_COORDINATES,
                json!([{ "axis": "day.future", "field": "day" }]),
            ),
            (CFG_INSTANCE_FIELD, json!("instance")),
        ]))
        .unwrap();
        assert_eq!(write.table.as_deref(), Some("forecasts"));
        assert_eq!(write.coordinates[0].field, "day");

        let err = configured_write(&config(&[(CFG_VARIABLE, json!("alpha"))])).unwrap_err();
        assert!(err.to_string().contains("writes no statistic"), "{err}");
    }

    /// `forecasts(day date, at timestamptz)`.
    fn forecasts() -> Table {
        use sc_catalog::{AccessRules, DataField, DbId, TableId, TableSource};
        use sc_types::TypeRef;
        Table {
            id: TableId("forecasts".into()),
            name: "forecasts".into(),
            database: DbId::primary(),
            source: TableSource::Database,
            fields: vec![
                DataField::plain("day", TypeRef::Basic(BasicType::Date)),
                DataField::plain("at", TypeRef::Basic(BasicType::Timestamp)),
            ],
            primary_key: vec!["id".into()],
            label: "forecasts".into(),
            description: String::new(),
            access: AccessRules::default(),
            attributes: Attrs::new(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        }
    }

    #[test]
    fn a_grids_instant_goes_into_a_date_field_as_its_day() {
        let write = |field: &str| {
            configured_write(&config(&[
                (CFG_VARIABLE, json!("y_future")),
                (CFG_MODE, json!("insert")),
                (CFG_TABLE, json!("forecasts")),
                (CFG_STATISTICS, json!({ "mean": "mean" })),
                (
                    CFG_COORDINATES,
                    json!([{ "axis": "day.future", "field": field }]),
                ),
            ]))
            .unwrap()
        };
        let row =
            |field: &str, instant: &str| config(&[(field, json!(instant)), ("mean", json!(1.5))]);

        let written = dates_as_days(
            &forecasts(),
            &write("day"),
            row("day", "2025-05-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(written["day"], json!("2025-05-01"));
        assert_eq!(written["mean"], json!(1.5));
        // A timestamp field takes the instant as it is.
        let written = dates_as_days(
            &forecasts(),
            &write("at"),
            row("at", "2025-05-01T06:00:00Z"),
        )
        .unwrap();
        assert_eq!(written["at"], json!("2025-05-01T06:00:00Z"));
        // An hour is not a day.
        let err = dates_as_days(
            &forecasts(),
            &write("day"),
            row("day", "2025-05-01T06:00:00Z"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(
                "`day` of `forecasts` is a date, and `2025-05-01T06:00:00Z` is not a \
                          midnight"
            ),
            "{err}"
        );
    }

    #[test]
    fn fit_model_waits_by_default_and_takes_its_flags_as_booleans() {
        assert!(flag(&Attrs::new(), CFG_WAIT, DEFAULT_WAIT).unwrap());
        assert!(!flag(&config(&[(CFG_WAIT, json!(false))]), CFG_WAIT, true).unwrap());
        let err = flag(
            &config(&[(CFG_ACTIVATE, json!("yes"))]),
            CFG_ACTIVATE,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("true or false"), "{err}");
    }
}
