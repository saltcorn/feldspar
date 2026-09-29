//! `models` — a fitted posterior, read from a code body (Stan TODO §17).
//!
//! "Making predictions about new cases is secondary and it is fine if the user
//! does some of the work in code": so a code body can reach the draws. It does
//! so through the `db` host, as an `op: "models"` request, rather than as a
//! sixth surface: what it reads is two system tables, it is bounded by the same
//! call budget, and it is the same seam in both languages — the JavaScript and
//! Python `models` objects are a few lines of prelude over it.
//!
//! ```json
//! { "op": "models", "what": "draws", "model": "Radon", "variable": "alpha",
//!   "elements": { "counties": [27001] }, "chains": [1, 2], "thin": 10 }
//! ```
//!
//! `model` is a model's name (meaning its active fit) or a fit's id. The answers are the admin API's — `getModelDraws`,
//! `getPosteriorSummary` — because they are the same functions of the same
//! rows; `instance` answers the fit itself. Every answer speaks keys and
//! labels: nothing that leaves the host numbers a county.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_model::{DrawsRequest, ModelInstance, Selection};
use serde_json::{Value as Json, json};

/// The most numbers one `models.draws` answer carries: a quarter of what the
/// admin API allows, because a code body holds its answer in an isolate with
/// a heap bound and a one-second slice to work through it.
pub const CODE_MAX_DRAWS: u64 = 500_000;

/// One `models` request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsPlan {
    /// Always `models`; read before the plan is.
    #[allow(dead_code)]
    op: String,
    /// `draws`, `summary` or `instance`.
    what: String,
    /// A model's name, or a fit's id.
    model: String,
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
    /// The handle's authority, which every `db` request carries and which a
    /// model read does not consult: the draws are the admin's own fit.
    #[serde(default)]
    #[allow(dead_code)]
    authority: Option<Json>,
}

/// Answer one `models` request.
pub(super) async fn answer(catalog: &Catalog, request: Json) -> Result<Json> {
    let plan: ModelsPlan = serde_json::from_value(request).map_err(|e| {
        Error::invalid(format!(
            "this models request is not one the server understands: {e}"
        ))
    })?;
    let instance = fit(catalog, plan.model.trim()).await?;
    let variable = || {
        plan.variable
            .clone()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::invalid(format!("models.{}() needs a variable", plan.what)))
    };
    let selection = || Selection::from_json(plan.elements.as_ref());
    match plan.what.as_str() {
        "draws" => {
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
            let summary =
                sc_model::summarise_variable(catalog, &instance, &variable()?, &selection()?)
                    .await?;
            serde_json::to_value(summary).map_err(|e| Error::msg(format!("summary: {e}")))
        }
        "instance" => instance_json(catalog, &instance).await,
        other => Err(Error::invalid(format!(
            "`models.{other}` does not exist; there are `models.draws`, `models.summary` and \
             `models.instance`"
        ))),
    }
}

/// The fit `model` names: a fit's id, or a model's name meaning its active fit.
async fn fit(catalog: &Catalog, model: &str) -> Result<ModelInstance> {
    if model.is_empty() {
        return Err(Error::invalid(
            "name the model (its active fit is read) or a fit's id",
        ));
    }
    if let Ok(id) = uuid::Uuid::parse_str(model) {
        return sc_model::require_model_instance(catalog, sc_model::InstanceId(id)).await;
    }
    let found = sc_model::require_model(catalog, model).await?;
    sc_model::active_model_instance(catalog, found.id)
        .await?
        .ok_or_else(|| {
            Error::invalid(format!(
                "model `{model}` has no active fit: fit it and mark a fit active, or name a fit \
                 by its id"
            ))
        })
}

/// A fit as `models.instance` answers it: what it is, how it went, and which
/// variables it drew.
async fn instance_json(catalog: &Catalog, instance: &ModelInstance) -> Result<Json> {
    let model = sc_model::load_model(catalog, instance.model).await?;
    Ok(json!({
        "id": instance.id.to_string(),
        "model": model.map(|m| m.name),
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
        "variables": sc_model::posterior_variables(instance)
            .into_iter()
            .filter(|v| !v.ends_with("__"))
            .collect::<Vec<_>>(),
    }))
}
