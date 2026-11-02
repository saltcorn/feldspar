//! Agents (configuration) and their runs.

use super::*;

/// Register this domain's admin handlers on `reg`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    let catalog = state.catalog.clone();
    let apps = state.apps.clone();

    // The row ⇄ live-set path again (§11.2), for the third record that has it:
    // every save goes through `save_agent`, which validates against the *same*
    // registry the chat socket runs with, and the list carries the reason a
    // stored agent is not usable beside it — because editing it is the repair.

    reg.register_admin("listAgents", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                // The **stored** rows, with the live set consulted only for its
                // issues: an agent that fails validation would otherwise vanish
                // from the screen that exists to repair it.
                let services = agents_of(&apps)?;
                let stored = sc_agent::list_agents(&catalog).await?;
                let live = sc_agent::validate::Agents::load(&catalog, services.registry()).await?;
                let out: Vec<Json> = stored
                    .iter()
                    .map(|agent| {
                        let problem = live
                            .issues()
                            .iter()
                            .find(|i| i.agent == agent.name)
                            .map(|i| i.problem.clone());
                        agent_json(agent, problem)
                    })
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("createAgent", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let services = agents_of(&apps)?;
                let agent = agent_from_body(sc_agent::AgentId::new(), &ctx.body)?;
                sc_agent::save_agent(&catalog, services.registry(), &agent).await?;
                Ok(HandlerResponse::ok(agent_json(&agent, None)).with_status(201))
            }
        }
    });

    reg.register_admin("updateAgent", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let services = agents_of(&apps)?;
                let id = parse_agent_id(ctx.path_param("id")?)?;
                let Some(stored) = sc_agent::load_agent(&catalog, id).await? else {
                    return Err(Error::not_found(format!("no agent with id {id}")));
                };
                // The id is the path's, not the body's — the row's identity is
                // not something a payload gets to reassign.
                let agent = agent_from_body(id, &ctx.body)?;
                // An application exposes it by name (§11.7), so a rename would
                // leave that application exposing nothing.
                if agent.name != stored.name {
                    refuse_if_exposed(&catalog, &stored.name, "renamed").await?;
                }
                sc_agent::save_agent(&catalog, services.registry(), &agent).await?;
                Ok(HandlerResponse::ok(agent_json(&agent, None)))
            }
        }
    });

    reg.register_admin("deleteAgent", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = parse_agent_id(ctx.path_param("id")?)?;
                if let Some(stored) = sc_agent::load_agent(&catalog, id).await? {
                    refuse_if_exposed(&catalog, &stored.name, "deleted").await?;
                }
                if !sc_agent::delete_agent(&catalog, id).await? {
                    return Err(Error::not_found(format!("no agent with id {id}")));
                }
                // Its runs are **not** deleted with it: a run is a record of what
                // happened, its subject is the agent's name rather than its id
                // (§11.4), and a transcript that disappeared with the definition
                // would take the evidence with it.
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });

    reg.register_admin("listAgentTraits", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let services = agents_of(&apps)?;
                let mut out = Vec::new();
                for trait_ in services.registry().all() {
                    // Resolved like every other spec the admin UI renders, so a
                    // setting whose options come from the catalog (a table name,
                    // a trigger name) arrives as a picker rather than a text box.
                    let spec = resolve_options(&catalog, trait_.config_spec()).await?;
                    out.push(json!({
                        "name": trait_.name(),
                        "description": say(trait_.description(), &ctx.locale),
                        "config_spec": spec_json(&spec, &ctx.locale),
                    }));
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("listRuns", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let agent = ctx.path_param("agent")?.to_owned();
                let out: Vec<Json> = sc_agent::list_runs(&catalog, &agent)
                    .await?
                    .iter()
                    .map(run_summary_json)
                    .collect();
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("getRun", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_agent::RunId(parse_uuid(ctx.path_param("id")?, "run")?);
                let run = sc_agent::require_run(&catalog, id).await?;
                Ok(HandlerResponse::ok(
                    run_json_with_workflow(&catalog, &run, &ctx.locale).await?,
                ))
            }
        }
    });

    reg.register_admin("getRunPart", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_agent::RunId(parse_uuid(ctx.path_param("id")?, "run")?);
                let part = parse_uuid(ctx.path_param("part")?, "part")?;
                let stored = sc_agent::load_display_part(&catalog, id, part)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("run {id} has no part {part}")))?;
                Ok(HandlerResponse::ok(run_part_json(&stored)))
            }
        }
    });

    reg.register_admin("getRunDiff", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_agent::RunId(parse_uuid(ctx.path_param("id")?, "run")?);
                let run = sc_agent::require_run(&catalog, id).await?;
                let (scopes, runs) = sc_agent_builtins::agent_run_diff(&catalog, &run).await?;
                Ok(HandlerResponse::ok(json!({
                    "run": run.id.0,
                    "runs": runs.iter().map(|r| r.0).collect::<Vec<_>>(),
                    "scopes": scopes.iter().map(scope_diff_json).collect::<Vec<_>>(),
                })))
            }
        }
    });

    reg.register_admin("deleteRun", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let id = sc_agent::RunId(parse_uuid(ctx.path_param("id")?, "run")?);
                if !sc_agent::delete_run(&catalog, id).await? {
                    return Err(Error::not_found(format!("no run with id {id}")));
                }
                Ok(HandlerResponse::ok(json!({ "deleted": true })))
            }
        }
    });
}

/// An [`Application`] as the API returns it (matching `application_schema`): the
/// id plus every field, with the nested framework/CSP/attributes as plain JSON.
/// Refuse to delete or rename agent `name` while an application exposes it
/// (§11.7), naming the applications — the way deleting an LLM provider an
/// agent names is refused.
async fn refuse_if_exposed(catalog: &Catalog, name: &str, verb: &str) -> Result<()> {
    let using = sc_app::applications_exposing_agent(catalog, name).await?;
    if using.is_empty() {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "agent `{name}` cannot be {verb}: application{} {} expose{} it; stop exposing it \
         there first",
        if using.len() == 1 { "" } else { "s" },
        using
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", "),
        if using.len() == 1 { "s" } else { "" },
    )))
}

/// Parse an agent id from a path parameter.
fn parse_agent_id(raw: &str) -> Result<sc_agent::AgentId> {
    Ok(sc_agent::AgentId(parse_uuid(raw, "agent")?))
}

/// A stored display part (matching `run_part_schema`), its bytes in base64.
pub(crate) fn run_part_json(stored: &sc_agent::StoredPart) -> Json {
    use base64::Engine as _;
    json!({
        "id": stored.reference.id,
        "kind": stored.reference.kind,
        "mime_type": stored.reference.mime_type,
        "summary": stored.reference.summary,
        "data": stored.data,
        "content_base64": stored
            .content
            .as_ref()
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes)),
    })
}

/// One scope's changes (matching `run_diff_schema`).
fn scope_diff_json(scope: &sc_agent_builtins::ScopeDiff) -> Json {
    let diff = &scope.diff;
    json!({
        "store": scope.scope.store,
        "root": scope.scope.root,
        "files": diff.files.iter().map(|f| json!({
            "path": f.path,
            "status": match f.status {
                sc_agent_builtins::ChangeStatus::Added => "added",
                sc_agent_builtins::ChangeStatus::Modified => "modified",
                sc_agent_builtins::ChangeStatus::Deleted => "deleted",
            },
            "added": f.added,
            "removed": f.removed,
        })).collect::<Vec<_>>(),
        "moves": diff.moves.iter().map(|(from, to)| json!({ "from": from, "to": to })).collect::<Vec<_>>(),
        "stat": diff.stat(),
        "unified": diff.unified,
    })
}
