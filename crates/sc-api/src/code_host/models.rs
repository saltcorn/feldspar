//! `models` — a fitted model, used from a code body (milestone 31 §3).
//!
//! A code body reaches a model through the `db` host, as `op: "models"`
//! requests, rather than as a surface of its own: what it reads is system
//! tables and the providers behind [`ModelHost`](sc_catalog::ModelHost), it is
//! bounded by the same call budget, and it is the same seam in both languages
//! — the JavaScript and Python model handles are prelude over it.
//!
//! ```json
//! { "op": "models", "what": "get", "model": "Radon" }
//! { "op": "models", "what": "predict", "model": "House prices", "fit": "<id>",
//!   "rows": [{ "id": 3 }, { "area": 90, "bedrooms": 2 }], "detail": false }
//! { "op": "models", "what": "draws", "model": "Radon", "fit": "<id>",
//!   "variable": "alpha", "elements": { "1": [27001] }, "chains": [1, 2], "thin": 10 }
//! { "op": "models", "what": "summary", "model": "Radon", "fit": "<id>", "variable": "alpha" }
//! { "op": "models", "what": "write_posterior", "model": "Radon", "fit": "<id>",
//!   "write": { "variable": "alpha", "statistics": { "mean": "alpha_mean" } },
//!   "authority": "admin" }
//! ```
//!
//! `get` resolves a model's name to its active fit (or to the fit `fit`
//! names) and answers everything a handle is built from, so a handle costs one
//! call. **Every later call names that fit's id**, so a handle does not change
//! fit halfway through a body because somebody activated another one.
//!
//! A prediction and a draws read read the admin's own fit. A write-back writes
//! under the handle's authority and this run's trigger chain and executor — the
//! same as `db.counties.update(…)`: ownership is checked, the target table's
//! triggers fire, and the chain bounds recursion.

use sc_catalog::{ModelHost, PredictRows, Table};
use sc_error::{Error, Result};
use sc_model::{DrawsRequest, Model, ModelInstance, PosteriorWrite, Selection};
use serde_json::{Value as Json, json};

use super::plan::Authority;
use super::{Actor, TableHost};

/// The most numbers one `m.draws` answer carries: a quarter of what the admin
/// API allows, because a code body holds its answer in an isolate with a heap
/// bound and a one-second slice to work through it.
pub const CODE_MAX_DRAWS: u64 = 500_000;

/// One `models` request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsPlan {
    /// Always `models`; read before the plan is.
    #[allow(dead_code)]
    op: String,
    /// `get`, `predict`, `draws`, `summary` or `write_posterior`.
    what: String,
    /// The model's name.
    model: String,
    /// The fit: for `get`, the one to use instead of the active fit; for every
    /// other request, the one `get` resolved, which is required.
    #[serde(default)]
    fit: Option<String>,
    /// `predict`'s rows.
    #[serde(default)]
    rows: Option<Vec<Json>>,
    /// `predict`: answer `{ value, probability }` rather than the value.
    #[serde(default)]
    detail: Option<bool>,
    #[serde(default)]
    variable: Option<String>,
    #[serde(default)]
    elements: Option<Json>,
    #[serde(default)]
    chains: Option<Vec<u32>>,
    #[serde(default)]
    warmup: Option<bool>,
    #[serde(default)]
    thin: Option<usize>,
    /// `write_posterior`'s specification, as the admin API's `writePosterior`
    /// takes it.
    #[serde(default)]
    write: Option<Json>,
    /// The handle's authority: what a write-back writes under. A read does not
    /// consult it — the fit is the admin's own.
    #[serde(default)]
    authority: Authority,
}

/// Answer one `models` request of `host`'s run.
pub(super) async fn answer(host: &TableHost<'_>, request: Json) -> Result<Json> {
    let plan: ModelsPlan = serde_json::from_value(request).map_err(|e| {
        Error::invalid(format!(
            "this models request is not one the server understands: {e}"
        ))
    })?;
    let catalog = host.catalog;
    if !["get", "predict", "draws", "summary", "write_posterior"].contains(&plan.what.as_str()) {
        return Err(Error::invalid(format!(
            "a model handle has no `{}`; it has predict, draws, summary and writePosterior",
            plan.what
        )));
    }
    if plan.what == "get" {
        let (model, instance) =
            crate::models::resolve_fit(catalog, &plan.model, plan.fit.as_deref()).await?;
        return handle_json(&model, &instance);
    }
    let fit = plan
        .fit
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .ok_or_else(|| {
            Error::invalid(format!(
                "a models `{}` request names the fit `models.get` resolved",
                plan.what
            ))
        })?;
    let (model, instance) = crate::models::resolve_fit(catalog, &plan.model, Some(fit)).await?;
    let variable = || {
        plan.variable
            .clone()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::invalid(format!("{}() needs a variable", plan.what)))
    };
    let selection = || Selection::from_json(plan.elements.as_ref());
    match plan.what.as_str() {
        "predict" => {
            let rows = plan.rows.as_deref().unwrap_or_default();
            predict(host, &model, &instance, rows, plan.detail.unwrap_or(false)).await
        }
        "draws" => {
            posterior_only(&model, &instance, "draws")?;
            let request = DrawsRequest {
                variable: variable()?,
                selection: selection()?,
                chains: plan.chains.clone(),
                warmup: plan.warmup.unwrap_or(false),
                thin: plan.thin.unwrap_or(1).max(1),
            };
            let draws = sc_model::read_draws(catalog, &instance, &request, CODE_MAX_DRAWS).await?;
            serde_json::to_value(draws).map_err(|e| Error::msg(format!("draws: {e}")))
        }
        "summary" => {
            posterior_only(&model, &instance, "summary")?;
            let summary =
                sc_model::summarise_variable(catalog, &instance, &variable()?, &selection()?)
                    .await?;
            serde_json::to_value(summary).map_err(|e| Error::msg(format!("summary: {e}")))
        }
        "write_posterior" => {
            posterior_only(&model, &instance, "writePosterior")?;
            let spec = plan.write.as_ref().ok_or_else(|| {
                Error::invalid("writePosterior() takes what to write: a variable and statistics")
            })?;
            let write = PosteriorWrite::from_json(spec)?;
            let actor = host.actor(&plan.authority).await?;
            let writer = match &actor {
                Actor::Admin(context) => crate::models::Writer::Context(context),
                Actor::Caller { role, user } => crate::models::Writer::As {
                    role: *role,
                    user: user.as_ref(),
                    evaluator: host.evaluator.as_ref(),
                    chain: &host.chain,
                },
            };
            crate::models::write_posterior(
                catalog,
                &model,
                &instance,
                &write,
                &writer,
                &host.executor,
            )
            .await
        }
        other => Err(Error::msg(format!(
            "models `{other}` was admitted and not answered"
        ))),
    }
}

/// Everything a handle is built from: the model, the fit, what the fit's
/// outcome is, what it drew, and — for one that answers nothing per row —
/// why, so `m.predict` can say it without a call.
fn handle_json(model: &Model, instance: &ModelInstance) -> Result<Json> {
    let outcome = instance.outcome().ok();
    let posterior = outcome
        .as_ref()
        .is_some_and(sc_model::Outcome::is_posterior);
    let no_prediction = match &outcome {
        Some(o) if !o.predicts() => Some(format!(
            "`{}` is {}",
            model.name,
            sc_model::no_per_row_prediction(o.is_posterior())
        )),
        _ => None,
    };
    Ok(json!({
        "name": model.name,
        "provider": model.provider,
        "table": model.table(),
        "outcome": outcome.as_ref().map(serde_json::to_value).transpose()
            .map_err(|e| Error::msg(format!("outcome: {e}")))?,
        "fit": fit_json(instance),
        "variables": posterior.then(|| {
            sc_model::posterior_variables(instance)
                .into_iter()
                .filter(|v| !v.ends_with("__"))
                .collect::<Vec<_>>()
        }),
        "no_prediction": no_prediction,
    }))
}

/// A fit as `m.fit` holds it.
fn fit_json(instance: &ModelInstance) -> Json {
    json!({
        "id": instance.id.to_string(),
        "name": instance.name,
        "status": instance.status.as_str(),
        "active": instance.active,
        "created": instance.created,
        "error": instance.error(),
        "warnings": instance
            .attributes
            .get(sc_model::ATTR_WARNINGS)
            .cloned()
            .unwrap_or_else(|| json!([])),
        "metrics": instance.metrics,
        "parameters": serde_json::to_value(&instance.parameters).unwrap_or(Json::Null),
    })
}

/// Refuse a posterior-only request of a fit that is not a posterior, in the
/// words the handle's own getter uses.
fn posterior_only(model: &Model, instance: &ModelInstance, what: &str) -> Result<()> {
    let outcome = instance.outcome()?;
    if outcome.is_posterior() {
        return Ok(());
    }
    Err(Error::invalid(not_posterior(model, &outcome, what)))
}

/// "`House prices` is a linear_regression regression; draws are for posterior
/// models".
fn not_posterior(model: &Model, outcome: &sc_model::Outcome, what: &str) -> String {
    format!(
        "`{}` is a {} {}; `{what}` is for posterior models",
        model.name,
        model.provider,
        outcome.name()
    )
}

/// Predict `rows` with `instance`, through the catalog's
/// [`ModelHost`](sc_catalog::ModelHost), answering in row order.
async fn predict(
    host: &TableHost<'_>,
    model: &Model,
    instance: &ModelInstance,
    rows: &[Json],
    detail: bool,
) -> Result<Json> {
    if rows.is_empty() {
        return Ok(json!([]));
    }
    let models = host.catalog.model_host().ok_or_else(|| {
        Error::invalid(format!(
            "`{}` cannot predict here: this server has no model support installed",
            model.name
        ))
    })?;
    let table = host.catalog.require(model.table())?;
    let fit = instance.id.to_string();
    predict_through(models.as_ref(), &model.name, &fit, &table, rows, detail).await
}

/// Split `rows` into the keyed and the literal and ask `models` about each kind
/// once, answering in the order the rows came in.
///
/// A row carrying `table`'s primary key is read through the dataset by that
/// key; any other row is taken as the dataset's columns as given — so a row
/// not inserted yet, or made up, must supply every feature.
pub(super) async fn predict_through(
    models: &dyn ModelHost,
    model: &str,
    fit: &str,
    table: &Table,
    rows: &[Json],
    detail: bool,
) -> Result<Json> {
    let pk = crate::rows::single_pk(table).ok();
    let mut keys = Vec::new();
    let mut literals = Vec::new();
    // For each row, whether it is keyed and its index among its kind.
    let mut order = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let Json::Object(fields) = row else {
            return Err(Error::invalid(format!(
                "predict() takes a row object or an array of them, and row {} is {row}",
                i + 1,
            )));
        };
        match pk
            .as_deref()
            .and_then(|pk| fields.get(pk))
            .filter(|k| !k.is_null())
        {
            Some(key) => {
                order.push((true, keys.len()));
                keys.push(key.clone());
            }
            None => {
                order.push((false, literals.len()));
                literals.push(row.clone());
            }
        }
    }
    let by_key = match keys.is_empty() {
        true => Vec::new(),
        false => {
            models
                .predict(
                    model,
                    Some(fit),
                    &table.name,
                    PredictRows::Keys(&keys),
                    detail,
                )
                .await?
        }
    };
    let by_value = match literals.is_empty() {
        true => Vec::new(),
        false => {
            models
                .predict(
                    model,
                    Some(fit),
                    &table.name,
                    PredictRows::Values(&literals),
                    detail,
                )
                .await?
        }
    };
    order
        .into_iter()
        .map(|(keyed, i)| {
            let from = if keyed { &by_key } else { &by_value };
            from.get(i).cloned().ok_or_else(|| {
                Error::msg(format!(
                    "the model host answered {} predictions for {} rows",
                    from.len(),
                    if keyed { keys.len() } else { literals.len() }
                ))
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Json::Array)
}
