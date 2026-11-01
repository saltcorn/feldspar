//! The [`Workflow`]: a program a trigger body can be (design §10.3).
//!
//! Pure data, and **the stored JSON is the API shape and the editor's shape**.
//! There is no second spelling: what `_fd_workflow_versions.steps` holds is what
//! `getWorkflow` answers and what the canvas round-trips, so a step the engine
//! understands and a step the editor draws cannot drift apart.
//!
//! ## Control flow is data, with one escape hatch
//!
//! v1 spells "what runs next" as a JavaScript expression over the step names.
//! That is expressive and impossible to *draw*: a visual editor cannot round-trip
//! an arbitrary expression into edges. So [`Next`] is an enum — a step, a branch
//! of guarded arms, a formula, or the end — where the first two are exactly what
//! the canvas draws and edits, and the third keeps v1's power for the case that
//! needs it (one dashed edge to a computed marker, editable as text, never
//! silently rewritten). All four lower to the one question the engine asks:
//! *given this context, which step is next?*
//!
//! ## Five step kinds, and why five
//!
//! GOALS asks for a minimal built-in set, and the count is what that decides.
//! [`StepKind::Action`] runs **any registered action**, which is how
//! `run_js_code`, `send_email`, the row actions and `run_agent` are all already
//! workflow steps; the other four are the things no action can be:
//! [`Set`](StepKind::Set) writes formulas into the run context,
//! [`ForEach`](StepKind::ForEach) is the explicit loop, [`Wait`](StepKind::Wait)
//! is a durable timer and [`UserForm`](StepKind::UserForm) suspends for a person.
//! Everything else an admin could want is an action, and adding an action is not
//! adding a step kind.
//!
//! ## Formulas
//!
//! Every formula here — a `Set` value, a `Branch` arm's guard, a `ForEach`'s
//! collection, a `Wait`'s deadline — is `sc-expr` in the scope
//! [`workflow_shape`](crate::workflow_shape) decides: what an action's
//! configuration sees, plus the ambient `context` (decision 8). They are stored
//! as **source**, not as parsed formulas, because the source is what the admin
//! typed and what the editor shows back.

use sc_action::TriggerId;
use sc_error::{Error, Result};
use sc_types::Role;
use sc_types::{Attrs, BasicType, FormField, OptionsSource, TypeRef};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// The step budget a run gets when the workflow does not say otherwise: how many
/// steps one run may take before the engine stops it.
///
/// A bound rather than a guess: a `Branch` that points back at itself is a loop
/// with no exit, and a run that spins forever is worse than a run that stops with
/// a reason (§10.3, and the agent loop's own budget for the same reason).
pub const DEFAULT_MAX_STEPS: u32 = 1000;

/// One version of one workflow: the program, and the flags that decide how it is
/// run.
///
/// The `id` is the **trigger's**, because a workflow is a trigger body rather
/// than an entity of its own (decision 1): everything that makes it fire — the
/// event, the `only_if`, the role floor, the enabled flag, the periodic timing —
/// is on the trigger, and what is here is only what it *does*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// The trigger this is the body of.
    pub id: TriggerId,
    /// Which version this is. Versions are rows and saving mints the next one
    /// (decision 2), so this is never rewritten.
    pub version: u32,
    /// The name of the step a run starts at.
    pub start: String,
    /// The steps, in the order the editor lists them. Order is presentation:
    /// what runs after what is [`Next`]'s answer, never the position here.
    pub steps: Vec<Step>,
    /// What happens to a step that fails and has no policy of its own.
    #[serde(default)]
    pub error_policy: ErrorPolicy,
    /// Whether every step writes a `_fd_run_traces` row (§9): the context after
    /// it, its timing and its outcome.
    ///
    /// Off by default, because a trace is a copy of the whole context per step
    /// and most workflows do not need one — but on it is what the run detail
    /// screen draws its timeline from.
    #[serde(default)]
    pub trace: bool,
    /// How many steps one run of this workflow may take
    /// ([`DEFAULT_MAX_STEPS`]).
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
}

fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

impl Workflow {
    /// The workflow a **new** trigger gets: version 1, one no-op start step, and
    /// nothing else.
    ///
    /// A placeholder step rather than no steps at all, so a new workflow opens on
    /// a canvas rather than on "the start step does not exist" — the first thing
    /// an admin would see being an error is not a good first thing. The step is a
    /// [`Set`](StepKind::Set) with nothing to set, which is the one kind that
    /// does nothing and means it.
    pub fn empty(id: TriggerId) -> Workflow {
        Workflow {
            id,
            version: 1,
            start: "start".to_owned(),
            steps: vec![Step::new(
                "start",
                StepKind::Set {
                    assignments: vec![],
                },
            )],
            error_policy: ErrorPolicy::default(),
            trace: false,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    /// A workflow with these steps, starting at the first of them.
    ///
    /// The start is the first step because that is what an admin drawing left to
    /// right means, and because a workflow whose start had to be named separately
    /// would have two ways to say the same thing.
    pub fn of(id: TriggerId, version: u32, steps: Vec<Step>) -> Workflow {
        let start = steps.first().map(|s| s.name.clone()).unwrap_or_default();
        Workflow {
            id,
            version,
            start,
            steps,
            error_policy: ErrorPolicy::default(),
            trace: false,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    /// The step of this name, if the workflow has one.
    pub fn step(&self, name: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.name == name)
    }

    /// The step of this name, or an error naming it and the workflow — what the
    /// engine gets when a `Next` points at a step that is not there.
    pub fn require_step(&self, name: &str) -> Result<&Step> {
        self.step(name).ok_or_else(|| {
            Error::invalid(format!(
                "workflow version {} has no step named `{name}`",
                self.version
            ))
        })
    }

    /// Turn tracing on.
    pub fn traced(mut self) -> Workflow {
        self.trace = true;
        self
    }

    /// Set the workflow-level error policy.
    pub fn on_error(mut self, policy: ErrorPolicy) -> Workflow {
        self.error_policy = policy;
        self
    }
}

/// One step: what it does, what happens next, and what happens when it fails.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// The step's name, unique within the workflow. It is what a [`Next`] points
    /// at, what the run's cursor holds, what a trace row records, and the key an
    /// action step's result is stored in the context under — so renaming one is
    /// a real edit, not a relabelling.
    pub name: String,
    /// What the admin wrote about it; the empty string means "none given" (§9's
    /// rule for a description).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// What the step does.
    pub kind: StepKind,
    /// What runs after it.
    #[serde(default)]
    pub next: Next,
    /// This step's own error policy, overriding the workflow's. `None` means
    /// "the workflow's", which is not the same as "no policy" — there is always
    /// a policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_policy: Option<ErrorPolicy>,
}

impl Step {
    /// A step of this name and kind, ending the run after it.
    pub fn new(name: impl Into<String>, kind: StepKind) -> Step {
        Step {
            name: name.into(),
            description: String::new(),
            kind,
            next: Next::End,
            error_policy: None,
        }
    }

    /// Set what runs after this step.
    pub fn then(mut self, next: Next) -> Step {
        self.next = next;
        self
    }

    /// Set this step's own error policy.
    pub fn on_error(mut self, policy: ErrorPolicy) -> Step {
        self.error_policy = Some(policy);
        self
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> Step {
        self.description = description.into();
        self
    }

    /// The policy that governs a failure of this step: its own, or the
    /// workflow's.
    pub fn policy<'a>(&'a self, workflow: &'a Workflow) -> &'a ErrorPolicy {
        self.error_policy.as_ref().unwrap_or(&workflow.error_policy)
    }
}

/// What a step does. Five kinds, and the count is a decision (see the module
/// docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    /// Run a **registered action** with a configuration of its own — the same
    /// `Action` a trigger body runs, in the same "settings as data" shape, so
    /// every action there has ever been is already a workflow step and a plugin's
    /// new one needs no change here.
    Action {
        /// The registered action's name.
        action: String,
        /// Its configuration, keyed by its `config_spec` field names.
        #[serde(default)]
        configuration: Attrs,
    },
    /// Write formulas into the run context — the one thing steps need that no
    /// action provides.
    Set {
        /// The assignments, applied in this order (so a later one may read what
        /// an earlier one wrote).
        assignments: Vec<Assignment>,
    },
    /// Loop over a collection, running the body once per item.
    ///
    /// `body` names the **first step of the body**, and a body path that ends —
    /// `Next::End`, or a step with nothing after it — comes back here for the
    /// next item rather than ending the run. That is what makes the loop drawable
    /// as an ordinary subgraph on the canvas: there is one list of steps, not a
    /// nested one.
    ForEach {
        /// A formula yielding the collection to iterate.
        over: String,
        /// The name the current item is bound under, inside the context, for the
        /// body's formulas.
        var: String,
        /// The first step of the body.
        body: String,
    },
    /// Wait, durably: the run leaves the queue's reach until the deadline and
    /// survives any number of restarts in between.
    Wait {
        /// A formula yielding a duration in milliseconds or an instant.
        until: String,
    },
    /// Suspend until a **person** answers — the human step §10.3 is written
    /// around.
    UserForm {
        /// What to ask for, declared as fields exactly as every other
        /// configurable thing declares its settings.
        fields: Vec<FieldDecl>,
        /// The context key the answers are merged in under.
        assign_to: String,
        /// The role floor for answering. `None` is admin-only, the same safe
        /// reading a trigger's own `min_role` has.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_role: Option<Role>,
        /// A formula yielding when to give up waiting, after which the run wakes
        /// and its error policy decides. `None` waits indefinitely.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<String>,
    },
}

impl StepKind {
    /// The word this kind is called by on the wire and on screen.
    pub fn type_name(&self) -> &'static str {
        match self {
            StepKind::Action { .. } => "action",
            StepKind::Set { .. } => "set",
            StepKind::ForEach { .. } => "for_each",
            StepKind::Wait { .. } => "wait",
            StepKind::UserForm { .. } => "user_form",
        }
    }
}

/// One `Set` assignment: a context key and the formula that computes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    /// The context key to write.
    pub target: String,
    /// The formula computing the value.
    pub formula: String,
}

impl Assignment {
    /// An assignment of `formula` to `target`.
    pub fn new(target: impl Into<String>, formula: impl Into<String>) -> Assignment {
        Assignment {
            target: target.into(),
            formula: formula.into(),
        }
    }
}

/// What runs after a step (see the module docs for why this is an enum and not a
/// formula).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Next {
    /// One named step: the plain edge the canvas draws.
    Step {
        /// Its name.
        step: String,
    },
    /// The first arm whose guard is true, else `otherwise` — the fan-out the
    /// canvas draws as one labelled edge per arm.
    Branch {
        /// The guarded arms, tried in this order.
        arms: Vec<BranchArm>,
        /// Where to go when no arm matched. `None` ends the run, which is the
        /// honest reading of a branch with nothing else to say.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        otherwise: Option<String>,
    },
    /// A formula yielding the **name** of the next step — v1's expressiveness,
    /// kept for the case that needs it and drawn as one dashed edge to a
    /// computed marker.
    Formula {
        /// The formula's source.
        formula: String,
    },
    /// Nothing: the run finishes here (or, inside a `ForEach` body, the iteration
    /// does).
    #[default]
    End,
}

impl Next {
    /// A plain edge to `step`.
    pub fn step(step: impl Into<String>) -> Next {
        Next::Step { step: step.into() }
    }

    /// Every step name this `Next` can reach, in the order it names them — what
    /// validation checks exist and what the editor draws edges for.
    ///
    /// A [`Formula`](Next::Formula) names none: which step it reaches is a
    /// question only the run can answer, which is exactly why it is drawn as a
    /// dashed edge to a marker rather than as edges.
    pub fn targets(&self) -> Vec<&str> {
        match self {
            Next::Step { step } => vec![step.as_str()],
            Next::Branch { arms, otherwise } => arms
                .iter()
                .map(|a| a.step.as_str())
                .chain(otherwise.as_deref())
                .collect(),
            Next::Formula { .. } | Next::End => Vec::new(),
        }
    }
}

/// One arm of a [`Branch`](Next::Branch): a guard and where it goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchArm {
    /// The formula that must be true for this arm to be taken.
    pub when: String,
    /// The step it goes to.
    pub step: String,
}

impl BranchArm {
    /// An arm going to `step` when `when` is true.
    pub fn new(when: impl Into<String>, step: impl Into<String>) -> BranchArm {
        BranchArm {
            when: when.into(),
            step: step.into(),
        }
    }
}

/// What happens to a step that fails.
///
/// There is **always** a policy: a step's own overrides the workflow's, and the
/// workflow's default is [`Fail`](ErrorPolicy::Fail) — stop, and say why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ErrorPolicy {
    /// Try the step again, up to `max` times, waiting longer each time.
    /// Exhausting `max` falls through to the workflow's policy.
    Retry {
        /// How many attempts in total.
        max: u32,
        /// How long to wait between them.
        #[serde(default)]
        backoff: Backoff,
    },
    /// Jump to a named step, with the error in the context under
    /// [`ERROR_KEY`](crate::ERROR_KEY).
    Handler {
        /// The step to jump to.
        step: String,
    },
    /// Stop: the run is failed, with the reason recorded on it.
    #[default]
    Fail,
}

/// How long a [`Retry`](ErrorPolicy::Retry) waits between attempts: exponential,
/// capped, and jittered.
///
/// Jitter is on by default because retries without it synchronise — a hundred
/// runs that failed on the same outage retry at the same instant, which is the
/// outage's second wave.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backoff {
    /// The first wait, in milliseconds.
    pub initial_ms: u64,
    /// What each wait is multiplied by.
    pub factor: f64,
    /// The longest any wait may be.
    pub max_ms: u64,
    /// Whether to spread the waits out randomly.
    pub jitter: bool,
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff {
            initial_ms: 1_000,
            factor: 2.0,
            max_ms: 60_000,
            jitter: true,
        }
    }
}

/// One field a [`UserForm`](StepKind::UserForm) step asks a person for.
///
/// A declaration of its own rather than [`FormField`] directly, because a
/// `FormField` carries a `TypeRef` that may name a **rich type** — a trait
/// object, resolved against the running installation — and a workflow version is
/// a JSON document that has to mean the same thing on any node that reads it. So
/// what is stored is the basic type's name, and [`to_form_field`](FieldDecl::to_form_field)
/// lowers it into the same `FormField` every other settings form is rendered and
/// validated from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDecl {
    /// The key the answer is stored under.
    pub name: String,
    /// What the person is shown. Empty falls back to the name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// The basic type's name (`text`, `int`, `bool`, …).
    #[serde(rename = "type")]
    pub type_: String,
    /// Whether an answer is required.
    #[serde(default)]
    pub required: bool,
    /// The value used when none is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Json>,
    /// A fixed list of permitted values, which the form renders as a select.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<Json>>,
    /// Whether the answer is many lines rather than one.
    #[serde(default)]
    pub multiline: bool,
}

impl FieldDecl {
    /// A field of this name and type.
    pub fn new(name: impl Into<String>, type_: impl Into<String>) -> FieldDecl {
        FieldDecl {
            name: name.into(),
            label: String::new(),
            type_: type_.into(),
            required: false,
            default: None,
            options: None,
            multiline: false,
        }
    }

    /// Make an answer required.
    pub fn required(mut self) -> FieldDecl {
        self.required = true;
        self
    }

    /// Restrict the answers to a fixed list.
    pub fn options<I, V>(mut self, options: I) -> FieldDecl
    where
        I: IntoIterator<Item = V>,
        V: Into<Json>,
    {
        self.options = Some(options.into_iter().map(Into::into).collect());
        self
    }

    /// The [`FormField`] this declares — what the admin UI renders and what
    /// `validate_attrs` checks an answer against.
    ///
    /// A type name nothing recognises is an error rather than a silent `text`:
    /// a field declared as `intt` would accept anything, and the admin who typed
    /// it needs telling while they are still looking at the step.
    pub fn to_form_field(&self) -> Result<FormField> {
        let basic = basic_type(&self.type_).ok_or_else(|| {
            Error::invalid(format!(
                "field `{}`: `{}` is not a type a form can ask for",
                self.name, self.type_
            ))
        })?;
        let mut field = FormField::new(self.name.clone(), TypeRef::Basic(basic));
        if !self.label.is_empty() {
            field = field.label(self.label.clone());
        }
        if self.required {
            field = field.required();
        }
        if let Some(default) = &self.default {
            field.default = Some(default.clone());
        }
        if let Some(options) = &self.options {
            field.options_source = OptionsSource::Static(options.clone());
        }
        field.multiline = self.multiline;
        Ok(field)
    }
}

/// The basic type a declared type name means, or `None` for a name that is not
/// one.
///
/// The recognised set is deliberately smaller than [`BasicType`]: `bytes` and
/// `other` are not things a form asks a person for, so declaring one is a
/// mistake worth naming rather than a control nobody can fill in.
fn basic_type(name: &str) -> Option<BasicType> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "bool" | "boolean" => BasicType::Bool,
        "int" | "integer" => BasicType::Int,
        "float" => BasicType::Float,
        "decimal" => BasicType::Decimal,
        "text" | "string" => BasicType::Text,
        "json" => BasicType::Json,
        "uuid" => BasicType::Uuid,
        "date" => BasicType::Date,
        "time" => BasicType::Time,
        "timestamp" => BasicType::Timestamp,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A workflow using **every** step kind and every `Next` variant — the thing
    /// the serde round-trip has to survive, and the fixture the editor's own
    /// round-trip test mirrors.
    pub(crate) fn every_kind() -> Workflow {
        let mut workflow = Workflow::of(
            TriggerId::new(),
            3,
            vec![
                Step::new(
                    "fetch",
                    StepKind::Action {
                        action: "fetch".to_owned(),
                        configuration: [("url".to_owned(), json!("https://example.com"))]
                            .into_iter()
                            .collect(),
                    },
                )
                .description("ask the supplier")
                .then(Next::step("total"))
                .on_error(ErrorPolicy::Retry {
                    max: 3,
                    backoff: Backoff::default(),
                }),
                Step::new(
                    "total",
                    StepKind::Set {
                        assignments: vec![Assignment::new("total", "context.fetch.amount")],
                    },
                )
                .then(Next::Branch {
                    arms: vec![BranchArm::new("context.total > 100", "approve")],
                    otherwise: Some("lines".to_owned()),
                }),
                Step::new(
                    "approve",
                    StepKind::UserForm {
                        fields: vec![
                            FieldDecl::new("approved", "bool").required(),
                            FieldDecl::new("note", "text"),
                        ],
                        assign_to: "approval".to_owned(),
                        min_role: Some(Role::of(40)),
                        timeout: Some("86400000".to_owned()),
                    },
                )
                .then(Next::step("lines")),
                Step::new(
                    "lines",
                    StepKind::ForEach {
                        over: "context.fetch.lines".to_owned(),
                        var: "line".to_owned(),
                        body: "pause".to_owned(),
                    },
                )
                .then(Next::Formula {
                    formula: "context.total > 0 ? 'pause' : null".to_owned(),
                }),
                Step::new(
                    "pause",
                    StepKind::Wait {
                        until: "1000".to_owned(),
                    },
                ),
            ],
        );
        workflow.trace = true;
        workflow.error_policy = ErrorPolicy::Handler {
            step: "pause".to_owned(),
        };
        workflow
    }

    #[test]
    fn a_workflow_using_every_kind_round_trips_through_its_stored_json() {
        let workflow = every_kind();
        let json = serde_json::to_value(&workflow).unwrap();
        // The stored shape is the API shape: flat, named, and readable by
        // somebody who has never seen this file.
        assert_eq!(json["version"], json!(3));
        assert_eq!(json["steps"][0]["kind"]["type"], json!("action"));
        assert_eq!(
            json["steps"][0]["next"],
            json!({"type": "step", "step": "total"})
        );
        assert_eq!(json["steps"][1]["next"]["type"], json!("branch"));
        assert_eq!(json["steps"][2]["kind"]["fields"][0]["type"], json!("bool"));
        assert_eq!(
            json["error_policy"],
            json!({"type": "handler", "step": "pause"})
        );

        let back: Workflow = serde_json::from_value(json).unwrap();
        assert_eq!(back, workflow);
    }

    #[test]
    fn a_step_with_nothing_optional_set_stores_nothing_optional() {
        let step = Step::new(
            "only",
            StepKind::Set {
                assignments: vec![],
            },
        );
        let json = serde_json::to_value(&step).unwrap();
        // §9's sparse rule, applied to a document: what was not said is not
        // written, so a diff between two versions shows what changed.
        assert_eq!(
            json,
            json!({
                "name": "only",
                "kind": {"type": "set", "assignments": []},
                "next": {"type": "end"},
            })
        );
    }

    #[test]
    fn an_unknown_field_in_a_stored_step_is_refused_rather_than_ignored() {
        // A step carrying `nxt` instead of `next` would otherwise be read as one
        // that ends the run — the workflow would be *stored* correctly and *run*
        // wrongly, which is the worst of the two failures.
        let err = serde_json::from_value::<Step>(json!({
            "name": "a",
            "kind": {"type": "set", "assignments": []},
            "nxt": {"type": "end"},
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("nxt"), "{err}");
    }

    #[test]
    fn defaults_are_what_a_workflow_that_says_nothing_gets() {
        let workflow: Workflow = serde_json::from_value(json!({
            "id": uuid::Uuid::nil(),
            "version": 1,
            "start": "a",
            "steps": [],
        }))
        .unwrap();
        assert_eq!(workflow.error_policy, ErrorPolicy::Fail);
        assert_eq!(workflow.max_steps, DEFAULT_MAX_STEPS);
        assert!(!workflow.trace);
    }

    #[test]
    fn a_new_workflow_opens_on_a_canvas_rather_than_on_an_error() {
        let workflow = Workflow::empty(TriggerId::new());
        assert_eq!(workflow.version, 1);
        // The start step exists, which is the rule a fresh workflow would
        // otherwise break the moment it was validated.
        assert!(workflow.step(&workflow.start).is_some());
        assert_eq!(workflow.steps.len(), 1);
    }

    #[test]
    fn a_next_names_the_steps_it_can_reach_and_a_formula_names_none() {
        assert_eq!(Next::step("b").targets(), ["b"]);
        assert_eq!(
            Next::Branch {
                arms: vec![BranchArm::new("x", "b"), BranchArm::new("y", "c")],
                otherwise: Some("d".to_owned()),
            }
            .targets(),
            ["b", "c", "d"]
        );
        assert!(Next::End.targets().is_empty());
        assert!(
            Next::Formula {
                formula: "'b'".to_owned()
            }
            .targets()
            .is_empty(),
            "which step a formula reaches is the run's answer, not the editor's"
        );
    }

    #[test]
    fn a_step_falls_back_to_the_workflows_policy_but_its_own_wins() {
        let workflow = every_kind();
        // `fetch` retries; the workflow as a whole jumps to a handler.
        let fetch = workflow.step("fetch").unwrap();
        assert!(matches!(fetch.policy(&workflow), ErrorPolicy::Retry { .. }));
        let total = workflow.step("total").unwrap();
        assert!(matches!(
            total.policy(&workflow),
            ErrorPolicy::Handler { .. }
        ));
    }

    #[test]
    fn a_declared_field_lowers_to_the_form_field_everything_else_renders() {
        let field = FieldDecl::new("colour", "text")
            .required()
            .options(["red", "green"])
            .to_form_field()
            .unwrap();
        assert_eq!(field.base.name, "colour");
        assert!(field.required);
        assert_eq!(field.static_options(), [json!("red"), json!("green")]);

        // A type nothing recognises is named, not silently read as text.
        let err = FieldDecl::new("n", "intt").to_form_field().unwrap_err();
        assert!(err.to_string().contains("not a type"), "{err}");
        // Nor is a type a form cannot ask for.
        assert!(FieldDecl::new("blob", "bytes").to_form_field().is_err());
    }
}
