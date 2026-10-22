//! `undo` — step back through the caller's own session (§14.12 "The Undo
//! action", TODO-history H4).

use sc_auth::User;
use sc_catalog::history;
use sc_error::{Error, Result};
use sc_types::{BasicType, FormField};
use serde_json::{Value as Json, json};

use sc_action::{Action, ActionContext, ConfigCheck};
use sc_api::undo::{Reverser, UndoOutcome};

use crate::rows_scope::CFG_TABLE;

/// Undo the most recent change the caller made in this session, as an
/// editor's undo does: running it again steps back one action further.
///
/// The undo is [`Reverser::undo_changeset`]'s, as the caller, so the write
/// rules apply and nothing is granted. Without `force`: a conflict refuses it.
///
/// - **No `table`**: the session's most recent changeset that still has
///   entries not undone, skipping undos and the request running the action.
/// - **`table`**: only that table's entries, of the session's most recent
///   changeset that has some not yet undone. A later unconfigured undo of the
///   same changeset takes back the rest.
///
/// Changesets of other sessions, the same user's other devices included, are
/// never picked. A refusal is an error with a sentence, because the button
/// that ran it has someone waiting for the answer.
pub struct Undo;

/// The configured table, when one is.
fn configured_table(config: &sc_types::Attrs) -> Option<&str> {
    config
        .get(CFG_TABLE)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

#[async_trait::async_trait]
impl Action for Undo {
    fn name(&self) -> &str {
        "undo"
    }

    fn description(&self) -> &str {
        "Undo the caller's most recent change in this session"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![FormField::new(CFG_TABLE, BasicType::Text).label("Only in table")]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let Some(name) = configured_table(check.config) else {
            return Ok(());
        };
        if !check.catalog.require(name)?.history_enabled() {
            return Err(Error::invalid(format!(
                "table `{name}` does not keep row history, so there is nothing in it to \
                 undo; switch its history on first"
            )));
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let event = ctx.event;
        let table = configured_table(ctx.config);
        let user = User::from_json(event.role, event.user.as_ref())?;
        let session = event.changeset.as_ref().and_then(|c| c.session.clone());
        let (Some(user), Some(session)) = (user, session) else {
            return Err(Error::invalid(
                "there is nothing to undo without signing in: changes are undone per session",
            ));
        };
        let running = event.changeset.as_ref().map(|c| c.id);
        let Some(target) =
            history::session_undo_target(ctx.catalog, &session, table, running).await?
        else {
            return Err(Error::invalid(match table {
                Some(t) => format!("there is nothing in `{t}` to undo"),
                None => "there is nothing to undo".to_owned(),
            }));
        };
        let reverser = Reverser::new(ctx.catalog, Some(&user), ctx.evaluator().ok());
        match reverser.undo_changeset(target, table, false).await? {
            UndoOutcome::Undone {
                changeset, entries, ..
            } => Ok(json!({ "undid": target, "changeset": changeset, "entries": entries })),
            refused => Err(Error::invalid(refused.refusal().unwrap_or_default())),
        }
    }
}
