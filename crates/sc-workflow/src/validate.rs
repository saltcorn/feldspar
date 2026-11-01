//! Checking a [`Workflow`] before it is stored — and again before a run of it is
//! started (design §10.3, decision 11).
//!
//! One function, [`workflow_issues`], and everything that can be wrong with a
//! workflow is checked in it. It runs **on save**, because that is when the admin
//! is looking at the canvas and can fix it, and again **on load** in
//! [`start_run`](crate::start_run), because the world moves underneath a stored
//! document: a table is dropped, a plugin that provided a step's action is
//! removed, a dump is restored somewhere the formulas do not resolve.
//!
//! That is the same shape — and the same reason —
//! [`validate_trigger`](sc_action::validate_trigger) has for the other half of a
//! trigger. What is checked *there* is what both bodies share (the name, the
//! event, the channel, the role floor, the timing, the `only_if`); what is
//! checked **here** is the program, because the program lives in a version row
//! rather than on the trigger.
//!
//! ## Issues, not one error
//!
//! [`workflow_issues`] answers a **list**, each entry naming the step it is
//! about, because the caller is a visual editor: an admin who has three broken
//! steps wants three markers on three nodes, not one message about whichever the
//! checker happened to reach first. [`validate_workflow`] is the same pass
//! reduced to a `Result` for the callers that only need a yes or no — the save,
//! and the start of a run.
//!
//! A workflow that fails is **still stored, still listed and still editable**,
//! with its reason kept: editing it is the repair, and a document that vanished
//! from the screen when it stopped validating would be a document nobody can fix.
//! What it cannot do is start a run.
//!
//! ## Reachability, and the one thing that suspends it
//!
//! Every step should be reachable from the start, because a step nothing points
//! at is either a mistake or a leftover, and both are worth saying. The exception
//! is [`Next::Formula`]: which step a computed `next` reaches is a question only
//! the run can answer (that is why it is drawn as one dashed edge to a marker
//! rather than as edges), so a workflow that contains a reachable one has **no**
//! decidable unreachable set, and this check stands down rather than inventing
//! one.

use std::collections::{BTreeSet, VecDeque};

use sc_action::{ActionRegistry, ConfigCheck, check_formula};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_expr::Formula;
use sc_types::{Attrs, FormField, OptionsSource, validate_attrs};

use crate::ERROR_KEY;
use crate::scope::{WORKFLOW_SCOPE, workflow_shape};
use crate::workflow::{ErrorPolicy, Next, Step, StepKind, Workflow};

/// One thing wrong with a workflow, and where it is.
///
/// `step` is what the editor marks the node with; `None` is a problem about the
/// workflow as a whole (a start step that does not exist, an error policy naming
/// a stranger) and belongs on the canvas rather than on any one node.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowIssue {
    /// The step this is about, or `None` for the workflow itself.
    pub step: Option<String>,
    /// What is wrong, as the admin reads it.
    pub problem: String,
}

impl WorkflowIssue {
    /// An issue about the workflow as a whole.
    pub fn workflow(problem: impl Into<String>) -> WorkflowIssue {
        WorkflowIssue {
            step: None,
            problem: problem.into(),
        }
    }

    /// An issue about one step.
    pub fn step(step: impl Into<String>, problem: impl Into<String>) -> WorkflowIssue {
        WorkflowIssue {
            step: Some(step.into()),
            problem: problem.into(),
        }
    }

    /// How this issue reads on one line, with the step named when there is one.
    pub fn message(&self) -> String {
        match &self.step {
            Some(step) => format!("step `{step}`: {}", self.problem),
            None => self.problem.clone(),
        }
    }
}

/// Everything wrong with `workflow`, in the order a reader would want it: the
/// workflow's own problems first, then each step's in the order the steps are
/// listed.
///
/// `channel` is the **trigger's** — the table for a table event, `None`
/// otherwise — because that is what decides both an action's declaration
/// ([`config_spec_for`](sc_action::Action::config_spec_for)) and what a formula
/// may name ([`workflow_shape`]). A step validated against the wrong channel
/// would be a step accepted on save and unbound when it runs, which is the worst
/// of the two.
///
/// Fails, rather than answering issues, only when the *checking* could not
/// happen: the catalog would not answer. A workflow that is merely wrong is
/// never an error here — it is a list.
pub async fn workflow_issues(
    catalog: &Catalog,
    registry: &ActionRegistry,
    workflow: &Workflow,
    channel: Option<&str>,
) -> Result<Vec<WorkflowIssue>> {
    let mut issues = Vec::new();
    let shape = workflow_shape(catalog, channel)?;
    let names = check_names(workflow, &mut issues);

    if workflow.max_steps == 0 {
        issues.push(WorkflowIssue::workflow(
            "a workflow's step budget (`max_steps`) must be at least 1",
        ));
    }
    if workflow.start.trim().is_empty() {
        issues.push(WorkflowIssue::workflow(
            "a workflow needs a start step; this one names none",
        ));
    } else if !names.contains(workflow.start.as_str()) {
        issues.push(WorkflowIssue::workflow(format!(
            "the start step `{}` is not a step of this workflow",
            workflow.start
        )));
    }
    issues.extend(
        policy_problems(&workflow.error_policy, &names)
            .into_iter()
            .map(|p| WorkflowIssue::workflow(format!("the workflow's error policy: {p}"))),
    );

    for step in &workflow.steps {
        check_step(
            catalog,
            registry,
            step,
            &names,
            channel,
            &shape,
            &mut issues,
        )
        .await?;
    }

    check_reachable(workflow, &names, &mut issues);
    Ok(issues)
}

/// [`workflow_issues`] as a yes or no: `Ok(())`, or one error listing everything
/// that is wrong.
///
/// Everything, not the first thing: this is what refuses a save and what refuses
/// to start a run, and an admin told about one of three problems fixes it and is
/// told about the next one.
pub async fn validate_workflow(
    catalog: &Catalog,
    registry: &ActionRegistry,
    workflow: &Workflow,
    channel: Option<&str>,
) -> Result<()> {
    let issues = workflow_issues(catalog, registry, workflow, channel).await?;
    if issues.is_empty() {
        return Ok(());
    }
    Err(Error::invalid(issues_message(&issues)))
}

/// How a list of issues reads as one message.
pub fn issues_message(issues: &[WorkflowIssue]) -> String {
    let mut out = match issues.len() {
        1 => String::from("the workflow is not usable: "),
        n => format!("the workflow is not usable ({n} problems): "),
    };
    out.push_str(
        &issues
            .iter()
            .map(WorkflowIssue::message)
            .collect::<Vec<_>>()
            .join("; "),
    );
    out
}

/// Whether an action can be offered as a **step** on a workflow with this
/// channel (§10.3, phase 5.4).
///
/// The rule is what the sentence says and nothing more: a step whose
/// configuration cannot be filled in must not be in the palette, and a
/// configuration cannot be filled in when the declaration has a **required**
/// field with an empty list of permitted values. That is exactly the shape an
/// action takes on a channel it cannot serve — `send_email`'s attachment
/// pickers are the table's File fields, and a workflow with no table has none —
/// and it is decided from the declaration rather than from a list of action
/// names, so a plugin's action is judged by the same rule as a built-in.
///
/// A required field with no option list at all is fillable: the admin types a
/// value.
pub fn usable_as_step(spec: &[FormField]) -> bool {
    !spec.iter().any(|field| {
        field.required
            && matches!(&field.options_source, OptionsSource::Static(options) if options.is_empty())
    })
}

// ---- the checks themselves ----------------------------------------------

/// The step names, having complained about the ones that are missing or repeated.
fn check_names<'a>(workflow: &'a Workflow, issues: &mut Vec<WorkflowIssue>) -> BTreeSet<&'a str> {
    let mut names: BTreeSet<&str> = BTreeSet::new();
    if workflow.steps.is_empty() {
        issues.push(WorkflowIssue::workflow(
            "a workflow needs at least one step",
        ));
    }
    for step in &workflow.steps {
        let name = step.name.trim();
        if name.is_empty() {
            issues.push(WorkflowIssue::workflow("a step needs a name"));
            continue;
        }
        // A step's name is what a `Next` points at, what the cursor holds and
        // what an action step's result is stored under — so two steps of one name
        // is not a cosmetic clash, it is two steps the engine cannot tell apart.
        if !names.insert(step.name.as_str()) {
            issues.push(WorkflowIssue::step(
                &step.name,
                "two steps of this workflow have this name",
            ));
        }
    }
    names
}

/// One step: what it does, what runs after it, and what happens when it fails.
#[allow(clippy::too_many_arguments)]
async fn check_step(
    catalog: &Catalog,
    registry: &ActionRegistry,
    step: &Step,
    names: &BTreeSet<&str>,
    channel: Option<&str>,
    shape: &sc_expr::SchemaShape,
    issues: &mut Vec<WorkflowIssue>,
) -> Result<()> {
    // Collected as plain sentences and labelled with the step's name at the
    // end, so every problem this step has arrives together and in the order it
    // was found — which is the order the inspector lists them in.
    let mut problems: Vec<String> = Vec::new();
    // The closure borrows `problems`, so it lives in a block of its own: what
    // comes after needs the list back.
    {
        let mut problem = |msg: String| problems.push(msg);

        match &step.kind {
            StepKind::Action {
                action,
                configuration,
            } => {
                for found in
                    action_problems(catalog, registry, action, configuration, channel, shape)
                        .await?
                {
                    problem(found);
                }
            }
            StepKind::Set { assignments } => {
                let mut targets: BTreeSet<&str> = BTreeSet::new();
                for assignment in assignments {
                    let target = assignment.target.trim();
                    if target.is_empty() {
                        problem("an assignment needs a context key to write".to_owned());
                    } else if target == ERROR_KEY {
                        // Reserved: a handler step reads `context.error` expecting the
                        // engine's report of what failed, and a `Set` writing it would
                        // make the handler read its own workflow's invention instead.
                        problem(format!(
                            "`{ERROR_KEY}` is the key the engine puts a failure under, so a step \
                         may not assign to it"
                        ));
                    } else if !targets.insert(target) {
                        problem(format!(
                            "`{target}` is assigned twice in this step; the later one would win"
                        ));
                    }
                    check_source(
                        shape,
                        &assignment.formula,
                        &format!("the value of `{}`", assignment.target),
                        &mut problem,
                    );
                }
            }
            StepKind::ForEach { over, var, body } => {
                check_source(shape, over, "the collection", &mut problem);
                let var = var.trim();
                if var.is_empty() {
                    problem("a loop needs a name to bind each item under".to_owned());
                } else if var == ERROR_KEY {
                    problem(format!(
                        "`{ERROR_KEY}` is the key the engine puts a failure under, so a loop may \
                     not bind its item under it"
                    ));
                }
                if body.trim().is_empty() {
                    problem("a loop needs a first step for its body".to_owned());
                } else if !names.contains(body.as_str()) {
                    problem(format!(
                        "its body starts at `{body}`, which is not a step of this workflow"
                    ));
                }
            }
            StepKind::Wait { until } => {
                if until.trim().is_empty() {
                    problem("a wait needs a deadline".to_owned());
                } else {
                    check_source(shape, until, "the deadline", &mut problem);
                }
            }
            StepKind::UserForm {
                fields,
                assign_to,
                min_role: _,
                timeout,
            } => {
                if assign_to.trim().is_empty() {
                    problem("a form needs a context key to store its answers under".to_owned());
                } else if assign_to.trim() == ERROR_KEY {
                    problem(format!(
                        "`{ERROR_KEY}` is the key the engine puts a failure under, so a form may \
                     not store its answers under it"
                    ));
                }
                if fields.is_empty() {
                    problem("a form with no fields asks a person for nothing".to_owned());
                }
                let mut seen: BTreeSet<&str> = BTreeSet::new();
                for field in fields {
                    if !seen.insert(field.name.as_str()) {
                        problem(format!("the form asks for `{}` twice", field.name));
                    }
                    // The declaration has to lower to the `FormField` the admin UI
                    // renders and `validate_attrs` checks an answer against — a type
                    // name nothing recognises would otherwise be discovered by the
                    // person the form is put in front of.
                    if let Err(e) = field.to_form_field() {
                        problem(e.to_string());
                    }
                }
                if let Some(timeout) = timeout {
                    check_source(shape, timeout, "the timeout", &mut problem);
                }
            }
        }

        check_next(&step.next, names, shape, &mut problem);
    }
    if let Some(policy) = &step.error_policy {
        problems.extend(policy_problems(policy, names));
    }
    issues.extend(
        problems
            .into_iter()
            .map(|p| WorkflowIssue::step(&step.name, p)),
    );
    Ok(())
}

/// An `Action` step: the action exists, and its configuration is what the action
/// declares **for this channel**.
///
/// The same two checks a trigger's action body gets, in the same order and
/// through the same functions — because a step *is* an action body, and the only
/// difference is where its settings are stored, and that it is read in the
/// step's scope: `shape` is [`workflow_shape`]'s, so `context.total` in an
/// `insert_row` value is the same identifier the `Set` before it wrote
/// (decision 8).
async fn action_problems(
    catalog: &Catalog,
    registry: &ActionRegistry,
    action: &str,
    configuration: &Attrs,
    channel: Option<&str>,
    shape: &sc_expr::SchemaShape,
) -> Result<Vec<String>> {
    if action.trim().is_empty() {
        return Ok(vec!["this step names no action".to_owned()]);
    }
    // The registry's own error lists the alternatives, which is what an admin
    // whose plugin is missing needs to see.
    let resolved = match registry.require(action.trim()) {
        Ok(resolved) => resolved,
        Err(e) => return Ok(vec![e.to_string()]),
    };
    let spec = resolved.config_spec_for(catalog, channel);
    if let Err(e) = validate_attrs(&spec, configuration) {
        // The action's own check reads the same settings, and running it over a
        // configuration already known to be the wrong shape would report the same
        // problem in a second voice.
        return Ok(vec![format!("action `{}`: {e}", resolved.name())]);
    }
    // Everything the spec cannot express — that a named table exists, that a
    // configured formula resolves in the scope this event gives it. Only the
    // action knows what its own settings mean.
    if let Err(e) = resolved
        .validate_config(&ConfigCheck {
            catalog,
            config: configuration,
            channel,
            shape,
        })
        .await
    {
        return Ok(vec![format!("action `{}`: {e}", resolved.name())]);
    }
    Ok(Vec::new())
}

/// What runs after a step: every named target exists, and every guard resolves.
fn check_next(
    next: &Next,
    names: &BTreeSet<&str>,
    shape: &sc_expr::SchemaShape,
    problem: &mut impl FnMut(String),
) {
    for target in next.targets() {
        if target.trim().is_empty() {
            problem("`next` names an empty step".to_owned());
        } else if !names.contains(target) {
            problem(format!(
                "`next` goes to `{target}`, which is not a step of this workflow"
            ));
        }
    }
    match next {
        Next::Branch { arms, .. } => {
            for arm in arms {
                check_source(
                    shape,
                    &arm.when,
                    &format!("the guard on the arm to `{}`", arm.step),
                    problem,
                );
            }
        }
        Next::Formula { formula } => {
            check_source(shape, formula, "the computed `next`", problem);
        }
        Next::Step { .. } | Next::End => {}
    }
}

/// An error policy: a handler that names a real step, and a retry that would
/// actually retry.
fn policy_problems(policy: &ErrorPolicy, names: &BTreeSet<&str>) -> Vec<String> {
    let mut problems = Vec::new();
    match policy {
        ErrorPolicy::Handler { step: handler } => {
            if !names.contains(handler.as_str()) {
                problems.push(format!(
                    "its error handler `{handler}` is not a step of this workflow"
                ));
            }
        }
        ErrorPolicy::Retry { max, backoff } => {
            if *max == 0 {
                problems.push(
                    "a retry policy of 0 attempts never retries; use `fail` to say so".to_owned(),
                );
            }
            if backoff.factor < 1.0 {
                problems.push(format!(
                    "a backoff factor of {} would make each wait shorter than the last",
                    backoff.factor
                ));
            }
            if backoff.max_ms < backoff.initial_ms {
                problems.push(format!(
                    "the backoff's ceiling ({}ms) is below its first wait ({}ms)",
                    backoff.max_ms, backoff.initial_ms
                ));
            }
        }
        ErrorPolicy::Fail => {}
    }
    problems
}

/// One formula: it parses, and every identifier in it resolves in the scope the
/// step will have.
///
/// [`check_formula`] is `sc-action`'s, unchanged, so a formula in a step and a
/// formula in an action's configuration are held to the same rule — including
/// the refusal of the operation flags, which mean nothing where the event *is*
/// the operation.
fn check_source(
    shape: &sc_expr::SchemaShape,
    source: &str,
    what: &str,
    problem: &mut impl FnMut(String),
) {
    match Formula::parse(source) {
        Ok(formula) => {
            if let Err(e) = check_formula(shape, WORKFLOW_SCOPE, &formula, what) {
                problem(e.to_string());
            }
        }
        Err(e) => problem(format!("{what}: `{source}`: {e}")),
    }
}

/// Every step is reachable from the start — unless a computed `next` makes the
/// question undecidable (see the module docs).
fn check_reachable(workflow: &Workflow, names: &BTreeSet<&str>, issues: &mut Vec<WorkflowIssue>) {
    if !names.contains(workflow.start.as_str()) {
        // Already reported, and there is nothing to walk from.
        return;
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    // Two roots: the start, and — because it is reachable from *any* step that
    // fails — the workflow's own error handler.
    let roots = [
        Some(workflow.start.as_str()),
        match &workflow.error_policy {
            ErrorPolicy::Handler { step } => Some(step.as_str()),
            _ => None,
        },
    ];
    for root in roots.into_iter().flatten() {
        if let Some(found) = names.get(root)
            && seen.insert(found)
        {
            queue.push_back(found);
        }
    }
    let mut computed = false;
    while let Some(name) = queue.pop_front() {
        let Some(step) = workflow.step(name) else {
            continue;
        };
        let mut reach = |target: &str| {
            if let Some(found) = names.get(target)
                && seen.insert(found)
            {
                queue.push_back(found);
            }
        };
        for target in step.next.targets() {
            reach(target);
        }
        if let Next::Formula { .. } = step.next {
            computed = true;
        }
        // A loop's body and a handler are edges too: the canvas draws both, and a
        // step reachable only as somebody's error handler is reachable.
        if let StepKind::ForEach { body, .. } = &step.kind {
            reach(body);
        }
        if let Some(ErrorPolicy::Handler { step: handler }) = &step.error_policy {
            reach(handler);
        }
    }
    if computed {
        // Which step a computed `next` reaches is the run's answer, so no
        // unreachable set can be proved. Saying nothing is the honest reading;
        // inventing one would mark good steps as dead.
        return;
    }
    for step in &workflow.steps {
        if !seen.contains(step.name.as_str()) {
            issues.push(WorkflowIssue::step(
                &step.name,
                "nothing reaches this step from the start of the workflow",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{Assignment, Backoff, BranchArm, FieldDecl};
    use sc_types::{BasicType, OptionsSource, TypeRef};

    fn wf(steps: Vec<Step>) -> Workflow {
        Workflow::of(sc_action::TriggerId::new(), 1, steps)
    }

    fn set(name: &str) -> Step {
        Step::new(
            name,
            StepKind::Set {
                assignments: vec![],
            },
        )
    }

    /// The checks that need no catalog are the graph ones, and they are most of
    /// the rules. The formula and action halves need a database and are pinned in
    /// `tests/validation.rs`.
    fn graph_issues(workflow: &Workflow) -> Vec<String> {
        let mut issues = Vec::new();
        let names = check_names(workflow, &mut issues);
        if !names.contains(workflow.start.as_str()) {
            issues.push(WorkflowIssue::workflow(format!(
                "the start step `{}` is not a step of this workflow",
                workflow.start
            )));
        }
        issues.extend(
            policy_problems(&workflow.error_policy, &names)
                .into_iter()
                .map(|p| WorkflowIssue::workflow(format!("the workflow's error policy: {p}"))),
        );
        for step in &workflow.steps {
            {
                let mut problem = |msg: String| issues.push(WorkflowIssue::step(&step.name, msg));
                if let StepKind::ForEach { body, .. } = &step.kind
                    && !names.contains(body.as_str())
                {
                    problem(format!(
                        "its body starts at `{body}`, which is not a step of this workflow"
                    ));
                }
                for target in step.next.targets() {
                    if !names.contains(target) {
                        problem(format!(
                            "`next` goes to `{target}`, which is not a step of this workflow"
                        ));
                    }
                }
            }
            if let Some(policy) = &step.error_policy {
                issues.extend(
                    policy_problems(policy, &names)
                        .into_iter()
                        .map(|p| WorkflowIssue::step(&step.name, p)),
                );
            }
        }
        check_reachable(workflow, &names, &mut issues);
        issues.iter().map(WorkflowIssue::message).collect()
    }

    #[test]
    fn a_next_that_names_a_stranger_is_named_with_the_step_it_is_on() {
        let workflow = wf(vec![set("a").then(Next::step("b"))]);
        let issues = graph_issues(&workflow);
        assert!(
            issues
                .iter()
                .any(|i| i.contains("step `a`") && i.contains('b')),
            "{issues:?}"
        );
    }

    #[test]
    fn a_step_nothing_points_at_is_reported_as_unreachable() {
        let workflow = wf(vec![set("a"), set("orphan")]);
        let issues = graph_issues(&workflow);
        assert!(
            issues
                .iter()
                .any(|i| i.contains("orphan") && i.contains("nothing reaches")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_step_reached_only_as_a_handler_or_a_loop_body_is_reachable() {
        let workflow = wf(vec![
            Step::new(
                "loop",
                StepKind::ForEach {
                    over: "context.items".to_owned(),
                    var: "item".to_owned(),
                    body: "body".to_owned(),
                },
            )
            .on_error(ErrorPolicy::Handler {
                step: "rescue".to_owned(),
            }),
            set("body"),
            set("rescue"),
        ]);
        assert!(
            graph_issues(&workflow).is_empty(),
            "{:?}",
            graph_issues(&workflow)
        );
    }

    #[test]
    fn a_computed_next_stands_the_unreachable_check_down() {
        // `a` computes where it goes, so nothing can prove `far` is dead — and
        // marking a good step dead is worse than saying nothing.
        let workflow = wf(vec![
            set("a").then(Next::Formula {
                formula: "context.where".to_owned(),
            }),
            set("far"),
        ]);
        assert!(
            graph_issues(&workflow).is_empty(),
            "{:?}",
            graph_issues(&workflow)
        );
    }

    #[test]
    fn two_steps_of_one_name_are_two_the_engine_cannot_tell_apart() {
        let workflow = wf(vec![set("a").then(Next::step("a")), set("a")]);
        let issues = graph_issues(&workflow);
        assert!(
            issues.iter().any(|i| i.contains("have this name")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_handler_that_is_not_a_step_is_reported_against_the_workflow_or_the_step() {
        let workflow = wf(vec![set("a")]).on_error(ErrorPolicy::Handler {
            step: "nowhere".to_owned(),
        });
        let issues = graph_issues(&workflow);
        assert!(
            issues
                .iter()
                .any(|i| i.contains("workflow's error policy") && i.contains("nowhere")),
            "{issues:?}"
        );

        let workflow = wf(vec![set("a").on_error(ErrorPolicy::Handler {
            step: "nowhere".to_owned(),
        })]);
        let issues = graph_issues(&workflow);
        assert!(
            issues
                .iter()
                .any(|i| i.starts_with("step `a`") && i.contains("nowhere")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_retry_that_would_never_retry_is_said_so() {
        let workflow = wf(vec![set("a").on_error(ErrorPolicy::Retry {
            max: 0,
            backoff: Backoff::default(),
        })]);
        assert!(
            graph_issues(&workflow)
                .iter()
                .any(|i| i.contains("never retries")),
            "{:?}",
            graph_issues(&workflow)
        );

        let workflow = wf(vec![set("a").on_error(ErrorPolicy::Retry {
            max: 3,
            backoff: Backoff {
                initial_ms: 10_000,
                factor: 0.5,
                max_ms: 1_000,
                jitter: false,
            },
        })]);
        let issues = graph_issues(&workflow);
        assert!(
            issues.iter().any(|i| i.contains("shorter than the last")),
            "{issues:?}"
        );
        assert!(issues.iter().any(|i| i.contains("ceiling")), "{issues:?}");
    }

    #[test]
    fn the_start_step_has_to_exist() {
        let mut workflow = wf(vec![set("a")]);
        workflow.start = "elsewhere".to_owned();
        assert!(
            graph_issues(&workflow)
                .iter()
                .any(|i| i.contains("start step")),
            "{:?}",
            graph_issues(&workflow)
        );
    }

    #[test]
    fn an_action_step_with_no_action_and_a_form_with_no_fields_are_both_reported() {
        // Checked through the pieces that need no catalog: the shapes above are
        // what `check_step` reaches before it consults the registry.
        let step = Step::new(
            "ask",
            StepKind::UserForm {
                fields: vec![],
                assign_to: String::new(),
                min_role: None,
                timeout: None,
            },
        );
        let StepKind::UserForm {
            fields,
            assign_to,
            min_role,
            ..
        } = &step.kind
        else {
            unreachable!()
        };
        assert!(fields.is_empty());
        assert!(assign_to.is_empty());
        assert!(min_role.is_none());
    }

    #[test]
    fn a_field_declaration_that_will_not_lower_is_caught_before_a_person_sees_the_form() {
        assert!(FieldDecl::new("n", "intt").to_form_field().is_err());
    }

    #[test]
    fn a_set_may_not_write_the_key_the_engine_puts_a_failure_under() {
        // Asserted on the rule rather than through the async pass: the reserved
        // key is `ERROR_KEY`, and a handler reading it must read the engine's
        // report.
        let assignment = Assignment::new(ERROR_KEY, "1");
        assert_eq!(assignment.target, ERROR_KEY);
    }

    #[test]
    fn an_action_whose_required_setting_has_nothing_to_pick_is_not_offered_as_a_step() {
        let free = FormField::new("url", TypeRef::Basic(BasicType::Text)).required();
        assert!(usable_as_step(std::slice::from_ref(&free)));

        let mut empty_picker = FormField::new("attach", TypeRef::Basic(BasicType::Text)).required();
        empty_picker.options_source = OptionsSource::Static(vec![]);
        assert!(
            !usable_as_step(&[free.clone(), empty_picker.clone()]),
            "a required picker with nothing in it cannot be filled in"
        );

        // Not required: the admin leaves it alone, and the step is fine.
        empty_picker.required = false;
        assert!(usable_as_step(&[free, empty_picker]));
    }

    #[test]
    fn a_branch_arms_guards_are_the_formulas_a_next_contributes() {
        let next = Next::Branch {
            arms: vec![BranchArm::new("context.total > 100", "approve")],
            otherwise: None,
        };
        assert_eq!(next.targets(), ["approve"]);
    }
}
