//! `check` — the project's own checks, the baseline, and the test ratchet
//! (TODO Phase 6, §7).
//!
//! **The model is never told a command to remember.** `check` runs what the
//! admin listed in the `checks` setting — `package.json` script names, in order —
//! and then, when the `application` setting names one, the application build,
//! through the same `sc_app::build_application` the admin's Build button runs
//! (not mounted: the question is "does it compile?"). A build after a type check
//! that found new errors is skipped, and the result says so: it would fail for
//! the same reason, and slowly.
//!
//! **New or pre-existing.** Before the run changes anything, the checks'
//! results are recorded as its [`Baseline`]: before the first edit (with the
//! post-turn `diagnose` script, [`record_baseline`]), or by a `check` called
//! before any edit. Each diagnostic is then new or pre-existing (keyed by file
//! and message, since edits move lines), and a check **has new failures** when it
//! fails with a new diagnostic, or fails where the baseline passed. The run is
//! green when no check has new failures, so a project that was broken before the
//! run started can still be worked on.
//!
//! **The ratchet** is a pseudo-check read from the change ledger: a run may not
//! delete a test file, reduce the number of `it(`/`test(`/`describe(` blocks in
//! one, or add `.skip`, `.only`, `xit` or `xdescribe` to one. A green check with
//! weakened tests is not green.
//!
//! A red check raises [`Signal::CheckFailed`], which the loop's escalation
//! ladder counts.
//!
//! **A green build is previewed.** Where the server mounts previews, the bundle
//! of a successful application build is mounted as the run's preview (TODO
//! §7b), beside the live mount, for `view_app` to look at. Publishing is still
//! the admin's Build button.
//!
//! **And the application's checklists run against the preview**
//! (TODO-checklists C2.6, [`super::checklists`]): each is one more check,
//! `checklist:<name>`, with a baseline like the others, so a checklist the run
//! breaks makes `check` red and one that was already red does not.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use sc_agent::{Signal, TraitContext};
use sc_app::Diagnostic;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_files::FileStore;
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::checklists::ChecklistCheck;
use super::ledger::{Ledger, PreImage};
use super::script::{Project, Ran, npm_run, project, tail};
use super::state::CodingState;
use super::{CFG_DIAGNOSE, CFG_MAY_CHECK, DEFAULT_DIAGNOSE, may};
use crate::build_application::{resolve_application, success_log};
use crate::files::{FileScope, config_count};
use crate::table::{arguments, config_str};

/// The checks `check` runs: `package.json` script names, in order.
pub const CFG_CHECKS: &str = "checks";

/// The most diagnostics shown for one check.
pub const MAX_DIAGNOSTICS: usize = 20;

/// The most characters of output shown when a failed check's output names no
/// diagnostic.
const MAX_TAIL_CHARS: usize = 2000;

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("check_{}", scope.slug())
}

/// The `check` tool.
pub fn spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        tool_name(scope),
        "Run the project's configured checks (type check, tests, and the application's \
         build where there is one). Each diagnostic is marked new or pre-existing; the work \
         is done when there are no new failures. A green build of the application becomes \
         this run's preview, which view_app then looks at."
            .to_owned(),
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
    )
}

/// The configured check scripts, in order.
pub fn configured_checks(config: &Attrs) -> Result<Vec<String>> {
    let invalid = || {
        Error::invalid(format!(
            "`{CFG_CHECKS}` should be a list of package.json script names"
        ))
    };
    let names: Vec<String> = match config.get(CFG_CHECKS) {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(invalid)
            })
            .collect::<Result<_>>()?,
        Some(_) => return Err(invalid()),
    };
    for (i, name) in names.iter().enumerate() {
        if names[..i].contains(name) {
            return Err(Error::invalid(format!(
                "`{CFG_CHECKS}` lists `{name}` twice"
            )));
        }
    }
    Ok(names)
}

/// The configured application's subdomain, or `None`.
pub fn configured_application(config: &Attrs) -> Option<String> {
    Some(config_str(config, crate::CFG_APPLICATION)).filter(|s| !s.is_empty())
}

/// Validate `checks` and `application` on save and on load.
pub async fn validate(catalog: &Catalog, config: &Attrs) -> Result<()> {
    configured_checks(config)?;
    if let Some(subdomain) = configured_application(config) {
        checked_application(catalog, &subdomain).await?;
    }
    Ok(())
}

/// The application `check` builds, and the source it builds from — `None` for
/// one with nothing to build (a `none` application, Saltcorn UI), which is
/// served as it is and still has a preview to look at.
///
/// Unlike `build_application`'s, which exists only to build, this setting is
/// also how an agent is known as an application's builder, so an application
/// with no build step is not refused here.
pub(super) async fn checked_application(
    catalog: &Catalog,
    subdomain: &str,
) -> Result<(sc_app::Application, Option<sc_app::AppSource>)> {
    let app = sc_app::load_application_by_subdomain(catalog, subdomain)
        .await?
        .ok_or_else(|| Error::invalid(format!("no application is served at `{subdomain}`")))?;
    if sc_app::framework_factory(&app.framework.name).is_some() {
        return Ok((app, None));
    }
    let (app, source) = resolve_application(catalog, subdomain).await?;
    Ok((app, Some(source)))
}

/// The post-turn type-check script's name.
pub fn diagnose_script(config: &Attrs) -> String {
    match config_str(config, CFG_DIAGNOSE) {
        name if name.is_empty() => DEFAULT_DIAGNOSE.to_owned(),
        name => name,
    }
}

/// The name the application build is reported and recorded under.
fn build_name(subdomain: &str) -> String {
    format!("build:{subdomain}")
}

/// What makes two diagnostics the same one, across edits that move lines.
pub fn key(d: &Diagnostic) -> String {
    format!("{}\u{1f}{}", d.file, d.message)
}

/// One check's result before the run changed anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// Whether it passed.
    pub passed: bool,
    /// Its diagnostics' [`key`]s.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

/// How one check ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// `package.json` declares no such script.
    Undeclared,
    /// It did not finish in time.
    TimedOut(u64),
    /// It was not run, for this reason.
    Skipped(String),
    /// It ran.
    Ran {
        /// Whether it succeeded.
        passed: bool,
        /// What its output named.
        diagnostics: Vec<Diagnostic>,
        /// Its output.
        output: String,
    },
}

impl Outcome {
    /// What the baseline records of it; `None` for a check that did not run.
    pub(super) fn baseline(&self) -> Option<Baseline> {
        match self {
            Outcome::Skipped(_) => None,
            Outcome::Undeclared | Outcome::TimedOut(_) => Some(Baseline {
                passed: false,
                diagnostics: Vec::new(),
            }),
            Outcome::Ran {
                passed,
                diagnostics,
                ..
            } => Some(Baseline {
                passed: *passed,
                diagnostics: diagnostics.iter().map(key).collect(),
            }),
        }
    }
}

/// One check, run.
#[derive(Debug, Clone)]
pub struct CheckRun {
    /// The script name, or `build:<subdomain>`.
    pub name: String,
    /// How it ended.
    pub outcome: Outcome,
    /// How long it took.
    pub elapsed: Duration,
}

/// Run one `package.json` script as a check. Diagnostics under the project
/// directory are made relative to it.
pub async fn run_script(project: &Project, script: &str, timeout: u64) -> Result<CheckRun> {
    let started = Instant::now();
    let outcome = if !project.scripts.iter().any(|s| s == script) {
        Outcome::Undeclared
    } else {
        match npm_run(&project.dir, script, timeout).await? {
            Ran::TimedOut => Outcome::TimedOut(timeout),
            Ran::Finished(output) => {
                let log = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let prefix = format!("{}/", project.dir.to_string_lossy());
                let diagnostics = sc_app::parse_diagnostics(&log)
                    .into_iter()
                    .map(|mut d| {
                        if let Some(rest) = d.file.strip_prefix(&prefix) {
                            d.file = rest.to_owned();
                        }
                        d
                    })
                    .collect();
                Outcome::Ran {
                    passed: output.status.success(),
                    diagnostics,
                    output: log,
                }
            }
        }
    };
    Ok(CheckRun {
        name: script.to_owned(),
        outcome,
        elapsed: started.elapsed(),
    })
}

/// Build the configured application, as a check. A green build also says
/// where its bundle is, for the preview — an empty path for an application with
/// nothing to build, whose preview is mounted without one.
async fn run_build(
    subdomain: &str,
    ctx: &TraitContext<'_>,
) -> Result<(CheckRun, Option<std::path::PathBuf>)> {
    let started = Instant::now();
    let (app, source) = checked_application(ctx.catalog, subdomain).await?;
    // Nothing to build is a pass, and the preview is the application as it is
    // served — there is no bundle for it to be mounted from.
    let Some(source) = source else {
        let run = CheckRun {
            name: build_name(subdomain),
            outcome: Outcome::Ran {
                passed: true,
                diagnostics: Vec::new(),
                output: format!(
                    "`{subdomain}` has nothing to build: it is served from its file stores \
                     as they are."
                ),
            },
            elapsed: started.elapsed(),
        };
        return Ok((run, Some(std::path::PathBuf::new())));
    };
    // A failed build is news about the build, so it is an outcome, not an error.
    let (outcome, bundle) =
        match sc_app::build_application(ctx.catalog, &app, &source, ctx.triggers).await {
            Ok(report) => {
                let output = success_log(&report);
                let outcome = Outcome::Ran {
                    passed: true,
                    diagnostics: sc_app::parse_diagnostics(&output),
                    output,
                };
                (outcome, Some(report.output_dir))
            }
            Err(e) => {
                let output = e.to_string();
                let outcome = Outcome::Ran {
                    passed: false,
                    diagnostics: sc_app::parse_diagnostics(&output),
                    output,
                };
                (outcome, None)
            }
        };
    let run = CheckRun {
        name: build_name(subdomain),
        outcome,
        elapsed: started.elapsed(),
    };
    Ok((run, bundle))
}

/// Mount or refresh the run's preview of `subdomain` from a green build's
/// bundle (TODO §7b), and say so; with the preview, when it was mounted.
/// Nothing is said where the server mounts no previews.
async fn mount_preview(
    subdomain: &str,
    bundle: &std::path::Path,
    ctx: &TraitContext<'_>,
) -> Option<(String, Option<sc_agent::PreviewInfo>)> {
    let previews = ctx.previews?;
    Some(
        match previews.mount_preview(ctx.run, subdomain, bundle).await {
            Ok(preview) => (
                format!(
                    "preview: this build of `{subdomain}` is mounted for this run at {}; \
                     the live application is unchanged.",
                    preview.host
                ),
                Some(preview),
            ),
            Err(e) => (format!("preview: this build could not be mounted: {e}"), None),
        },
    )
}

/// Record the baseline of every check the configuration names — the post-turn
/// `diagnose` script, the `checks` and the application build — that is not
/// recorded yet. Only while the run has changed nothing, and only with the
/// `may_check` grant: called just before each edit.
pub async fn record_baseline(
    scope: &FileScope,
    config: &Attrs,
    ctx: &mut TraitContext<'_>,
) -> Result<()> {
    let mut state = CodingState::load(ctx.trait_state);
    if !may(config, CFG_MAY_CHECK) || !state.ledger.is_empty() {
        return Ok(());
    }
    let mut scripts = vec![diagnose_script(config)];
    for name in configured_checks(config)? {
        if !scripts.contains(&name) {
            scripts.push(name);
        }
    }
    scripts.retain(|name| !state.baseline.contains_key(name));
    let application = configured_application(config)
        .filter(|subdomain| !state.baseline.contains_key(&build_name(subdomain)));
    if scripts.is_empty() && application.is_none() {
        return Ok(());
    }

    let timeout = config_count(config, super::CFG_TIMEOUT, super::DEFAULT_TIMEOUT_SECONDS)?;
    // A scope with no runnable project has no script baseline: every script
    // is then undeclared, before and after.
    if let Ok(project) = project(scope, ctx.catalog, &crate::files::file_caller(ctx.caller)).await {
        for script in scripts {
            let run = run_script(&project, &script, timeout).await?;
            if let Some(baseline) = run.outcome.baseline() {
                state.baseline.insert(script, baseline);
            }
        }
    }
    if let Some(subdomain) = application {
        let (run, _) = run_build(&subdomain, ctx).await?;
        if let Some(baseline) = run.outcome.baseline() {
            state.baseline.insert(run.name, baseline);
        }
    }
    state.store(ctx.trait_state);
    Ok(())
}

/// Record the baseline of each run that has none yet.
fn record_runs(state: &mut CodingState, runs: &[CheckRun]) {
    for run in runs {
        if !state.baseline.contains_key(&run.name)
            && let Some(baseline) = run.outcome.baseline()
        {
            state.baseline.insert(run.name.clone(), baseline);
        }
    }
}

/// A check's diagnostics, each marked new (`true`) or pre-existing against the
/// baseline. A diagnostic is new when there are more with its key than the
/// baseline had. With no baseline, everything is new.
pub fn classify<'d>(
    diagnostics: &'d [Diagnostic],
    baseline: Option<&Baseline>,
) -> Vec<(bool, &'d Diagnostic)> {
    let mut budget: BTreeMap<&str, usize> = BTreeMap::new();
    for key in baseline
        .map(|b| b.diagnostics.as_slice())
        .unwrap_or_default()
    {
        *budget.entry(key.as_str()).or_default() += 1;
    }
    diagnostics
        .iter()
        .map(|d| match budget.get_mut(key(d).as_str()) {
            Some(left) if *left > 0 => {
                *left -= 1;
                (false, d)
            }
            _ => (true, d),
        })
        .collect()
}

/// Whether a check's result has failures its baseline did not.
pub fn has_new_failures(outcome: &Outcome, baseline: Option<&Baseline>) -> bool {
    let failed_before = baseline.is_some_and(|b| !b.passed);
    match outcome {
        Outcome::Skipped(_) => false,
        Outcome::Undeclared | Outcome::TimedOut(_) => !failed_before,
        Outcome::Ran {
            passed,
            diagnostics,
            ..
        } => {
            !passed
                && (!failed_before || classify(diagnostics, baseline).iter().any(|(new, _)| *new))
        }
    }
}

/// The report of one check: its result, the counts, and its capped
/// diagnostics, the `first` files first, then new before pre-existing.
pub fn describe(
    name: &str,
    outcome: &Outcome,
    baseline: Option<&Baseline>,
    first: &[String],
    elapsed: Option<Duration>,
) -> String {
    let took = elapsed
        .map(|e| format!(" ({:.1}s)", e.as_secs_f64()))
        .unwrap_or_default();
    let (passed, diagnostics, output) = match outcome {
        Outcome::Undeclared => {
            return format!(
                "{name}: failed{took}, because package.json declares no `{name}` script."
            );
        }
        Outcome::TimedOut(seconds) => {
            return format!("{name}: still running after {seconds} seconds; stopped.");
        }
        Outcome::Skipped(reason) => return format!("{name}: skipped, {reason}."),
        Outcome::Ran {
            passed,
            diagnostics,
            output,
        } => (*passed, diagnostics, output),
    };
    if passed && diagnostics.is_empty() {
        return format!("{name}: passed{took}.");
    }
    let marked = classify(diagnostics, baseline);
    let new = marked.iter().filter(|(n, _)| *n).count();
    let mut out = format!(
        "{name}: {}{took}, {new} new, {} pre-existing.",
        if passed {
            "passed with diagnostics"
        } else {
            "failed"
        },
        marked.len() - new
    );
    if marked.is_empty() {
        let tail_text = tail(output);
        let short: String = tail_text
            .chars()
            .skip(tail_text.chars().count().saturating_sub(MAX_TAIL_CHARS))
            .collect();
        out.push_str(&format!(" Its output ends:\n{}", short.trim()));
        return out;
    }
    let mut ordered = marked;
    // Stable: the files named first, then new before pre-existing.
    ordered.sort_by_key(|(new, d)| (!first.contains(&d.file), !*new));
    for (new, d) in ordered.iter().take(MAX_DIAGNOSTICS) {
        out.push_str(&format!(
            "\n{}:{}:{}: {} ({})",
            d.file,
            d.line,
            d.column,
            d.message,
            if *new { "new" } else { "pre-existing" }
        ));
    }
    if ordered.len() > MAX_DIAGNOSTICS {
        out.push_str(&format!(
            "\n[{} more not shown]",
            ordered.len() - MAX_DIAGNOSTICS
        ));
    }
    out
}

/// Run every check, compare against the baseline, apply the ratchet, and
/// report. Raises [`Signal::CheckFailed`] when anything has new failures.
pub async fn call(
    scope: &FileScope,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    arguments(args, &[])?;
    let report = run_checks(scope, config, ctx).await?;
    if !report.green {
        ctx.signal(Signal::CheckFailed);
    }
    Ok(Json::String(report.text))
}

/// What one `check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// No check has new failures, and the ratchet passed.
    pub green: bool,
    /// The report the model reads; its first line is the verdict.
    pub text: String,
}

/// Run every check against the run state in `ctx`, and report — what the
/// `check` tool does, and what `implement_feature` does independently of the
/// session it checks (TODO §8, step 4). Raises no signal.
///
/// A red result before the run has changed anything but test files is
/// recorded in the state, as the reproduction a bug's session is asked for
/// (TODO 9.7).
pub async fn run_checks(
    scope: &FileScope,
    config: &Attrs,
    ctx: &mut TraitContext<'_>,
) -> Result<CheckReport> {
    let checks = configured_checks(config)?;
    let application = configured_application(config);
    let timeout = config_count(config, super::CFG_TIMEOUT, super::DEFAULT_TIMEOUT_SECONDS)?;
    let diagnose = diagnose_script(config);
    let mut state = CodingState::load(ctx.trait_state);

    let mut runs: Vec<CheckRun> = Vec::new();
    if !checks.is_empty() {
        let project = project(scope, ctx.catalog, &crate::files::file_caller(ctx.caller)).await?;
        for script in &checks {
            runs.push(run_script(&project, script, timeout).await?);
        }
    }
    // Before the run has changed anything, what is there *is* the baseline —
    // recorded before the build is decided on, so a type check that was already
    // failing does not hold the build back.
    let unchanged = state.ledger.is_empty();
    if unchanged {
        record_runs(&mut state, &runs);
    }
    let mut preview = None;
    let mut checklists: Vec<ChecklistCheck> = Vec::new();
    if let Some(subdomain) = &application {
        // The type check found new errors: the build would fail on them too.
        let blocked = runs.iter().find(|run| {
            run.name == diagnose && has_new_failures(&run.outcome, state.baseline.get(&run.name))
        });
        let build = match blocked {
            Some(run) => CheckRun {
                name: build_name(subdomain),
                outcome: Outcome::Skipped(format!("because {} has new failures", run.name)),
                elapsed: Duration::ZERO,
            },
            None => {
                let (build, bundle) = run_build(subdomain, ctx).await?;
                if let Some(bundle) = bundle
                    && let Some((said, mounted)) = mount_preview(subdomain, &bundle, ctx).await
                {
                    preview = Some(said);
                    if let Some(mounted) = mounted {
                        checklists =
                            super::checklists::run_on_preview(subdomain, &mounted, ctx).await?;
                    }
                }
                build
            }
        };
        if unchanged {
            record_runs(&mut state, std::slice::from_ref(&build));
            let checks: Vec<CheckRun> = checklists.iter().map(|c| c.check.clone()).collect();
            record_runs(&mut state, &checks);
        }
        runs.push(build);
    }
    let (store, _) = scope.connect(ctx.catalog).await?;
    let weakened = ratchet(scope, store.as_ref(), &state.ledger).await?;

    let mut red: Vec<&str> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for run in &runs {
        let baseline = state.baseline.get(&run.name);
        if has_new_failures(&run.outcome, baseline) {
            red.push(&run.name);
        }
        lines.push(describe(
            &run.name,
            &run.outcome,
            baseline,
            &[],
            Some(run.elapsed),
        ));
    }
    let mut checklist_lines: Vec<String> = Vec::new();
    for checklist in &checklists {
        let baseline = state.baseline.get(&checklist.check.name);
        if has_new_failures(&checklist.check.outcome, baseline) {
            red.push(&checklist.check.name);
        }
        checklist_lines.push(super::checklists::describe(checklist, baseline));
    }
    if weakened.is_empty() {
        lines.push("ratchet: passed.".to_owned());
    } else {
        red.push("ratchet");
        lines.push(format!(
            "ratchet: failed. Tests may not be deleted, reduced or skipped; restore them:\n{}",
            weakened
                .iter()
                .map(|w| format!("- {w}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    // With no checks listed, say what did run, and who can add the rest — a
    // `code` application's builder is created that way (TODO §12).
    let ran = match &application {
        None => "only the ratchet ran",
        Some(_) => "only the build and the ratchet ran",
    };
    let mut head = match (checks.is_empty(), red.is_empty()) {
        (_, false) => format!("check: red, new failures in {}.", red.join(", ")),
        (false, true) => "check: green, no new failures.".to_owned(),
        (true, true) => format!(
            "check: green, but {ran}: no checks are configured. \
             Tell the user an administrator can list package.json scripts in the \
             `coding` trait's `{CFG_CHECKS}` setting."
        ),
    };
    // Red before anything but tests changed: the failure was shown before
    // the fix.
    let only_tests = state
        .ledger
        .paths()
        .all(|path| is_test_file(&scope.relative(path)));
    let reproduced = !red.is_empty() && !unchanged && only_tests && !state.red_before_fix;
    if reproduced {
        state.red_before_fix = true;
    }
    if unchanged || reproduced {
        state.store(ctx.trait_state);
    }
    lines.extend(preview);
    if !checklist_lines.is_empty() {
        lines.push(format!(
            "checklists of `{}` against this run's preview (what the application's owner wrote \
             down as working; fix what you broke, never weaken a checklist, and name every one \
             still failing in your final summary):",
            application.as_deref().unwrap_or("")
        ));
        lines.extend(checklist_lines);
    }
    head.push('\n');
    head.push_str(&lines.join("\n"));
    Ok(CheckReport {
        green: red.is_empty(),
        text: head,
    })
}

/// Whether a path, relative to the scope, is a JavaScript or TypeScript test
/// file: `*.test.*`, `*.spec.*`, or anything under `__tests__/`.
pub fn is_test_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if !matches!(
        ext,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts"
    ) {
        return false;
    }
    stem.ends_with(".test")
        || stem.ends_with(".spec")
        || rel.split('/').any(|segment| segment == "__tests__")
}

/// What the ratchet counts in a test file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TestCounts {
    /// `it(`, `test(` and `describe(` blocks, in every variant.
    pub blocks: usize,
    /// `.skip`, `.only`, `xit`, `xtest`, `xdescribe`, `fit` and `fdescribe`.
    pub weakened: usize,
}

/// Count a test file's blocks and skips.
pub fn count_tests(text: &str) -> TestCounts {
    // `it(`, `describe.each([…])(`, `test.skip(`, `xit(` — every block, whatever
    // its modifiers, so turning `it(` into `it.skip(` is one skip, not also one
    // block fewer.
    let blocks = regex_lite::Regex::new(
        r"(?:^|[^\w.$])[xf]?(?:it|test|describe)(?:\.(?:each|concurrent|sequential|skip|only|todo|fails))*\s*[(`]",
    );
    let weakened = regex_lite::Regex::new(
        r"(?:^|[^\w.$])(?:(?:it|test|describe)(?:\.\w+)*\.(?:skip|only)\b|(?:xit|xtest|xdescribe|fit|fdescribe)\s*\()",
    );
    match (blocks, weakened) {
        (Ok(blocks), Ok(weakened)) => TestCounts {
            blocks: blocks.find_iter(text).count(),
            weakened: weakened.find_iter(text).count(),
        },
        _ => TestCounts::default(),
    }
}

/// How the run weakened its tests, one sentence each; empty when it did not.
pub async fn ratchet(
    scope: &FileScope,
    store: &dyn FileStore,
    ledger: &Ledger,
) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for path in ledger.paths() {
        let rel = scope.relative(path);
        if !is_test_file(&rel) {
            continue;
        }
        let Some(PreImage::Text { text: before }) = ledger.pre_image(path) else {
            // New in this run, or not text: nothing to weaken.
            continue;
        };
        // A test file moved keeps being checked, where it went.
        let mut now_at = path.to_owned();
        for m in ledger.moves() {
            if m.from == now_at {
                now_at = m.to.clone();
            }
        }
        let now = match store.stat(&now_at).await? {
            Some(stat) if !stat.is_dir => Some(store.read(&now_at).await?),
            _ => None,
        };
        let Some(now) = now else {
            found.push(format!("deleted the test file `{rel}`"));
            continue;
        };
        let now_rel = scope.relative(&now_at);
        if !is_test_file(&now_rel) {
            found.push(format!(
                "moved the test file `{rel}` to `{now_rel}`, which is not a test file"
            ));
            continue;
        }
        let before = count_tests(before);
        let after = count_tests(&String::from_utf8_lossy(&now));
        if after.blocks < before.blocks {
            found.push(format!(
                "`{now_rel}` has {} test blocks (it/test/describe), down from {}",
                after.blocks, before.blocks
            ));
        }
        if after.weakened > before.weakened {
            found.push(format!(
                "`{now_rel}` has {} added .skip/.only/xit/xdescribe",
                after.weakened - before.weakened
            ));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(file: &str, line: u32, message: &str) -> Diagnostic {
        Diagnostic {
            file: file.to_owned(),
            line,
            column: 1,
            message: message.to_owned(),
        }
    }

    fn ran(passed: bool, diagnostics: Vec<Diagnostic>) -> Outcome {
        Outcome::Ran {
            passed,
            diagnostics,
            output: String::new(),
        }
    }

    #[test]
    fn a_failure_is_new_unless_the_baseline_failed_the_same_way() {
        let old = diag("src/lib.ts", 3, "TS1: old");
        let broken = Baseline {
            passed: false,
            diagnostics: vec![key(&old)],
        };
        let clean = Baseline {
            passed: true,
            diagnostics: Vec::new(),
        };
        // The same error, moved: not new.
        let moved = ran(false, vec![diag("src/lib.ts", 9, "TS1: old")]);
        assert!(!has_new_failures(&moved, Some(&broken)));
        assert!(has_new_failures(&moved, Some(&clean)));
        assert!(has_new_failures(&moved, None));
        // A second one: new.
        let more = ran(false, vec![old.clone(), diag("src/a.ts", 1, "TS2: new")]);
        assert!(has_new_failures(&more, Some(&broken)));
        // A pass is never a new failure, and neither is a skip.
        assert!(!has_new_failures(&ran(true, vec![old]), None));
        assert!(!has_new_failures(&Outcome::Skipped("x".to_owned()), None));
        // A script that was missing before and still is: not new.
        let missing = Outcome::Undeclared.baseline();
        assert!(!has_new_failures(&Outcome::Undeclared, missing.as_ref()));
        assert!(has_new_failures(&Outcome::Undeclared, Some(&clean)));
    }

    #[test]
    fn a_report_names_the_counts_and_marks_each_diagnostic() {
        let old = diag("src/lib.ts", 3, "TS1: old");
        let baseline = Baseline {
            passed: false,
            diagnostics: vec![key(&old)],
        };
        let outcome = ran(
            false,
            vec![
                diag("src/lib.ts", 9, "TS1: old"),
                diag("src/lib.ts", 10, "TS2: also in lib"),
                diag("src/App.tsx", 4, "TS3: caused by the edit"),
            ],
        );
        assert_eq!(
            describe(
                "typecheck",
                &outcome,
                Some(&baseline),
                &["src/App.tsx".to_owned()],
                None
            ),
            "typecheck: failed, 2 new, 1 pre-existing.\n\
             src/App.tsx:4:1: TS3: caused by the edit (new)\n\
             src/lib.ts:10:1: TS2: also in lib (new)\n\
             src/lib.ts:9:1: TS1: old (pre-existing)"
        );
        assert_eq!(
            describe(
                "test",
                &ran(true, vec![]),
                None,
                &[],
                Some(Duration::from_millis(1250))
            ),
            "test: passed (1.2s)."
        );
        assert_eq!(
            describe("lint", &Outcome::Undeclared, None, &[], None),
            "lint: failed, because package.json declares no `lint` script."
        );
    }

    #[test]
    fn checks_are_a_list_of_distinct_names() {
        let with =
            |value: Json| -> Attrs { [(CFG_CHECKS.to_owned(), value)].into_iter().collect() };
        assert_eq!(
            configured_checks(&with(json!(["typecheck", " test "]))).unwrap(),
            ["typecheck", "test"]
        );
        assert!(configured_checks(&Attrs::new()).unwrap().is_empty());
        assert!(configured_checks(&with(json!("typecheck"))).is_err());
        assert!(configured_checks(&with(json!(["test", ""]))).is_err());
        let err = configured_checks(&with(json!(["test", "test"])))
            .unwrap_err()
            .to_string();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn test_files_are_recognised_by_name_and_directory() {
        assert!(is_test_file("src/App.test.tsx"));
        assert!(is_test_file("src/api.spec.ts"));
        assert!(is_test_file("src/__tests__/util.js"));
        assert!(!is_test_file("src/App.tsx"));
        assert!(!is_test_file("src/latest.ts"));
        assert!(!is_test_file("docs/App.test.md"));
    }

    #[test]
    fn blocks_and_skips_are_counted_whatever_their_modifiers() {
        let text = "\
describe('App', () => {
  it('renders', () => {});
  test.each([1, 2])('adds %i', (n) => {});
  it.skip('is slow', () => {});
  xit('is old', () => {});
  const submit = () => {};
});
";
        assert_eq!(
            count_tests(text),
            TestCounts {
                blocks: 5,
                weakened: 2
            }
        );
        // Words that merely end in a block's name are not blocks.
        assert_eq!(
            count_tests("submit(x); latest(y); it.skipped = true;"),
            TestCounts::default()
        );
        assert_eq!(count_tests("describe.only('x', f)").weakened, 1);
    }
}
