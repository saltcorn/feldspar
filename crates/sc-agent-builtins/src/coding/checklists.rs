//! The application's checklists, run against the run's preview — the last part
//! of `check` (TODO-checklists C2.6, docs/CHECKLISTS.md §3).
//!
//! A green build is mounted as the run's preview; then every enabled checklist
//! of the application runs against that preview, as its own `run_as`, with the
//! server's application router and headless browser. So an agent finds out it
//! broke what the application's owner wrote down as "working" **before** the
//! build is deployed, while it can still fix it.
//!
//! **New or pre-existing, like every other check.** Each checklist is a check
//! named `checklist:<name>` with the baseline the others have: a failing step
//! is a diagnostic keyed by the step's position, label and type — never by its
//! message, which names this run's values — so a checklist that was already
//! red before the run changed anything does not make `check` red, and one the
//! run breaks does.
//!
//! A mutating checklist is not run on a production environment unless it is
//! allowed there (the runner would refuse it); a checklist the host cannot run
//! in full (no browser) is reported incomplete, never green.

use std::time::{Duration, Instant};

use sc_agent::{PreviewInfo, TraitContext};
use sc_app::Diagnostic;
use sc_check::{ChecklistRun, ChecklistScope, Outcome as StepOutcome, RunStatus, StepRegistry};
use sc_error::Result;

use super::check::{Baseline, CheckRun, Outcome, classify};

/// One checklist, run against the preview.
#[derive(Debug, Clone)]
pub struct ChecklistCheck {
    /// The check: `checklist:<name>`, failing steps as diagnostics.
    pub check: CheckRun,
    /// How the run went.
    pub status: RunStatus,
    /// Each failing step's message, beside its diagnostic's key.
    pub details: Vec<(String, String)>,
    /// Why it was not run, when it was not.
    pub not_run: Option<String>,
}

/// The name a checklist is reported and recorded under.
pub fn check_name(checklist: &str) -> String {
    format!("checklist:{checklist}")
}

/// Run every enabled checklist of `subdomain` against `preview`.
pub async fn run_on_preview(
    subdomain: &str,
    preview: &PreviewInfo,
    ctx: &TraitContext<'_>,
) -> Result<Vec<ChecklistCheck>> {
    let Some(app) = sc_app::load_application_by_subdomain(ctx.catalog, subdomain).await? else {
        return Ok(Vec::new());
    };
    let checklists =
        sc_check::store::list_checklists(ctx.catalog, ChecklistScope::Application(app.id.0))
            .await?;
    if checklists.is_empty() {
        return Ok(Vec::new());
    }
    let registry = StepRegistry::installed();
    let target = preview.preview_target(ctx.run);
    let env = sc_check::CheckEnv::new(ctx.catalog, &registry)
        .with_triggers(ctx.triggers)
        .with_evaluator(ctx.evaluator)
        .with_requests(ctx.requests)
        .with_browser(ctx.browser)
        .with_preview(Some(&target));
    let trigger = format!("agent:{}", ctx.run);
    let mut out = Vec::new();
    for checklist in checklists.iter().filter(|c| c.enabled) {
        let name = check_name(&checklist.name);
        if ctx.catalog.is_production() && checklist.mutates && !checklist.allow_on_production() {
            let reason = "it writes data, and this is a production environment".to_owned();
            out.push(ChecklistCheck {
                check: CheckRun {
                    name,
                    outcome: Outcome::Skipped(reason.clone()),
                    elapsed: Duration::ZERO,
                },
                status: RunStatus::Incomplete,
                details: Vec::new(),
                not_run: Some(reason),
            });
            continue;
        }
        let started = Instant::now();
        let run = sc_check::run_checklist(&env, checklist, &trigger).await?;
        out.push(as_check(name, &run, started.elapsed()));
    }
    Ok(out)
}

/// A run as a check: passed unless it failed or errored, each failing step a
/// diagnostic.
fn as_check(name: String, run: &ChecklistRun, elapsed: Duration) -> ChecklistCheck {
    let mut diagnostics = Vec::new();
    let mut details = Vec::new();
    if let Some(message) = &run.message {
        // The run could not start: the checklist as a whole is the diagnostic.
        diagnostics.push(Diagnostic {
            file: name.clone(),
            line: 0,
            column: 0,
            message: "did not run".to_owned(),
        });
        details.push((super::check::key(&diagnostics[0]), message.clone()));
    }
    for (index, step) in run.steps.iter().enumerate() {
        if !matches!(step.outcome, StepOutcome::Fail | StepOutcome::Error) {
            continue;
        }
        let label = if step.label.is_empty() {
            step.type_.clone()
        } else {
            format!("{} ({})", step.label, step.type_)
        };
        let diagnostic = Diagnostic {
            file: name.clone(),
            line: u32::try_from(index + 1).unwrap_or(u32::MAX),
            column: 0,
            message: format!("{label} {}", step.outcome.as_str()),
        };
        details.push((super::check::key(&diagnostic), step.message.clone()));
        diagnostics.push(diagnostic);
    }
    let passed = !matches!(run.status, RunStatus::Failed | RunStatus::Error);
    ChecklistCheck {
        check: CheckRun {
            name,
            outcome: Outcome::Ran {
                passed,
                diagnostics,
                output: String::new(),
            },
            elapsed,
        },
        status: run.status,
        details,
        not_run: None,
    }
}

/// The report of one checklist: its status, and each failing step marked new
/// or pre-existing against the baseline, with what went wrong.
pub fn describe(checklist: &ChecklistCheck, baseline: Option<&Baseline>) -> String {
    let name = &checklist.check.name;
    if let Some(reason) = &checklist.not_run {
        return format!("{name}: skipped, {reason}.");
    }
    let took = format!(" ({:.1}s)", checklist.check.elapsed.as_secs_f64());
    let Outcome::Ran { diagnostics, .. } = &checklist.check.outcome else {
        return format!("{name}: {}{took}.", checklist.status.as_str());
    };
    let status = match checklist.status {
        RunStatus::Incomplete => {
            "incomplete: some steps could not run on this server (no headless browser?), so it \
             is not green"
        }
        other => other.as_str(),
    };
    if diagnostics.is_empty() {
        return format!("{name}: {status} against the preview{took}.");
    }
    let marked = classify(diagnostics, baseline);
    let new = marked.iter().filter(|(n, _)| *n).count();
    let mut out = format!(
        "{name}: {status} against the preview{took}, {new} new, {} pre-existing.",
        marked.len() - new
    );
    for (is_new, d) in marked {
        let key = super::check::key(d);
        let detail = checklist
            .details
            .iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, m)| m.as_str());
        let at = if d.line == 0 {
            String::new()
        } else {
            format!("step {}: ", d.line)
        };
        out.push_str(&format!(
            "\n- {at}{}: {detail} ({})",
            d.message,
            if is_new { "new" } else { "pre-existing" }
        ));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sc_check::{SkipReason, StepResult};
    use uuid::Uuid;

    fn step(label: &str, outcome: StepOutcome, message: &str) -> StepResult {
        StepResult {
            step: Uuid::new_v4(),
            type_: "browser.click".to_owned(),
            label: label.to_owned(),
            outcome,
            skip: (outcome == StepOutcome::Skipped).then_some(SkipReason::NotReached),
            message: message.to_owned(),
            duration_ms: 1,
            captured: None,
            console_errors: Vec::new(),
            failed_requests: Vec::new(),
            screenshot: None,
        }
    }

    fn run(status: RunStatus, steps: Vec<StepResult>) -> ChecklistRun {
        ChecklistRun {
            id: Uuid::new_v4(),
            checklist: Uuid::new_v4(),
            checklist_name: "orders".to_owned(),
            trigger: "agent:x".to_owned(),
            target: "abc".to_owned(),
            status,
            run_as: None,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            steps,
            cleanup_errors: Vec::new(),
            message: None,
            outbox: Vec::new(),
        }
    }

    #[test]
    fn a_failing_step_is_a_diagnostic_keyed_without_its_message() {
        let failed = run(
            RunStatus::Failed,
            vec![
                step("Open", StepOutcome::Pass, "opened /"),
                step(
                    "Save",
                    StepOutcome::Fail,
                    "no element matches `button \"Save\"` (run 1)",
                ),
                step("", StepOutcome::Skipped, "not run"),
            ],
        );
        let check = as_check(check_name("orders"), &failed, Duration::from_secs(1));
        let text = describe(&check, None);
        assert!(
            text.starts_with("checklist:orders: failed against the preview"),
            "{text}"
        );
        assert!(
            text.contains("- step 2: Save (browser.click) fail: no element matches"),
            "{text}"
        );
        assert!(text.contains("(new)"), "{text}");

        // The same step failing again with another message is pre-existing.
        let again = run(
            RunStatus::Failed,
            vec![
                step("Open", StepOutcome::Pass, "opened /"),
                step("Save", StepOutcome::Fail, "no element matches (run 2)"),
            ],
        );
        let baseline = check.check.outcome.baseline();
        let again = as_check(check_name("orders"), &again, Duration::ZERO);
        assert!(!super::super::check::has_new_failures(
            &again.check.outcome,
            baseline.as_ref()
        ));
        assert!(super::super::check::has_new_failures(
            &again.check.outcome,
            None
        ));
    }

    #[test]
    fn an_incomplete_run_is_not_a_failure_and_says_why_it_is_not_green() {
        let check = as_check(
            check_name("ui"),
            &run(
                RunStatus::Incomplete,
                vec![step("Open", StepOutcome::Skipped, "no browser")],
            ),
            Duration::ZERO,
        );
        assert!(!super::super::check::has_new_failures(
            &check.check.outcome,
            None
        ));
        assert!(describe(&check, None).contains("incomplete"));
    }
}
