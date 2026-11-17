//! The Simulation workspace's profiler and scenario comparison (analytics
//! TODO A7.4–A7.5), over `sc_analytics::simulate`. The endpoints are declared
//! in `sc-api`'s `analytics.rs`, beside the model editor's.

use super::*;

/// Register the profiler's handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    reg.handle(sc_api::handler_ids::PROFILE_MODEL, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let body = rows::require_object(&ctx.body)?;
                let (model, instance) = model_and_fit(&catalog, &ctx, body).await?;
                let request = sc_analytics::simulate::ProfileRequest {
                    values: values_of(body.get("values"), "`values`")?,
                    level: level_of(body)?,
                    points: match body.get("points") {
                        None | Some(Json::Null) => None,
                        Some(v) => Some(
                            v.as_u64()
                                .and_then(|n| usize::try_from(n).ok())
                                .ok_or_else(|| Error::invalid("`points` is a whole number"))?,
                        ),
                    },
                };
                let registry = models.registry();
                let source = models.source();
                let predictor = sc_analytics::simulate::Predictor {
                    registry: &registry,
                    source: source.as_ref(),
                    cap: models.max_rows(),
                    model: &model,
                    instance: &instance,
                };
                let profile = sc_analytics::simulate::profile(&predictor, &request).await?;
                answer(&model, &instance, &profile)
            }
        }
    });

    reg.handle(sc_api::handler_ids::COMPARE_SCENARIOS, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let body = rows::require_object(&ctx.body)?;
                let (model, instance) = model_and_fit(&catalog, &ctx, body).await?;
                let scenarios = match body.get("scenarios") {
                    Some(Json::Array(list)) => list
                        .iter()
                        .enumerate()
                        .map(|(i, s)| {
                            let name = s.get("name").and_then(Json::as_str).ok_or_else(|| {
                                Error::invalid(format!("scenario {} has no name", i + 1))
                            })?;
                            Ok(sc_analytics::simulate::Scenario {
                                name: name.to_owned(),
                                values: values_of(
                                    s.get("values"),
                                    &format!("the values of scenario “{name}”"),
                                )?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                    _ => return Err(Error::invalid("`scenarios` is a list of scenarios")),
                };
                let registry = models.registry();
                let source = models.source();
                let predictor = sc_analytics::simulate::Predictor {
                    registry: &registry,
                    source: source.as_ref(),
                    cap: models.max_rows(),
                    model: &model,
                    instance: &instance,
                };
                let comparison =
                    sc_analytics::simulate::compare(&predictor, &scenarios, level_of(body)?)
                        .await?;
                answer(&model, &instance, &comparison)
            }
        }
    });
}

/// The model the path names, and the fit the body names — else its active
/// fit, else its newest fitted one.
async fn model_and_fit<W>(
    catalog: &Catalog,
    ctx: &HandlerCtx<W>,
    body: &Map<String, Json>,
) -> Result<(sc_model::Model, sc_model::ModelInstance)> {
    let raw = ctx.path_param("id")?;
    let id = sc_model::ModelId(
        raw.parse()
            .map_err(|_| Error::invalid(format!("`{raw}` is not a model id")))?,
    );
    let model = sc_model::load_model(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no model with id {id}")))?;
    let instance = match body.get("fit").filter(|f| !f.is_null()) {
        Some(Json::String(raw)) => {
            let fit = sc_model::InstanceId(
                raw.parse()
                    .map_err(|_| Error::invalid(format!("`{raw}` is not a fit's id")))?,
            );
            let instance = sc_model::require_model_instance(catalog, fit).await?;
            if instance.model != id {
                return Err(Error::invalid(format!(
                    "fit {fit} is not a fit of the model `{}`",
                    model.name
                )));
            }
            instance
        }
        Some(_) => return Err(Error::invalid("`fit` is a fit's id")),
        None => {
            let fits = sc_model::list_model_instances(catalog, id).await?;
            let active = fits.iter().find(|i| i.active).cloned();
            active
                .or_else(|| {
                    fits.into_iter()
                        .find(|i| i.status == sc_model::FitStatus::Fitted)
                })
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "`{}` has not been fitted, so there is nothing to simulate: fit it first",
                        model.name
                    ))
                })?
        }
    };
    Ok((model, instance))
}

fn values_of(value: Option<&Json>, what: &str) -> Result<Map<String, Json>> {
    match value {
        None | Some(Json::Null) => Ok(Map::new()),
        Some(Json::Object(m)) => Ok(m.clone()),
        Some(_) => Err(Error::invalid(format!(
            "{what} are an object of input values by name"
        ))),
    }
}

fn level_of(body: &Map<String, Json>) -> Result<Option<f64>> {
    match body.get("level") {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| Error::invalid("`level` is a probability between 0 and 1")),
    }
}

/// `result`'s fields, with the model and the fit it was computed with.
fn answer(
    model: &sc_model::Model,
    instance: &sc_model::ModelInstance,
    result: &impl serde::Serialize,
) -> Result<HandlerResponse> {
    let mut out = serde_json::to_value(result)
        .map_err(|e| Error::serde(format!("a simulation does not serialise: {e}")))?;
    if let Some(fields) = out.as_object_mut() {
        fields.insert("model".to_owned(), json!(model.id.0));
        fields.insert(
            "fit".to_owned(),
            json!({
                "id": instance.id.0,
                "name": instance.name,
                "active": instance.active,
                "dataset_changed":
                    sc_model::dataset_changed(instance, &model.dataset, &model.related),
            }),
        );
    }
    Ok(HandlerResponse::ok(out))
}
