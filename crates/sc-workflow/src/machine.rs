//! The run, as a **steppable machine** rather than an `async fn` (§10.3,
//! decision 4).
//!
//! [`WorkflowRun`] owns every *decision* a run makes — which step is next, what
//! goes in the context, when to give up on a failing step, when the run is over —
//! and performs no IO at all. A driver advances it by asking
//! [`next_step`](WorkflowRun::next_step) what to do, doing it, and feeding the
//! outcome back:
//!
//! ```text
//! next_step() ──▶ RunAction { action, configuration } ──▶ step_succeeded(value)
//!                                                    └──▶ step_failed(error)
//!             ──▶ Evaluate  { formulas }             ──▶ evaluated(values)
//!             ──▶ Suspend   { until, awaiting_input } ─▶ (the clock, or resumed(input))
//!             ──▶ Done      { context }
//!             ──▶ Failed    { step, error }
//! ```
//!
//! ## Why this shape
//!
//! Because the whole state is a value, it is `Serialize + Deserialize`, and *the
//! state is what `_fd_runs.context` stores*. A run written after every step is
//! therefore a run that resumes: load it, ask for the next step, carry on. The
//! agent loop is the same shape for the same reason (§11.2, decision 8), and the
//! two share the runs table because they are the same problem.
//!
//! It also makes the engine testable synchronously — with no database, no
//! runtime, and **no clock**: `now` is a parameter of every method that could
//! want one, so a test that exercises a retry's backoff or a day-long `Wait`
//! moves the clock instead of sleeping.
//!
//! ## What it deliberately does not hold
//!
//! The [`Workflow`] itself. A run is *pinned* to the version it started on
//! (decision 2) and the driver loads that version from `_fd_workflow_versions`,
//! so every method that needs the program takes it as an argument. Copying the
//! steps into the run row would store the same document twice and invite the two
//! to disagree.
//!
//! Nor the JavaScript evaluator: a `Set`'s value, a `Branch`'s guard, a
//! `ForEach`'s collection and a `Wait`'s deadline are all *formulas*, and the
//! machine asks for them to be evaluated ([`Decision::Evaluate`]) rather than
//! evaluating them. That is what keeps it sans-IO, and it is why one enum covers
//! all four.
//!
//! ## Two rules worth reading before the code
//!
//! **A step failing and a workflow being wrong are different.** A step that
//! returns an error goes through the error policy — retried, handled, or fatal
//! (§2.4). A `Next` that names a step which does not exist, or a `Set` step whose
//! stored kind is not a `Set`, is the *program* being broken: validation should
//! have caught it, retrying it would never help, and a handler step written for
//! business failures is not an answer to it. Those end the run directly, naming
//! what is inconsistent.
//!
//! **The end of a `ForEach` body is the end of an iteration, not of the run.**
//! There is one flat list of steps and one frame stack, so `Next::End` inside a
//! loop means "come back for the next item" and `Next::End` outside one means
//! "the run is over". Which it is, is a question the stack answers.

use sc_types::Role;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Duration, Utc};
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::ERROR_KEY;
use crate::workflow::{Backoff, ErrorPolicy, FieldDecl, Next, StepKind, Workflow};

/// What the driver must do next to advance a [`WorkflowRun`].
///
/// Every variant is something only the outside world can do — call an action,
/// run the JavaScript evaluator, watch a clock, talk to a person — or a report
/// that there is nothing left to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Run this registered action with this configuration, and feed the result
    /// back through [`step_succeeded`](WorkflowRun::step_succeeded) or
    /// [`step_failed`](WorkflowRun::step_failed).
    ///
    /// The configuration goes through **unevaluated**: an action reads the run
    /// context through `ActionContext::context` and its own settings are its own
    /// business, which is why every action there has ever been is already a
    /// workflow step (decision 7).
    RunAction {
        /// The step being run — the key its result is stored under.
        step: String,
        /// The registered action's name.
        action: String,
        /// Its configuration.
        configuration: Attrs,
    },
    /// Evaluate these formulas, in this order, in the run's scope, and feed the
    /// values back through [`evaluated`](WorkflowRun::evaluated).
    ///
    /// One decision for four jobs — a `Set`'s value, a `ForEach`'s collection, a
    /// `Wait`'s deadline, a `Branch`'s guards — because the driver's part in all
    /// four is identical and which one it is, is the machine's business.
    Evaluate {
        /// The step whose formulas these are.
        step: String,
        /// The sources, in order. Exactly this many values must come back.
        formulas: Vec<String>,
    },
    /// Stop, durably. The run leaves the queue's reach until `until` comes round
    /// or somebody fills in `awaiting_input`.
    ///
    /// Both fields together rather than two variants, because a `UserForm` with
    /// a timeout is genuinely both: a person may answer it, and the clock may
    /// give up on them.
    Suspend {
        /// The step that suspended.
        step: String,
        /// When the run next wants the engine — `None` for one only a person can
        /// wake, which is what `_fd_runs.wake_at` being NULL means.
        until: Option<DateTime<Utc>>,
        /// The form somebody must fill in, for a `UserForm` suspension.
        awaiting_input: Option<PendingForm>,
    },
    /// The run reached the end. This is the context it finished with.
    Done {
        /// Everything the run accumulated.
        context: Attrs,
    },
    /// The run stopped without finishing, and this is why.
    Failed {
        /// The step it stopped at.
        step: String,
        /// What went wrong, as the admin will read it.
        error: String,
    },
}

/// The form a suspended run is waiting for somebody to fill in.
///
/// Carried on the run rather than looked up from the workflow when somebody asks,
/// because the run is pinned to a version and the form a person is *looking at*
/// must not change under them when the workflow is edited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingForm {
    /// What to ask for — lowered to `FormField`s by
    /// [`FieldDecl::to_form_field`] for rendering and validation.
    pub fields: Vec<FieldDecl>,
    /// The context key the answers are merged in under.
    pub assign_to: String,
    /// The role floor for answering; `None` is admin-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_role: Option<Role>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "conclusion", rename_all = "snake_case")]
pub enum Conclusion {
    /// It ran to the end of a path.
    Finished,
    /// It stopped at a step, for this reason: a failure the error policy did not
    /// recover from, a step budget that ran out, or a workflow that turned out to
    /// be inconsistent.
    Failed {
        /// Where it stopped.
        step: String,
        /// Why.
        error: String,
    },
}

/// One `ForEach` in flight: its collection, and how far through it the run is.
///
/// A stack rather than a field on the run, because loops nest: a `ForEach` over
/// orders whose body contains a `ForEach` over that order's lines has two of
/// these, and the inner one finishing must return to the outer one rather than to
/// the top of the workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Frame {
    /// The `ForEach` step this frame belongs to. Re-entering that step with this
    /// frame on top is what "another iteration" means.
    step: String,
    /// The context key the current item is bound under.
    var: String,
    /// The collection, as it was when the loop started. Snapshotted deliberately:
    /// a loop whose collection is re-evaluated every iteration is a loop whose
    /// length can change under it.
    items: Vec<Json>,
    /// Which item is running.
    index: usize,
}

/// What the run is waiting for the evaluator to work out.
///
/// The details — which assignments, which arms — are read back from the pinned
/// workflow rather than copied in here, so the state stays a cursor into the
/// program instead of a second copy of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "want", rename_all = "snake_case")]
enum Want {
    /// One `Set` assignment's value. One at a time, and in order, because a later
    /// assignment may read what an earlier one wrote — which a single batch
    /// could not offer.
    Set {
        /// Which assignment.
        index: u32,
    },
    /// A `ForEach`'s collection.
    Collection,
    /// A `Wait`'s deadline.
    Deadline,
    /// A `UserForm`'s timeout.
    Timeout,
    /// A `Branch`'s arms' guards, in order.
    Branch,
    /// A `Next::Formula`'s answer: the name of the next step.
    NextStep,
}

/// What a timed suspension means when its deadline arrives.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Wake {
    /// The step is finished; carry on to whatever is next. A `Wait`'s deadline.
    Continue,
    /// Run the same step again. A retry's backoff.
    Retry,
    /// Nobody answered in time: the step failed, and its error policy decides
    /// whether that is fatal or a branch. A `UserForm`'s timeout.
    Timeout,
}

/// Which side the machine is waiting on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    /// About to run this step. Internal: [`next_step`](WorkflowRun::next_step)
    /// never leaves the run here, because entering a step is the machine's own
    /// work.
    Enter { step: String },
    /// This step's action is the driver's to run.
    Running { step: String },
    /// This step needs formulas evaluated.
    Evaluating { step: String, want: Want },
    /// This step is done and its `Next` is the machine's to resolve. Internal,
    /// like `Enter`.
    Resolving { step: String },
    /// Suspended.
    Suspended {
        step: String,
        until: Option<DateTime<Utc>>,
        form: Option<PendingForm>,
        wake: Wake,
    },
    /// Over.
    Done { conclusion: Conclusion },
}

/// Everything one run of a workflow is: the context it has accumulated, the
/// loops it is inside, where it has got to, and what it has spent.
///
/// `Serialize + Deserialize`, and that is not incidental — this value *is*
/// `_fd_runs.context` for a workflow run, exactly as `sc-agent`'s `AgentLoop` is
/// for an agent one. Resuming is a deserialise, not a reconstruction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRun {
    /// What the steps have accumulated: an action's result under its step's name,
    /// a `Set`'s targets, a `ForEach`'s loop variable, a `UserForm`'s answers,
    /// and [`ERROR_KEY`] when a handler was jumped to.
    context: Attrs,
    /// The `ForEach`es this run is inside, outermost first.
    frames: Vec<Frame>,
    /// Which side is next.
    phase: Phase,
    /// How many attempts at the **current** step have been started: 1 on the
    /// first, 2 after one retry. Reset when the run moves to a different step,
    /// which is why "retried three times" counts what it says.
    attempt: u32,
    /// How many steps this run has entered, against [`max_steps`](Self::max_steps).
    steps_taken: u32,
    /// The budget, copied from the workflow when the run started so the number
    /// that stopped a run is the number the run was started with.
    max_steps: u32,
    /// How many trace rows this run has written — the `seq` of the next one.
    trace_seq: u32,
}

impl WorkflowRun {
    /// A run of `workflow`, at its start step with an empty context.
    pub fn new(workflow: &Workflow) -> WorkflowRun {
        WorkflowRun {
            context: Attrs::new(),
            frames: Vec::new(),
            phase: Phase::Enter {
                step: workflow.start.clone(),
            },
            attempt: 0,
            steps_taken: 0,
            max_steps: workflow.max_steps.max(1),
            trace_seq: 0,
        }
    }

    /// Seed the context — what the caller of a workflow passes in, before any
    /// step has run.
    pub fn with_context(mut self, context: Attrs) -> WorkflowRun {
        self.context = context;
        self
    }

    /// What the driver must do next.
    ///
    /// **Idempotent**: asking twice without answering gives the same instruction
    /// twice, which is what makes a resumed run resumable — a node that died
    /// between being told to run an action and running it comes back and is told
    /// again. That is decision 6's "at least once", and it is why steps should be
    /// idempotent.
    ///
    /// `now` is how a suspension ends: a run whose deadline has passed is woken
    /// here rather than by a separate call, so the queue handing back a run early
    /// is answered with the same `Suspend` rather than with a step that should
    /// not have run.
    pub fn next_step(&mut self, workflow: &Workflow, now: DateTime<Utc>) -> Decision {
        loop {
            match self.phase.clone() {
                Phase::Done { conclusion } => {
                    return match conclusion {
                        Conclusion::Finished => Decision::Done {
                            context: self.context.clone(),
                        },
                        Conclusion::Failed { step, error } => Decision::Failed { step, error },
                    };
                }
                Phase::Running { step } => {
                    let found = match workflow.require_step(&step) {
                        Ok(found) => found,
                        Err(e) => return self.broken(&step, e.to_string()),
                    };
                    let StepKind::Action {
                        action,
                        configuration,
                    } = &found.kind
                    else {
                        return self.broken(
                            &step,
                            format!(
                                "step `{step}` was started as an action and is stored as a {}",
                                found.kind.type_name()
                            ),
                        );
                    };
                    return Decision::RunAction {
                        step: step.clone(),
                        action: action.clone(),
                        configuration: configuration.clone(),
                    };
                }
                Phase::Evaluating { step, want } => {
                    return match self.formulas(workflow, &step, &want) {
                        Ok(formulas) => Decision::Evaluate { step, formulas },
                        Err(e) => self.broken(&step, e.to_string()),
                    };
                }
                Phase::Suspended {
                    step,
                    until,
                    form,
                    wake,
                } => match until {
                    Some(deadline) if now >= deadline => match wake {
                        Wake::Continue => self.phase = Phase::Resolving { step },
                        Wake::Retry => self.phase = Phase::Enter { step },
                        Wake::Timeout => {
                            let error = format!("nobody answered step `{step}` before its timeout");
                            self.apply_failure(workflow, &step, &error, now);
                        }
                    },
                    _ => {
                        return Decision::Suspend {
                            step,
                            until,
                            awaiting_input: form,
                        };
                    }
                },
                Phase::Enter { step } => self.enter(workflow, &step),
                Phase::Resolving { step } => self.resolve(workflow, &step),
            }
        }
    }

    /// Feed back what an action returned.
    ///
    /// The value is stored in the context **under the step's name**, which is how
    /// a later formula reads it (`context.fetch.amount`) and why renaming a step
    /// is a real edit rather than a relabelling.
    pub fn step_succeeded(&mut self, value: Json) -> Result<()> {
        let Phase::Running { step } = self.phase.clone() else {
            return Err(Error::msg(
                "the workflow run was given an action result it did not ask for",
            ));
        };
        self.context.insert(step.clone(), value);
        self.phase = Phase::Resolving { step };
        Ok(())
    }

    /// Take back the context an action was handed, with whatever it wrote into
    /// it.
    ///
    /// An action reads and writes `ActionContext::context` (§10.1 left the seam
    /// for exactly this), and the driver hands it a copy of the run's. This is
    /// how the copy comes home — called **before**
    /// [`step_succeeded`](WorkflowRun::step_succeeded), so a step that writes
    /// `context.total` *and* returns a value gets both, with the return value
    /// under the step's name winning any collision.
    ///
    /// Strict about the phase, like every other piece of feedback: a context
    /// handed back by something that was not running a step is a driver bug, and
    /// silently accepting it would let one run's context be written by another's
    /// action.
    pub fn context_written(&mut self, context: Attrs) -> Result<()> {
        if !matches!(self.phase, Phase::Running { .. }) {
            return Err(Error::msg(
                "the workflow run was given a context it did not hand out",
            ));
        }
        self.context = context;
        Ok(())
    }

    /// Feed back that the current step failed, and let its error policy decide
    /// what that means (§2.4).
    ///
    /// The same path for an action that returned an error and for a formula that
    /// would not evaluate: both are the step not doing its job, and giving them
    /// two policies would mean an admin configuring retries twice.
    pub fn step_failed(
        &mut self,
        workflow: &Workflow,
        error: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let step = match self.phase.clone() {
            Phase::Running { step }
            | Phase::Evaluating { step, .. }
            | Phase::Suspended { step, .. }
            | Phase::Enter { step }
            | Phase::Resolving { step } => step,
            Phase::Done { .. } => {
                return Err(Error::msg(
                    "the workflow run was told a step failed after it had finished",
                ));
            }
        };
        self.apply_failure(workflow, &step, error, now);
        Ok(())
    }

    /// Feed back the values of the formulas [`Decision::Evaluate`] asked for, in
    /// the order it asked for them.
    ///
    /// The count must match. A value missing or spare would be silently assigned
    /// to the wrong target or read as the wrong guard, which is a run that does
    /// the wrong thing rather than one that stops — the worse of the two.
    pub fn evaluated(
        &mut self,
        workflow: &Workflow,
        values: Vec<Json>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let Phase::Evaluating { step, want } = self.phase.clone() else {
            return Err(Error::msg(
                "the workflow run was given formula values it did not ask for",
            ));
        };
        let asked = self.formulas(workflow, &step, &want)?.len();
        if values.len() != asked {
            return Err(Error::msg(format!(
                "step `{step}` asked for {asked} formula values and got {}",
                values.len()
            )));
        }
        match want {
            Want::Set { index } => self.assigned(workflow, &step, index, values),
            Want::Collection => self.collected(workflow, &step, first(values), now),
            Want::Deadline => match deadline(&first(values), now) {
                Ok(until) => {
                    self.phase = Phase::Suspended {
                        step,
                        until: Some(until),
                        form: None,
                        wake: Wake::Continue,
                    };
                }
                Err(e) => self.apply_failure(workflow, &step, &e.to_string(), now),
            },
            Want::Timeout => self.timed_form(workflow, &step, first(values), now),
            Want::Branch => self.branched(workflow, &step, values),
            Want::NextStep => self.computed_next(workflow, &step, first(values)),
        }
        Ok(())
    }

    /// Feed back the answers a person gave a `UserForm` step.
    ///
    /// They are merged into the context under the step's `assign_to`, whole: a
    /// form's answers belong together, and scattering them across the top level
    /// would let two forms with a `note` field overwrite each other.
    ///
    /// Validating them against the declaration is the API layer's (§4.2) — the
    /// machine is fed values that have already been checked, exactly as it is fed
    /// an action's result.
    pub fn resumed(&mut self, input: Attrs) -> Result<()> {
        let Phase::Suspended {
            step,
            form: Some(form),
            ..
        } = self.phase.clone()
        else {
            return Err(Error::invalid(
                "this run is not waiting for anybody to fill in a form",
            ));
        };
        self.context.insert(form.assign_to, Json::Object(input));
        self.phase = Phase::Resolving { step };
        Ok(())
    }

    /// Everything the run has accumulated.
    pub fn context(&self) -> &Attrs {
        &self.context
    }

    /// The step the run is at, or `None` once it is over.
    pub fn current_step(&self) -> Option<&str> {
        match &self.phase {
            Phase::Enter { step }
            | Phase::Running { step }
            | Phase::Evaluating { step, .. }
            | Phase::Resolving { step }
            | Phase::Suspended { step, .. } => Some(step.as_str()),
            Phase::Done { .. } => None,
        }
    }

    /// Which attempt at the current step is in flight: 1 on the first, 2 after
    /// one retry. What a trace row records, so the driver reads it **before**
    /// feeding an outcome back.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// How many steps the run has entered, against its budget.
    pub fn steps_taken(&self) -> u32 {
        self.steps_taken
    }

    /// The budget this run was started with.
    pub fn max_steps(&self) -> u32 {
        self.max_steps
    }

    /// How deep in `ForEach`es the run is.
    pub fn loop_depth(&self) -> usize {
        self.frames.len()
    }

    /// The form a suspended run is waiting on, if it is waiting on one.
    pub fn pending_form(&self) -> Option<&PendingForm> {
        match &self.phase {
            Phase::Suspended { form, .. } => form.as_ref(),
            _ => None,
        }
    }

    /// When the run next wants the engine: a `Wait`'s end, a retry's deadline, a
    /// form's timeout — and `None` both for a run waiting only on a person and
    /// for one that is runnable right now.
    ///
    /// What `_fd_runs.wake_at` is written from; the difference between the two
    /// `None`s is the run's state, which the driver sets — `waiting` for a
    /// suspension, `running` for a run the queue may pick up now.
    pub fn wake_at(&self) -> Option<DateTime<Utc>> {
        match &self.phase {
            Phase::Suspended { until, .. } => *until,
            _ => None,
        }
    }

    /// Whether the run is still waiting on the **evaluator** for the step it is
    /// on — a `Set` with another assignment to compute, a `Branch` whose guards
    /// have not been asked yet.
    ///
    /// The driver's test for "this step is not finished with me": one advance
    /// covers one step, and a `Set` of three assignments is three evaluations of
    /// one step, not three steps.
    pub fn is_evaluating(&self) -> bool {
        matches!(self.phase, Phase::Evaluating { .. })
    }

    /// Whether the run is suspended — durably stopped, and not runnable until its
    /// deadline or a person.
    pub fn is_suspended(&self) -> bool {
        matches!(self.phase, Phase::Suspended { .. })
    }

    /// Whether the run is over.
    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done { .. })
    }

    /// How it ended, if it has.
    pub fn conclusion(&self) -> Option<&Conclusion> {
        match &self.phase {
            Phase::Done { conclusion } => Some(conclusion),
            _ => None,
        }
    }

    /// Start again at the step this run stopped at, as a **fresh attempt**
    /// (§10.3, phase 4.4).
    ///
    /// The one thing an admin can do to a failed run other than read it: the
    /// outage that failed the step is over, and what the run should do is carry
    /// on from where it stopped rather than start again from the beginning — the
    /// steps before it already had their effects, and re-running them is exactly
    /// what "at least once" is trying not to do more of.
    ///
    /// The attempt count goes back to zero, so a step whose policy allows three
    /// tries gets three again. The **budget does not**: a run that stopped
    /// because it ran out of steps would only run out again, so it is refused by
    /// name here instead of being restarted into the same wall.
    pub fn retry(&mut self, workflow: &Workflow) -> Result<()> {
        let Phase::Done {
            conclusion: Conclusion::Failed { step, .. },
        } = self.phase.clone()
        else {
            return Err(Error::invalid(
                "only a run that stopped at a step can be retried; this one did not fail",
            ));
        };
        if self.steps_taken >= self.max_steps {
            return Err(Error::invalid(format!(
                "this run stopped at its budget of {} steps, so retrying it would stop again \
                 at once; raise the workflow's `max_steps` and start a new run",
                self.max_steps
            )));
        }
        // The run is pinned to a version, so a step that is not in it will not
        // appear later: retrying into it would fail identically and tell the
        // admin nothing they do not already know.
        workflow.require_step(&step)?;
        self.attempt = 0;
        self.phase = Phase::Enter { step };
        Ok(())
    }

    /// The `seq` the next trace row gets, consuming it.
    ///
    /// The counter lives in the run state rather than in the driver because it
    /// has to survive a restart: a recovered run whose trace restarted at 1 would
    /// collide with the rows it already wrote.
    pub fn next_trace_seq(&mut self) -> u32 {
        self.trace_seq += 1;
        self.trace_seq
    }

    // ---- the machine's own work ------------------------------------------

    /// The formulas the current phase needs evaluated. Recomputed from the
    /// pinned workflow on every ask, which is what makes `next_step` idempotent.
    fn formulas(&self, workflow: &Workflow, step: &str, want: &Want) -> Result<Vec<String>> {
        let found = workflow.require_step(step)?;
        let mismatch = |expected: &str| {
            Error::invalid(format!(
                "step `{step}` is evaluating {expected} and is stored as a {}",
                found.kind.type_name()
            ))
        };
        Ok(match want {
            Want::Set { index } => {
                let StepKind::Set { assignments } = &found.kind else {
                    return Err(mismatch("an assignment"));
                };
                let assignment = assignments.get(*index as usize).ok_or_else(|| {
                    Error::invalid(format!(
                        "step `{step}` has no assignment {index}; it has {}",
                        assignments.len()
                    ))
                })?;
                vec![assignment.formula.clone()]
            }
            Want::Collection => {
                let StepKind::ForEach { over, .. } = &found.kind else {
                    return Err(mismatch("a collection"));
                };
                vec![over.clone()]
            }
            Want::Deadline => {
                let StepKind::Wait { until } = &found.kind else {
                    return Err(mismatch("a deadline"));
                };
                vec![until.clone()]
            }
            Want::Timeout => {
                let StepKind::UserForm {
                    timeout: Some(timeout),
                    ..
                } = &found.kind
                else {
                    return Err(mismatch("a form timeout"));
                };
                vec![timeout.clone()]
            }
            Want::Branch => {
                let Next::Branch { arms, .. } = &found.next else {
                    return Err(Error::invalid(format!(
                        "step `{step}` is choosing a branch arm and its `next` is not a branch"
                    )));
                };
                arms.iter().map(|arm| arm.when.clone()).collect()
            }
            Want::NextStep => {
                let Next::Formula { formula } = &found.next else {
                    return Err(Error::invalid(format!(
                        "step `{step}` is computing its next step and its `next` is not a formula"
                    )));
                };
                vec![formula.clone()]
            }
        })
    }

    /// Begin a step: spend one of the budget, and work out what the step needs
    /// from the outside world.
    fn enter(&mut self, workflow: &Workflow, name: &str) {
        if self.steps_taken >= self.max_steps {
            self.finish_failed(
                name,
                format!(
                    "the run stopped at step `{name}` after {} steps, which is its workflow's \
                     budget; a workflow that needs more says so, and one that does not has a \
                     loop with no exit",
                    self.max_steps
                ),
            );
            return;
        }
        self.steps_taken += 1;
        self.attempt += 1;
        let found = match workflow.require_step(name) {
            Ok(found) => found,
            Err(e) => return self.finish_failed(name, e.to_string()),
        };
        match &found.kind {
            StepKind::Action { .. } => {
                self.phase = Phase::Running {
                    step: name.to_owned(),
                };
            }
            StepKind::Set { assignments } => {
                // Nothing to evaluate is not a round trip: the empty `Set` is the
                // step a brand-new workflow opens with, and asking the evaluator
                // to run no formulas would be a write and a wake-up for nothing.
                self.phase = if assignments.is_empty() {
                    Phase::Resolving {
                        step: name.to_owned(),
                    }
                } else {
                    Phase::Evaluating {
                        step: name.to_owned(),
                        want: Want::Set { index: 0 },
                    }
                };
            }
            StepKind::ForEach { .. } => {
                // A frame of our own on top means this is the *next* iteration,
                // not the start of the loop — so the collection is not evaluated
                // again and the cursor moves on.
                if self.frames.last().is_some_and(|f| f.step == name) {
                    if let Some(frame) = self.frames.last_mut() {
                        frame.index += 1;
                    }
                    self.advance_loop(workflow, name);
                } else {
                    self.phase = Phase::Evaluating {
                        step: name.to_owned(),
                        want: Want::Collection,
                    };
                }
            }
            StepKind::Wait { .. } => {
                self.phase = Phase::Evaluating {
                    step: name.to_owned(),
                    want: Want::Deadline,
                };
            }
            StepKind::UserForm {
                fields,
                assign_to,
                min_role,
                timeout,
            } => {
                self.phase = if timeout.is_some() {
                    Phase::Evaluating {
                        step: name.to_owned(),
                        want: Want::Timeout,
                    }
                } else {
                    Phase::Suspended {
                        step: name.to_owned(),
                        until: None,
                        form: Some(PendingForm {
                            fields: fields.clone(),
                            assign_to: assign_to.clone(),
                            min_role: *min_role,
                        }),
                        wake: Wake::Timeout,
                    }
                };
            }
        }
    }

    /// A step is finished: work out what runs after it.
    fn resolve(&mut self, workflow: &Workflow, name: &str) {
        let found = match workflow.require_step(name) {
            Ok(found) => found,
            Err(e) => return self.finish_failed(name, e.to_string()),
        };
        match found.next.clone() {
            Next::Step { step } => self.goto(step),
            Next::End => self.finish_or_loop(),
            Next::Branch { arms, otherwise } => {
                if arms.is_empty() {
                    match otherwise {
                        Some(step) => self.goto(step),
                        None => self.finish_or_loop(),
                    }
                } else {
                    self.phase = Phase::Evaluating {
                        step: name.to_owned(),
                        want: Want::Branch,
                    };
                }
            }
            Next::Formula { .. } => {
                self.phase = Phase::Evaluating {
                    step: name.to_owned(),
                    want: Want::NextStep,
                };
            }
        }
    }

    /// Bind the current item and run the body, or leave the loop.
    fn advance_loop(&mut self, workflow: &Workflow, name: &str) {
        let Some(frame) = self.frames.last() else {
            return self.finish_failed(name, format!("step `{name}` lost track of its loop"));
        };
        if frame.index >= frame.items.len() {
            self.frames.pop();
            self.phase = Phase::Resolving {
                step: name.to_owned(),
            };
            return;
        }
        let item = frame.items[frame.index].clone();
        let var = frame.var.clone();
        self.context.insert(var, item);
        let body = match workflow.require_step(name) {
            Ok(found) => match &found.kind {
                StepKind::ForEach { body, .. } => body.clone(),
                other => {
                    return self.finish_failed(
                        name,
                        format!(
                            "step `{name}` is looping and is stored as a {}",
                            other.type_name()
                        ),
                    );
                }
            },
            Err(e) => return self.finish_failed(name, e.to_string()),
        };
        self.goto(body);
    }

    /// One `Set` assignment came back: write it, and either ask for the next or
    /// move on.
    fn assigned(&mut self, workflow: &Workflow, step: &str, index: u32, values: Vec<Json>) {
        let count = match workflow.step(step).map(|s| &s.kind) {
            Some(StepKind::Set { assignments }) => {
                match assignments.get(index as usize) {
                    Some(assignment) => {
                        self.context
                            .insert(assignment.target.clone(), first(values));
                    }
                    None => {
                        return self.finish_failed(
                            step,
                            format!("step `{step}` has no assignment {index}"),
                        );
                    }
                }
                assignments.len()
            }
            _ => {
                return self.finish_failed(step, format!("step `{step}` is not a set step"));
            }
        };
        let next = index + 1;
        self.phase = if (next as usize) < count {
            Phase::Evaluating {
                step: step.to_owned(),
                want: Want::Set { index: next },
            }
        } else {
            Phase::Resolving {
                step: step.to_owned(),
            }
        };
    }

    /// A `ForEach`'s collection came back: open the loop.
    fn collected(&mut self, workflow: &Workflow, step: &str, value: Json, now: DateTime<Utc>) {
        let var = match workflow.step(step).map(|s| &s.kind) {
            Some(StepKind::ForEach { var, .. }) => var.clone(),
            _ => return self.finish_failed(step, format!("step `{step}` is not a loop")),
        };
        let Json::Array(items) = value else {
            // Not an empty loop: a formula that reads null because it names
            // something the run never wrote is a mistake, and running the body
            // zero times would hide it until somebody wondered why nothing
            // happened. An empty *list* is the way to say "nothing to do".
            let error = format!(
                "step `{step}` loops over {}, which is not a list",
                describe(&value)
            );
            return self.apply_failure(workflow, step, &error, now);
        };
        self.frames.push(Frame {
            step: step.to_owned(),
            var,
            items,
            index: 0,
        });
        self.advance_loop(workflow, step);
    }

    /// A `UserForm`'s timeout came back: suspend for a person, with a deadline.
    fn timed_form(&mut self, workflow: &Workflow, step: &str, value: Json, now: DateTime<Utc>) {
        let form = match workflow.step(step).map(|s| &s.kind) {
            Some(StepKind::UserForm {
                fields,
                assign_to,
                min_role,
                ..
            }) => PendingForm {
                fields: fields.clone(),
                assign_to: assign_to.clone(),
                min_role: *min_role,
            },
            _ => return self.finish_failed(step, format!("step `{step}` is not a form")),
        };
        // A timeout formula that answers null means "no timeout after all" —
        // which is how `context.urgent ? 3600000 : null` is written.
        let until = match value {
            Json::Null => None,
            value => match deadline(&value, now) {
                Ok(until) => Some(until),
                Err(e) => return self.apply_failure(workflow, step, &e.to_string(), now),
            },
        };
        self.phase = Phase::Suspended {
            step: step.to_owned(),
            until,
            form: Some(form),
            wake: Wake::Timeout,
        };
    }

    /// A `Branch`'s guards came back: take the first arm that is true.
    fn branched(&mut self, workflow: &Workflow, step: &str, values: Vec<Json>) {
        let Some(Next::Branch { arms, otherwise }) = workflow.step(step).map(|s| s.next.clone())
        else {
            return self.finish_failed(step, format!("step `{step}` does not branch"));
        };
        let taken = arms
            .iter()
            .zip(values)
            .find(|(_, value)| truthy(value))
            .map(|(arm, _)| arm.step.clone());
        match taken.or(otherwise) {
            Some(step) => self.goto(step),
            None => self.finish_or_loop(),
        }
    }

    /// A `Next::Formula` came back: go where it says.
    fn computed_next(&mut self, workflow: &Workflow, step: &str, value: Json) {
        match value {
            // Null is how a computed `next` says "the run ends here" — the
            // reading v1's `next_step` already had.
            Json::Null => self.finish_or_loop(),
            Json::String(name) if workflow.step(&name).is_some() => self.goto(name),
            Json::String(name) => self.finish_failed(
                step,
                format!(
                    "step `{step}` computed `{name}` as the step to run next, and the workflow \
                     has no step of that name"
                ),
            ),
            other => self.finish_failed(
                step,
                format!(
                    "step `{step}` computed {} as the step to run next, which is not a step name",
                    describe(&other)
                ),
            ),
        }
    }

    /// Move to a step, as a fresh attempt.
    fn goto(&mut self, step: String) {
        self.attempt = 0;
        self.phase = Phase::Enter { step };
    }

    /// Nothing follows this step: another iteration if the run is inside a loop,
    /// otherwise the end.
    fn finish_or_loop(&mut self) {
        match self.frames.last() {
            Some(frame) => {
                let step = frame.step.clone();
                self.goto(step);
            }
            None => {
                self.phase = Phase::Done {
                    conclusion: Conclusion::Finished,
                };
            }
        }
    }

    /// A step failed: apply the policy that governs it (§2.4).
    fn apply_failure(&mut self, workflow: &Workflow, step: &str, error: &str, now: DateTime<Utc>) {
        let own = workflow.step(step).and_then(|s| s.error_policy.clone());
        let policy = own.clone().unwrap_or_else(|| workflow.error_policy.clone());
        if let ErrorPolicy::Retry { max, backoff } = &policy {
            if self.attempt < *max {
                let wait = retry_delay(backoff, self.attempt, step, now);
                self.phase = Phase::Suspended {
                    step: step.to_owned(),
                    until: Some(now + wait),
                    form: None,
                    wake: Wake::Retry,
                };
                return;
            }
            // Exhausted. The fallback is the *workflow's* policy — but only when
            // that is not where this policy came from, because a policy falling
            // through to itself would retry forever, and a fallback that is
            // itself a retry would start the count again. Both read as `Fail`.
            let fallback = if own.is_some() {
                workflow.error_policy.clone()
            } else {
                ErrorPolicy::Fail
            };
            let error = format!("{error} (after {} attempts)", self.attempt);
            return self.apply_terminal(workflow, &fallback, step, &error);
        }
        self.apply_terminal(workflow, &policy, step, error);
    }

    /// The half of the policy that ends the attempt one way or the other.
    fn apply_terminal(
        &mut self,
        workflow: &Workflow,
        policy: &ErrorPolicy,
        step: &str,
        error: &str,
    ) {
        match policy {
            ErrorPolicy::Handler { step: handler } => {
                if workflow.step(handler).is_none() {
                    return self.finish_failed(
                        step,
                        format!(
                            "step `{step}` failed ({error}) and its error handler `{handler}` \
                             is not a step of this workflow"
                        ),
                    );
                }
                self.context.insert(
                    ERROR_KEY.to_owned(),
                    json!({
                        "step": step,
                        "message": error,
                        "attempt": self.attempt,
                    }),
                );
                self.goto(handler.clone());
            }
            ErrorPolicy::Fail | ErrorPolicy::Retry { .. } => self.finish_failed(step, error),
        }
    }

    /// End the run as failed.
    fn finish_failed(&mut self, step: &str, error: impl Into<String>) {
        self.phase = Phase::Done {
            conclusion: Conclusion::Failed {
                step: step.to_owned(),
                error: error.into(),
            },
        };
    }

    /// The workflow itself is inconsistent: end the run saying so, without
    /// consulting the error policy. Retrying a step that is stored as something
    /// other than what the cursor says it is would never succeed, and a handler
    /// written for a failing HTTP call is not an answer to a broken program.
    fn broken(&mut self, step: &str, error: String) -> Decision {
        self.finish_failed(step, error.clone());
        Decision::Failed {
            step: step.to_owned(),
            error,
        }
    }
}

/// The first of the values fed back, or null for none — never a panic on an
/// empty vector, because the count was checked one frame up.
fn first(values: Vec<Json>) -> Json {
    values.into_iter().next().unwrap_or(Json::Null)
}

/// JavaScript's truthiness, which is what a `Branch` guard is written in.
fn truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Json::String(s) => !s.is_empty(),
        Json::Array(_) | Json::Object(_) => true,
    }
}

/// A short rendering of a value for an error message, so a failure names what it
/// actually got without printing a whole order into the run's `error` column.
fn describe(value: &Json) -> String {
    let rendered = value.to_string();
    if rendered.chars().count() <= 60 {
        rendered
    } else {
        format!("{}…", rendered.chars().take(60).collect::<String>())
    }
}

/// When a `Wait`'s or a timeout's formula says to wake up.
///
/// A **number** is a duration in milliseconds from now, which is what a formula
/// like `1000 * 60 * 60` means and the spelling most `Wait`s use. A **string** is
/// an instant in RFC 3339, which is what a `Date` becomes on its way out of the
/// evaluator and how "wait until the order's due date" is written. Anything else
/// is the formula being wrong, and saying so is better than waking immediately or
/// never.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a delay in milliseconds clamped to the i64 range first"
)]
fn deadline(value: &Json, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    match value {
        Json::Number(n) => {
            let ms = n
                .as_f64()
                .filter(|f| f.is_finite())
                .ok_or_else(|| Error::invalid(format!("`{n}` is not a number of milliseconds")))?;
            // A deadline already past is a wait of no time rather than an error:
            // "wait until the due date" on an overdue order should carry on.
            Ok(now + Duration::milliseconds(ms.max(0.0).min(i64::MAX as f64) as i64))
        }
        Json::String(s) => DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .map_err(|e| {
                Error::invalid(format!(
                    "`{s}` is not an instant a run can wait until: {e}; \
                     write a duration in milliseconds or an RFC 3339 timestamp"
                ))
            }),
        other => Err(Error::invalid(format!(
            "a run cannot wait until {}; write a duration in milliseconds or an RFC 3339 timestamp",
            describe(other)
        ))),
    }
}

/// How long to wait before attempt `attempt + 1`: exponential, capped, and
/// jittered.
///
/// Jitter is **equal jitter** — half the delay, plus a spread over the other half
/// — because retries without it synchronise: a hundred runs that failed on the
/// same outage would retry at the same instant, which is the outage's second
/// wave. The spread is derived from the step, the attempt and the sub-second part
/// of the failure's instant rather than from a random number generator, so the
/// machine stays sans-IO and a test with a fixed clock gets a fixed answer.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a finite delay in milliseconds, capped at the backoff maximum; the cast saturates"
)]
fn retry_delay(backoff: &Backoff, attempt: u32, step: &str, now: DateTime<Utc>) -> Duration {
    let exponent = attempt.saturating_sub(1).min(63) as i32;
    let raw = (backoff.initial_ms as f64) * backoff.factor.max(1.0).powi(exponent);
    let capped = if raw.is_finite() {
        (raw as u64).min(backoff.max_ms)
    } else {
        backoff.max_ms
    };
    let ms = if backoff.jitter {
        let half = capped / 2;
        let mut hasher = DefaultHasher::new();
        step.hash(&mut hasher);
        attempt.hash(&mut hasher);
        now.timestamp_subsec_nanos().hash(&mut hasher);
        half + hasher.finish() % (capped - half + 1)
    } else {
        capped
    };
    Duration::milliseconds(ms.min(i64::MAX as u64) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{Assignment, BranchArm, Step};
    use sc_action::TriggerId;
    use std::collections::BTreeMap;

    /// A driver that does no IO: formulas answer from a table, actions answer
    /// from a queue per step, and the clock is whatever the test says it is.
    ///
    /// Everything §2.3 and §2.4 promise is asserted through this, synchronously,
    /// because the machine's whole point is that it decides without doing.
    struct Fake {
        /// Formula source → the value the evaluator would return.
        values: BTreeMap<String, Json>,
        /// Step name → its outcomes, one per attempt. A step with none succeeds
        /// with null.
        results: BTreeMap<String, Vec<std::result::Result<Json, String>>>,
        /// How many times each action step was actually run.
        calls: BTreeMap<String, u32>,
        /// Now.
        now: DateTime<Utc>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                values: BTreeMap::new(),
                results: BTreeMap::new(),
                calls: BTreeMap::new(),
                now: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            }
        }

        fn value(mut self, formula: &str, value: Json) -> Fake {
            self.values.insert(formula.to_owned(), value);
            self
        }

        fn outcomes(
            mut self,
            step: &str,
            outcomes: Vec<std::result::Result<Json, String>>,
        ) -> Fake {
            self.results.insert(step.to_owned(), outcomes);
            self
        }

        /// Advance the run until it wants the outside world for something the
        /// clock or a person must supply.
        fn drive(&mut self, workflow: &Workflow, run: &mut WorkflowRun) -> Decision {
            for _ in 0..10_000 {
                let decision = run.next_step(workflow, self.now);
                match &decision {
                    Decision::RunAction { step, .. } => {
                        *self.calls.entry(step.clone()).or_default() += 1;
                        let outcome = match self.results.get_mut(step) {
                            Some(queue) if !queue.is_empty() => queue.remove(0),
                            _ => Ok(Json::Null),
                        };
                        match outcome {
                            Ok(value) => run.step_succeeded(value).unwrap(),
                            Err(error) => run.step_failed(workflow, &error, self.now).unwrap(),
                        }
                    }
                    Decision::Evaluate { formulas, .. } => {
                        let values = formulas
                            .iter()
                            .map(|f| self.values.get(f).cloned().unwrap_or(Json::Null))
                            .collect();
                        run.evaluated(workflow, values, self.now).unwrap();
                    }
                    _ => return decision,
                }
            }
            panic!("the run never stopped");
        }
    }

    fn act(name: &str) -> Step {
        Step::new(
            name,
            StepKind::Action {
                action: name.to_owned(),
                configuration: Attrs::new(),
            },
        )
    }

    fn wf(steps: Vec<Step>) -> Workflow {
        Workflow::of(TriggerId::new(), 1, steps)
    }

    fn done(decision: Decision) -> Attrs {
        match decision {
            Decision::Done { context } => context,
            other => panic!("expected a finished run, got {other:?}"),
        }
    }

    fn failure(decision: Decision) -> (String, String) {
        match decision {
            Decision::Failed { step, error } => (step, error),
            other => panic!("expected a failed run, got {other:?}"),
        }
    }

    // ---- §2.1 the state ---------------------------------------------------

    #[test]
    fn the_whole_state_of_a_run_in_flight_round_trips_through_its_stored_json() {
        // A run inside a loop, waiting for a person, having retried once: every
        // field that makes it resumable is carrying something.
        let workflow = wf(vec![
            Step::new(
                "lines",
                StepKind::ForEach {
                    over: "context.lines".to_owned(),
                    var: "line".to_owned(),
                    body: "approve".to_owned(),
                },
            ),
            Step::new(
                "approve",
                StepKind::UserForm {
                    fields: vec![FieldDecl::new("ok", "bool").required()],
                    assign_to: "approval".to_owned(),
                    min_role: Some(Role::of(40)),
                    timeout: None,
                },
            ),
        ]);
        let mut fake = Fake::new().value("context.lines", json!([1, 2]));
        let mut run = WorkflowRun::new(&workflow);
        let suspended = fake.drive(&workflow, &mut run);
        assert!(matches!(suspended, Decision::Suspend { .. }));
        assert_eq!(run.loop_depth(), 1);

        let json = serde_json::to_value(&run).unwrap();
        let mut back: WorkflowRun = serde_json::from_value(json).unwrap();
        assert_eq!(back, run);

        // And it is a *resumable* copy, not merely an equal one: the restored run
        // carries on where the original was.
        back.resumed([("ok".to_owned(), json!(true))].into_iter().collect())
            .unwrap();
        let second = fake.drive(&workflow, &mut back);
        assert!(
            matches!(second, Decision::Suspend { .. }),
            "the second line's approval is the next thing it wants"
        );
        assert_eq!(back.context()["line"], json!(2));
    }

    #[test]
    fn a_run_is_pinned_to_the_budget_it_started_with() {
        let mut workflow = wf(vec![act("a")]);
        workflow.max_steps = 7;
        assert_eq!(WorkflowRun::new(&workflow).max_steps(), 7);
        // Zero is raised to one, so a caller who wrote it gets a run that does
        // something rather than one that reports a number nobody can act on.
        workflow.max_steps = 0;
        assert_eq!(WorkflowRun::new(&workflow).max_steps(), 1);
    }

    // ---- §2.2 the decisions -----------------------------------------------

    #[test]
    fn asking_twice_without_answering_asks_for_the_same_thing_twice() {
        // The property a resumed run rests on: a node that died between being
        // told to run an action and running it is told again.
        let workflow = wf(vec![act("send")]);
        let mut run = WorkflowRun::new(&workflow);
        let now = Fake::new().now;
        let first = run.next_step(&workflow, now);
        assert_eq!(first, run.next_step(&workflow, now));
        assert_eq!(run.steps_taken(), 1, "and it is not charged twice");
        assert!(matches!(first, Decision::RunAction { .. }));
    }

    #[test]
    fn a_result_fed_back_out_of_turn_is_refused() {
        let workflow = wf(vec![Step::new(
            "s",
            StepKind::Set {
                assignments: vec![Assignment::new("x", "1")],
            },
        )]);
        let mut run = WorkflowRun::new(&workflow);
        run.next_step(&workflow, Fake::new().now);
        // It asked for a formula, not for an action's result.
        let err = run.step_succeeded(json!(1)).unwrap_err();
        assert!(err.to_string().contains("did not ask for"), "{err}");
        // And the wrong number of values is refused rather than misassigned.
        let err = run
            .evaluated(&workflow, vec![json!(1), json!(2)], Fake::new().now)
            .unwrap_err();
        assert!(err.to_string().contains("asked for 1"), "{err}");
    }

    // ---- §2.3 the advance rules -------------------------------------------

    #[test]
    fn an_actions_result_is_stored_under_its_steps_name() {
        let workflow = wf(vec![act("fetch")]);
        let mut fake = Fake::new().outcomes("fetch", vec![Ok(json!({"amount": 12}))]);
        let mut run = WorkflowRun::new(&workflow);
        let context = done(fake.drive(&workflow, &mut run));
        assert_eq!(context["fetch"], json!({"amount": 12}));
    }

    #[test]
    fn a_sets_assignments_merge_into_the_context_in_order() {
        let workflow = wf(vec![Step::new(
            "totals",
            StepKind::Set {
                assignments: vec![
                    Assignment::new("net", "100"),
                    Assignment::new("gross", "context.net * 1.2"),
                ],
            },
        )]);
        let mut fake = Fake::new()
            .value("100", json!(100))
            .value("context.net * 1.2", json!(120));
        let mut run = WorkflowRun::new(&workflow);
        let context = done(fake.drive(&workflow, &mut run));
        assert_eq!(context["net"], json!(100));
        assert_eq!(context["gross"], json!(120));
    }

    #[test]
    fn an_assignment_is_evaluated_after_the_one_before_it_has_been_written() {
        // Not one batch: `Assignment`'s promise is that a later formula may read
        // what an earlier one wrote, which a batch could not offer.
        let workflow = wf(vec![Step::new(
            "s",
            StepKind::Set {
                assignments: vec![Assignment::new("a", "1"), Assignment::new("b", "context.a")],
            },
        )]);
        let mut run = WorkflowRun::new(&workflow);
        let now = Fake::new().now;
        let Decision::Evaluate { formulas, .. } = run.next_step(&workflow, now) else {
            panic!("the first assignment is asked for on its own");
        };
        assert_eq!(formulas, ["1"]);
        run.evaluated(&workflow, vec![json!(1)], now).unwrap();
        // By the time the second is asked for, the first is in the context the
        // evaluator will be given.
        assert_eq!(run.context()["a"], json!(1));
        let Decision::Evaluate { formulas, .. } = run.next_step(&workflow, now) else {
            panic!("the second assignment follows");
        };
        assert_eq!(formulas, ["context.a"]);
    }

    #[test]
    fn next_resolves_through_all_four_variants() {
        let workflow = wf(vec![
            // Step
            act("one").then(Next::step("two")),
            // Branch
            act("two").then(Next::Branch {
                arms: vec![
                    BranchArm::new("context.big", "big"),
                    BranchArm::new("context.small", "small"),
                ],
                otherwise: Some("computed".to_owned()),
            }),
            act("big").then(Next::step("computed")),
            act("small").then(Next::step("computed")),
            // Formula
            act("computed").then(Next::Formula {
                formula: "context.where".to_owned(),
            }),
            // End
            act("last"),
        ]);
        let mut fake = Fake::new()
            .value("context.big", json!(false))
            .value("context.small", json!(true))
            .value("context.where", json!("last"));
        let mut run = WorkflowRun::new(&workflow);
        done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("one"), Some(&1));
        assert_eq!(fake.calls.get("small"), Some(&1), "the second arm was true");
        assert_eq!(fake.calls.get("big"), None, "the first arm was not");
        assert_eq!(fake.calls.get("last"), Some(&1), "the formula named it");
    }

    #[test]
    fn a_branch_with_nothing_true_and_no_otherwise_finishes() {
        let workflow = wf(vec![
            act("check").then(Next::Branch {
                arms: vec![BranchArm::new("context.big", "big")],
                otherwise: None,
            }),
            act("big"),
        ]);
        let mut fake = Fake::new().value("context.big", json!(false));
        let mut run = WorkflowRun::new(&workflow);
        done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("big"), None);
    }

    #[test]
    fn a_computed_next_of_null_ends_the_run_and_of_a_stranger_fails_it() {
        let workflow = wf(vec![act("a").then(Next::Formula {
            formula: "f".to_owned(),
        })]);

        let mut fake = Fake::new().value("f", Json::Null);
        let mut run = WorkflowRun::new(&workflow);
        done(fake.drive(&workflow, &mut run));

        let mut fake = Fake::new().value("f", json!("nowhere"));
        let mut run = WorkflowRun::new(&workflow);
        let (step, error) = failure(fake.drive(&workflow, &mut run));
        assert_eq!(step, "a");
        assert!(error.contains("no step of that name"), "{error}");
    }

    #[test]
    fn a_for_each_binds_the_item_runs_the_body_and_leaves_by_its_own_next() {
        let workflow = wf(vec![
            Step::new(
                "lines",
                StepKind::ForEach {
                    over: "context.order.lines".to_owned(),
                    var: "line".to_owned(),
                    body: "ship".to_owned(),
                },
            )
            .then(Next::step("invoice")),
            // The body ends, which inside a loop means the iteration ends.
            act("ship"),
            act("invoice"),
        ]);
        let mut fake = Fake::new().value("context.order.lines", json!(["a", "b", "c"]));
        let mut run = WorkflowRun::new(&workflow);
        let context = done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("ship"), Some(&3), "once per item");
        assert_eq!(
            fake.calls.get("invoice"),
            Some(&1),
            "and then the next step"
        );
        assert_eq!(context["line"], json!("c"), "the last item stays bound");
        assert_eq!(run.loop_depth(), 0);
    }

    #[test]
    fn an_empty_collection_runs_the_body_no_times() {
        let workflow = wf(vec![
            Step::new(
                "lines",
                StepKind::ForEach {
                    over: "context.lines".to_owned(),
                    var: "line".to_owned(),
                    body: "ship".to_owned(),
                },
            )
            .then(Next::step("invoice")),
            act("ship"),
            act("invoice"),
        ]);
        let mut fake = Fake::new().value("context.lines", json!([]));
        let mut run = WorkflowRun::new(&workflow);
        done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("ship"), None);
        assert_eq!(fake.calls.get("invoice"), Some(&1));
    }

    #[test]
    fn a_collection_that_is_not_a_list_is_a_failure_rather_than_an_empty_loop() {
        // Running the body zero times would hide a formula that names something
        // the run never wrote until somebody wondered why nothing happened.
        let workflow = wf(vec![
            Step::new(
                "lines",
                StepKind::ForEach {
                    over: "context.lines".to_owned(),
                    var: "line".to_owned(),
                    body: "ship".to_owned(),
                },
            ),
            act("ship"),
        ]);
        let mut fake = Fake::new().value("context.lines", Json::Null);
        let mut run = WorkflowRun::new(&workflow);
        let (step, error) = failure(fake.drive(&workflow, &mut run));
        assert_eq!(step, "lines");
        assert!(error.contains("not a list"), "{error}");
    }

    #[test]
    fn loops_nest_and_the_inner_one_returns_to_the_outer() {
        let workflow = wf(vec![
            Step::new(
                "orders",
                StepKind::ForEach {
                    over: "orders".to_owned(),
                    var: "order".to_owned(),
                    body: "lines".to_owned(),
                },
            )
            .then(Next::step("report")),
            Step::new(
                "lines",
                StepKind::ForEach {
                    over: "lines".to_owned(),
                    var: "line".to_owned(),
                    body: "ship".to_owned(),
                },
            ),
            act("ship"),
            act("report"),
        ]);
        let mut fake = Fake::new()
            .value("orders", json!([1, 2]))
            .value("lines", json!(["x", "y", "z"]));
        let mut run = WorkflowRun::new(&workflow);
        done(fake.drive(&workflow, &mut run));
        assert_eq!(
            fake.calls.get("ship"),
            Some(&6),
            "two orders of three lines"
        );
        assert_eq!(fake.calls.get("report"), Some(&1));
        assert_eq!(run.loop_depth(), 0);
    }

    #[test]
    fn the_step_budget_stops_a_loop_with_no_exit() {
        let mut workflow = wf(vec![act("spin").then(Next::step("spin"))]);
        workflow.max_steps = 12;
        let mut fake = Fake::new();
        let mut run = WorkflowRun::new(&workflow);
        let (step, error) = failure(fake.drive(&workflow, &mut run));
        assert_eq!(step, "spin");
        assert!(error.contains("12 steps"), "{error}");
        assert_eq!(run.steps_taken(), 12);
        assert_eq!(fake.calls.get("spin"), Some(&12));
    }

    #[test]
    fn a_next_that_names_a_step_the_workflow_does_not_have_ends_the_run() {
        // The program is wrong, not the step: retrying it would never help, and a
        // handler written for a failing HTTP call is not an answer to it.
        let workflow = wf(vec![act("a").then(Next::step("ghost")).on_error(
            ErrorPolicy::Retry {
                max: 5,
                backoff: Backoff::default(),
            },
        )]);
        let mut fake = Fake::new();
        let mut run = WorkflowRun::new(&workflow);
        let (step, error) = failure(fake.drive(&workflow, &mut run));
        assert_eq!(step, "ghost");
        assert!(error.contains("no step named `ghost`"), "{error}");
        assert_eq!(fake.calls.get("a"), Some(&1), "and it did not retry");
    }

    // ---- §2.4 the error rules ---------------------------------------------

    fn no_jitter() -> Backoff {
        Backoff {
            initial_ms: 1_000,
            factor: 2.0,
            max_ms: 60_000,
            jitter: false,
        }
    }

    #[test]
    fn a_retry_waits_longer_each_time_and_re_runs_the_same_step() {
        let workflow = wf(vec![
            act("flaky")
                .then(Next::step("after"))
                .on_error(ErrorPolicy::Retry {
                    max: 3,
                    backoff: no_jitter(),
                }),
            act("after"),
        ]);
        let start = Fake::new().now;
        let mut fake = Fake::new().outcomes(
            "flaky",
            vec![
                Err("connection refused".to_owned()),
                Err("connection refused".to_owned()),
                Ok(json!("ok")),
            ],
        );
        let mut run = WorkflowRun::new(&workflow);

        let Decision::Suspend { step, until, .. } = fake.drive(&workflow, &mut run) else {
            panic!("a failed step with a retry policy suspends");
        };
        assert_eq!(step, "flaky");
        assert_eq!(until, Some(start + Duration::milliseconds(1_000)));
        assert_eq!(run.attempt(), 1);

        // Not runnable before the deadline: the queue handing it back early gets
        // the same answer rather than a step that should not have run.
        assert!(matches!(
            run.next_step(&workflow, start + Duration::milliseconds(999)),
            Decision::Suspend { .. }
        ));

        fake.now = start + Duration::milliseconds(1_000);
        let Decision::Suspend { until, .. } = fake.drive(&workflow, &mut run) else {
            panic!("the second attempt failed too");
        };
        assert_eq!(
            until,
            Some(fake.now + Duration::milliseconds(2_000)),
            "the wait doubles"
        );
        assert_eq!(run.attempt(), 2);

        fake.now += Duration::milliseconds(2_000);
        let context = done(fake.drive(&workflow, &mut run));
        assert_eq!(
            fake.calls.get("flaky"),
            Some(&3),
            "the same step, three times"
        );
        assert_eq!(context["flaky"], json!("ok"));
        assert_eq!(fake.calls.get("after"), Some(&1));
    }

    #[test]
    fn exhausting_a_steps_retries_falls_through_to_the_workflows_policy() {
        let workflow = wf(vec![
            act("flaky").on_error(ErrorPolicy::Retry {
                max: 2,
                backoff: no_jitter(),
            }),
            act("cleanup"),
        ])
        .on_error(ErrorPolicy::Handler {
            step: "cleanup".to_owned(),
        });
        let mut fake = Fake::new().outcomes(
            "flaky",
            vec![Err("nope".to_owned()), Err("nope".to_owned())],
        );
        let mut run = WorkflowRun::new(&workflow);
        assert!(matches!(
            fake.drive(&workflow, &mut run),
            Decision::Suspend { .. }
        ));
        fake.now += Duration::milliseconds(1_000);
        let context = done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("flaky"), Some(&2));
        assert_eq!(fake.calls.get("cleanup"), Some(&1));
        assert_eq!(context[ERROR_KEY]["step"], json!("flaky"));
        assert!(
            context[ERROR_KEY]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("after 2 attempts"),
            "{:?}",
            context[ERROR_KEY]
        );
    }

    #[test]
    fn a_workflow_wide_retry_that_runs_out_fails_rather_than_starting_again() {
        // The policy came from the workflow, so there is nothing above it to fall
        // through to; falling through to itself would retry forever.
        let workflow = wf(vec![act("flaky")]).on_error(ErrorPolicy::Retry {
            max: 2,
            backoff: no_jitter(),
        });
        let mut fake = Fake::new().outcomes(
            "flaky",
            vec![Err("nope".to_owned()), Err("still nope".to_owned())],
        );
        let mut run = WorkflowRun::new(&workflow);
        assert!(matches!(
            fake.drive(&workflow, &mut run),
            Decision::Suspend { .. }
        ));
        fake.now += Duration::milliseconds(1_000);
        let (step, error) = failure(fake.drive(&workflow, &mut run));
        assert_eq!(step, "flaky");
        assert!(
            error.contains("still nope") && error.contains("2 attempts"),
            "{error}"
        );
        assert_eq!(fake.calls.get("flaky"), Some(&2));
    }

    #[test]
    fn a_handler_jump_puts_the_error_in_the_context_under_the_reserved_key() {
        let workflow = wf(vec![
            act("charge").on_error(ErrorPolicy::Handler {
                step: "refund".to_owned(),
            }),
            act("refund"),
        ]);
        let mut fake =
            Fake::new().outcomes("charge", vec![Err("the card was declined".to_owned())]);
        let mut run = WorkflowRun::new(&workflow);
        let context = done(fake.drive(&workflow, &mut run));
        assert_eq!(
            context[ERROR_KEY],
            json!({"step": "charge", "message": "the card was declined", "attempt": 1})
        );
        assert_eq!(fake.calls.get("refund"), Some(&1));
    }

    #[test]
    fn a_handler_that_is_not_a_step_of_the_workflow_is_said_so() {
        let workflow = wf(vec![act("charge").on_error(ErrorPolicy::Handler {
            step: "refund".to_owned(),
        })]);
        let mut fake = Fake::new().outcomes("charge", vec![Err("declined".to_owned())]);
        let mut run = WorkflowRun::new(&workflow);
        let (_, error) = failure(fake.drive(&workflow, &mut run));
        assert!(error.contains("`refund` is not a step"), "{error}");
    }

    #[test]
    fn fail_is_the_default_and_a_steps_policy_beats_the_workflows() {
        // No policy anywhere: the run stops and says why.
        let workflow = wf(vec![act("a")]);
        let mut fake = Fake::new().outcomes("a", vec![Err("boom".to_owned())]);
        let mut run = WorkflowRun::new(&workflow);
        assert_eq!(
            failure(fake.drive(&workflow, &mut run)),
            ("a".to_owned(), "boom".to_owned())
        );

        // "No per-step policy" is not "no policy": the workflow's governs.
        let workflow = wf(vec![act("a"), act("h")]).on_error(ErrorPolicy::Handler {
            step: "h".to_owned(),
        });
        let mut fake = Fake::new().outcomes("a", vec![Err("boom".to_owned())]);
        let mut run = WorkflowRun::new(&workflow);
        done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("h"), Some(&1));

        // And a step that has one overrides it.
        let workflow = wf(vec![act("a").on_error(ErrorPolicy::Fail), act("h")]).on_error(
            ErrorPolicy::Handler {
                step: "h".to_owned(),
            },
        );
        let mut fake = Fake::new().outcomes("a", vec![Err("boom".to_owned())]);
        let mut run = WorkflowRun::new(&workflow);
        failure(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("h"), None);
    }

    #[test]
    fn a_formula_that_will_not_evaluate_goes_through_the_same_policy_as_an_action() {
        let workflow = wf(vec![
            Step::new(
                "s",
                StepKind::Set {
                    assignments: vec![Assignment::new("x", "boom()")],
                },
            )
            .on_error(ErrorPolicy::Handler {
                step: "h".to_owned(),
            }),
            act("h"),
        ]);
        let mut run = WorkflowRun::new(&workflow);
        let now = Fake::new().now;
        run.next_step(&workflow, now);
        run.step_failed(&workflow, "boom is not defined", now)
            .unwrap();
        let mut fake = Fake::new();
        done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("h"), Some(&1));
    }

    #[test]
    fn the_backoff_is_capped_and_jitter_stays_within_half_the_delay() {
        let backoff = Backoff {
            initial_ms: 1_000,
            factor: 10.0,
            max_ms: 5_000,
            jitter: false,
        };
        let now = Fake::new().now;
        assert_eq!(
            retry_delay(&backoff, 1, "s", now),
            Duration::milliseconds(1_000)
        );
        assert_eq!(
            retry_delay(&backoff, 2, "s", now),
            Duration::milliseconds(5_000)
        );
        // Far enough out that the exponential overflows, which must still cap.
        assert_eq!(
            retry_delay(&backoff, 99, "s", now),
            Duration::milliseconds(5_000)
        );

        let jittered = Backoff {
            jitter: true,
            ..backoff
        };
        for attempt in 1..40 {
            let delay = retry_delay(&jittered, attempt, "step", now).num_milliseconds();
            assert!(
                (500..=5_000).contains(&delay),
                "attempt {attempt} waited {delay}ms"
            );
        }
    }

    // ---- suspension -------------------------------------------------------

    #[test]
    fn a_wait_leaves_the_queues_reach_until_its_deadline() {
        let workflow = wf(vec![
            Step::new(
                "pause",
                StepKind::Wait {
                    until: "60 * 60 * 1000".to_owned(),
                },
            )
            .then(Next::step("after")),
            act("after"),
        ]);
        let start = Fake::new().now;
        let mut fake = Fake::new().value("60 * 60 * 1000", json!(3_600_000));
        let mut run = WorkflowRun::new(&workflow);
        let Decision::Suspend {
            step,
            until,
            awaiting_input,
        } = fake.drive(&workflow, &mut run)
        else {
            panic!("a wait suspends");
        };
        assert_eq!(step, "pause");
        assert_eq!(until, Some(start + Duration::hours(1)));
        assert_eq!(awaiting_input, None);
        assert_eq!(run.wake_at(), until);
        assert!(run.is_suspended());
        assert_eq!(fake.calls.get("after"), None);

        // No test sleeps: the clock is a parameter.
        fake.now = start + Duration::hours(1);
        done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("after"), Some(&1));
    }

    #[test]
    fn a_wait_can_be_written_as_an_instant() {
        let workflow = wf(vec![Step::new(
            "pause",
            StepKind::Wait {
                until: "context.due".to_owned(),
            },
        )]);
        let mut fake = Fake::new().value("context.due", json!("2026-06-01T09:00:00Z"));
        let mut run = WorkflowRun::new(&workflow);
        let Decision::Suspend { until, .. } = fake.drive(&workflow, &mut run) else {
            panic!("a wait suspends");
        };
        assert_eq!(
            until.map(|t| t.to_rfc3339()),
            Some("2026-06-01T09:00:00+00:00".to_owned())
        );

        // And a deadline that is neither is the formula being wrong.
        let mut fake = Fake::new().value("context.due", json!({"soon": true}));
        let mut run = WorkflowRun::new(&workflow);
        let (_, error) = failure(fake.drive(&workflow, &mut run));
        assert!(error.contains("RFC 3339"), "{error}");
    }

    #[test]
    fn a_user_form_suspends_with_its_declaration_and_resumes_under_assign_to() {
        let workflow = wf(vec![
            Step::new(
                "approve",
                StepKind::UserForm {
                    fields: vec![
                        FieldDecl::new("approved", "bool").required(),
                        FieldDecl::new("note", "text"),
                    ],
                    assign_to: "approval".to_owned(),
                    min_role: Some(Role::of(40)),
                    timeout: None,
                },
            )
            .then(Next::step("notify")),
            act("notify"),
        ]);
        let mut fake = Fake::new();
        let mut run = WorkflowRun::new(&workflow);
        let Decision::Suspend {
            until,
            awaiting_input: Some(form),
            ..
        } = fake.drive(&workflow, &mut run)
        else {
            panic!("a form suspends for a person");
        };
        // No clock will make this runnable, which is what a NULL `wake_at` means.
        assert_eq!(until, None);
        assert_eq!(form.assign_to, "approval");
        assert_eq!(form.min_role, Some(Role::of(40)));
        assert_eq!(form.fields.len(), 2);
        assert_eq!(run.pending_form(), Some(&form));

        // A day later, and after any number of restarts.
        fake.now += Duration::days(1);
        run.resumed(
            [
                ("approved".to_owned(), json!(true)),
                ("note".to_owned(), json!("looks fine")),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap();
        let context = done(fake.drive(&workflow, &mut run));
        assert_eq!(
            context["approval"],
            json!({"approved": true, "note": "looks fine"})
        );
        assert_eq!(fake.calls.get("notify"), Some(&1));
    }

    #[test]
    fn resuming_a_run_that_is_not_waiting_for_a_person_is_refused() {
        let workflow = wf(vec![act("a")]);
        let mut run = WorkflowRun::new(&workflow);
        let err = run.resumed(Attrs::new()).unwrap_err();
        assert!(err.to_string().contains("not waiting"), "{err}");
    }

    #[test]
    fn an_abandoned_approval_times_out_and_its_error_policy_decides() {
        let workflow = wf(vec![
            Step::new(
                "approve",
                StepKind::UserForm {
                    fields: vec![FieldDecl::new("approved", "bool")],
                    assign_to: "approval".to_owned(),
                    min_role: None,
                    timeout: Some("3 * 86400000".to_owned()),
                },
            )
            .on_error(ErrorPolicy::Handler {
                step: "escalate".to_owned(),
            }),
            act("escalate"),
        ]);
        let start = Fake::new().now;
        let mut fake = Fake::new().value("3 * 86400000", json!(259_200_000));
        let mut run = WorkflowRun::new(&workflow);
        let Decision::Suspend {
            until,
            awaiting_input: Some(_),
            ..
        } = fake.drive(&workflow, &mut run)
        else {
            panic!("a form with a timeout suspends for both a person and the clock");
        };
        assert_eq!(until, Some(start + Duration::days(3)));

        fake.now = start + Duration::days(3);
        let context = done(fake.drive(&workflow, &mut run));
        assert_eq!(fake.calls.get("escalate"), Some(&1));
        assert!(
            context[ERROR_KEY]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("before its timeout"),
            "{:?}",
            context[ERROR_KEY]
        );
    }

    #[test]
    fn a_timeout_formula_of_null_means_there_is_no_timeout_after_all() {
        let workflow = wf(vec![Step::new(
            "approve",
            StepKind::UserForm {
                fields: vec![],
                assign_to: "approval".to_owned(),
                min_role: None,
                timeout: Some("context.urgent ? 3600000 : null".to_owned()),
            },
        )]);
        let mut fake = Fake::new().value("context.urgent ? 3600000 : null", Json::Null);
        let mut run = WorkflowRun::new(&workflow);
        let Decision::Suspend {
            until,
            awaiting_input: Some(_),
            ..
        } = fake.drive(&workflow, &mut run)
        else {
            panic!("it still suspends for a person");
        };
        assert_eq!(until, None);
    }

    // ---- bookkeeping ------------------------------------------------------

    #[test]
    fn the_trace_sequence_carries_on_where_a_restored_run_left_off() {
        let workflow = wf(vec![act("a")]);
        let mut run = WorkflowRun::new(&workflow);
        assert_eq!(run.next_trace_seq(), 1);
        assert_eq!(run.next_trace_seq(), 2);
        let back: WorkflowRun =
            serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
        let mut back = back;
        assert_eq!(back.next_trace_seq(), 3, "a recovered run does not collide");
    }

    #[test]
    fn the_current_step_and_attempt_are_readable_while_a_step_is_in_flight() {
        let workflow = wf(vec![act("send").on_error(ErrorPolicy::Retry {
            max: 3,
            backoff: no_jitter(),
        })]);
        let mut run = WorkflowRun::new(&workflow);
        let now = Fake::new().now;
        run.next_step(&workflow, now);
        // What the driver writes into the trace row before feeding the outcome
        // back.
        assert_eq!(run.current_step(), Some("send"));
        assert_eq!(run.attempt(), 1);
        run.step_failed(&workflow, "nope", now).unwrap();
        run.next_step(&workflow, now + Duration::milliseconds(1_000));
        assert_eq!(run.attempt(), 2);
        assert!(!run.is_done());
    }

    #[test]
    fn a_retry_starts_again_at_the_step_that_failed_and_not_at_the_beginning() {
        let workflow = wf(vec![act("first").then(Next::step("send")), act("send")]);
        let mut run = WorkflowRun::new(&workflow);
        let now = Fake::new().now;
        // `first` succeeds and is not run again; `send` fails and the run stops.
        run.next_step(&workflow, now);
        run.step_succeeded(json!("done")).unwrap();
        run.next_step(&workflow, now);
        run.step_failed(&workflow, "the supplier is down", now)
            .unwrap();
        let (step, _) = failure(run.next_step(&workflow, now));
        assert_eq!(step, "send");
        let spent = run.steps_taken();

        run.retry(&workflow).unwrap();
        // The step that failed, with its attempts back to zero — and what the
        // steps before it wrote still in the context, because they are not run
        // again.
        assert_eq!(run.current_step(), Some("send"));
        assert_eq!(run.attempt(), 0);
        assert_eq!(run.context().get("first"), Some(&json!("done")));
        let decision = run.next_step(&workflow, now);
        assert!(
            matches!(&decision, Decision::RunAction { step, .. } if step == "send"),
            "{decision:?}"
        );
        assert_eq!(run.attempt(), 1);
        assert_eq!(
            run.steps_taken(),
            spent + 1,
            "the budget is the run's, so a retried step spends one of it"
        );
    }

    #[test]
    fn only_a_run_that_stopped_at_a_step_can_be_retried() {
        let workflow = wf(vec![act("a")]);
        let mut run = WorkflowRun::new(&workflow);
        let now = Fake::new().now;
        run.next_step(&workflow, now);
        // Still in flight: there is nothing to start again.
        let err = run.retry(&workflow).unwrap_err().to_string();
        assert!(err.contains("did not fail"), "{err}");

        run.step_succeeded(json!(1)).unwrap();
        run.next_step(&workflow, now);
        assert!(run.is_done());
        assert!(
            run.retry(&workflow).is_err(),
            "a finished run has no failure"
        );
    }

    #[test]
    fn a_run_that_stopped_at_its_budget_is_refused_rather_than_restarted_into_the_same_wall() {
        // A loop with no exit, stopped by the budget: retrying it would spend
        // the same zero remaining steps and stop again at once, so it is refused
        // by name instead.
        let mut workflow = wf(vec![act("a").then(Next::step("a"))]);
        workflow.max_steps = 3;
        let mut run = WorkflowRun::new(&workflow);
        let now = Fake::new().now;
        for _ in 0..3 {
            run.next_step(&workflow, now);
            run.step_succeeded(json!(1)).unwrap();
        }
        let (_, error) = failure(run.next_step(&workflow, now));
        assert!(error.contains("budget"), "{error}");
        let err = run.retry(&workflow).unwrap_err().to_string();
        assert!(err.contains("max_steps"), "{err}");
    }

    #[test]
    fn truthiness_is_javascripts() {
        for falsy in [json!(null), json!(false), json!(0), json!(""), json!(0.0)] {
            assert!(!truthy(&falsy), "{falsy} should be falsy");
        }
        for t in [json!(true), json!(1), json!("no"), json!([]), json!({})] {
            assert!(truthy(&t), "{t} should be truthy");
        }
    }
}
