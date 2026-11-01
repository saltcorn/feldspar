//! LLM providers and their models.

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();

    //
    // The file-store handlers one section up, with one difference that runs
    // through all of them: a provider's config holds an API key, so it is
    // redacted on the way out (`llm_provider_json`) and the sentinel is merged
    // back on the way in (`unredacted_provider_config`). Both happen here, where
    // the record is serialised, rather than in the screen — a second reader added
    // later gets the same treatment without having to know it needs it (§11.1).

    reg.register_admin("listLlmProviders", {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let default = default_provider_id(&catalog).await?;
                let out: Vec<Json> = list_llm_providers(&catalog)
                    .await?
                    .iter()
                    .map(|def| llm_provider_json(&catalog, def, default))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("createLlmProvider", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                // A create mints a fresh id; the body carries everything else.
                // There is nothing stored to merge a sentinel against, so a
                // submitted sentinel is dropped by `merge_secrets` and the save
                // fails on the missing required key — which is the right answer
                // for a form that was never given one.
                let def = llm_provider_from_body(LlmProviderDefId::new(), &ctx.body)?;
                save_llm_provider(&catalog, &def).await?;
                let default = default_provider_id(&catalog).await?;
                Ok(
                    HandlerResponse::ok(llm_provider_json(&catalog, &def, default))
                        .with_status(201),
                )
            }
        }
    });

    reg.register_admin("updateLlmProvider", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_llm_provider_id(ctx.path_param("id")?)?;
                let existing = load_llm_provider(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no LLM provider with id {id:?}")))?;
                // The id is the path's, not the body's — the row's identity is
                // not something a payload gets to reassign.
                let mut def = llm_provider_from_body(id, &ctx.body)?;
                // Attributes are server-managed (§9) and the form neither shows
                // nor sends them, so they are carried across rather than reset.
                def.attributes = existing.attributes.clone();
                // The key the form sent back untouched is the sentinel it was
                // shown, not the key: restore what is stored.
                def.config =
                    unredacted_provider_config(&def.backend, &existing.config, &def.config);
                save_llm_provider(&catalog, &def).await?;
                let default = default_provider_id(&catalog).await?;
                Ok(HandlerResponse::ok(llm_provider_json(
                    &catalog, &def, default,
                )))
            }
        }
    });

    reg.register_admin("setDefaultLlmProvider", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_llm_provider_id(ctx.path_param("id")?)?;
                sc_llm::set_default_llm_provider(&catalog, Some(id)).await?;
                let def = load_llm_provider(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no LLM provider with id {id:?}")))?;
                Ok(HandlerResponse::ok(llm_provider_json(
                    &catalog,
                    &def,
                    Some(id),
                )))
            }
        }
    });

    reg.register_admin("deleteLlmProvider", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_llm_provider_id(ctx.path_param("id")?)?;
                // The agents naming this provider are the references `sc-llm`
                // cannot see for itself: `_fd_agents` is a layer above it, so
                // they are collected here and passed in (the same arrangement
                // `delete_file_store` has with applications). Deleting a
                // provider an agent still calls through would leave that agent
                // unable to answer, with the reason a layer away from the
                // action that caused it.
                let users = agents_using_provider(&catalog, id).await?;
                let deleted = delete_llm_provider(&catalog, id, &users).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.register_admin("listLlmProviderBackends", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let mut out = Vec::new();
                for name in sc_llm::registered_backends() {
                    let spec = resolve_options(&catalog, provider_config_spec(&name)?).await?;
                    out.push(json!({
                        "name": name,
                        "config_spec": spec_json(&spec, &ctx.locale),
                    }));
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    //
    // One row per model a provider serves (TODO §3a). A model's config holds no
    // secret, so nothing is redacted here; what a listed model carries beside
    // its row is what its blank settings resolve to.

    reg.register_admin("listLlmModels", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let provider = require_provider_by_id(&catalog, ctx.path_param("id")?).await?;
                let out: Vec<Json> = list_llm_models(&catalog, &provider)
                    .await?
                    .iter()
                    .map(|m| llm_model_json(&provider, m))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("createLlmModel", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let provider = require_provider_by_id(&catalog, ctx.path_param("id")?).await?;
                let model = llm_model_from_body(LlmModelDefId::new(), provider.id, &ctx.body)?;
                let saved = save_llm_model(&catalog, &model).await?;
                Ok(HandlerResponse::ok(llm_model_json(&provider, &saved)).with_status(201))
            }
        }
    });

    reg.register_admin("updateLlmModel", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_llm_model_id(ctx.path_param("id")?)?;
                let existing = load_llm_model(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no LLM model with id {}", id.0)))?;
                let provider = load_llm_provider(&catalog, existing.provider_id)
                    .await?
                    .ok_or_else(|| {
                        Error::not_found(format!(
                            "the provider of LLM model `{}` is gone",
                            existing.name
                        ))
                    })?;
                let mut model = llm_model_from_body(id, existing.provider_id, &ctx.body)?;
                model.attributes = existing.attributes.clone();
                // A body that leaves the kind out keeps it; one that changes it
                // is refused by the save.
                if ctx.body.get("kind").is_none_or(Json::is_null) {
                    model.kind = existing.kind;
                }
                // A rename breaks the agents naming it, and losing the default
                // flag breaks the agents naming no model, so each is refused the
                // way a delete is. Making *another* model the default is not a
                // break: those agents follow the default.
                let renamed = model.name.trim() != existing.name;
                let undefaulted = existing.is_default && !model.is_default;
                if renamed || undefaulted {
                    let users =
                        agents_using_model(&catalog, &provider, &existing, renamed, undefaulted)
                            .await?;
                    if !users.is_empty() {
                        return Err(Error::invalid(format!(
                            "LLM model `{}` is still used by {} as it is now; \
                             change those agents first",
                            existing.name,
                            users.join(", ")
                        )));
                    }
                }
                let saved = save_llm_model(&catalog, &model).await?;
                Ok(HandlerResponse::ok(llm_model_json(&provider, &saved)))
            }
        }
    });

    reg.register_admin("deleteLlmModel", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_llm_model_id(ctx.path_param("id")?)?;
                let Some(model) = load_llm_model(&catalog, id).await? else {
                    return Ok(HandlerResponse::ok(json!({ "deleted": false })));
                };
                let users = match load_llm_provider(&catalog, model.provider_id).await? {
                    Some(provider) => {
                        agents_using_model(&catalog, &provider, &model, true, true).await?
                    }
                    None => Vec::new(),
                };
                let deleted = delete_llm_model(&catalog, id, &users).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": deleted })))
            }
        }
    });

    reg.register_admin("listLlmModelSettings", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let backend = ctx.path_param("backend")?;
                let kind = ctx
                    .query
                    .iter()
                    .find(|(k, _)| k == "kind")
                    .map_or(Ok(sc_llm::ModelKind::Chat), |(_, v)| {
                        sc_llm::ModelKind::parse(v)
                    })?;
                let spec = resolve_options(&catalog, model_config_spec(backend, kind)?).await?;
                Ok(HandlerResponse::ok(Json::Array(spec_json(
                    &spec,
                    &ctx.locale,
                ))))
            }
        }
    });

    // Asks the host which models it serves. Like a test, a host that cannot
    // answer is a 200 with `ok: false`: the refusal *is* the answer, and "type
    // the name instead" is what the admin needs to read.
    reg.register_admin("fetchLlmModels", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let provider = require_provider_by_id(&catalog, ctx.path_param("id")?).await?;
                let existing: Vec<String> = list_llm_models(&catalog, &provider)
                    .await?
                    .into_iter()
                    .map(|m| m.name)
                    .collect();
                Ok(HandlerResponse::ok(
                    match fetch_host_models(&provider).await {
                        Ok(names) => {
                            let names: Vec<String> = names
                                .into_iter()
                                .filter(|n| !existing.contains(n))
                                .collect();
                            json!({
                                "ok": true,
                                "message": format!("{} model(s) without a row", names.len()),
                                "names": names,
                            })
                        }
                        Err(e) => json!({
                            "ok": false,
                            "message": sc_error::format_causes(&e),
                            "names": [],
                        }),
                    },
                ))
            }
        }
    });

    // Send one trivial prompt to one model and report what came back. This is
    // the only handler in the admin API besides *Fetch models* that waits on a
    // third party, and it is worth it: without it a wrong key or model name is
    // discovered inside a chat transcript, where it looks like the agent
    // misbehaving rather than the configuration being wrong.
    //
    // A failure is a **200 with `ok: false`**, not an error status. The provider
    // refusing is the answer to the question that was asked — "does this work?"
    // — and an error response would make the UI show it as a broken request
    // rather than as the diagnostic it is.
    reg.register_admin("testLlmModel", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let obj = require_object(&ctx.body)?;
                let backend = non_empty_str_field(obj, "backend")?.to_owned();
                let submitted = object_field(obj, "config")?;
                let name = non_empty_str_field(obj, "name")?.to_owned();
                let model_config = match obj.get("model_config") {
                    None | Some(Json::Null) => sc_types::Attrs::new(),
                    Some(_) => object_field(obj, "model_config")?,
                };

                // Testing a *saved* provider must not require retyping its key,
                // so a submitted sentinel resolves against the stored row when
                // the body names one.
                let stored = match obj.get("provider_id").and_then(Json::as_str) {
                    Some(raw) => load_llm_provider(&catalog, parse_llm_provider_id(raw)?).await?,
                    None => None,
                };
                let provider = match &stored {
                    // The configuration file's provider is tested exactly as it
                    // is, and only with its own models. Merging the form's
                    // settings over it would let an admin who cannot see its key
                    // send it to a `base_url` of their choosing.
                    Some(stored) if sc_llm::is_host_llm_provider(&catalog, stored.id) => {
                        if sc_llm::load_llm_model_by_name(&catalog, stored, &name)
                            .await?
                            .is_none()
                        {
                            return Err(Error::invalid(format!(
                                "LLM provider `{}` is set by the server's configuration \
                                 file, which does not list a model `{name}`",
                                stored.name
                            )));
                        }
                        stored.clone()
                    }
                    Some(stored) => LlmProviderDef {
                        config: unredacted_provider_config(&backend, &stored.config, &submitted),
                        ..LlmProviderDef::new(&stored.name, &backend).id(stored.id)
                    },
                    None => LlmProviderDef {
                        config: submitted,
                        ..LlmProviderDef::new("test", &backend)
                    },
                };
                let kind =
                    sc_llm::ModelKind::parse(obj.get("kind").and_then(Json::as_str).unwrap_or(""))?;
                let model = LlmModelDef {
                    config: model_config,
                    ..LlmModelDef::new(provider.id, &name).kind(kind)
                };
                if kind != sc_llm::ModelKind::Chat {
                    sc_llm::validate_model_config(&provider.backend, &model)?;
                    return Ok(HandlerResponse::ok(
                        test_non_chat_model(&provider, &model).await,
                    ));
                }
                // A structurally wrong config is an ordinary `Err`: it is the
                // admin's typo, not the provider's answer, and the form should
                // show it the way it shows a failed save.
                let connected = connect_model(&provider, &model)?;
                let capabilities = serde_json::to_value(connected.capabilities)
                    .map_err(|e| Error::msg(e.to_string()))?;
                let prices = serde_json::to_value(connected.prices)
                    .map_err(|e| Error::msg(e.to_string()))?;

                // Capped hard: the question is "does this endpoint answer",
                // and a provider that takes it as an invitation to write an
                // essay would bill the admin for asking.
                let probe = LlmRequest::prompt("Reply with the single word: ok").max_tokens(16);
                let outcome = match connected.provider.stream(probe).await {
                    Ok(stream) => stream.collect().await,
                    // Failing to *start* — a refused connection, a rejected key
                    // — is the same kind of answer as failing mid-stream, and
                    // the admin reads it the same way.
                    Err(e) => Err(e),
                };

                Ok(HandlerResponse::ok(match outcome {
                    Ok(msg) => json!({
                        "ok": true,
                        // What the model actually said, so an admin who pointed
                        // at the wrong endpoint sees a wrong answer rather than
                        // a green tick.
                        "message": msg.content.trim(),
                        "model": name,
                        "capabilities": capabilities,
                        "prices": prices,
                    }),
                    // The provider's own words, whole. A category ("auth
                    // failed") would throw away the part that says *which* key
                    // or *which* model.
                    Err(e) => json!({
                        "ok": false,
                        "message": sc_error::format_causes(&e),
                        "model": name,
                        "capabilities": capabilities,
                        "prices": prices,
                    }),
                }))
            }
        }
    });
}

/// The reverse, on the way in: sentinels replaced by what is stored, so editing
/// a provider's name does not save the mask over its key.
fn unredacted_provider_config(
    backend: &str,
    stored: &sc_types::Attrs,
    submitted: &sc_types::Attrs,
) -> sc_types::Attrs {
    match provider_config_spec(backend) {
        Ok(spec) => sc_types::merge_secrets(&spec, stored, submitted),
        Err(_) => submitted.clone(),
    }
}

/// Parse a provider id from a path parameter.
fn parse_llm_provider_id(raw: &str) -> Result<LlmProviderDefId> {
    uuid::Uuid::parse_str(raw)
        .map(LlmProviderDefId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid LLM provider id")))
}

/// Parse a model id from a path parameter.
fn parse_llm_model_id(raw: &str) -> Result<LlmModelDefId> {
    uuid::Uuid::parse_str(raw)
        .map(LlmModelDefId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid LLM model id")))
}

/// The provider a path's `id` names, or not found.
async fn require_provider_by_id(catalog: &Catalog, raw: &str) -> Result<LlmProviderDef> {
    let id = parse_llm_provider_id(raw)?;
    load_llm_provider(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no LLM provider with id {}", id.0)))
}

/// *Test* for a non-chat model (TODO-agent-skills AS3.1): embed or speak one
/// word, and say what came back. An image model is not tested, since every
/// test would generate (and bill) an image.
async fn test_non_chat_model(provider: &LlmProviderDef, model: &LlmModelDef) -> Json {
    let prices = serde_json::to_value(model.prices()).unwrap_or(Json::Null);
    let outcome = match model.kind {
        sc_llm::ModelKind::Embedding => sc_llm::embed(provider, model, &["ok".to_owned()])
            .await
            .map(|e| {
                format!(
                    "a vector of {} dimensions",
                    e.vectors.first().map_or(0, Vec::len)
                )
            }),
        sc_llm::ModelKind::Speech => {
            sc_llm::speak(provider, model, &sc_llm::SpeechRequest::new("ok"))
                .await
                .map(|s| format!("{} bytes of {} audio", s.bytes.len(), s.format))
        }
        _ => Err(Error::invalid(
            "an image model is not tested here, since each test would generate (and bill) \
             an image; it is checked the first time it is used",
        )),
    };
    match outcome {
        Ok(message) => json!({
            "ok": true,
            "message": message,
            "model": model.name,
            "capabilities": Json::Null,
            "prices": prices,
        }),
        Err(e) => json!({
            "ok": false,
            "message": sc_error::format_causes(&e),
            "model": model.name,
            "capabilities": Json::Null,
            "prices": prices,
        }),
    }
}

/// The agents that call through the LLM provider `id`, by name — what a delete
/// has to refuse over.
///
/// By the provider's **name**, because that is what an agent stores: an agent
/// references a provider the way a trigger references an action, so a provider
/// renamed out from under one is already a broken agent and deleting the row is
/// the same question.
async fn agents_using_provider(catalog: &Catalog, id: LlmProviderDefId) -> Result<Vec<String>> {
    let Some(def) = load_llm_provider(catalog, id).await? else {
        return Ok(Vec::new());
    };
    // No `_fd_agents` table *means* no agent has ever been defined — the same
    // reading `Agents::load` takes — so a server without agents installed
    // deletes a provider rather than failing over a table nobody made.
    if catalog.get(sc_agent::AGENTS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    Ok(sc_agent::list_agents(catalog)
        .await?
        .into_iter()
        .filter(|agent| {
            model_refs(agent)
                .iter()
                .any(|named| named.provider.trim() == def.name.trim())
        })
        .map(|agent| format!("agent `{}`", agent.name))
        .collect())
}

/// Every model an agent names: its own, and each role it sets (TODO §3). A
/// role stored in a shape that does not read names nothing.
fn model_refs(agent: &sc_agent::Agent) -> Vec<sc_agent::ModelRef> {
    sc_agent::ModelRole::ALL
        .iter()
        .filter_map(|role| agent.configured_role(*role).ok().flatten())
        .collect()
}

/// The agents that call the model row `model` of `provider`, by name: those
/// naming it (when `by_name`), and — when it is the default — those naming no
/// model (when `by_default`).
async fn agents_using_model(
    catalog: &Catalog,
    provider: &LlmProviderDef,
    model: &LlmModelDef,
    by_name: bool,
    by_default: bool,
) -> Result<Vec<String>> {
    if catalog.get(sc_agent::AGENTS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    Ok(sc_agent::list_agents(catalog)
        .await?
        .into_iter()
        .filter(|agent| {
            model_refs(agent).iter().any(|named| {
                named.provider.trim() == provider.name.trim()
                    && match named.model.as_deref() {
                        Some(name) => by_name && name == model.name,
                        None => {
                            by_default && model.is_default && model.kind == sc_llm::ModelKind::Chat
                        }
                    }
            })
        })
        .map(|agent| format!("agent `{}`", agent.name))
        .collect())
}
