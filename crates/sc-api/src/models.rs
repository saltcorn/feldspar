//! Models, from the row layer's side (milestone 31 §§1, 3, 4).
//!
//! [`write_posterior`] is the one path from a posterior's summary to rows,
//! shared by the admin API's `writePosterior` and a code body's
//! `m.writePosterior(…)`. It writes through the row layer, so a write-back is
//! validated, ownership-checked under the authority it is given, and fires the
//! target table's own triggers whichever way it was asked for.
//!
//! [`predict_for`] and [`describe_model`] are the two halves of
//! [`ModelHost`](sc_catalog::ModelHost), which `sc-server`'s `ModelServices`
//! implements by calling them with its registry and dataset source: a formula's
//! `predict("…")`, the read path's calculated fields and a code body's
//! `m.predict(…)` all end here.

use std::collections::HashMap;
use std::sync::Arc;

use sc_auth::User;
use sc_catalog::{CallerContext, Catalog, ModelSummary, PredictRows, Table};
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_model::{
    DatasetSource, Model, ModelInstance, ModelRegistry, PosteriorView, PosteriorWrite, Prediction,
    Subject, WriteMode, canonical_key, instance_coordinates, plan_write, summarise_variable,
};
use sc_query::{Expr, InSet};
use sc_types::{Attrs, BasicType};
use serde_json::{Value as Json, json};

/// The model called `name` and the fit that answers for it: the one `fit`
/// names (which must be one of the model's), or else its **active** fit.
///
/// Naming the model and not the fit is the point of `active`: the admin
/// refits, activates, and every formula and code body that named the model
/// follows without being edited.
pub async fn resolve_fit(
    catalog: &Catalog,
    name: &str,
    fit: Option<&str>,
) -> Result<(Model, ModelInstance)> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::invalid("name the model to use"));
    }
    let model = sc_model::require_model(catalog, name).await?;
    let instance = match fit.map(str::trim).filter(|f| !f.is_empty()) {
        Some(raw) => {
            let id = uuid::Uuid::parse_str(raw).map_err(|_| {
                Error::invalid(format!(
                    "`{raw}` is not a fit's id (fits are named by uuid)"
                ))
            })?;
            let instance =
                sc_model::require_model_instance(catalog, sc_model::InstanceId(id)).await?;
            if instance.model != model.id {
                return Err(Error::invalid(format!(
                    "fit {raw} is not a fit of model `{}`",
                    model.name
                )));
            }
            instance
        }
        None => sc_model::active_model_instance(catalog, model.id)
            .await?
            .ok_or_else(|| {
                Error::invalid(format!(
                    "model `{}` has no active fit: fit it and make a fit active",
                    model.name
                ))
            })?,
    };
    Ok((model, instance))
}

/// Predict `rows` of `table` with `model`'s active fit (or `fit`), in the
/// order asked — [`ModelHost::predict`](sc_catalog::ModelHost::predict).
///
/// Keys are read **through the model's dataset**, by key and unfiltered, in
/// one read restricted to them (`pk IN (…)`), and the answers are put back in
/// the order the keys were asked in: the dataset has an order of its own, and
/// a caller lines the answers up against its rows. Literal values are one
/// frame, typed as the fit's features were.
// Nine, because a prediction needs the model machinery (three), the bound,
// and the four things the caller asked; grouping them would invent a struct
// with one user.
#[allow(clippy::too_many_arguments)]
pub async fn predict_for(
    catalog: &Catalog,
    registry: &ModelRegistry,
    source: &dyn DatasetSource,
    cap: u64,
    model: &str,
    fit: Option<&str>,
    table: &str,
    rows: PredictRows<'_>,
    detail: bool,
) -> Result<Vec<Json>> {
    let (model, instance) = resolve_fit(catalog, model, fit).await?;
    if model.table() != table {
        return Err(Error::invalid(format!(
            "`{}` is a model of `{}`, and these rows are of `{table}`",
            model.name,
            model.table()
        )));
    }
    if let Some(grain) = grain_refusal(&model) {
        return Err(Error::invalid(format!(
            "`{}` cannot predict a row of `{table}`: its dataset changes what a row is \
             ({grain})",
            model.name
        )));
    }
    let predictions = match rows {
        PredictRows::Values(values) => {
            if values.is_empty() {
                return Ok(Vec::new());
            }
            sc_model::predict_subject(
                registry,
                source,
                &model,
                &instance,
                Subject::Rows(values),
                cap,
            )
            .await?
            .predictions
        }
        PredictRows::Keys(keys) => {
            if keys.is_empty() {
                return Ok(Vec::new());
            }
            let target = catalog.require(table)?;
            let pk = crate::rows::single_pk(&target)?;
            let mut asked = Vec::with_capacity(keys.len());
            for key in keys {
                asked.push(crate::rows::column_value(&target, &pk, key)?);
            }
            let restrict = Expr::In {
                e: Box::new(Expr::col(pk.clone())),
                set: InSet::List(asked.iter().cloned().map(Expr::lit).collect()),
            };
            let answer = sc_model::predict_subject(
                registry,
                source,
                &model,
                &instance,
                Subject::Dataset(Some(&restrict)),
                cap,
            )
            .await?;
            let by_key: HashMap<&str, &Prediction> = answer
                .keys
                .iter()
                .map(String::as_str)
                .zip(answer.predictions.iter())
                .collect();
            let mut ordered = Vec::with_capacity(asked.len());
            for (value, key) in asked.iter().zip(keys) {
                let prediction = by_key.get(canonical_key(value).as_str()).ok_or_else(|| {
                    Error::invalid(format!(
                        "no row of `{table}` has the {pk} {}, so `{}` has nothing to predict \
                         for it",
                        match key {
                            Json::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                        model.name
                    ))
                })?;
                ordered.push((*prediction).clone());
            }
            ordered
        }
    };
    predictions
        .iter()
        .map(|prediction| {
            let value = prediction.to_json()?;
            if !detail {
                return Ok(value);
            }
            let mut out = serde_json::Map::new();
            out.insert("value".to_owned(), value);
            if let Prediction::Class {
                probability: Some(p),
                ..
            } = prediction
            {
                out.insert("probability".to_owned(), json!(p));
            }
            Ok(Json::Object(out))
        })
        .collect()
}

/// What a formula's save check needs to know about the model called `name` —
/// [`ModelHost::describe`](sc_catalog::ModelHost::describe).
///
/// The prediction types are the provider's **declaration**, not the active
/// fit's outcome: a field is typed before any fit exists, and a refit must not
/// be able to change what the field can hold.
pub async fn describe_model(
    catalog: &Catalog,
    registry: &ModelRegistry,
    name: &str,
) -> Result<ModelSummary> {
    let model = sc_model::require_model(catalog, name.trim()).await?;
    let provider = registry.require(model.provider.trim()).map_err(|e| {
        Error::invalid(format!(
            "model `{}` is fitted by `{}`, which this server does not have: {e}",
            model.name, model.provider
        ))
    })?;
    let spec = provider.outcome_spec();
    let prediction_types = spec.possible_prediction_types();
    let no_prediction = prediction_types.is_empty().then(|| {
        let posterior = matches!(spec, sc_model::OutcomeSpec::Posterior { .. });
        sc_model::no_per_row_prediction(posterior).to_owned()
    });
    let active_fit = sc_model::active_model_instance(catalog, model.id)
        .await?
        .map(|i| i.id.to_string());
    Ok(ModelSummary {
        name: model.name.clone(),
        table: model.table().to_owned(),
        provider: model.provider.clone(),
        prediction_types,
        no_prediction,
        active_fit,
        not_rows_of_table: grain_refusal(&model),
    })
}

/// Why `model`'s dataset's rows are not rows of its table, when they are not
/// (analytics TODO A1.9): what refuses a `predict("…")` over it.
fn grain_refusal(model: &Model) -> Option<String> {
    match &model.dataset.grain {
        Some(sc_dataset::Grain::Table { .. }) => None,
        Some(grain) => Some(grain.describe()),
        None => None,
    }
}

use crate::rows;

/// Statistics that may go into an integer field, rounded: the effective sample
/// sizes, which are counts of draws.
const COUNT_STATISTICS: [&str; 2] = ["ess_bulk", "ess_tail"];

/// Whose authority a write-back writes under.
///
/// The same two shapes a code body's `db` handle has: a context the row layer
/// writes under as it is (the admin API's, and an undelegated body's — admin,
/// in the event's user's name, with its trigger chain), and a **delegated**
/// caller, whose every write goes through [`crate::ownership`]'s `*_as`
/// functions so §7.3's rule decides it exactly as it decides
/// `db.counties.asUser().update(…)`.
pub enum Writer<'a> {
    /// Written under this context, through the row layer.
    Context(&'a CallerContext),
    /// Written as this caller, through the ownership rule.
    As {
        /// The role every floor is checked against.
        role: u8,
        /// The user an ownership formula and an RLS policy read.
        user: Option<&'a User>,
        /// For an ownership formula only the evaluator can decide.
        evaluator: Option<&'a Arc<dyn JsEvaluator>>,
        /// The triggers that led here, which each write's event carries.
        chain: &'a [String],
    },
}

impl Writer<'_> {
    async fn insert(
        &self,
        catalog: &Catalog,
        table: &Table,
        body: &Json,
        executor: &rows::Executor,
    ) -> Result<Json> {
        match self {
            Writer::Context(context) => {
                rows::create_row_in(catalog, table, body, Some(context), executor).await
            }
            Writer::As {
                role,
                user,
                evaluator,
                chain,
            } => {
                crate::ownership::insert_row_as(
                    catalog, table, body, *role, *user, *evaluator, chain, executor,
                )
                .await
            }
        }
    }

    async fn update(
        &self,
        catalog: &Catalog,
        table: &Table,
        key: &str,
        body: &Json,
        executor: &rows::Executor,
    ) -> Result<Json> {
        match self {
            Writer::Context(context) => {
                rows::update_row_in(catalog, table, key, body, Some(context), executor).await
            }
            Writer::As {
                role,
                user,
                evaluator,
                chain,
            } => {
                crate::ownership::update_row_as(
                    catalog, table, key, body, *role, *user, *evaluator, chain, executor,
                )
                .await
            }
        }
    }
}

/// Write `write` of `instance` (a fit of `model`) into rows, as `writer`,
/// through `executor` — **the** write-back, for the admin API and a code
/// body's handle alike.
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
    writer: &Writer<'_>,
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
                writer
                    .update(catalog, &table, key, &body, executor)
                    .await
                    .map_err(|e| {
                        Error::invalid(format!(
                            "writing `{}` into row `{key}` of `{}`: {e}",
                            write.variable, table.name
                        ))
                    })?;
            }
            _ => {
                writer
                    .insert(catalog, &table, &body, executor)
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

/// A field a write-back may write: one the table has, and not a calculated
/// one.
fn writable_field(table: &Table, field: &str) -> Result<()> {
    match table.field(field) {
        None => Err(Error::invalid(format!(
            "`{}` has no field `{field}`",
            table.name
        ))),
        Some(f) if f.is_calc() => Err(Error::invalid(format!(
            "`{field}` is a calculated field and cannot be written"
        ))),
        Some(_) => Ok(()),
    }
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
                "`{field}` of `{}` is a date, and `{instant}` is not a midnight: write this \
                 axis into a timestamp field",
                table.name
            )));
        }
        let day = day.to_owned();
        values.insert(field.clone(), Json::from(day));
    }
    Ok(values)
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
            PosteriorWrite::from_json(&json!({
                "variable": "y_future",
                "mode": "insert",
                "table": "forecasts",
                "statistics": { "mean": "mean" },
                "coordinates": [{ "axis": "day.future", "field": field }],
            }))
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
}
