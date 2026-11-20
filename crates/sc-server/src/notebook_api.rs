//! The notebook's endpoints (analytics TODO A10.6): run a cell, run them all,
//! interrupt, restart, and the session's status — pushed to the browser as a
//! fit's progress is. Declared in `sc-api`'s `analytics.rs`; admin only.
//!
//! A run answers the cell as it now is and stores it: its outputs and its run
//! are written into the notebook's state as it is when the run ends, so a
//! change the browser saved meanwhile is kept, and a notebook reopens with
//! its outputs without running anything.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use sc_analytics::notebook::{Cell, CellKind, NotebookState};
use sc_analytics::{Workspace, WorkspaceId, WorkspaceKind};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

use crate::apps::AppMounts;
use crate::handler::{Admin, HandlerCtx, HandlerRegistry, HandlerResponse};
use crate::notebook::{CellResult, NotebookSessions, RunContext, SessionStatus};

/// The route the browser opens a notebook's session socket on.
pub const NOTEBOOK_SESSION_ROUTE: &str = "/api/workspaces/{id}/session/live";

/// How often the socket looks at the session: it is in memory, so this costs
/// a lock.
const POLL: Duration = Duration::from_millis(250);

/// Register the notebook's handlers.
pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>, apps: Arc<AppMounts>) {
    reg.handle(sc_api::handler_ids::RUN_NOTEBOOK_CELL, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move { run_cell(&catalog, &apps, &ctx).await }
        }
    });

    reg.handle(sc_api::handler_ids::RUN_NOTEBOOK, {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move { run_all(&catalog, &apps, &ctx).await }
        }
    });

    reg.handle(sc_api::handler_ids::INTERRUPT_NOTEBOOK, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let ws = notebook(&catalog, &ctx).await?;
                let status = NotebookSessions::of(&catalog).interrupt(ws.id, &user_key(&ctx.who));
                Ok(HandlerResponse::ok(status_json(&status)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::RESTART_NOTEBOOK, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let ws = notebook(&catalog, &ctx).await?;
                let status = NotebookSessions::of(&catalog).restart(ws.id, &user_key(&ctx.who));
                Ok(HandlerResponse::ok(status_json(&status)))
            }
        }
    });

    reg.handle(sc_api::handler_ids::GET_NOTEBOOK_SESSION, {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let ws = notebook(&catalog, &ctx).await?;
                let status = NotebookSessions::of(&catalog).status(ws.id, &user_key(&ctx.who));
                Ok(HandlerResponse::ok(status_json(&status)))
            }
        }
    });
}

/// Whose session a request's is: the administrator's id.
fn user_key(admin: &Admin) -> String {
    admin
        .user()
        .map_or_else(|| "system".to_owned(), |u| u.id().to_string())
}

fn status_json(status: &SessionStatus) -> Json {
    serde_json::to_value(status).unwrap_or(Json::Null)
}

/// The notebook the path names, or why it is not one.
async fn notebook(catalog: &Catalog, ctx: &HandlerCtx<Admin>) -> Result<Workspace> {
    let raw = ctx.path_param("id")?;
    let id = raw
        .parse()
        .map(WorkspaceId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a workspace id")))?;
    let ws = sc_analytics::load_workspace(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("there is no workspace with id {raw}")))?;
    if ws.kind != WorkspaceKind::Notebook {
        return Err(Error::invalid(format!(
            "`{}` is a {}, not a notebook: only a notebook's cells are run",
            ws.name,
            ws.kind.label()
        )));
    }
    Ok(ws)
}

fn context<'a>(
    catalog: &'a Catalog,
    apps: &'a AppMounts,
    ctx: &HandlerCtx<Admin>,
    ws: &Workspace,
    state: &NotebookState,
    user: &'a str,
) -> RunContext<'a> {
    RunContext {
        catalog,
        apps,
        workspace: ws.id,
        user,
        language: state.language,
        admin: ctx.who.principal(),
        run_caller: ctx.who.session().map_or_else(
            sc_agent::RunCaller::system,
            crate::admin_session::AdminSession::run_caller,
        ),
        regenerate: ctx.body.get("regenerate").and_then(Json::as_bool) == Some(true),
    }
}

/// `runNotebookCell`.
async fn run_cell(
    catalog: &Catalog,
    apps: &AppMounts,
    ctx: &HandlerCtx<Admin>,
) -> Result<HandlerResponse> {
    let ws = notebook(catalog, ctx).await?;
    let cell_id = ctx.path_param("cell")?.to_owned();
    let mut state = NotebookState::from_json(&ws.state)?;
    let at = state
        .cells
        .iter()
        .position(|c| c.id == cell_id)
        .ok_or_else(|| {
            Error::not_found(format!(
                "the notebook `{}` has no cell `{cell_id}`",
                ws.name
            ))
        })?;
    let source = ctx
        .body
        .get("source")
        .and_then(Json::as_str)
        .map(str::to_owned);
    if let Some(source) = &source {
        state.cells[at].source.clone_from(source);
    }
    if state.cells[at].kind == CellKind::Text {
        return Err(Error::invalid(format!(
            "cell `{cell_id}` is text, which is shown rather than run"
        )));
    }
    let user = user_key(&ctx.who);
    let cx = context(catalog, apps, ctx, &ws, &state, &user);
    let cell = state.cells[at].clone();
    let result = NotebookSessions::of(catalog)
        .run(&cx, &cell, &state.cells[..at])
        .await?;
    let stored = store(catalog, ws.id, &[(cell_id.clone(), source, result.clone())]).await?;
    let cell = stored
        .into_iter()
        .next()
        .unwrap_or_else(|| ran(cell, &result));
    Ok(HandlerResponse::ok(json!({
        "cell": cell,
        "session": status_json(&result.status),
    })))
}

/// `runNotebook`: every code cell in order, until one fails.
async fn run_all(
    catalog: &Catalog,
    apps: &AppMounts,
    ctx: &HandlerCtx<Admin>,
) -> Result<HandlerResponse> {
    let ws = notebook(catalog, ctx).await?;
    let state = NotebookState::from_json(&ws.state)?;
    let user = user_key(&ctx.who);
    let cx = context(catalog, apps, ctx, &ws, &state, &user);
    let sessions = NotebookSessions::of(catalog);
    let mut results = Vec::new();
    let mut status = sessions.status(ws.id, &user);
    for (at, cell) in state.cells.iter().enumerate() {
        if cell.kind != CellKind::Code || cell.source.trim().is_empty() {
            continue;
        }
        let result = sessions.run(&cx, cell, &state.cells[..at]).await?;
        status = result.status.clone();
        let failed = result.failed;
        results.push((cell.id.clone(), None, result));
        if failed {
            break;
        }
    }
    let cells = store(catalog, ws.id, &results).await?;
    Ok(HandlerResponse::ok(json!({
        "cells": cells,
        "session": status_json(&status),
    })))
}

/// `cell` with what a run made.
fn ran(mut cell: Cell, result: &CellResult) -> Cell {
    cell.outputs.clone_from(&result.outputs);
    cell.run = Some(result.run.clone());
    cell
}

/// Write the runs into the notebook's state as it is now, answering the
/// cells they changed. A cell deleted while it ran is not brought back.
async fn store(
    catalog: &Catalog,
    ws: WorkspaceId,
    runs: &[(String, Option<String>, CellResult)],
) -> Result<Vec<Cell>> {
    let latest = sc_analytics::require_workspace(catalog, ws).await?;
    let mut state = NotebookState::from_json(&latest.state)?;
    let mut changed = Vec::new();
    for (id, source, result) in runs {
        if let Some(cell) = state.cells.iter_mut().find(|c| &c.id == id) {
            if let Some(source) = source {
                cell.source.clone_from(source);
            }
            *cell = ran(cell.clone(), result);
            changed.push(cell.clone());
        }
    }
    if !changed.is_empty() {
        sc_analytics::save_workspace_state(catalog, ws, state.to_json()).await?;
    }
    Ok(changed)
}

/// Serve one session socket: the status of `user`'s session of `ws`, sent
/// whenever it changes, until the browser goes. The caller has established
/// that the request is an admin's.
pub(crate) fn session_upgrade(
    ws: WebSocketUpgrade,
    catalog: Arc<Catalog>,
    workspace: WorkspaceId,
    user: String,
    session: crate::admin_session::AdminWatch,
) -> axum::response::Response {
    crate::ws::upgrade(ws, crate::ws::LISTEN_ONLY, move |socket| {
        session.guard(watch(socket, catalog, workspace, user))
    })
}

async fn watch(mut socket: WebSocket, catalog: Arc<Catalog>, workspace: WorkspaceId, user: String) {
    let mut last: Option<Json> = None;
    loop {
        let status = status_json(&NotebookSessions::of(&catalog).status(workspace, &user));
        let frame = json!({ "type": "status", "session": status });
        if last.as_ref() != Some(&frame) {
            if socket
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
            last = Some(frame);
        }
        tokio::select! {
            () = tokio::time::sleep(POLL) => {}
            message = socket.recv() => match message {
                None | Some(Err(_) | Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
