//! The run outbox (docs/CHECKLISTS.md §2, TODO-checklists C4.1): what a
//! checklist run *would have sent*, kept on the run instead of sent.
//!
//! A checklist that places an order runs the application's triggers, and the
//! confirmation e-mail, the webhook and the payment call those make are real.
//! While a run is open, everything done on its behalf runs inside its
//! [`RunOutbox`], and the places a message leaves the server — the mailer, the
//! `fetch` action, `fetch` in a code body, a module action that may reach the
//! network — ask [`current`] first: inside a run they record what they would
//! have done ([`Sent`]) and return as if they had, and nothing leaves. The run
//! keeps the record, and `data.expect_sent` asserts on it.
//!
//! **How "on behalf of" travels.** A task-local, like the ambient
//! [changeset](crate::changeset). Triggers fire inline, in the task of the
//! write that raised them, so a scope the runner opens around a step covers
//! the `data.*` steps, the triggers they fire, and an `http.*` step's request
//! through the in-process router. A browser's request arrives on a connection
//! of its own: the browser context of a run carries the run's id in a cookie,
//! and the router puts a request naming an **open** run inside that run's
//! outbox ([`open_run`]). An id that names no open run is ignored, so the
//! cookie grants nothing a client could forge: a run id is a random UUID, it
//! is only honoured while the run is in progress, and all it does is withhold
//! the request's own messages.
//!
//! Not a field of [`Origin`](crate::Origin): the dispatcher puts an event's
//! origin back in scope for each action, which would drop a field the event
//! does not carry, and a check run is not something an event should persist.
//! A workflow resumed after the run has ended is outside it — and sends.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde_json::{Value as Json, json};
use uuid::Uuid;

/// The cookie a checklist run's browser context carries: the run's id.
pub const CHECK_RUN_COOKIE: &str = "sc_check_run";

/// What kind of message a [`Sent`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentKind {
    /// An e-mail, to the mailer.
    Email,
    /// An HTTP request — the `fetch` action, `fetch` in a code body.
    Webhook,
    /// A module action that may reach the network — a payment call.
    Module,
}

impl SentKind {
    /// The kind's name, as a run stores it and `data.expect_sent` names it.
    pub fn as_str(self) -> &'static str {
        match self {
            SentKind::Email => "email",
            SentKind::Webhook => "webhook",
            SentKind::Module => "module",
        }
    }

    /// The kind named `name`.
    pub fn parse(name: &str) -> Option<SentKind> {
        match name {
            "email" => Some(SentKind::Email),
            "webhook" => Some(SentKind::Webhook),
            "module" => Some(SentKind::Module),
            _ => None,
        }
    }
}

/// One message a run withheld.
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    /// What it is.
    pub kind: SentKind,
    /// Its recipients: the addresses of an e-mail (to, cc, bcc), the URL of a
    /// request, `module/action` for a module action.
    pub to: Vec<String>,
    /// An e-mail's subject; a request's method; empty for a module action.
    pub subject: String,
    /// The body: an e-mail's text (or HTML when it has no text), a request's
    /// body, a module action's arguments.
    pub body: Json,
    /// When it was withheld.
    pub at: DateTime<Utc>,
}

impl Sent {
    /// A withheld message, now.
    pub fn new(kind: SentKind, to: Vec<String>, subject: impl Into<String>, body: Json) -> Sent {
        Sent {
            kind,
            to,
            subject: subject.into(),
            body,
            at: Utc::now(),
        }
    }

    /// As a run stores it.
    pub fn to_json(&self) -> Json {
        json!({
            "kind": self.kind.as_str(),
            "to": self.to,
            "subject": self.subject,
            "body": self.body,
            "at": self.at.to_rfc3339(),
        })
    }

    /// [`to_json`](Sent::to_json) read back; `None` for anything else.
    pub fn from_json(json: &Json) -> Option<Sent> {
        Some(Sent {
            kind: SentKind::parse(json.get("kind")?.as_str()?)?,
            to: json
                .get("to")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            subject: json
                .get("subject")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
            body: json.get("body").cloned().unwrap_or(Json::Null),
            at: json
                .get("at")
                .and_then(Json::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map_or_else(Utc::now, |t| t.with_timezone(&Utc)),
        })
    }
}

/// One checklist run's outbox.
#[derive(Debug)]
pub struct RunOutbox {
    run: Uuid,
    sent: Mutex<Vec<Sent>>,
}

impl RunOutbox {
    /// The run this is the outbox of.
    pub fn run(&self) -> Uuid {
        self.run
    }

    /// Keep `sent` instead of sending it.
    pub fn capture(&self, sent: Sent) {
        match self.sent.lock() {
            Ok(mut all) => all.push(sent),
            Err(poisoned) => poisoned.into_inner().push(sent),
        }
    }

    /// Everything withheld so far, in order.
    pub fn sent(&self) -> Vec<Sent> {
        match self.sent.lock() {
            Ok(all) => all.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// The open runs, by id: what the router looks a browser's cookie up in.
fn open_runs() -> &'static Mutex<HashMap<Uuid, Arc<RunOutbox>>> {
    static OPEN: OnceLock<Mutex<HashMap<Uuid, Arc<RunOutbox>>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_open<T>(f: impl FnOnce(&mut HashMap<Uuid, Arc<RunOutbox>>) -> T) -> T {
    match open_runs().lock() {
        Ok(mut open) => f(&mut open),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// An open run's outbox. Dropping it closes the run: its id is no longer
/// honoured, and what is sent on its behalf afterwards is sent.
#[derive(Debug)]
pub struct OpenRun {
    outbox: Arc<RunOutbox>,
}

impl OpenRun {
    /// The outbox.
    pub fn outbox(&self) -> &Arc<RunOutbox> {
        &self.outbox
    }
}

impl Drop for OpenRun {
    fn drop(&mut self) {
        let run = self.outbox.run;
        with_open(|open| open.remove(&run));
    }
}

/// Open run `run`'s outbox.
pub fn open(run: Uuid) -> OpenRun {
    let outbox = Arc::new(RunOutbox {
        run,
        sent: Mutex::new(Vec::new()),
    });
    with_open(|open| open.insert(run, Arc::clone(&outbox)));
    OpenRun { outbox }
}

/// The outbox of run `run` while it is open.
pub fn open_run(run: Uuid) -> Option<Arc<RunOutbox>> {
    with_open(|open| open.get(&run).cloned())
}

tokio::task_local! {
    /// The outbox the running task acts on behalf of — see the module docs.
    static CURRENT: Arc<RunOutbox>;
}

/// Run `future` on behalf of `outbox`'s run.
pub async fn scope<F: Future>(outbox: Arc<RunOutbox>, future: F) -> F::Output {
    CURRENT.scope(outbox, future).await
}

/// [`scope`] when there is an outbox, and `future` as it is when not.
pub async fn scope_if<F: Future>(outbox: Option<Arc<RunOutbox>>, future: F) -> F::Output {
    match outbox {
        Some(outbox) => scope(outbox, future).await,
        None => future.await,
    }
}

/// The outbox of the run the task acts on behalf of, if any: where a message
/// goes instead of out.
pub fn current() -> Option<Arc<RunOutbox>> {
    CURRENT.try_with(Arc::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_message_inside_a_run_is_kept_and_the_run_closes_on_drop() {
        assert!(current().is_none());
        let run = Uuid::new_v4();
        let opened = open(run);
        assert!(open_run(run).is_some());
        scope(Arc::clone(opened.outbox()), async {
            current().unwrap().capture(Sent::new(
                SentKind::Email,
                vec!["a@example.com".into()],
                "Hello",
                json!("body"),
            ));
        })
        .await;
        let sent = opened.outbox().sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(Sent::from_json(&sent[0].to_json()).unwrap(), sent[0]);
        drop(opened);
        assert!(open_run(run).is_none());
    }
}
