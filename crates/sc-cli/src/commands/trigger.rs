//! `feldspar trigger approve-system|revoke-system NAME`: the operator's half of
//! approving a trigger to run as system (TODO-type-hardening H8.10, §7.3a).
//!
//! Only a person approves a trigger to run as system: an administrator in the
//! admin UI, or the operator here. An agent, an MCP client or a code body may
//! set the trigger's `runs_as_system` setting, which leaves a pending request;
//! this approves the configuration the trigger has **now**, and a later change
//! to it makes the approval lapse.

use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::{Error, Result};

/// The operator approving at the CLI: a person (TODO-type-hardening H8.10).
/// Listed in the system-authority inventory.
const APPROVE: sc_catalog::Operator = sc_catalog::Operator::at_cli("trigger approve-system");

/// `feldspar trigger approve-system NAME` / `feldspar trigger revoke-system NAME`.
pub(crate) async fn trigger_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let (verb, name) = match rest.as_slice() {
        [verb, name] if verb == "approve-system" || verb == "revoke-system" => {
            (verb.as_str(), name.as_str())
        }
        _ => {
            return Err(Error::config(
                "usage: feldspar trigger approve-system NAME | feldspar trigger revoke-system NAME",
            ));
        }
    };
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    // The approval columns arrive with the additive bootstrap.
    sc_action::bootstrap_triggers(&catalog).await?;
    let trigger = sc_action::load_trigger_by_name(&catalog, name)
        .await?
        .ok_or_else(|| Error::not_found(format!("there is no trigger named `{name}`")))?;
    if verb == "revoke-system" {
        sc_action::clear_system_approval(&catalog, trigger.id).await?;
        eprintln!(
            "feldspar: `{name}` no longer runs as system; it runs as the user who caused it \
             (restart a running server, or save the trigger in the admin UI, for it to see this)"
        );
        return Ok(());
    }
    if !trigger.runs_as_system() {
        return Err(Error::invalid(format!(
            "trigger `{name}` does not ask to run as system; turn on its `runs_as_system` \
             setting first"
        )));
    }
    // What is being approved, so the operator has read it.
    eprintln!(
        "feldspar: approving `{name}` ({} on {}{}) to run as system, for this configuration:",
        trigger.body.as_str(),
        trigger.when.as_str(),
        trigger
            .channel
            .as_deref()
            .map(|t| format!(" of `{t}`"))
            .unwrap_or_default(),
    );
    if let Some(action) = trigger.action() {
        eprintln!("  action: {action}");
    }
    if let Some(configuration) = trigger.configuration() {
        let shown = serde_json::to_string_pretty(configuration).unwrap_or_default();
        for line in shown.lines() {
            eprintln!("  {line}");
        }
    }
    eprintln!(
        "feldspar: it will run at system authority on input the user controls, and whatever \
         it returns or writes where the caller can read it is disclosed on purpose"
    );
    let hash = sc_workflow::approval_hash(&catalog, &trigger).await?;
    sc_action::approve_system_trigger(
        &catalog,
        &sc_catalog::Editor::Operator(APPROVE),
        trigger.id,
        &hash,
    )
    .await?;
    eprintln!(
        "feldspar: approved; a change to its configuration makes the approval lapse (restart a \
         running server for it to see this)"
    );
    Ok(())
}
