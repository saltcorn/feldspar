//! `implement_feature` — the outer loop's whole step, in harness code (TODO §8).
//!
//! The planner names a feature of its plan, and everything else is done here,
//! not by a model:
//!
//! 1. The feature is marked `in_progress`, a run id is minted for its session
//!    and recorded against it, and the planner run's state is **saved before
//!    the session starts** ([`Delegator::save_state`]).
//! 2. The session is briefed with the feature and the last few progress
//!    entries ([`plan::briefing`]). Its session header adds `AGENTS.md`, a
//!    repo map focused on the brief and the git log.
//! 3. The session runs: a fresh `act` run of this same agent, answered by the
//!    executor role.
//! 4. **`check` runs again, here.** The session saying it is done is not
//!    trusted: the checks run over the session's change ledger and the
//!    ratchet, as this planner run — so a green build is mounted as *the
//!    planner's* preview, which outlives the session. They are compared with
//!    **the plan's baseline**, recorded in the planner's state before its first
//!    session, not the session's own: a failed session's changes stay in the
//!    tree, and the next session would otherwise count its breakage as
//!    pre-existing and pass without fixing anything.
//! 5. When green and the feature lists `pages`, each is looked at on that
//!    preview ([`look_at_pages`]).
//! 6. When green, the feature is committed for a git work tree ([`commit`]),
//!    with a message from the cheap role, and marked `done`.
//! 7. When red, or when the session ended stuck, the failure is counted, and
//!    the second in a row marks the feature `failed`. Either returns a
//!    **re-plan** instruction.
//! 8. The planner gets the verdict, the session's summary, the check report,
//!    the diffstat and the capped diff, the page snapshots, and the checklist:
//!    the review, on a short input.
//!
//! **Resuming.** A feature still `in_progress` when `implement_feature` is
//! called for it has a session that did not finish being reported — the server
//! stopped while it ran, and the resumed planner re-dispatched this call. That
//! session's run is driven on from where it stopped, never started twice.
//! Aborting the planner aborts the session with it (`abort_run`).

use std::str::FromStr;

use sc_agent::{Conclusion, DelegateRequest, Delegator, ModelRole, RunId, RunMode, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::check::{record_baseline, run_checks};
use super::commit::{self, CFG_COMMIT, MESSAGE_SYSTEM, clean_message, message_prompt};
use super::ledger::diff_ledger;
use super::plan::{self, Kind, Plan, Progress, Status, checklist};
use super::state::CodingState;
use super::{CFG_MAY_CHECK, CFG_MAY_VIEW_APP, may, view_app};
use crate::files::{FileScope, config_count, string_arg};
use crate::table::arguments;

/// How many sessions one feature may have, counting retries.
pub const CFG_MAX_SESSIONS: &str = "max_sessions_per_feature";

/// [`CFG_MAX_SESSIONS`] when the admin sets none.
pub const DEFAULT_MAX_SESSIONS: u64 = 3;

/// Failed sessions in a row that mark a feature `failed`.
pub const FAILURES_TO_FAIL: u32 = 2;

/// The most characters of the diff the planner is shown.
pub const MAX_DIFF_CHARS: usize = 8_000;

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("implement_feature_{}", scope.slug())
}

/// The `implement_feature` tool.
pub fn spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        tool_name(scope),
        "Implement one feature of the saved plan, by `id`, in a fresh working session. The \
         feature is then checked independently and, in a git work tree, committed. Returns \
         the session's result, the check report and the diff, for you to review before the \
         next feature."
            .to_owned(),
        json!({
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
            "additionalProperties": false,
        }),
    )
}

/// How a run ended, in a few words.
pub fn conclusion_words(conclusion: &Conclusion) -> String {
    match conclusion {
        Conclusion::Answered { .. } => "answered".to_owned(),
        Conclusion::MaxSteps => "ran out of steps".to_owned(),
        Conclusion::Aborted => "was aborted".to_owned(),
        Conclusion::OverBudget { budget } => format!("ran out of its {budget} budget"),
        Conclusion::Stuck { reason } => format!("got stuck: {reason}"),
        Conclusion::AwaitingUser { request } => {
            format!("is waiting for the person to answer: {}", request.prompt)
        }
    }
}

/// Implement one feature: brief, run, check, look, commit, report.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let args = arguments(args, &["id"])?;
    let id = string_arg(&args, "id")?;
    let max_sessions = config_count(config, CFG_MAX_SESSIONS, DEFAULT_MAX_SESSIONS)?;
    // Copied out of the context, so the state can be written while it is held.
    let Some(delegate) = ctx.delegate else {
        return Err(ctx.require_delegate().err().unwrap_or_else(|| {
            Error::config("implement_feature needs to start a session of this agent")
        }));
    };
    let may_check = may(config, CFG_MAY_CHECK);

    // 1. Mark it in progress with its session's run id, and save that first.
    let mut state = CodingState::load(ctx.trait_state);
    let plan = state.plan.as_mut().ok_or_else(|| {
        Error::invalid("there is no plan yet: write one with the save_plan tool first")
    })?;
    let progress = plan.progress.clone();
    let feature = plan.feature_mut(&id)?;
    let session = match feature.status {
        Status::Done => {
            return Err(Error::invalid(format!(
                "feature `{id}` is already done; to change it further, add a feature with \
                 save_plan"
            )));
        }
        Status::Blocked => {
            return Err(Error::invalid(format!(
                "feature `{id}` is blocked; set its status to todo with save_plan first"
            )));
        }
        Status::InProgress => match feature.runs.last().map(|r| uuid_of(r)) {
            Some(Ok(run)) => {
                sc_log::log_info!(
                    "agent `{}` run {}: feature `{id}` was in progress; resuming its session {run}",
                    ctx.agent,
                    ctx.run
                );
                run
            }
            _ => new_session(feature, max_sessions)?,
        },
        Status::Todo | Status::Failed => new_session(feature, max_sessions)?,
    };
    let feature = feature.clone();
    let briefing = plan::briefing(&feature, &progress, may_check);
    state.store(ctx.trait_state);
    // The plan's baseline, once, before any session has changed anything.
    if may_check {
        record_baseline(scope, config, ctx).await?;
    }
    delegate.save_state(ctx.trait_state).await?;

    // 2–3. The session: a fresh `act` run of this agent, on the executor.
    let request = DelegateRequest::new(ctx.agent, &briefing, ctx.run)
        .mode(RunMode::Act)
        .role(ModelRole::Executor)
        .resume(session);
    let done = match delegate.delegate(request).await {
        Ok(done) => done,
        Err(e) => {
            let text = record(ctx, &id, session, false, |p| Progress {
                summary: format!("the session could not run: {e}"),
                ..p
            })?;
            return Err(Error::invalid(format!(
                "feature `{id}`: its session could not run: {e}\n{text}"
            )));
        }
    };
    if matches!(done.conclusion, Conclusion::Aborted) {
        return Err(Error::invalid(format!(
            "feature `{id}`: its session (run {}) was aborted",
            done.run
        )));
    }
    let summary = done.answer().unwrap_or_default().trim().to_owned();

    // 4. The independent check, over the session's ledger and the plan's
    // baseline, as this run.
    let mut checked_state = CodingState::load(&done.trait_state);
    checked_state.baseline = CodingState::load(ctx.trait_state).baseline;
    let mut checked_json = Json::Null;
    checked_state.store(&mut checked_json);
    let saved = std::mem::replace(ctx.trait_state, checked_json);
    let checked = match may_check {
        true => Some(run_checks(scope, config, ctx).await),
        false => None,
    };
    *ctx.trait_state = saved;
    let child = CodingState::load(&done.trait_state);
    let (store, _) = scope.connect(ctx.catalog).await?;
    let diff = diff_ledger(scope, store.as_ref(), &child.ledger).await?;
    let (check_green, check_text) = match checked {
        Some(Ok(report)) => (report.green, report.text),
        Some(Err(e)) => (false, format!("check: red, the checks could not run: {e}")),
        None => (
            true,
            format!(
                "check: not run, because the `coding` trait has `{CFG_MAY_CHECK}` off; this \
                 feature is unverified."
            ),
        ),
    };
    let stuck = match &done.conclusion {
        Conclusion::Stuck { reason } => Some(reason.clone()),
        _ => None,
    };
    let green = check_green && stuck.is_none();

    // 5. The pages, on this run's preview.
    let pages = match green && !feature.pages.is_empty() {
        true => {
            let vision = delegate
                .capabilities(ModelRole::Strong)
                .await
                .is_ok_and(|c| c.vision);
            Some(look_at_pages(config, &feature.pages, vision, ctx).await)
        }
        false => None,
    };

    // 6. The commit.
    let committed = match green {
        true => Some(commit_feature(scope, config, &feature, &diff, delegate, ctx).await),
        false => None,
    };

    // 6–7. The verdict, recorded.
    let first_check_line = check_text.lines().next().unwrap_or_default().to_owned();
    let checklist_text = record(ctx, &id, session, green, |p| Progress {
        summary: summary.clone(),
        check: first_check_line.clone(),
        diffstat: match diff.is_empty() {
            true => String::new(),
            false => diff.stat(),
        },
        ..p
    })?;
    let status = CodingState::load(ctx.trait_state)
        .plan
        .as_ref()
        .and_then(|p| p.features.iter().find(|f| f.id == id).map(|f| f.status))
        .unwrap_or_default();

    // 8. The review.
    let mut out = format!(
        "feature `{id}`: {}\nsession: run {}, {} after {} steps",
        match status {
            Status::Done => "done".to_owned(),
            Status::Failed => format!("failed: {FAILURES_TO_FAIL} sessions in a row did not pass"),
            _ => "not done; it is back to todo".to_owned(),
        },
        done.run,
        conclusion_words(&done.conclusion),
        done.steps
    );
    out.push_str(&format!(
        "\nsummary:\n{}",
        match summary.is_empty() {
            true => "(the session gave no summary)",
            false => summary.as_str(),
        }
    ));
    out.push_str(&format!("\n{check_text}"));
    if feature.kind == Kind::Bug {
        out.push_str(match child.red_before_fix {
            true => "\nreproduced: yes, a check failed before the fix.",
            false => "\nreproduced: no, no failing check came before the fix.",
        });
    }
    if let Some(line) = committed {
        out.push_str(&format!("\n{line}"));
    }
    match diff.is_empty() {
        true => out.push_str("\ndiff: nothing changed."),
        false => {
            let unified: String = diff.unified.chars().take(MAX_DIFF_CHARS).collect();
            out.push_str(&format!("\ndiffstat:\n{}\ndiff:\n{unified}", diff.stat()));
            if unified.len() < diff.unified.len() {
                out.push_str(&format!(
                    "\n[… diff cut at {MAX_DIFF_CHARS} characters; read the files for the rest]"
                ));
            }
        }
    }
    if let Some(pages) = pages {
        out.push_str(&format!("\npages:\n{pages}"));
    }
    if let Some(reason) = &stuck {
        out.push_str(&format!(
            "\nre-plan: the session got stuck ({reason}). Change the plan with save_plan, \
             splitting `{id}` or saying more about how to do it, before implementing it again."
        ));
    } else if status == Status::Failed {
        out.push_str(&format!(
            "\nre-plan: `{id}` failed twice in a row. Read the check report and the diff, then \
             change the plan with save_plan before implementing it again."
        ));
    }
    out.push_str(&format!("\n{checklist_text}"));
    Ok(Json::String(out))
}

/// Start a feature's next session: refused at the session limit, and
/// otherwise a new run id, recorded, with the feature in progress.
fn new_session(feature: &mut plan::Feature, max_sessions: u64) -> Result<RunId> {
    if feature.runs.len() as u64 >= max_sessions {
        return Err(Error::invalid(format!(
            "feature `{}` has had {} sessions, which is the limit \
             (`{CFG_MAX_SESSIONS}`). Re-plan it with save_plan: split it, reword it, or set \
             its status to blocked.",
            feature.id,
            feature.runs.len()
        )));
    }
    let run = RunId::new();
    feature.runs.push(run.to_string());
    feature.status = Status::InProgress;
    Ok(run)
}

/// Record how feature `id`'s session went, and return the checklist.
fn record(
    ctx: &mut TraitContext<'_>,
    id: &str,
    session: RunId,
    green: bool,
    entry: impl FnOnce(Progress) -> Progress,
) -> Result<String> {
    let mut state = CodingState::load(ctx.trait_state);
    let plan: &mut Plan = state.plan.get_or_insert_with(Plan::default);
    let feature = plan.feature_mut(id)?;
    if green {
        feature.status = Status::Done;
        feature.attempts = 0;
    } else {
        feature.attempts += 1;
        feature.status = match feature.attempts >= FAILURES_TO_FAIL {
            true => Status::Failed,
            false => Status::Todo,
        };
    }
    let status = feature.status;
    plan.progress.push(entry(Progress {
        feature: id.to_owned(),
        run: session.to_string(),
        status,
        ..Progress::default()
    }));
    let text = checklist(plan);
    state.store(ctx.trait_state);
    Ok(text)
}

/// Look at each page on this run's preview, and screenshot it for a planner
/// that takes images. Stops at the first page that cannot be looked at, since
/// the reason — no preview, no browser — is the same for the rest.
pub async fn look_at_pages(
    config: &Attrs,
    pages: &[String],
    vision: bool,
    ctx: &mut TraitContext<'_>,
) -> String {
    if !may(config, CFG_MAY_VIEW_APP) {
        return format!("not looked at, because the `coding` trait has `{CFG_MAY_VIEW_APP}` off.");
    }
    let mut out: Vec<String> = Vec::new();
    for page in pages {
        match view_app::call(config, &json!({"action": "goto", "path": page}), ctx).await {
            Ok(text) => out.push(text.as_str().unwrap_or_default().to_owned()),
            Err(e) => {
                out.push(format!("{page}: not looked at: {e}"));
                break;
            }
        }
        if vision {
            match view_app::call(config, &json!({"action": "screenshot"}), ctx).await {
                Ok(_) => out.push(format!("screenshot of {page}: attached")),
                Err(e) => out.push(format!("screenshot of {page}: not taken: {e}")),
            }
        }
    }
    out.join("\n")
}

/// Commit a green feature where there is a git work tree, and say what
/// happened in one line.
async fn commit_feature(
    scope: &FileScope,
    config: &Attrs,
    feature: &plan::Feature,
    diff: &super::ledger::RunDiff,
    delegate: &dyn Delegator,
    ctx: &TraitContext<'_>,
) -> String {
    if !commit::enabled(config) {
        return format!("commit: none, because the `{CFG_COMMIT}` setting is off.");
    }
    if diff.is_empty() {
        return "commit: none, nothing changed.".to_owned();
    }
    let Some(top) = commit::work_tree(scope, ctx.catalog).await else {
        return "commit: none, the store is not a git work tree; the diff is below.".to_owned();
    };
    let written = delegate
        .ask(
            ModelRole::Cheap,
            MESSAGE_SYSTEM,
            &message_prompt(&feature.title, &feature.description, diff),
        )
        .await;
    let message = match written {
        Ok(text) => clean_message(&text, &feature.title),
        Err(e) => {
            sc_log::log_warn!(
                "agent `{}` run {}: no commit message from the cheap role, using the feature's \
                 title — {e}",
                ctx.agent,
                ctx.run
            );
            feature.title.clone()
        }
    };
    match commit::commit(&top, scope, ctx.catalog, diff, &message).await {
        Ok(Some(hash)) => format!(
            "commit: {hash} {}",
            message.lines().next().unwrap_or_default()
        ),
        Ok(None) => "commit: none, git found nothing to commit.".to_owned(),
        Err(e) => format!("commit: failed: {e}"),
    }
}

fn uuid_of(text: &str) -> Result<RunId> {
    uuid::Uuid::from_str(text)
        .map(RunId)
        .map_err(|e| Error::invalid(format!("`{text}` is not a run id: {e}")))
}
