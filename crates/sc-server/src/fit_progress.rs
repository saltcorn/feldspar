//! A fit's progress, pushed to the browser (analytics TODO A3.3).
//!
//! A fit is a job whose registry is its row (§14.2): the job writes its
//! `Progress` to the instance at most once a second, from whichever node runs
//! it. The model editor wants to see it move without polling, so this socket
//! watches the row on the browser's behalf and **sends a frame whenever it
//! changes**, then one more when the fit finishes, and closes. Reading the row
//! rather than listening to the job is what makes it work from any node, and
//! keeps the job free of anyone watching it.
//!
//! ## The protocol
//!
//! Nothing comes in. What goes out, as JSON text frames:
//!
//! - `{"type":"progress","status":"fitting","progress":{…}|null,
//!    "cancel_requested":bool}` — first, and whenever the stage, a chain's
//!   iteration or the cancel request changes. `progress` is the job's
//!   `Progress` (`stage`: `reading`, `fitting`, `scoring`, or a posterior's
//!   `queued`, `compiling`, `sampling`, `summarising`; and each chain's
//!   iteration).
//! - `{"type":"finished","status":"fitted"|"failed","error":…}` — once, last;
//!   then the socket closes normally. A fit that had already finished when the
//!   socket opened sends only this.
//!
//! A fit that does not exist closes the socket with a reason; a request that
//! is not an admin's is refused with a status before the upgrade, as every
//! socket here is (`router.rs`).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use sc_catalog::Catalog;
use sc_model::{FitStatus, InstanceId, ModelInstance};
use serde_json::{Value as Json, json};

/// The route the Analytics UI opens a fit's progress socket on.
pub const FIT_PROGRESS_ROUTE: &str = "/api/model-instances/{id}/progress";

/// How often the socket re-reads the fit's row. The job writes it at most once
/// a second, so twice a second sees every write without falling behind one.
const POLL: Duration = Duration::from_millis(500);

/// Serve one progress socket for the fit `id`. The caller has established that
/// the request is an admin's.
pub(crate) fn fit_progress_upgrade(
    ws: WebSocketUpgrade,
    catalog: Arc<Catalog>,
    id: InstanceId,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| watch(socket, catalog, id))
}

/// Send the fit's progress until it finishes or the browser goes.
async fn watch(mut socket: WebSocket, catalog: Arc<Catalog>, id: InstanceId) {
    let mut last: Option<Json> = None;
    loop {
        let instance = match sc_model::load_model_instance(&catalog, id).await {
            Ok(Some(instance)) => instance,
            Ok(None) => {
                return close(socket, close_code::POLICY, format!("there is no fit {id}")).await;
            }
            Err(e) => {
                let reason = format!("the fit could not be read: {e}");
                return close(socket, close_code::ERROR, reason).await;
            }
        };
        if instance.status != FitStatus::Fitting {
            let _ = send(&mut socket, &finished_frame(&instance)).await;
            return close(
                socket,
                close_code::NORMAL,
                "the fit has finished".to_owned(),
            )
            .await;
        }
        let frame = progress_frame(&instance);
        if last.as_ref() != Some(&frame) {
            if send(&mut socket, &frame).await.is_err() {
                return;
            }
            last = Some(frame);
        }
        tokio::select! {
            () = tokio::time::sleep(POLL) => {}
            message = socket.recv() => match message {
                // The browser closed, or the connection went: nobody is
                // watching any more.
                None | Some(Err(_) | Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

/// A running fit's frame.
pub(crate) fn progress_frame(instance: &ModelInstance) -> Json {
    json!({
        "type": "progress",
        "status": instance.status.as_str(),
        "progress": instance.attributes.get(sc_model::ATTR_PROGRESS).cloned().unwrap_or(Json::Null),
        "cancel_requested": sc_model::cancel_requested(instance),
    })
}

/// A finished fit's frame.
pub(crate) fn finished_frame(instance: &ModelInstance) -> Json {
    json!({
        "type": "finished",
        "status": instance.status.as_str(),
        "error": instance.error(),
    })
}

async fn send(socket: &mut WebSocket, frame: &Json) -> Result<(), axum::Error> {
    socket.send(Message::Text(frame.to_string().into())).await
}

/// Close the socket, saying why — within what a close frame can carry.
async fn close(mut socket: WebSocket, code: u16, reason: String) {
    let mut reason = reason;
    if reason.len() > 120 {
        let mut cut = 120;
        while !reason.is_char_boundary(cut) {
            cut -= 1;
        }
        reason.truncate(cut);
    }
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}
