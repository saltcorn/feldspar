//! Triggers, the actions they run, workflows and what an admin does to a workflow run.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    // The row ⇄ live-set path (§10.2). Every mutation goes through
    // `save_trigger`/`delete_trigger` — which validate against the *same*
    // registry the dispatcher runs — and then reloads the live set, so the list
    // an admin is looking at is the set that will fire.

    reg.register_admin("listTriggers", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                // The **stored** rows, not the live set: a trigger that fails
                // validation is dropped from the live set and would otherwise
                // vanish from the screen that exists to repair it. Its reason
                // comes from the live set's issues, alongside it.
                let stored = list_triggers(&catalog).await?;
                let dispatcher = triggers_of(&apps)?;
                let issues = dispatcher.triggers()?;
                let mut out: Vec<Json> = Vec::with_capacity(stored.len());
                for t in &stored {
                    let problem = issues
                        .issues()
                        .iter()
                        .find(|i| i.trigger == t.name)
                        .map(|i| i.problem.clone());
                    // One extra read per **workflow** trigger, and none for an
                    // action one: a list of triggers is short, and "workflow" on
                    // its own says nothing that tells one apart from another.
                    let workflow = if t.body == TriggerBody::Workflow {
                        sc_workflow::current_workflow(&catalog, t.id)
                            .await?
                            .map(|w| (w.version, w.steps.len()))
                    } else {
                        None
                    };
                    // Masked (§11.1): `send_email`'s `smtp_password` is a
                    // secret setting, and this list is what the editor reads.
                    let shown = dispatcher.registry().redact_trigger(&catalog, t);
                    out.push(trigger_json_with(&shown, problem, workflow));
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("createTrigger", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let mut trigger = trigger_from_body(TriggerId::new(), &ctx.body)?;
                // A sentinel with nothing stored under it is dropped, never
                // saved as the password.
                dispatcher
                    .registry()
                    .merge_trigger_secrets(&catalog, None, &mut trigger)?;
                check_stream_channel(&catalog, &apps, &trigger).await?;
                save_trigger(&catalog, &dispatcher.registry(), &trigger).await?;
                // A workflow body gets **version 1** here, not on the editor's
                // first save (§10.3, phase 5.5): an empty workflow with one
                // start step, so a new workflow opens on a canvas rather than on
                // "no version of this has been saved yet", which is not a good
                // first thing for an admin to be told about something they have
                // just created.
                if trigger.body == TriggerBody::Workflow {
                    sc_workflow::save_workflow(
                        &catalog,
                        &sc_workflow::Workflow::empty(trigger.id),
                        "created",
                        ctx.user().map(|u| u.id()),
                    )
                    .await?;
                }
                dispatcher.reload(&catalog).await?;
                let shown = dispatcher.registry().redact_trigger(&catalog, &trigger);
                Ok(HandlerResponse::ok(trigger_json(&shown, None)).with_status(201))
            }
        }
    });

    reg.register_admin("updateTrigger", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let id = parse_trigger_id(ctx.path_param("id")?)?;
                let Some(stored) = load_trigger(&catalog, id).await? else {
                    return Err(Error::not_found(format!("no trigger with id {id}")));
                };
                // The id is the path's, not the body's — the row's identity is
                // not something a payload gets to reassign.
                let mut trigger = trigger_from_body(id, &ctx.body)?;
                // The editor was shown the sentinel for a secret setting, and
                // sends it back for "unchanged": keep the stored value.
                dispatcher.registry().merge_trigger_secrets(
                    &catalog,
                    Some(&stored),
                    &mut trigger,
                )?;
                check_stream_channel(&catalog, &apps, &trigger).await?;
                save_trigger(&catalog, &dispatcher.registry(), &trigger).await?;
                // Switching an action body to a workflow one is the other way a
                // trigger comes to need a version 1 (§10.3, phase 5.5), and it
                // gets the same empty canvas a create does. Only when it has
                // none: a trigger switched to an action and back finds the
                // program it had, because the versions were never deleted.
                if trigger.body == TriggerBody::Workflow
                    && sc_workflow::max_version(&catalog, id).await?.is_none()
                {
                    sc_workflow::save_workflow(
                        &catalog,
                        &sc_workflow::Workflow::empty(id),
                        "created",
                        ctx.user().map(|u| u.id()),
                    )
                    .await?;
                }
                dispatcher.reload(&catalog).await?;
                let shown = dispatcher.registry().redact_trigger(&catalog, &trigger);
                Ok(HandlerResponse::ok(trigger_json(&shown, None)))
            }
        }
    });

    reg.register_admin("deleteTrigger", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let id = parse_trigger_id(ctx.path_param("id")?)?;
                if !delete_trigger(&catalog, id).await? {
                    return Err(Error::not_found(format!("no trigger with id {id}")));
                }
                // Append-only is a rule about *editing* a workflow, not a promise
                // that a deleted trigger leaves rows nobody can reach: the
                // versions are keyed by this id and there is now nothing to
                // address them by.
                sc_workflow::delete_workflow_versions(&catalog, id).await?;
                dispatcher.reload(&catalog).await?;
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    reg.register_admin("runTrigger", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let id = parse_trigger_id(ctx.path_param("id")?)?;
                // By id, then by name: the row is what the admin clicked, and
                // resolving it here means a rename cannot make the button run
                // somebody else's trigger.
                let trigger = load_trigger(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no trigger with id {id}")))?;
                // The posted body is the event's payload, and the caller is the
                // admin who pressed the button. A failing action comes back as
                // an error — a 200 carrying a hidden failure is the thing a
                // "test this now" button exists to prevent.
                let caller = admin_caller(&ctx.who);
                let result = dispatcher
                    .run_trigger(
                        &catalog,
                        &trigger.name,
                        ctx.body.clone(),
                        Authority::Caller(&caller),
                    )
                    .await?;
                Ok(HandlerResponse::ok(json!({ "result": result })))
            }
        }
    });

    reg.register_admin("testRunTrigger", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let id = parse_trigger_id(ctx.path_param("id")?)?;
                let trigger = load_trigger(&catalog, id)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("no trigger with id {id}")))?;
                let caller = admin_caller(&ctx.who);
                // A table trigger has no occurrence of its own when a person
                // presses a button, so one is borrowed: a row picked at random
                // from the table it fires on. Read *before* the run and sent
                // back with the result, because "which row was that?" is the
                // first question a surprising answer raises.
                let row = match trigger.when.is_table_event() {
                    true => random_row(&catalog, trigger.channel.as_deref(), &caller).await?,
                    false => None,
                };
                let run = dispatcher
                    .test_run_trigger(
                        &catalog,
                        &trigger.name,
                        ctx.body.clone(),
                        row.clone(),
                        Authority::Caller(&caller),
                    )
                    .await;
                // The failure is the answer here, not an error status: an admin
                // testing a trigger is asking what happens, and "it failed, with
                // this message, having printed this" is a complete answer that a
                // 400 would have thrown half of away.
                // The row is shown with its secrets as their sentinel; the run
                // itself was handed the stored row, which it reveals as any
                // trigger does.
                let mut shown_row = row.unwrap_or(Json::Null);
                if let Some(table) = trigger
                    .channel
                    .as_deref()
                    .and_then(|t| catalog.get(t).ok().flatten())
                {
                    sc_catalog::secrets::redact_json(&table, &mut shown_row);
                }
                let console: Vec<Json> = run
                    .console
                    .iter()
                    .map(|line| json!({ "level": line.level, "text": line.text }))
                    .collect();
                let (ok, result, error) = match run.result {
                    Ok(result) => (true, result, Json::Null),
                    Err(e) => (false, Json::Null, Json::String(sc_error::format_chain(&e))),
                };
                Ok(HandlerResponse::ok(json!({
                    "ok": ok,
                    "result": result,
                    "error": error,
                    "console": console,
                    "row": shown_row,
                })))
            }
        }
    });

    reg.register_admin("listActions", {
        let apps = apps.clone();
        let catalog = catalog.clone();
        move |ctx| {
            let apps = apps.clone();
            let catalog = catalog.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                // The table the trigger being edited fires on, when it has one.
                // An action's declaration may depend on it — `send_email`'s
                // attachment checkboxes are that table's File fields — and every
                // other action ignores it.
                let channel = ctx
                    .query_get("table")
                    .map(str::trim)
                    .filter(|table| !table.is_empty());
                // The models over this table, for the one declaration that asks
                // for them: `fit_model`'s picker. Read once rather than
                // per action, and **only when the request named a table** — with
                // no channel there is no "this table's models" to answer with,
                // and answering with an empty list would be claiming there are
                // none rather than that the question was not asked.
                let models = match channel {
                    Some(table) if catalog.get(sc_model::MODELS_TABLE)?.is_some() => Some(
                        sc_model::models_for_table(&catalog, table)
                            .await?
                            .into_iter()
                            .map(|m| m.name)
                            .collect::<Vec<_>>(),
                    ),
                    Some(_) => Some(Vec::new()),
                    None => None,
                };
                // The internal streams, for `publish`'s picker: streams are rows,
                // so the declaration can only ask for them, and this is where it
                // is answered — with an empty list on a server that has none,
                // which is how the step palette learns `publish` has nothing
                // to publish to yet.
                let internal_streams = internal_stream_names(&catalog).await?;
                let mut out = Vec::new();
                for action in dispatcher.registry().all() {
                    let spec = resolve_internal_streams(
                        resolve_model_options(action.config_spec_for(&catalog, channel), &models),
                        &internal_streams,
                    );
                    out.push(json!({
                        "name": action.name(),
                        "description": say(action.description(), &ctx.locale),
                        "config_spec": spec_json(&spec, &ctx.locale),
                        // Whether the step palette may offer it here (§10.3,
                        // phase 5.4). Decided from the declaration this same
                        // call is answering with, so the flag and the form
                        // the client would render cannot disagree.
                        "workflow_step": sc_workflow::usable_as_step(&spec),
                    }));
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    // The program half of a trigger whose body is a workflow (§10.3). Addressed
    // by the **trigger's** id, because that is what a workflow is identified by:
    // decision 1 made it a body rather than an entity, and inventing a second id
    // here would be inventing the entity back.
    //
    // Every read validates, and every write validates before it stores
    // (decision 11). A workflow that does not validate is still returned, still
    // listed and still editable — with its issues beside it, because editing it
    // is the repair — and what it cannot do is start a run.

    reg.register_admin("getWorkflow", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let trigger = require_workflow_trigger(&catalog, ctx.path_param("id")?).await?;
                let versions = sc_workflow::list_workflow_versions(&catalog, trigger.id).await?;
                // A specific version when the caller asked for one — which is
                // what the run detail screen does, because a run is pinned to the
                // version it started on and drawing it on today's steps would be
                // drawing the wrong program. Otherwise the newest, which is the
                // current one; a trigger created before its first save has none,
                // which is a real state and is answered as an empty canvas rather
                // than as an error.
                let asked = match ctx
                    .query_get("version")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(text) => Some(
                        text.parse::<u32>()
                            .map_err(|_| Error::invalid(format!("`{text}` is not a version")))?,
                    ),
                    None => None,
                };
                let workflow = match asked {
                    Some(version) => {
                        sc_workflow::require_workflow_version(&catalog, trigger.id, version).await?
                    }
                    None => match versions.first() {
                        Some(version) => version.workflow.clone(),
                        None => sc_workflow::Workflow::empty(trigger.id),
                    },
                };
                Ok(HandlerResponse::ok(
                    workflow_json(&catalog, &dispatcher, &trigger, &workflow, &versions).await?,
                ))
            }
        }
    });

    reg.register_admin("saveWorkflow", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let trigger = require_workflow_trigger(&catalog, ctx.path_param("id")?).await?;
                let obj = require_object(&ctx.body)?;
                let posted = obj
                    .get("workflow")
                    .ok_or_else(|| Error::invalid("field `workflow` is required"))?;
                let workflow = workflow_from_document(trigger.id, posted)?;
                // Refused *before* it is stored, with the whole list of problems
                // rather than the first: an admin told about one of three fixes
                // it and is told about the next one, which is three round trips
                // to save one workflow.
                sc_workflow::validate_workflow(
                    &catalog,
                    &dispatcher.registry(),
                    &workflow,
                    trigger.channel.as_deref(),
                )
                .await?;
                let saved = sc_workflow::save_workflow(
                    &catalog,
                    &workflow,
                    &optional_str(obj, "description"),
                    ctx.user().map(|u| u.id()),
                )
                .await?;
                let versions = sc_workflow::list_workflow_versions(&catalog, trigger.id).await?;
                Ok(HandlerResponse::ok(
                    workflow_json(&catalog, &dispatcher, &trigger, &saved, &versions).await?,
                ))
            }
        }
    });

    // Revert: a **new** version whose steps are an old one's. Not a rewrite, and
    // not a deletion of the versions in between — a run suspended on any of them
    // still has to be able to load the one it is pinned to tomorrow, which is
    // the whole reason the table is append-only (decision 2).
    reg.register_admin("revertWorkflow", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let trigger = require_workflow_trigger(&catalog, ctx.path_param("id")?).await?;
                let obj = require_object(&ctx.body)?;
                let version = obj
                    .get("version")
                    .and_then(Json::as_u64)
                    .ok_or_else(|| Error::invalid("field `version` must be a version number"))?;
                let version = u32::try_from(version)
                    .map_err(|_| Error::invalid(format!("there is no version {version}")))?;
                let old =
                    sc_workflow::require_workflow_version(&catalog, trigger.id, version).await?;
                // Checked again on the way back in: a version that was fine when
                // it was saved may name a table that has since been dropped, and
                // restoring it silently would restore a workflow that cannot run.
                sc_workflow::validate_workflow(
                    &catalog,
                    &dispatcher.registry(),
                    &old,
                    trigger.channel.as_deref(),
                )
                .await?;
                let description = match optional_str(obj, "description") {
                    given if given.is_empty() => format!("reverted to version {version}"),
                    given => given,
                };
                let saved = sc_workflow::save_workflow(
                    &catalog,
                    &old,
                    &description,
                    ctx.user().map(|u| u.id()),
                )
                .await?;
                let versions = sc_workflow::list_workflow_versions(&catalog, trigger.id).await?;
                Ok(HandlerResponse::ok(
                    workflow_json(&catalog, &dispatcher, &trigger, &saved, &versions).await?,
                ))
            }
        }
    });

    reg.register_admin("listWorkflowRuns", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let trigger = require_workflow_trigger(&catalog, ctx.path_param("id")?).await?;
                let state = match ctx
                    .query_get("state")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(state) => Some(sc_agent::RunState::parse(state)?),
                    None => None,
                };
                let runs = sc_workflow::list_workflow_runs(
                    &catalog,
                    &trigger.name,
                    state,
                    query_count(&ctx, "limit", DEFAULT_RUN_PAGE),
                    query_count(&ctx, "offset", 0),
                )
                .await?;
                let out: Vec<Json> = runs.iter().map(run_summary_json).collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("resumeRun", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let id = sc_agent::RunId(parse_uuid(ctx.path_param("id")?, "run")?);
                let run = sc_agent::require_run(&catalog, id).await?;
                // The step's own floor, checked here rather than assumed
                // (§10.3, phase 4.3). Today every caller that reaches this is an
                // admin and meets every floor; the check is still made, because
                // a form exposed to a role is exactly the thing that must not be
                // reachable by way of a surface that forgot to ask.
                if let Some(form) = sc_workflow::run_pending_form(&run)? {
                    sc_workflow::check_may_resume(&form, sc_auth::role_of(ctx.user()))?;
                }
                let values = match &ctx.body {
                    Json::Null => sc_types::Attrs::new(),
                    body => require_object(body)?.clone().into_iter().collect(),
                };
                let run = sc_workflow::resume_run(
                    &catalog,
                    &dispatcher,
                    &sc_types::clock::SystemClock,
                    id,
                    values,
                )
                .await?;
                Ok(HandlerResponse::ok(
                    run_json_with_workflow(&catalog, &run, &ctx.locale).await?,
                ))
            }
        }
    });

    reg.register_admin("cancelRun", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_agent::RunId(parse_uuid(ctx.path_param("id")?, "run")?);
                let reason = match &ctx.body {
                    Json::Null => None,
                    body => Some(optional_str(require_object(body)?, "reason").to_owned()),
                };
                let run = sc_workflow::cancel_run(
                    &catalog,
                    &sc_types::clock::SystemClock,
                    id,
                    reason.as_deref(),
                )
                .await?;
                Ok(HandlerResponse::ok(
                    run_json_with_workflow(&catalog, &run, &ctx.locale).await?,
                ))
            }
        }
    });

    reg.register_admin("retryRun", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let dispatcher = triggers_of(&apps)?;
                let id = sc_agent::RunId(parse_uuid(ctx.path_param("id")?, "run")?);
                let run = sc_workflow::retry_run(
                    &catalog,
                    &dispatcher,
                    &sc_types::clock::SystemClock,
                    id,
                )
                .await?;
                Ok(HandlerResponse::ok(
                    run_json_with_workflow(&catalog, &run, &ctx.locale).await?,
                ))
            }
        }
    });
}

/// How many runs a page holds when the caller does not say.
const DEFAULT_RUN_PAGE: usize = 50;

/// A non-negative count from a query parameter, or `fallback` for one that is
/// absent, blank or not a number.
///
/// Lenient on purpose, and only here: a paging parameter a client got wrong is
/// not worth failing a list over, and the failure it would cause — a screen that
/// shows nothing — is worse than a page of the default size.
fn query_count<W>(ctx: &crate::handler::HandlerCtx<W>, name: &str, fallback: usize) -> usize {
    ctx.query_get(name)
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(fallback)
}

/// The trigger of this id, refusing one whose body is an **action**.
///
/// By id, and strict about the body: the workflow endpoints all address a
/// trigger, and answering an empty canvas for a trigger that runs one action
/// would invite an admin to draw a program nothing would ever run.
async fn require_workflow_trigger(catalog: &Catalog, id: &str) -> Result<Trigger> {
    let id = parse_trigger_id(id)?;
    let trigger = load_trigger(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no trigger with id {id}")))?;
    if trigger.body != TriggerBody::Workflow {
        return Err(Error::invalid(format!(
            "trigger `{}` runs an action, not a workflow",
            trigger.name
        )));
    }
    Ok(trigger)
}

/// One workflow as the editor reads it (matching `workflow_schema`): the
/// program, the checks against it, and the history behind it.
async fn workflow_json(
    catalog: &Catalog,
    dispatcher: &sc_action::TriggerDispatcher,
    trigger: &Trigger,
    workflow: &sc_workflow::Workflow,
    versions: &[sc_workflow::WorkflowVersion],
) -> Result<Json> {
    let issues = sc_workflow::workflow_issues(
        catalog,
        &dispatcher.registry(),
        workflow,
        trigger.channel.as_deref(),
    )
    .await?;
    Ok(json!({
        "id": trigger.id.0,
        "name": trigger.name,
        "channel": trigger.channel,
        "version": workflow.version,
        "workflow": serde_json::to_value(workflow).unwrap_or(Json::Null),
        "issues": issues
            .iter()
            .map(|issue| json!({ "step": issue.step, "problem": issue.problem }))
            .collect::<Vec<_>>(),
        "versions": versions
            .iter()
            .map(|v| json!({
                "version": v.version,
                "description": v.description,
                "created_at": v.created_at,
                "created_by": v.created_by,
            }))
            .collect::<Vec<_>>(),
    }))
}

fn parse_trigger_id(raw: &str) -> Result<TriggerId> {
    uuid::Uuid::parse_str(raw)
        .map(TriggerId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid trigger id")))
}

/// Fill in the options of every field declaring
/// [`MODELS_QUERY`](sc_model::MODELS_QUERY).
///
/// A `config_spec_for` is **synchronous** and the models are rows, so an action
/// that wants "the models over this table" as a pick-list can only declare that
/// it wants them; the list is filled in where the declaration is served, which
/// is here. The same arrangement a File field's file-store picker already has
/// (`sc_catalog::resolve_options`), and the reason the query vocabulary exists.
///
/// `None` means the caller named no table, and the field is then **left
/// unresolved** rather than resolved to nothing. The difference matters: an
/// empty option list on a required field is what "this action cannot be
/// configured here" is spelled as (`sc_workflow::usable_as_step`), and a
/// channel-less workflow has not been told there are no models — it has not
/// asked.
fn resolve_model_options(spec: Vec<FormField>, models: &Option<Vec<String>>) -> Vec<FormField> {
    let Some(models) = models else {
        return spec;
    };
    spec.into_iter()
        .map(|field| match field.query() {
            Some(q) if q == sc_model::MODELS_QUERY => field.with_resolved_options(models.clone()),
            _ => field,
        })
        .collect()
}

/// The names of the stored `internal` streams, in name order — what the
/// `publish` action's stream picker offers.
///
/// A catalog with no `_fd_streams` table has none, which is what its absence
/// means; asking the catalog first keeps a genuine database error an error.
async fn internal_stream_names(catalog: &Catalog) -> Result<Vec<String>> {
    if catalog.get(sc_stream::STREAMS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = sc_stream::list_streams(catalog)
        .await?
        .into_iter()
        .filter(|stream| stream.provider == sc_stream::INTERNAL)
        .map(|stream| stream.name)
        .collect();
    names.sort();
    Ok(names)
}

/// Fill in the options of every field declaring
/// [`INTERNAL_STREAMS_QUERY`](sc_action_builtins::INTERNAL_STREAMS_QUERY) — the
/// arrangement [`resolve_model_options`] has, for its reason.
fn resolve_internal_streams(spec: Vec<FormField>, streams: &[String]) -> Vec<FormField> {
    spec.into_iter()
        .map(|field| match field.query() {
            Some(q) if q == sc_action_builtins::INTERNAL_STREAMS_QUERY => {
                field.with_resolved_options(streams.to_vec())
            }
            _ => field,
        })
        .collect()
}

/// Refuse a `stream` trigger whose channel names no stream (§8).
///
/// `sc-action` validates that the channel is **present and non-empty** and
/// stops there, because it cannot resolve a stream name without a dependency it
/// should not have. This is the other half, and it lives here because this is
/// where the admin is: the form offers the live stream list (`listStreams`), so
/// an unknown name is a stale form or a hand-written request, and either way
/// the trigger it would save is one waiting for an event nothing will ever
/// raise.
///
/// The error **names the streams there are**, because "no stream named
/// `boiler`" with a typo in it and "no stream named `boiler` because you have
/// not created it yet" are different problems and the list tells them apart.
///
/// Every other event kind passes straight through, including a server with no
/// stream support installed — refusing to save a table trigger because this
/// build has no streams would be the wrong end of the stick entirely.
async fn check_stream_channel(
    catalog: &Catalog,
    apps: &AppMounts,
    trigger: &sc_action::Trigger,
) -> Result<()> {
    if trigger.when != EventKind::Stream {
        return Ok(());
    }
    let Some(channel) = trigger
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        // `save_trigger`'s own validation says this better than a second
        // message here would.
        return Ok(());
    };
    // A server with no stream support cannot answer the question, and a trigger
    // on a stream it has no machinery for is refused where it would fire, not
    // where it is saved.
    streams_of(apps)?;
    if sc_stream::load_stream_by_name(catalog, channel)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let known = sc_stream::list_streams(catalog).await?;
    let names = if known.is_empty() {
        "there are no streams".to_owned()
    } else {
        format!(
            "the streams are {}",
            known
                .iter()
                .map(|s| format!("`{}`", s.name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Err(Error::invalid(format!(
        "trigger `{}`: no stream is named `{channel}`; {names}",
        trigger.name
    )))
}

/// One row of `table`, chosen at random — what a **test run** of an
/// `insert`/`update`/`delete` trigger is given to work on.
///
/// `None` rather than an error when there is nothing to choose from: a trigger
/// on an empty table is a trigger an admin can still test (its body may never
/// touch `row`), and "this table has no rows" is something the screen says
/// beside the result rather than something that stops the run.
///
/// Random by **offset**, not by a `random()` in the statement: the ordering
/// functions differ between the backends this server speaks to, and a count plus
/// an offset is one shape that works on all of them and goes through the same
/// row layer — ownership, RLS and provided tables included — as every other read.
/// The entropy is a v4 UUID's, which is the operating system's, because this
/// crate already mints those and a test row worth trusting is one nobody can
/// predict from the clock.
async fn random_row(
    catalog: &Catalog,
    table: Option<&str>,
    caller: &sc_catalog::AdminPrincipal,
) -> Result<Option<Json>> {
    let Some(name) = table else {
        return Ok(None);
    };
    let table = catalog.require(name)?;
    let count =
        rows::count_rows_where(catalog, &table, None, sc_access::admin_access(caller)).await?;
    if count <= 0 {
        return Ok(None);
    }
    // `count` is positive (checked above), and the remainder is less than it,
    // so both conversions hold.
    let below = u128::try_from(count).unwrap_or(1);
    let offset = u64::try_from(uuid::Uuid::new_v4().as_u128() % below).unwrap_or(0);
    let query = rows::RowQuery::new().limit(1).offset(offset);
    let read =
        rows::list_rows_query(catalog, &table, &query, sc_access::admin_access(caller)).await?;
    Ok(match read {
        Json::Array(rows) => rows.into_iter().next(),
        _ => None,
    })
}
