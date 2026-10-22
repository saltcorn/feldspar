//! Changesets (§14.12): the user-level action a change-log entry belongs to.
//!
//! A changeset is **not a transaction**. A trigger's action commits after the
//! write that fired it, in a transaction of its own (§10.2), and its writes are
//! as much the user's action as the write itself. So the changeset travels with
//! the caller instead: [`CallerContext`](crate::CallerContext) carries one, and
//! [`set_caller_context`](crate::set_caller_context) hands it to the change-log
//! trigger as the transaction-local [`CHANGESET_GUC`], beside the role and the
//! user. An event carries it on to the triggers it fires, so their writes join
//! it too.
//!
//! Where nothing hands one over, the **ambient** changeset is used: whatever
//! [`scope`] set for the task. A request runs inside one, minted by the server
//! for that request, and so does a CSV import, an MCP tool call, a workflow
//! step and a trigger's action. [`CallerContext::new`](crate::CallerContext::new)
//! picks it up, which is what lets the many places that build a caller keep
//! doing so without each being told about changesets.
//!
//! A changeset's `_fd_changesets` row is written by the trigger, with its first
//! recorded entry: a request that changes nothing leaves nothing behind.

use std::future::Future;
use std::sync::{Arc, OnceLock};

use sc_db::ColumnGenerator;
use sc_types::{BasicType, TypeRef};
use serde_json::{Map, Value as Json};
use uuid::Uuid;

use crate::field::DataField;

/// One row per changeset that recorded something.
pub const CHANGESETS_TABLE: &str = "_fd_changesets";

/// The transaction-local setting carrying the writer's changeset to the
/// change-log trigger, as a JSON object ([`Changeset::guc_value`]). Unset or
/// empty, the trigger mints a changeset of its own (`source = 'sql'`).
pub const CHANGESET_GUC: &str = "sc.changeset";

/// `off` skips recording for the rest of the transaction. Only a backup
/// restore sets it: the log is backed up and restored as itself.
pub const CHANGE_LOG_GUC: &str = "sc.change_log";

/// What started a changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangesetSource {
    /// An HTTP request: the admin UI, an application's API.
    Request,
    /// A CSV import.
    Import,
    /// A workflow run, for all of its steps.
    Workflow,
    /// An undo of another changeset (H3).
    Undo,
    /// A restore of a deleted row or an old version (H3).
    Restore,
    /// One change uploaded by an offline device.
    Sync,
    /// One MCP tool call.
    Mcp,
    /// A write with no Feldspar context: `psql`, another program. Minted by
    /// the trigger, never here, but read back.
    Sql,
}

impl ChangesetSource {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangesetSource::Request => "request",
            ChangesetSource::Import => "import",
            ChangesetSource::Workflow => "workflow",
            ChangesetSource::Undo => "undo",
            ChangesetSource::Restore => "restore",
            ChangesetSource::Sync => "sync",
            ChangesetSource::Mcp => "mcp",
            ChangesetSource::Sql => "sql",
        }
    }
}

/// One user-level action, as the writes it causes carry it.
///
/// The user is not stored here: it is the writer's
/// ([`CallerContext::user`](crate::CallerContext::user)), which for a trigger
/// running with admin authority is still the user who caused it — the answer
/// to "who changed this?".
#[derive(Debug, Clone, PartialEq)]
pub struct Changeset {
    /// The changeset's id, minted here so no round trip is needed.
    pub id: Uuid,
    /// What started it.
    pub source: ChangesetSource,
    /// A key derived from the caller's session or bearer token (never the
    /// token's hash itself). Recorded only when the writer has a user.
    pub session: Option<String>,
    /// The application the action was made in. Settable once, later than the
    /// changeset is minted: a request's application is known only when it is
    /// routed.
    application: OnceLock<Uuid>,
    /// For an undo, the changeset it undid; for a workflow run, the changeset
    /// whose event started it.
    pub cause: Option<Uuid>,
    /// An optional reason.
    pub description: Option<String>,
    /// For an undo limited to one table, that table.
    pub undo_table: Option<String>,
    /// For a sync upload, the device.
    pub client_id: Option<Uuid>,
    /// For a sync upload, the device's own number for the change.
    pub mutation: Option<i64>,
}

impl Changeset {
    /// A new changeset from `source`, with a fresh id and nothing else.
    pub fn new(source: ChangesetSource) -> Changeset {
        Changeset::with_id(Uuid::new_v4(), source)
    }

    /// A changeset with a known id: one kept on a workflow run, resumed.
    pub fn with_id(id: Uuid, source: ChangesetSource) -> Changeset {
        Changeset {
            id,
            source,
            session: None,
            application: OnceLock::new(),
            cause: None,
            description: None,
            undo_table: None,
            client_id: None,
            mutation: None,
        }
    }

    /// Set the session key ([`session`](Changeset::session)).
    pub fn session(mut self, session: Option<String>) -> Changeset {
        self.session = session;
        self
    }

    /// Set the cause.
    pub fn cause(mut self, cause: Option<Uuid>) -> Changeset {
        self.cause = cause;
        self
    }

    /// Limit an undo to one table's entries ([`undo_table`](Changeset::undo_table)).
    pub fn undoing_table(mut self, table: Option<String>) -> Changeset {
        self.undo_table = table;
        self
    }

    /// Set the application, unless one is set already.
    pub fn in_application(self, application: Option<Uuid>) -> Changeset {
        if let Some(application) = application {
            let _ = self.application.set(application);
        }
        self
    }

    /// Name the device and its change number, for a sync upload.
    pub fn device(mut self, client_id: Uuid, mutation: i64) -> Changeset {
        self.client_id = Some(client_id);
        self.mutation = Some(mutation);
        self
    }

    /// Record the application the action was made in, once it is known. A
    /// second call is ignored: one action is in one application.
    pub fn set_application(&self, application: Uuid) {
        let _ = self.application.set(application);
    }

    /// The application, when one is known.
    pub fn application(&self) -> Option<Uuid> {
        self.application.get().copied()
    }

    /// What [`CHANGESET_GUC`] carries for a write by `user` (the writer's
    /// fields, `None` when anonymous): everything the trigger needs to write
    /// the `_fd_changesets` row, as a JSON object without its nulls.
    pub fn guc_value(&self, user: Option<&Json>) -> String {
        let mut out = Map::new();
        out.insert("id".into(), Json::String(self.id.to_string()));
        out.insert("source".into(), Json::String(self.source.as_str().into()));
        let user_id = user.and_then(|u| u.get("id")).filter(|id| !id.is_null());
        if let Some(user_id) = user_id {
            let user_id = match user_id {
                Json::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.insert("user_id".into(), Json::String(user_id));
            // Anonymous callers have no session, whatever cookie they carry.
            if let Some(session) = &self.session {
                out.insert("session".into(), Json::String(session.clone()));
            }
        }
        let text = |s: &str| Json::String(s.to_owned());
        let optional = [
            ("application", self.application().map(|a| a.to_string())),
            ("cause", self.cause.map(|c| c.to_string())),
            ("description", self.description.clone()),
            ("undo_table", self.undo_table.clone()),
            ("client_id", self.client_id.map(|c| c.to_string())),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                out.insert(key.into(), text(&value));
            }
        }
        if let Some(mutation) = self.mutation {
            out.insert("mutation".into(), Json::from(mutation));
        }
        Json::Object(out).to_string()
    }
}

tokio::task_local! {
    /// The ambient changeset of the running task — see the module docs.
    static CURRENT: Arc<Changeset>;
}

/// Run `future` with `changeset` as the ambient changeset: every
/// [`CallerContext`](crate::CallerContext) built inside it belongs to it.
pub async fn scope<F: Future>(changeset: Arc<Changeset>, future: F) -> F::Output {
    CURRENT.scope(changeset, future).await
}

/// [`scope`] when there is a changeset, and `future` as it is when not.
pub async fn scope_if<F: Future>(changeset: Option<Arc<Changeset>>, future: F) -> F::Output {
    match changeset {
        Some(changeset) => scope(changeset, future).await,
        None => future.await,
    }
}

/// The ambient changeset, when the task runs inside one.
pub fn current() -> Option<Arc<Changeset>> {
    CURRENT.try_with(Arc::clone).ok()
}

/// `_fd_changesets`' columns (§14.12 Storage).
pub(crate) fn changesets_fields() -> Vec<DataField> {
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let text = || TypeRef::Basic(BasicType::Text);
    let int = || TypeRef::Basic(BasicType::Int);
    vec![
        DataField::plain("id", uuid()).required().primary_key(),
        DataField::plain("at", TypeRef::Basic(BasicType::Timestamp))
            .required()
            .generated(ColumnGenerator::Default("now()".into())),
        // The first transaction that recorded an entry of it.
        DataField::plain("txid", int()).required(),
        DataField::plain("user_id", uuid()),
        DataField::plain("session", text()),
        DataField::plain("source", text()).required(),
        DataField::plain("application", uuid()),
        DataField::plain("description", text()),
        DataField::plain("cause", uuid()),
        DataField::plain("undo_table", text()),
        DataField::plain("client_id", uuid()),
        DataField::plain("mutation", int()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_guc_carries_the_writer_and_leaves_out_what_is_unset() {
        let changeset = Changeset::new(ChangesetSource::Request).session(Some("s1".into()));
        changeset.set_application(Uuid::nil());
        // A second application is ignored: one action, one application.
        changeset.set_application(Uuid::max());
        let user = json!({ "id": "00000000-0000-4000-8000-000000000001", "role": 1 });
        let value: Json = serde_json::from_str(&changeset.guc_value(Some(&user))).unwrap();
        assert_eq!(
            value,
            json!({
                "id": changeset.id.to_string(),
                "source": "request",
                "user_id": "00000000-0000-4000-8000-000000000001",
                "session": "s1",
                "application": Uuid::nil().to_string(),
            })
        );
        // Anonymous: no user, and so no session either.
        let value: Json = serde_json::from_str(&changeset.guc_value(None)).unwrap();
        assert!(value.get("user_id").is_none() && value.get("session").is_none());
    }

    #[tokio::test]
    async fn a_scope_makes_a_changeset_ambient() {
        assert!(current().is_none());
        let changeset = Arc::new(Changeset::new(ChangesetSource::Import));
        let seen = scope(changeset.clone(), async { current() }).await;
        assert_eq!(seen, Some(changeset));
        assert!(current().is_none());
    }
}
