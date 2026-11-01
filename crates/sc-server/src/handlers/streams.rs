//! Streams.

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    // Dataflows as an entity (TODO "Streams", task 6.2). The row ⇄ live-set
    // path once more, with one difference from every other record that has it:
    // the live set here is a set of **connections**, so a save does not just
    // reload a registry, it drops and remakes broker sessions. That is why
    // every write below ends in `reload` — the flow follows the row without a
    // restart (§6) — and why the read path carries the supervisor's status
    // beside the stored definition.
    //
    // Two things happen *here* rather than in `sc-stream`: secrets are redacted
    // on the way out and merged back on the way in (§2.3, the file-store and
    // LLM-provider arrangement), and the delete's refusal is handed the
    // triggers that name the stream, because `sc-stream` sits below `sc-action`
    // and cannot ask.

    reg.register_admin("listStreamProviders", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let streams = streams_of(&apps)?;
                let registry = streams.registry();
                // The configuration so far, because the element type is a
                // function of it (§3): `payload = json` with four declared keys
                // is a different element type from `payload = text`, and the
                // form asks this endpoint again every time one of them changes.
                let configuration = match query_json(&ctx, "configuration")? {
                    Some(Json::Object(map)) => Some(map),
                    Some(Json::Null) | None => None,
                    Some(_) => {
                        return Err(Error::invalid("`configuration` must be a JSON object"));
                    }
                };
                // Which provider that configuration belongs to. Without it a
                // configuration would be resolved against every provider in
                // turn, and the answer for the ones it was not written for is
                // noise — `mqtt`'s settings mean nothing to a polled feed.
                let for_provider = ctx
                    .query_get("provider")
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let mut providers = Vec::with_capacity(registry.len());
                for provider in registry.all() {
                    let kind = provider.kind();
                    // Resolved like every other spec the admin UI renders, so a
                    // setting whose options come from the catalog arrives as a
                    // picker rather than a text box.
                    let spec = resolve_options(&catalog, kind.config_spec.clone()).await?;
                    let asked_about = for_provider.is_none_or(|name| name == kind.name);
                    let (element_type, element_type_error) = match &configuration {
                        Some(config) if asked_about => {
                            // `resolve_element_type`: a module's provider
                            // declares its type as a function on a worker, and
                            // this is the form's own "what would this
                            // configuration produce?" — the question that has
                            // to cross the seam rather than read a cache.
                            match provider
                                .resolve_element_type(config)
                                .await
                                .and_then(|ty| ty.validate().map(|()| ty))
                            {
                                Ok(ty) => (serde_json::to_value(&ty).ok(), None),
                                // Not an error for the request: "`json` with no
                                // declared keys" is what the form is waiting to
                                // be told, and every other provider still
                                // answers.
                                Err(e) => (None, Some(e.to_string())),
                            }
                        }
                        _ => (None, None),
                    };
                    providers.push(json!({
                        "name": kind.name,
                        "label": say_unless_module(&kind.label, kind.module.as_ref(), &ctx.locale),
                        "description": say_unless_module(
                            &kind.description,
                            kind.module.as_ref(),
                            &ctx.locale,
                        ),
                        "module": kind.module,
                        "config_spec": spec_json(&spec, &ctx.locale),
                        "element_type": element_type,
                        "element_type_error": element_type_error,
                    }));
                }
                // A picker missing the one broker client the docs name reads
                // like a bug; the sentence reads like the decision it is.
                // (`internal` is always built in, so the picker is never
                // empty — the notice is about what is not in it.)
                Ok(HandlerResponse::ok(json!({
                    "providers": providers,
                    "builtins_compiled_out": !sc_stream::MQTT_COMPILED_IN,
                    "notice": if !sc_stream::MQTT_COMPILED_IN {
                        Json::String(sc_stream::BUILTINS_COMPILED_OUT.to_owned())
                    } else {
                        Json::Null
                    },
                })))
            }
        }
    });

    reg.register_admin("listStreams", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let streams = streams_of(&apps)?;
                // The **stored** rows, with the running set consulted only for
                // its status: a stream that fails validation would otherwise
                // vanish from the screen that exists to repair it.
                let stored = sc_stream::list_streams(&catalog).await?;
                resolve_element_types(&streams, &stored).await;
                let out: Vec<Json> = stored
                    .iter()
                    .map(|stream| stream_json(&streams, &apps, stream))
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("getStream", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let streams = streams_of(&apps)?;
                let stream = require_stream_by_id(&catalog, ctx.path_param("id")?).await?;
                resolve_element_types(&streams, std::slice::from_ref(&stream)).await;
                Ok(HandlerResponse::ok(stream_json(&streams, &apps, &stream)))
            }
        }
    });

    reg.register_admin("saveStream", typed(state, save_stream));

    reg.register_admin("deleteStream", typed(state, delete_stream));

    reg.register_admin("streamStatus", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let streams = streams_of(&apps)?;
                let stream = require_stream_by_id(&catalog, ctx.path_param("id")?).await?;
                let running = streams.supervisor().get(stream.id);
                // A 200 saying `running: false` rather than a 404: "this stream
                // is not running" is exactly what the screen asked, and a row
                // that exists is not a missing thing.
                //
                // `running` is about the **subscription**, not about whether
                // the supervisor is holding the stream: a disabled one it has
                // stopped is still in its map, with `stopped` as its status and
                // its counters intact, and reporting that as running would make
                // the switch on the list look like it had not worked.
                Ok(HandlerResponse::ok(json!({
                    "id": stream.id.0,
                    "name": stream.name,
                    "running": running.as_ref().is_some_and(|r| r.status().is_running()),
                    "status": running
                        .as_ref()
                        .and_then(|r| serde_json::to_value(r.status()).ok()),
                    "counters": running.as_ref().map(|r| r.counters().to_json()),
                    "element_type": running
                        .as_ref()
                        .and_then(|r| r.element_type())
                        .and_then(|ty| serde_json::to_value(ty).ok()),
                    "listeners": running.as_ref().map_or(0, |r| r.listeners()),
                    "subscribers": apps.live().hub().subscribers(stream.id),
                    "changes": change_counts_json(&apps, stream.id),
                })))
            }
        }
    });
}

/// One stored stream by the id in a path, or a 404 naming it.
async fn require_stream_by_id(catalog: &Catalog, raw: &str) -> Result<sc_stream::Stream> {
    let id = sc_stream::StreamId(parse_uuid(raw, "stream")?);
    sc_stream::load_stream(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no stream with id {id}")))
}

/// The triggers that name `stream` as their channel, as the phrases
/// [`delete_stream`](sc_stream::delete_stream) refuses with.
///
/// Passed *in* rather than looked up inside `sc-stream`, because that crate
/// sits below `sc-action` and has no way to ask — the arrangement
/// `delete_llm_model` already has with the agents that name a model.
async fn stream_trigger_referents(catalog: &Catalog, stream: &str) -> Result<Vec<String>> {
    Ok(list_triggers(catalog)
        .await?
        .into_iter()
        .filter(|t| {
            t.when == EventKind::Stream && t.channel.as_deref().map(str::trim) == Some(stream)
        })
        .map(|t| sc_stream::trigger_referent(&t.name))
        .collect())
}

/// Ask each stream's provider for its element type, so the synchronous
/// [`stream_json`] below can answer with it (TODO "Streams" §12).
///
/// A no-op for every compiled-in provider, whose element type is a pure
/// function of the configuration. It matters for a **module's**, whose
/// declaration is JavaScript on a worker: `PollingProvider` answers the
/// synchronous side from what it last resolved, and this is what resolves it
/// for a row that is stored but not running — a disabled stream, or one on a
/// server that has just started.
///
/// A failure is not reported here: `stream_json` makes the same call a moment
/// later and puts the sentence in the row's `error`, which is where the admin
/// reads it.
async fn resolve_element_types(streams: &crate::StreamServices, stored: &[sc_stream::Stream]) {
    let registry = streams.registry();
    for stream in stored {
        if let Some(provider) = registry.get(stream.provider.trim()) {
            let _ = provider.resolve_element_type(&stream.configuration).await;
        }
    }
}

/// The `saveStream` endpoint.
pub(crate) async fn save_stream(
    state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    body: Json,
) -> Result<HandlerResponse> {
    let HandlerState { catalog, apps } = state;
    let streams = streams_of(&apps)?;
    let registry = streams.registry();
    let body = require_object(&body)?;
    let created = body.get("id").is_none_or(Json::is_null);
    let submitted = stream_from_body(body)?;
    // The sentinel the form was handed comes back as the sentinel;
    // what is behind it is whatever the row already holds (§2.3).
    // Read *before* the save, because the save is what overwrites
    // it.
    let stored = if created {
        None
    } else {
        sc_stream::load_stream(&catalog, submitted.id).await?
    };
    let stream = sc_stream::restore_secrets(&registry, stored.as_ref(), &submitted);
    sc_stream::save_stream(&catalog, &registry, &stream).await?;
    // The flow follows the row: a stream saved enabled is connected
    // by the time this response is written, and one whose broker
    // moved has dropped the old session. A reload that fails is a
    // stream that is `failed` with its reason, not a save that did
    // not happen — so it is reported rather than returned.
    if let Err(e) = streams.reload(&catalog).await {
        eprintln!(
            "feldspar: the stream set could not be reloaded after saving `{}`: {}",
            stream.name,
            sc_error::format_causes(&e)
        );
    }
    let response = HandlerResponse::ok(stream_json(&streams, &apps, &stream));
    Ok(if created {
        response.with_status(201)
    } else {
        response
    })
}

/// The `deleteStream` endpoint.
pub(crate) async fn delete_stream(
    state: HandlerState,
    ctx: HandlerCtx<Admin>,
    _body: Json,
) -> Result<HandlerResponse> {
    let HandlerState { catalog, apps } = state;
    let streams = streams_of(&apps)?;
    let stream = require_stream_by_id(&catalog, ctx.path_param("id")?).await?;
    // The referents are passed in, as `delete_llm_model`'s are:
    // `sc-stream` sits below `sc-action` and cannot ask which
    // triggers listen to a channel, and a stream deleted out from
    // under one leaves a trigger waiting for an event nothing will
    // ever raise.
    let referents = stream_trigger_referents(&catalog, &stream.name).await?;
    if !sc_stream::delete_stream(&catalog, stream.id, &referents).await? {
        return Err(Error::not_found(format!("no stream with id {}", stream.id)));
    }
    if let Err(e) = streams.reload(&catalog).await {
        eprintln!(
            "feldspar: the stream set could not be reloaded after deleting `{}`: {}",
            stream.name,
            sc_error::format_causes(&e)
        );
    }
    Ok(HandlerResponse::ok(json!({ "deleted": true })))
}
