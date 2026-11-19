//! A checklist run with its progress pushed as it goes (TODO-checklists C5.1).
//!
//! `POST …/run` answers only when the run has finished — minutes, for one
//! that drives a browser — so the admin's Run buttons open one of these
//! sockets instead: the socket **is** the run. The server runs the checklist
//! (or an application's checklists) on this connection's own node and sends a
//! frame as each step begins and ends, then the result. Because the run and
//! its progress share the connection, there is nothing to poll and nothing for
//! the nodes to share.
//!
//! The run is its own task, not the socket's: a tab closed halfway (or an
//! admin session that lapses) stops the frames, not the run, which finishes
//! and is recorded as one started with `POST` would be.
//!
//! ## The protocol
//!
//! Nothing comes in. What goes out, as JSON text frames:
//!
//! - `{"type":"progress","progress":{checklist, checklists_done,
//!   checklists_total, total, step, label, type, steps:[{label,type,outcome}]}}`
//!   — whenever a step begins or ends
//!   ([`sc_check::progress::Progress::to_json`]). The run folds its events into
//!   one [`Progress`] the socket sends the latest of, so a browser that reads
//!   slowly is sent less, never a backlog, and every frame is the whole state:
//!   a step that was over in a millisecond may not get a frame of its own, but
//!   the next frame lists it done.
//! - `{"type":"finished","runs":[…]}` — once, last: the runs, as `runChecklist`
//!   / `runChecklists` answer them (one for a single checklist). Then the
//!   socket closes normally.
//! - `{"type":"failed","error":"…"}` — instead, when the run could not be
//!   made (no such checklist, a bad `application`), then a close.
//!
//! A request that is not an admin's, or from another origin, is refused with a
//! status before the upgrade (`router.rs`).

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use sc_catalog::Catalog;
use sc_check::progress::{Event, Progress};
use sc_check::{ChecklistRun, ChecklistScope, StepRegistry, store};
use sc_error::Result;
use serde_json::{Value as Json, json};
use tokio::sync::watch;
use uuid::Uuid;

use crate::apps::AppMounts;

/// Run one checklist: the socket twin of `POST /api/checklists/{id}/run`.
pub const CHECKLIST_RUN_LIVE_ROUTE: &str = "/api/checklists/{id}/run/live";
/// Run the enabled checklists of `?application=` (`none` for the data-only
/// ones): the socket twin of `POST /api/checklist-runs`.
pub const CHECKLISTS_RUN_LIVE_ROUTE: &str = "/api/checklist-runs/live";

/// What a socket runs.
#[derive(Debug, Clone)]
pub(crate) enum LiveRun {
    One(Uuid),
    /// The `application` query, unparsed: a bad one is a `failed` frame.
    All(Option<String>),
}

/// Serve one run socket. The caller has established that the request is an
/// admin's from this host.
pub(crate) fn run_live_upgrade(
    ws: WebSocketUpgrade,
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    what: LiveRun,
    session: crate::admin_session::AdminWatch,
) -> axum::response::Response {
    crate::ws::upgrade(ws, crate::ws::LISTEN_ONLY, move |socket| async move {
        let (progress, seen) = watch::channel(Progress::default());
        let run = sc_catalog::spawn_scoped(run(catalog, apps, what, progress));
        session.guard(follow(socket, seen, run)).await;
    })
}

/// The run itself, folding its events into `progress` until it is done (which
/// drops it).
async fn run(
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    what: LiveRun,
    progress: watch::Sender<Progress>,
) -> Result<Vec<ChecklistRun>> {
    // Whether or not anyone is watching any more: the run goes on.
    let sink = move |event: Event| progress.send_modify(|p| p.apply(event));
    // Per run, as the typed handlers do: a module change installs a new set
    // of step types (C3.5).
    let registry = StepRegistry::installed();
    match what {
        LiveRun::One(id) => {
            let checklist = store::require_checklist(&catalog, id).await?;
            let run = crate::checklists::with_env(&catalog, &registry, &apps, |env| async move {
                sc_check::run_checklist(&env.with_progress(Some(&sink)), &checklist, "manual").await
            })
            .await?;
            Ok(vec![run])
        }
        LiveRun::All(application) => {
            let scope: ChecklistScope = crate::checklists::scope(application.as_deref())?;
            crate::checklists::with_env(&catalog, &registry, &apps, |env| async move {
                sc_check::run_checklists(&env.with_progress(Some(&sink)), scope, "manual").await
            })
            .await
        }
    }
}

/// Send the run's progress as it moves, then its result.
async fn follow(
    mut socket: WebSocket,
    mut progress: watch::Receiver<Progress>,
    run: tokio::task::JoinHandle<Result<Vec<ChecklistRun>>>,
) {
    loop {
        tokio::select! {
            changed = progress.changed() => match changed {
                Ok(()) => {
                    let frame = progress_frame(&progress.borrow_and_update());
                    if send(&mut socket, &frame).await.is_err() {
                        return;
                    }
                }
                // The run has dropped its end: it is over.
                Err(_) => break,
            },
            message = socket.recv() => match message {
                // The browser closed, or the connection went: nobody is
                // watching any more. The run carries on regardless.
                None | Some(Err(_) | Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
        }
    }
    let frame = match run.await {
        Ok(Ok(runs)) => json!({
            "type": "finished",
            "runs": runs.iter().map(ChecklistRun::to_json).collect::<Vec<_>>(),
        }),
        Ok(Err(e)) => json!({ "type": "failed", "error": e.to_string() }),
        Err(e) => json!({ "type": "failed", "error": format!("the run stopped: {e}") }),
    };
    let _ = send(&mut socket, &frame).await;
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code::NORMAL,
            reason: "the run has finished".into(),
        })))
        .await;
}

/// A run in progress's frame.
fn progress_frame(progress: &Progress) -> Json {
    json!({ "type": "progress", "progress": progress.to_json() })
}

async fn send(socket: &mut WebSocket, frame: &Json) -> std::result::Result<(), axum::Error> {
    socket.send(Message::Text(frame.to_string().into())).await
}
