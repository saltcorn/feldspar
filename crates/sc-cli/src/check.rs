//! `feldspar check` — checklists from the command line (TODO-checklists C1.12).
//!
//! `check run` is what CI calls: it runs the enabled checklists — one by name,
//! an application's, or all of them — prints each step's result, and **exits
//! non-zero when any checklist fails, errors or is incomplete**, so a host that
//! could not run a browser step never reports green. `check list` lists them.
//!
//! This module is the parsing and the printing; connecting and bringing the
//! server's capabilities up (so `http.*` steps reach the applications through
//! the same router a browser does) is the binary's.

use sc_check::{ChecklistRun, Outcome, RunStatus};
use sc_error::{Error, Result};

/// `feldspar check run`'s and `check list`'s arguments, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckArgs {
    /// `--app SUBDOMAIN`: only this application's checklists.
    pub app: Option<String>,
    /// `--checklist NAME`: only this checklist.
    pub checklist: Option<String>,
}

/// Parse `--app` and `--checklist`; anything else is an error naming it.
pub fn parse_args(args: &[String]) -> Result<CheckArgs> {
    let mut out = CheckArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| Error::config(format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--app" => out.app = Some(value()?),
            "--checklist" => out.checklist = Some(value()?),
            other => {
                return Err(Error::config(format!(
                    "unknown check argument `{other}`; it takes --app SUBDOMAIN and \
                     --checklist NAME (and the database flags)"
                )));
            }
        }
    }
    Ok(out)
}

/// A run as the terminal shows it: the checklist and its status, then a line
/// per step.
pub fn format_run(run: &ChecklistRun) -> String {
    let mut out = format!(
        "{} {} — {}\n",
        status_mark(run.status),
        run.checklist_name,
        run.status.as_str()
    );
    if let Some(message) = &run.message {
        out.push_str(&format!("    {message}\n"));
    }
    for step in &run.steps {
        let label = if step.label.is_empty() {
            step.type_.as_str()
        } else {
            step.label.as_str()
        };
        out.push_str(&format!(
            "  {:<7} {label}: {}\n",
            step.outcome.as_str(),
            step.message
        ));
    }
    for error in &run.cleanup_errors {
        out.push_str(&format!("  cleanup {error}\n"));
    }
    out
}

fn status_mark(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Passed => "PASS",
        RunStatus::Failed => "FAIL",
        RunStatus::Error => "ERROR",
        RunStatus::Incomplete => "INCOMPLETE",
    }
}

/// The error `check run` exits with when not every checklist passed, or
/// `Ok` when every one did. No checklist at all is not a pass: a CI step that
/// checks nothing because a name was mistyped should not go green.
pub fn verdict(runs: &[ChecklistRun]) -> Result<()> {
    if runs.is_empty() {
        return Err(Error::config("no enabled checklist matched, so nothing was checked"));
    }
    let bad: Vec<&ChecklistRun> = runs
        .iter()
        .filter(|r| r.status != RunStatus::Passed)
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    let failed_steps: usize = bad
        .iter()
        .map(|r| {
            r.steps
                .iter()
                .filter(|s| matches!(s.outcome, Outcome::Fail | Outcome::Error))
                .count()
        })
        .sum();
    Err(Error::invalid(format!(
        "{} of {} checklist{} did not pass ({}; {failed_steps} step{} failed)",
        bad.len(),
        runs.len(),
        if runs.len() == 1 { "" } else { "s" },
        bad.iter()
            .map(|r| format!("{}: {}", r.checklist_name, r.status.as_str()))
            .collect::<Vec<_>>()
            .join(", "),
        if failed_steps == 1 { "" } else { "s" },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_the_app_and_the_checklist() {
        let parsed = parse_args(&args(&["--app", "shop", "--checklist", "Orders"])).unwrap();
        assert_eq!(parsed.app.as_deref(), Some("shop"));
        assert_eq!(parsed.checklist.as_deref(), Some("Orders"));
        assert!(parse_args(&args(&["--app"])).is_err());
        assert!(parse_args(&args(&["--verbose"])).is_err());
    }

    #[test]
    fn nothing_checked_is_not_a_pass() {
        assert!(verdict(&[]).is_err());
    }
}
