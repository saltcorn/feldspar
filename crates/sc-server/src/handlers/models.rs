//! Predictive models, their instances and their posteriors.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    // The row ⇄ live-set path once more (TODO "Predictive models", 2.5): every
    // save is validated against the same registry a fit runs with, and a stored
    // model that no longer validates stays listed with its reason, because
    // editing it is the repair.
    //
    // Two things happen *here* rather than in `sc-model`, and both are the
    // layering: a dataset can only be read through the row layer (§4), and a fit
    // is a spawned job (§8) — so the handler that starts one answers as soon as
    // the row exists and the screen polls it.

    reg.register_admin("listModelProviders", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let registry = models.registry();
                // The dataset, if the caller has one: a provider naming a label
                // has to offer *these* columns as its options (§10), and the
                // only way to know their types is to have read some rows — a
                // `SchemaShape` carries no types at all.
                let shape = match query_json(&ctx, "dataset")? {
                    Some(value) => {
                        let dataset = dataset_from_json(&catalog, &value).await?;
                        let frame = models
                            .source()
                            .read(
                                &dataset,
                                &sc_model::Read::all(models.max_rows()).first(DATASET_PREVIEW_ROWS),
                            )
                            .await?;
                        Some(sc_model::DatasetShape::of_frame(&dataset.table, &frame))
                    }
                    None => None,
                };
                let configuration = match query_json(&ctx, "configuration")? {
                    Some(Json::Object(map)) => map,
                    Some(Json::Null) | None => Attrs::new(),
                    Some(_) => {
                        return Err(Error::invalid("`configuration` must be a JSON object"));
                    }
                };
                let mut providers = Vec::with_capacity(registry.len());
                for provider in registry.all() {
                    let kind = provider.kind();
                    let spec = match &shape {
                        Some(shape) => provider.config_spec(shape),
                        None => provider.config_declaration(),
                    };
                    // Resolved like every other spec the admin UI renders, so a
                    // setting whose options come from the catalog arrives as a
                    // picker rather than a text box.
                    let spec = resolve_options(&catalog, spec).await?;
                    let (outcome, outcome_error) = match &shape {
                        Some(shape) => match provider.outcome(shape, &configuration) {
                            Ok(outcome) => (serde_json::to_value(&outcome).ok(), None),
                            // Not an error for the request: "no column is named
                            // as the label" is what the form is waiting to be
                            // told, and every other provider still answers.
                            Err(e) => (None, Some(e.to_string())),
                        },
                        None => (None, None),
                    };
                    providers.push(json!({
                        "name": kind.name,
                        "description": say_unless_module(
                            &kind.description,
                            kind.module.as_ref(),
                            &ctx.locale,
                        ),
                        "module": kind.module,
                        "config_spec": spec_json(&spec, &ctx.locale),
                        "hyperparameters": spec_json(&provider.hyperparameters(), &ctx.locale),
                        "outcome_spec": serde_json::to_value(&kind.outcome)
                            .unwrap_or(Json::Null),
                        "outcome": outcome,
                        "outcome_error": outcome_error,
                        "standardise": kind.standardise,
                        "binds_data": kind.binds_data,
                        "cancellable": kind.cancellable,
                        // Only a provider this server knows how to ask has an
                        // availability; CmdStan's is found at boot (Stan §20).
                        "unavailable": (kind.name == sc_stan::STAN_PROVIDER)
                            .then(|| models.stan().unavailable().map(|why| {
                                format!("CmdStan was not found: {why}")
                            }))
                            .flatten(),
                    }));
                }
                // An empty picker reads like a bug; the sentence reads like the
                // decision it is (§13).
                Ok(HandlerResponse::ok(json!({
                    "providers": providers,
                    "builtins_compiled_out": !sc_model::SMARTCORE,
                    "notice": if sc_model::SMARTCORE {
                        Json::Null
                    } else {
                        Json::String(sc_model::BUILTINS_COMPILED_OUT.to_owned())
                    },
                })))
            }
        }
    });

    reg.register_admin("previewDataset", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let body = rows::require_object(&ctx.body)?;
                let dataset = dataset_from_json(
                    &catalog,
                    body.get("dataset")
                        .ok_or_else(|| Error::invalid("`dataset` is required"))?,
                )
                .await?;
                // A named dataset that does not read says why, before a read
                // would say it with a database error.
                dataset.readable()?;
                let limit = body
                    .get("limit")
                    .and_then(Json::as_u64)
                    .unwrap_or(DATASET_PREVIEW_ROWS)
                    .clamp(1, DATASET_PREVIEW_MAX);
                let frame = models
                    .source()
                    .read(
                        &dataset,
                        &sc_model::Read::all(models.max_rows()).first(limit),
                    )
                    .await?;
                let shape = sc_model::DatasetShape::of_frame(&dataset.table, &frame);
                // The split's refusal, reported while the model is still being
                // built: rows that are neither a table's nor a group's read
                // perfectly well and cannot be fitted (§5).
                let primary_key = match &dataset.grain {
                    Some(sc_dataset::Grain::Table { key, .. }) => Json::String(key.clone()),
                    _ => Json::Null,
                };
                let split_error = dataset.split_refusal().map_or(Json::Null, Json::String);
                Ok(HandlerResponse::ok(json!({
                    "columns": shape
                        .columns
                        .iter()
                        .map(|c| json!({ "name": c.name, "type": c.ty.name() }))
                        .collect::<Vec<_>>(),
                    "rows": frame.to_rows(),
                    "primary_key": primary_key,
                    "split_error": split_error,
                })))
            }
        }
    });

    reg.register_admin("listModels", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let table = ctx.query_get("table").map(str::to_owned);
                // The **stored** rows, with the live set consulted only for its
                // issues: a model that fails validation would otherwise vanish
                // from the screen that exists to repair it.
                let stored = match &table {
                    Some(table) => sc_model::models_for_table(&catalog, table).await?,
                    None => sc_model::list_models(&catalog).await?,
                };
                let live = sc_model::Models::load(&catalog, &models.registry()).await?;
                let mut out = Vec::with_capacity(stored.len());
                for model in &stored {
                    let problem = live
                        .issues()
                        .iter()
                        .find(|i| i.model == model.name)
                        .map(|i| i.problem.clone());
                    out.push(model_json(&catalog, model, problem).await?);
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("getModel", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::ModelId(parse_uuid(ctx.path_param("id")?, "model")?);
                let model = sc_model::load_model(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no model with id {id}")))?;
                let problem = sc_model::validate_model(&catalog, &models.registry(), &model, None)
                    .await
                    .err()
                    .map(|e| e.to_string());
                Ok(HandlerResponse::ok(
                    model_json(&catalog, &model, problem).await?,
                ))
            }
        }
    });

    reg.register_admin("saveModel", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let body = rows::require_object(&ctx.body)?;
                let created = body.get("id").is_none_or(Json::is_null);
                let model = model_from_body(&catalog, body).await?;
                // A program is checked by its compiler before it is saved (Stan
                // §5): `stanc` when this server has CmdStan, which refuses a
                // program it would refuse at fit time a minute into a compile;
                // our own parse, with the notice saying so, when it has not.
                // Here rather than in `validate_model`, which also runs when the
                // models are loaded and must not start a process per model.
                let program_check = if model.provider.trim() == sc_stan::STAN_PROVIDER {
                    let check = models.stan().check_program(&model.configuration).await?;
                    Some(json!({
                        "warnings": Some(check.warnings).filter(|w| !w.trim().is_empty()),
                        "notice": check.notice,
                    }))
                } else {
                    None
                };
                // Validated against the same registry a fit runs with, and with
                // the dataset's **shape** where it can be read: the provider's
                // form is over the dataset's columns, so checking the
                // configuration without them would push half the checking to fit
                // time and the guessing to the admin.
                let shape = dataset_shape(&models, &model.dataset).await;
                sc_model::save_model(&catalog, &models.registry(), &model, shape.as_ref()).await?;
                let mut saved = model_json(&catalog, &model, None).await?;
                if let (Some(check), Json::Object(map)) = (program_check, &mut saved) {
                    map.insert("program_check".to_owned(), check);
                }
                let response = HandlerResponse::ok(saved);
                Ok(if created {
                    response.with_status(201)
                } else {
                    response
                })
            }
        }
    });

    reg.register_admin("cloneModel", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::ModelId(parse_uuid(ctx.path_param("id")?, "model")?);
                let name = match &ctx.body {
                    Json::Object(body) => {
                        body.get("name").and_then(Json::as_str).map(str::to_owned)
                    }
                    _ => None,
                };
                let copy = sc_model::clone_model(&catalog, &models.registry(), id, name.as_deref())
                    .await?;
                Ok(HandlerResponse::ok(model_json(&catalog, &copy, None).await?).with_status(201))
            }
        }
    });

    reg.register_admin("modelUsage", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_model::ModelId(parse_uuid(ctx.path_param("id")?, "model")?);
                let model = sc_model::load_model(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no model with id {id}")))?;
                Ok(HandlerResponse::ok(model_usage(&catalog, &model).await?))
            }
        }
    });

    reg.register_admin("patchModelViewState", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_model::ModelId(parse_uuid(ctx.path_param("id")?, "model")?);
                let patch = match ctx.body.get("patch") {
                    Some(Json::Object(patch)) => patch.clone(),
                    _ => {
                        return Err(Error::invalid(
                            "`patch` is an object of the keys to set, and `null` for each key \
                             to remove",
                        ));
                    }
                };
                let state = sc_model::patch_model_view_state(&catalog, id, &patch).await?;
                Ok(HandlerResponse::ok(
                    json!({ "view_state": Json::Object(state) }),
                ))
            }
        }
    });

    reg.register_admin("deleteModel", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::ModelId(parse_uuid(ctx.path_param("id")?, "model")?);
                // `delete_model` takes its instances with it, which is the
                // difference from an agent's runs: an instance is not a record
                // of what happened, it is a fit *of this model*, and its
                // coefficients mean nothing without the dataset they were fitted
                // over.
                if !sc_model::delete_model(&catalog, &models.registry(), id).await? {
                    return Err(Error::not_found(format!("no model with id {id}")));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    reg.register_admin("fitModel", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::ModelId(parse_uuid(ctx.path_param("id")?, "model")?);
                let model = sc_model::load_model(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no model with id {id}")))?;
                // Checked before the row exists: discovering a bad provider or
                // an unresolvable column *inside* the job would mean an instance
                // id that was already returned and a fit that failed a second
                // later, and the admin is standing in front of the button.
                sc_model::validate_model(&catalog, &models.registry(), &model, None).await?;
                let mut instance = sc_model::ModelInstance::starting(model.id);
                if let Json::Object(body) = &ctx.body {
                    instance.name = optional_str(body, "name");
                    instance.description = optional_str(body, "description");
                }
                let instance = models.start_fit(&model, instance).await?;
                Ok(HandlerResponse::ok(model_instance_json(&instance)).with_status(201))
            }
        }
    });

    reg.register_admin("listModelInstances", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_model::ModelId(parse_uuid(ctx.path_param("id")?, "model")?);
                let model = sc_model::load_model(&catalog, id).await?;
                let out: Vec<Json> = sc_model::list_model_instances(&catalog, id)
                    .await?
                    .iter()
                    .map(|i| with_dataset_changed(model_instance_json(i), i, model.as_ref()))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("getModelInstance", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let id = sc_model::InstanceId(parse_uuid(ctx.path_param("id")?, "model instance")?);
                let instance = sc_model::require_model_instance(&catalog, id).await?;
                let owner = sc_model::load_model(&catalog, instance.model)
                    .await
                    .ok()
                    .flatten();
                let mut out = with_dataset_changed(
                    model_instance_detail_json(&instance),
                    &instance,
                    owner.as_ref(),
                );
                // "The program has changed since this fit" (Stan TODO §18):
                // the provider compares its snapshot with the model's program
                // now. Null for a provider with no program, and whenever it
                // cannot tell — a screen must still open when a store is down.
                let changed = match (
                    models_of(&apps),
                    sc_model::load_model(&catalog, instance.model).await,
                ) {
                    (Ok(models), Ok(Some(model))) => {
                        match models.registry().require(model.provider.trim()) {
                            Ok(provider) => {
                                provider
                                    .program_changed(&model.configuration, &instance.state)
                                    .await
                            }
                            Err(_) => None,
                        }
                    }
                    _ => None,
                };
                if let Json::Object(map) = &mut out {
                    map.insert("program_changed".to_owned(), json!(changed));
                }
                Ok(HandlerResponse::ok(out))
            }
        }
    });

    reg.register_admin("deleteModelInstance", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::InstanceId(parse_uuid(ctx.path_param("id")?, "model instance")?);
                if !sc_model::delete_model_instance(&catalog, &models.registry(), id).await? {
                    return Err(Error::not_found(format!("no model instance with id {id}")));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    reg.register_admin("activateModelInstance", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_model::InstanceId(parse_uuid(ctx.path_param("id")?, "model instance")?);
                let mut instance = sc_model::require_model_instance(&catalog, id).await?;
                if !instance.is_usable() {
                    return Err(Error::invalid(format!(
                        "instance {id} is `{}`, so it cannot be made active: only a fit that \
                         finished can answer a prediction",
                        instance.status
                    )));
                }
                instance.active = true;
                // The store enforces at most one active instance per model, so
                // activating this one is what deactivates whichever was.
                sc_model::save_model_instance(&catalog, &instance).await?;
                Ok(HandlerResponse::ok(model_instance_json(&instance)))
            }
        }
    });

    reg.register_admin("predictRows", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let body = rows::require_object(&ctx.body)?;
                let (model, instance) = prediction_target(&catalog, body).await?;
                let literal = match body.get("rows") {
                    None | Some(Json::Null) => None,
                    Some(Json::Array(rows)) => Some(rows.clone()),
                    Some(_) => return Err(Error::invalid("`rows` must be an array of objects")),
                };
                let filter = body
                    .get("filter")
                    .and_then(Json::as_str)
                    .map(str::trim)
                    .filter(|f| !f.is_empty());
                if literal.is_some() && filter.is_some() {
                    return Err(Error::invalid(
                        "give either `rows` or `filter`: literal rows are not rows of the table, \
                         so a filter over them means nothing",
                    ));
                }
                // The filter is translated here rather than inside the seam,
                // because it is the *same* translation a dataset's own filter
                // goes through and a second set of rules about what a filter may
                // say would be a second thing to keep in step.
                let restrict = match filter {
                    Some(formula) => Some(sc_model::translate_filter(
                        model.table(),
                        formula,
                        &catalog.schema_shape()?,
                    )?),
                    None => None,
                };
                let subject = match &literal {
                    Some(rows) => sc_model::Subject::Rows(rows),
                    None => sc_model::Subject::Dataset(restrict.as_ref()),
                };
                let answer = sc_model::predict_subject(
                    &models.registry(),
                    models.source().as_ref(),
                    &model,
                    &instance,
                    subject,
                    models.max_rows(),
                )
                .await?;
                let mut predictions = Vec::with_capacity(answer.predictions.len());
                for (i, prediction) in answer.predictions.iter().enumerate() {
                    predictions.push(json!({
                        "prediction": serde_json::to_value(prediction)
                            .unwrap_or(Json::Null),
                        "value": prediction.to_json()?,
                        "key": answer.keys.get(i).cloned(),
                    }));
                }
                Ok(HandlerResponse::ok(json!({
                    "instance": instance.id.0,
                    "outcome": serde_json::to_value(instance.outcome()?)
                        .unwrap_or(Json::Null),
                    "predictions": predictions,
                })))
            }
        }
    });

    // Stan TODO §§5, 13, 16, 18. The reading and the write-back are the host's
    // (`sc_model::read_draws`, `summarise_variable`, `plan_write`) and the
    // write is `sc_api::models::write_posterior`, which a code body's model
    // handle makes too; what is here is the
    // reading of the request and the two things only a server can do — read a
    // model's datasets through the row layer, and start or stop a job.

    reg.register_admin("getProgramInterface", {
        let apps = apps.clone();
        move |ctx| {
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let store = ctx
                    .query_get("store")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Error::invalid("`store` is required"))?;
                let path = ctx
                    .query_get("path")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Error::invalid("`path` is required"))?;
                let mut config = Attrs::new();
                config.insert(
                    sc_stan::config_keys::PROGRAM_STORE.to_owned(),
                    Json::from(store),
                );
                config.insert(sc_stan::config_keys::PROGRAM.to_owned(), Json::from(path));
                // A refused program is the answer the button exists for, so it
                // is a field of a successful response rather than a failed one.
                Ok(HandlerResponse::ok(
                    match models.stan().check_program(&config).await {
                        Ok(check) => json!({
                            "interface": serde_json::to_value(&check.interface)
                                .unwrap_or(Json::Null),
                            "warnings": Some(check.warnings).filter(|w| !w.trim().is_empty()),
                            "notice": check.notice,
                            "error": Json::Null,
                        }),
                        Err(e) => json!({
                            "interface": Json::Null,
                            "warnings": Json::Null,
                            "notice": Json::Null,
                            "error": plain_sentence(&e),
                        }),
                    },
                ))
            }
        }
    });

    reg.register_admin("previewModelData", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let model = model_from_body(&catalog, rows::require_object(&ctx.body)?).await?;
                let (_, interface) = crate::posterior::binding_interface(&models, &model).await?;
                let datasets = crate::posterior::posterior_datasets(&models, &model).await?;
                let preview = sc_model::preview_data(
                    &interface,
                    &model.configuration,
                    &datasets,
                    models.posterior_limits().max_data_values,
                );
                Ok(HandlerResponse::ok(
                    serde_json::to_value(&preview)
                        .map_err(|e| Error::msg(format!("the preview: {e}")))?,
                ))
            }
        }
    });

    reg.register_admin("suggestBindings", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let model = model_from_body(&catalog, rows::require_object(&ctx.body)?).await?;
                let (_, interface) = crate::posterior::binding_interface(&models, &model).await?;
                let suggestions =
                    sc_model::suggest_bindings(&interface, &model, &catalog.schema_shape()?);
                Ok(HandlerResponse::ok(json!({
                    "bindings": Json::Object(suggestions.bindings),
                    "reasons": serde_json::to_value(&suggestions.reasons)
                        .unwrap_or(Json::Null),
                })))
            }
        }
    });

    reg.register_admin("compileModel", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::ModelId(parse_uuid(ctx.path_param("id")?, "model")?);
                let model = sc_model::load_model(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no model with id {id}")))?;
                if model.provider.trim() != sc_stan::STAN_PROVIDER {
                    return Err(Error::invalid(format!(
                        "model `{}` has no program to compile: its provider is `{}`",
                        model.name, model.provider
                    )));
                }
                let minutes = model
                    .configuration
                    .get(sc_stan::config_keys::MAX_RUNTIME_MINUTES)
                    .and_then(Json::as_u64)
                    .unwrap_or(60);
                let answer = models
                    .stan()
                    .compile(
                        &model.configuration,
                        std::time::Duration::from_secs(minutes * 60),
                    )
                    .await?;
                Ok(HandlerResponse::ok(
                    serde_json::to_value(&answer)
                        .map_err(|e| Error::msg(format!("the compile: {e}")))?,
                ))
            }
        }
    });

    reg.register_admin("cancelModelFit", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::InstanceId(parse_uuid(ctx.path_param("id")?, "model instance")?);
                let instance = sc_model::require_model_instance(&catalog, id).await?;
                let model = sc_model::load_model(&catalog, instance.model)
                    .await?
                    .ok_or_else(|| {
                        Error::not_found(format!("the model of instance {id} is gone"))
                    })?;
                // Any fit (analytics TODO A3.3): a provider whose fit is a
                // process is killed within a second; any other is stopped at
                // the next stage of the fit — after the read, between grid
                // points, before scoring — since a call into a library cannot
                // be interrupted halfway.
                models.registry().require(model.provider.trim())?;
                if !sc_model::request_fit_cancel(&catalog, id).await? {
                    return Err(Error::invalid(format!(
                        "instance {id} is `{}`: there is no running fit to cancel",
                        instance.status
                    )));
                }
                let instance = sc_model::require_model_instance(&catalog, id).await?;
                Ok(HandlerResponse::ok(model_instance_json(&instance)))
            }
        }
    });

    reg.register_admin("getModelDraws", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::InstanceId(parse_uuid(ctx.path_param("id")?, "model instance")?);
                let instance = sc_model::require_model_instance(&catalog, id).await?;
                let request = sc_model::DrawsRequest {
                    variable: required_query(&ctx, "variable")?,
                    selection: sc_model::Selection::from_json(
                        query_json(&ctx, "elements")?.as_ref(),
                    )?,
                    chains: match ctx.query_get("chains").map(str::trim) {
                        None | Some("") => None,
                        Some(list) => Some(
                            list.split(',')
                                .map(|c| {
                                    c.trim().parse::<u32>().map_err(|_| {
                                        Error::invalid(format!(
                                            "`chains` is a comma-separated list of chain                                              numbers, and `{}` is not one",
                                            c.trim()
                                        ))
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?,
                        ),
                    },
                    warmup: matches!(ctx.query_get("warmup"), Some("true" | "1")),
                    thin: match ctx.query_get("thin").map(str::trim) {
                        None | Some("") => 1,
                        Some(n) => n.parse::<usize>().ok().filter(|n| *n >= 1).ok_or_else(
                            || Error::invalid(format!("`thin` must be a whole number from 1, not `{n}`")),
                        )?,
                    },
                };
                let draws = sc_model::read_draws(
                    &catalog,
                    &instance,
                    &request,
                    models.max_draws_response(),
                )
                .await?;
                Ok(HandlerResponse::ok(serde_json::to_value(&draws).map_err(
                    |e| Error::msg(format!("the draws: {e}")),
                )?))
            }
        }
    });

    reg.register_admin("getPosteriorSummary", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_model::InstanceId(parse_uuid(ctx.path_param("id")?, "model instance")?);
                let instance = sc_model::require_model_instance(&catalog, id).await?;
                let variable = required_query(&ctx, "variable")?;
                let selection =
                    sc_model::Selection::from_json(query_json(&ctx, "elements")?.as_ref())?;
                let summary =
                    sc_model::summarise_variable(&catalog, &instance, &variable, &selection)
                        .await?;
                Ok(HandlerResponse::ok(
                    serde_json::to_value(&summary)
                        .map_err(|e| Error::msg(format!("the summary: {e}")))?,
                ))
            }
        }
    });

    reg.register_admin("downloadModelRun", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let models = models_of(&apps)?;
                let id = sc_model::InstanceId(parse_uuid(ctx.path_param("id")?, "model instance")?);
                let instance = sc_model::require_model_instance(&catalog, id).await?;
                let model = sc_model::load_model(&catalog, instance.model)
                    .await?
                    .ok_or_else(|| {
                        Error::not_found(format!("the model of instance {id} is gone"))
                    })?;
                let bytes = crate::posterior::run_zip(&catalog, &models, &model, &instance).await?;
                Ok(HandlerResponse::download(crate::handler::Download {
                    body: crate::handler::DownloadBody::Bytes(Bytes::from(bytes)),
                    content_type: "application/zip".to_owned(),
                    filename: crate::posterior::run_filename(&model, &instance),
                }))
            }
        }
    });

    reg.register_admin("writePosterior", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_model::InstanceId(parse_uuid(ctx.path_param("id")?, "model instance")?);
                let instance = sc_model::require_model_instance(&catalog, id).await?;
                let model = sc_model::load_model(&catalog, instance.model)
                    .await?
                    .ok_or_else(|| {
                        Error::not_found(format!("the model of instance {id} is gone"))
                    })?;
                // Nulls the generated client sends for absent optional fields
                // are absent here: the write-back refuses fields it does not
                // take, and "no table" is not "a table called null".
                let body: Map<String, Json> = rows::require_object(&ctx.body)?
                    .iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let write = sc_model::PosteriorWrite::from_json(&Json::Object(body))?;
                // The admin's own authority — the same the row API grants them —
                // through the ordinary write path, so the target table's own
                // triggers fire.
                let authority = admin_caller(ctx.user());
                let written = sc_api::models::write_posterior(
                    &catalog,
                    &model,
                    &instance,
                    &write,
                    &sc_access::Scoped::new(&catalog, sc_catalog::Authority::Caller(&authority)),
                )
                .await?;
                Ok(HandlerResponse::ok(written))
            }
        }
    });
}

/// The ceiling on a preview's `limit` — see [`DATASET_PREVIEW_ROWS`].
const DATASET_PREVIEW_MAX: u64 = 500;

/// A required, non-empty query parameter.
fn required_query<W>(ctx: &crate::handler::HandlerCtx<W>, name: &str) -> Result<String> {
    ctx.query_get(name)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("`{name}` is required")))
}

/// An error as the sentence it was written as, without its kind's prefix — for
/// an answer that carries a refusal as a field rather than as a failure. A
/// fault (a [`System`](sc_error::ErrorKind::System) error) keeps its whole
/// chain, for whoever reports it.
fn plain_sentence(e: &Error) -> String {
    match e.kind() {
        sc_error::ErrorKind::Application => e.message_and_causes(),
        sc_error::ErrorKind::System => sc_error::format_chain(e),
    }
}

/// A model's dataset as the list and the form see it: the reference, and what
/// it resolved to.
fn dataset_json(dataset: &sc_model::Dataset) -> Json {
    json!({
        "dataset_id": dataset.id,
        "name": dataset.name,
        "table": dataset.table,
        "grain": dataset.grain,
        "columns": dataset
            .columns
            .iter()
            .map(|c| json!({ "name": c.name, "expr": c.expr, "type": c.ty, "key": c.key }))
            .collect::<Vec<_>>(),
        "error": dataset.error,
    })
}

/// A fit's JSON with whether its model's datasets have changed since it read
/// them (`null` when it recorded none, or the model is gone).
fn with_dataset_changed(
    mut out: Json,
    instance: &sc_model::ModelInstance,
    model: Option<&sc_model::Model>,
) -> Json {
    let changed = model.and_then(|m| sc_model::dataset_changed(instance, &m.dataset, &m.related));
    if let Json::Object(map) = &mut out {
        map.insert("dataset_changed".to_owned(), json!(changed));
    }
    out
}

/// What refers to the model `name` (`modelUsage`, analytics TODO A3.5).
///
/// A calculated field calls it when its formula's analysis has a
/// `predict("name")`; a formula that no longer validates is matched on its
/// text instead, since it is exactly the kind of use a delete warning should
/// still find. A trigger, or a step of a workflow's current version, **fits**
/// it when it is a `fit_model` naming it, and otherwise **names** it when its
/// configuration mentions `predict("name")` or `models.get("name")` — a stored
/// calculation's `update_rows`, a code body. A workspace **shows** it when one
/// of its panels reads one of the model's fits (the usage index, A4.2).
async fn model_usage(catalog: &Catalog, model: &sc_model::Model) -> Result<Json> {
    let name = model.name.as_str();
    let quoted = serde_json::to_string(name).unwrap_or_default();
    let mentions = |text: &str| {
        [
            format!("predict({quoted})"),
            format!("models.get({quoted})"),
        ]
        .iter()
        .any(|needle| text.contains(needle.as_str()))
    };
    let shape = catalog.schema_shape()?;
    let mut fields = Vec::new();
    for table in catalog.tables()? {
        if table.is_system() {
            continue;
        }
        for field in &table.fields {
            let DataFieldKind::Calc { expression } = &field.kind else {
                continue;
            };
            let calls = sc_expr::Formula::parse(expression)
                .ok()
                .and_then(|f| f.validate(&shape, &table.name).ok())
                .map(|a| a.model_calls.iter().any(|c| c.model == name));
            if calls.unwrap_or_else(|| mentions(expression)) {
                fields.push(json!({ "table": table.name, "field": field.base.name }));
            }
        }
    }
    let how = |action: &str, configuration: &Attrs| -> Option<&'static str> {
        let config = Json::Object(configuration.clone());
        // `fit_model`'s name and its `model` setting.
        if action == "fit_model" && config.get("model").and_then(Json::as_str) == Some(name) {
            Some("fits")
        } else if any_string(&config, &mentions) {
            Some("names")
        } else {
            None
        }
    };
    let mut triggers = Vec::new();
    for trigger in list_triggers(catalog).await? {
        let found = match &trigger.body {
            TriggerBody::Action {
                action,
                configuration,
            } => how(action, configuration),
            TriggerBody::Workflow => {
                let mut found = None;
                if let Some(workflow) = sc_workflow::current_workflow(catalog, trigger.id).await? {
                    for step in &workflow.steps {
                        if let sc_workflow::StepKind::Action {
                            action,
                            configuration,
                        } = &step.kind
                            && let Some(h) = how(action, configuration)
                        {
                            // A fit outranks a mention.
                            if found != Some("fits") {
                                found = Some(h);
                            }
                        }
                    }
                }
                found
            }
        };
        if let Some(how) = found {
            triggers.push(json!({ "id": trigger.id.0, "name": trigger.name, "how": how }));
        }
    }
    let fits = sc_model::list_model_instances(catalog, model.id)
        .await?
        .into_iter()
        .map(|i| i.id.0);
    let workspaces = sc_analytics::panel::UsageIndex::build(catalog)
        .await?
        .fits(fits);
    Ok(json!({ "fields": fields, "triggers": triggers, "workspaces": workspaces }))
}

/// Whether any string inside `value` satisfies `test` — a code body, a
/// formula, at any depth of an action's configuration.
fn any_string(value: &Json, test: &impl Fn(&str) -> bool) -> bool {
    match value {
        Json::String(s) => test(s),
        Json::Array(items) => items.iter().any(|v| any_string(v, test)),
        Json::Object(map) => map.values().any(|v| any_string(v, test)),
        _ => false,
    }
}

/// One stored model as JSON (matching `model_schema`), with the reason it cannot
/// be fitted when there is one.
///
/// The instance summary rides along because that is what the list is read for:
/// which models have been fitted, when, and whether the last one worked.
async fn model_json(
    catalog: &Catalog,
    model: &sc_model::Model,
    problem: Option<String>,
) -> Result<Json> {
    let instances = sc_model::list_model_instances(catalog, model.id).await?;
    // Newest first is the store's order, so the head is the last fit.
    let summary =
        |i: &sc_model::ModelInstance| with_dataset_changed(model_instance_json(i), i, Some(model));
    let last_fit = instances.first().map(summary);
    let active = instances.iter().find(|i| i.active).map(summary);
    Ok(json!({
        "id": model.id.0,
        "name": model.name,
        "description": model.description,
        "provider": model.provider,
        "table_name": model.table(),
        "dataset": dataset_json(&model.dataset),
        "related": Json::Array(
            model
                .related
                .iter()
                .map(|r| json!({
                    "name": r.name,
                    "dataset_id": r.dataset.id,
                    "label": r.label,
                    "dataset_name": r.dataset.name,
                    "table": r.dataset.table,
                    "columns": r
                        .dataset
                        .columns
                        .iter()
                        .map(|c| json!({ "name": c.name, "type": c.ty, "key": c.key }))
                        .collect::<Vec<_>>(),
                    "error": r.dataset.error,
                }))
                .collect(),
        ),
        "configuration": Json::Object(model.configuration.clone()),
        "hyperparameters": Json::Object(model.hyperparameters.clone()),
        "split": serde_json::to_value(model.split)
            .map_err(|e| Error::msg(format!("split: {e}")))?,
        "attributes": Json::Object(model.attributes.clone()),
        "error": problem,
        "instances": instances.len(),
        "last_fit": last_fit,
        "active_instance": active,
        "program_check": Json::Null,
        // The screens' layout (analytics TODO A3.4); `{}` where the row has
        // none yet.
        "view_state": Json::Object(sc_model::model_view_state(catalog, model.id).await?),
    }))
}

/// One fit as a list entry (matching `model_instance_schema`): everything except
/// the parameters, the encoding and the search, which a list of forty fits must
/// not carry.
fn model_instance_json(instance: &sc_model::ModelInstance) -> Json {
    json!({
        "id": instance.id.0,
        "model": instance.model.0,
        "name": instance.name,
        "description": instance.description,
        "status": instance.status.as_str(),
        "created": instance.created,
        "active": instance.active,
        // Present only on the rows that failed, which is why it is an attribute
        // in the row and a nullable field here (§15).
        "error": instance.error(),
        "hyperparameters": Json::Object(instance.hyperparameters.clone()),
        "outcome": instance.attributes.get(sc_model::ATTR_OUTCOME),
        "metrics": instance.metrics,
        "rows": instance.attributes.get(sc_model::ATTR_ROWS),
        "progress": instance.attributes.get(sc_model::ATTR_PROGRESS),
        "cancel_requested": sc_model::cancel_requested(instance),
        "warnings": instance
            .attributes
            .get(sc_model::ATTR_WARNINGS)
            .cloned()
            .unwrap_or_else(|| Json::Array(Vec::new())),
    })
}

/// One fit in full (matching `model_instance_detail_schema`): the list's fields
/// plus what the instance screen renders.
///
/// The provider's serialised state is deliberately **not** here: it is opaque to
/// everything but the provider and it is the big column — a random forest's is
/// every tree.
fn model_instance_detail_json(instance: &sc_model::ModelInstance) -> Json {
    let mut out = model_instance_json(instance);
    if let Json::Object(map) = &mut out {
        map.insert(
            "parameters".to_owned(),
            serde_json::to_value(&instance.parameters).unwrap_or(Json::Array(Vec::new())),
        );
        map.insert("encoding".to_owned(), instance.encoding.clone());
        map.insert(
            "search".to_owned(),
            instance
                .attributes
                .get(sc_model::ATTR_SEARCH)
                .cloned()
                .unwrap_or(Json::Array(Vec::new())),
        );
        map.insert(
            "variables".to_owned(),
            instance
                .attributes
                .get(sc_model::ATTR_AXES)
                .cloned()
                .unwrap_or_else(|| Json::Object(Map::new())),
        );
        map.insert(
            "binding".to_owned(),
            instance
                .attributes
                .get(sc_model::ATTR_BINDING)
                .cloned()
                .unwrap_or(Json::Null),
        );
    }
    out
}

/// Which model and which fit a `predictRows` is about.
///
/// Naming a **model** means its active instance, and that is the whole reason
/// `active` exists: the admin refits, activates, and every caller that named the
/// model follows without being edited. Naming an **instance** is how you compare
/// two fits, or predict with one you have not activated.
async fn prediction_target(
    catalog: &Catalog,
    body: &Map<String, Json>,
) -> Result<(sc_model::Model, sc_model::ModelInstance)> {
    let instance = match body.get("instance").filter(|v| !v.is_null()) {
        Some(Json::String(raw)) => {
            let id = sc_model::InstanceId(parse_uuid(raw, "model instance")?);
            Some(sc_model::require_model_instance(catalog, id).await?)
        }
        Some(_) => return Err(Error::invalid("`instance` must be a model instance id")),
        None => None,
    };
    let model_id = match (&instance, body.get("model").filter(|v| !v.is_null())) {
        (Some(instance), _) => instance.model,
        (None, Some(Json::String(raw))) => sc_model::ModelId(parse_uuid(raw, "model")?),
        (None, Some(_)) => return Err(Error::invalid("`model` must be a model id")),
        (None, None) => {
            return Err(Error::invalid(
                "name either a `model`, meaning its active instance, or an `instance`",
            ));
        }
    };
    let model = sc_model::load_model(catalog, model_id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no model with id {model_id}")))?;
    let instance = match instance {
        Some(instance) => instance,
        None => sc_model::active_model_instance(catalog, model_id)
            .await?
            .ok_or_else(|| {
                Error::invalid(format!(
                    "model `{}` has no active instance: fit it and mark a fit active, or name \
                     an `instance` directly",
                    model.name
                ))
            })?,
    };
    Ok((model, instance))
}
