//! The [`Ledger`]: what each step of a run cost, and who answered it (TODO §10).
//!
//! Part of the loop's state, so it is stored with the run and survives a
//! resume. Each model call adds one [`LedgerStep`] with its role, usage, cost and
//! time. A delegated child run's totals are rolled up into its parent's as a
//! [`ChildLedger`], so a planner's totals include every session it started.
//!
//! **An unknown cost is not zero.** A step whose model has no price records
//! `None`, and any total that includes one is `None` too: a cost budget cannot
//! be kept against a number that is missing a term. That is why a cost budget
//! is refused on save for an agent with an unpriced model.

use std::collections::BTreeMap;

use sc_llm::{ModelKind, Usage};
use serde::{Deserialize, Serialize};

use crate::agent::ModelRole;
use crate::run::RunId;

/// One model call, as the ledger records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerStep {
    /// Which model call this was, counting from 1.
    pub step: u32,
    /// The role whose model answered.
    pub role: ModelRole,
    /// The model's name, as sent on the wire.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// What the provider reported.
    pub usage: Usage,
    /// What it cost, or `None` when the model has no price.
    pub cost: Option<f64>,
    /// How long the model call took, in milliseconds.
    pub elapsed_ms: u64,
    /// The signals traits raised during the step (Phase 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<String>,
    /// Whether the context was compacted before this call (TODO §9). On a
    /// summary call, always set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compacted: bool,
    /// What kind of model call it was: a chat step, or an embedding, image or
    /// speech call a tool made (TODO-agent-skills AS3.5), recorded as an aside.
    #[serde(default, skip_serializing_if = "is_chat")]
    pub kind: ModelKind,
}

fn is_chat(kind: &ModelKind) -> bool {
    *kind == ModelKind::Chat
}

/// One role's totals.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoleTotals {
    /// Model calls answered by this role.
    pub steps: u32,
    /// Their usage, summed.
    pub usage: Usage,
    /// Their cost, summed, or `None` when any term is unknown.
    pub cost: Option<f64>,
    /// Their model time, in milliseconds.
    pub elapsed_ms: u64,
}

impl Default for RoleTotals {
    fn default() -> Self {
        RoleTotals {
            steps: 0,
            usage: Usage::default(),
            // Nothing spent is a known zero; the first unknown term poisons it.
            cost: Some(0.0),
            elapsed_ms: 0,
        }
    }
}

impl RoleTotals {
    fn add_step(&mut self, step: &LedgerStep) {
        self.steps += 1;
        self.usage.add(step.usage);
        self.cost = add_cost(self.cost, step.cost);
        self.elapsed_ms += step.elapsed_ms;
    }

    fn add(&mut self, other: &RoleTotals) {
        self.steps += other.steps;
        self.usage.add(other.usage);
        self.cost = add_cost(self.cost, other.cost);
        self.elapsed_ms += other.elapsed_ms;
    }

    /// The share of input tokens read from the cache, or `None` with no input.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        cache_hit_ratio(&self.usage)
    }
}

/// A delegated child run's totals, rolled up into its parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildLedger {
    /// The child run's id, as a string (a JSON number cannot hold a UUID).
    pub run: String,
    /// The agent the child ran.
    pub agent: String,
    /// The child's per-role totals, including its own children.
    pub totals: BTreeMap<ModelRole, RoleTotals>,
}

/// A run's record of what it spent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Ledger {
    /// Every model call, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    steps: Vec<LedgerStep>,
    /// Model calls the loop made for itself rather than for a step: the cheap
    /// role's summaries when compacting (TODO §9). Counted in the totals, not
    /// in the steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    summaries: Vec<LedgerStep>,
    /// Model calls a trait's tool made through the run's own machinery rather
    /// than as a step: a commit message on the cheap role (TODO §8). Counted
    /// in the totals, not in the steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    asides: Vec<LedgerStep>,
    /// Delegated runs' totals, one entry per child run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<ChildLedger>,
    /// Time spent working — model calls and tools, children included — in
    /// milliseconds. What the wall-clock budget is kept against: time between
    /// two chat turns is not work.
    #[serde(default)]
    working_ms: u64,
}

impl Ledger {
    /// Record one model call.
    pub fn record_step(&mut self, step: LedgerStep) {
        self.steps.push(step);
    }

    /// Record a summary call made while compacting before model call
    /// `step.step`.
    pub fn record_summary(&mut self, step: LedgerStep) {
        self.summaries.push(step);
    }

    /// Record a model call a tool made for itself.
    pub fn record_aside(&mut self, step: LedgerStep) {
        self.asides.push(step);
    }

    /// The calls tools made for themselves, oldest first.
    pub fn asides(&self) -> &[LedgerStep] {
        &self.asides
    }

    /// The summary calls, oldest first.
    pub fn summaries(&self) -> &[LedgerStep] {
        &self.summaries
    }

    /// Record a child run's totals. A child that is driven again (a resumed
    /// feature) replaces its earlier entry rather than being counted twice.
    pub fn record_child(&mut self, child: ChildLedger) {
        match self.children.iter_mut().find(|c| c.run == child.run) {
            Some(existing) => *existing = child,
            None => self.children.push(child),
        }
    }

    /// Add working time.
    pub fn add_working(&mut self, elapsed: std::time::Duration) {
        self.working_ms = self
            .working_ms
            .saturating_add(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
    }

    /// Every step, oldest first.
    pub fn steps(&self) -> &[LedgerStep] {
        &self.steps
    }

    /// The last step's entry, for a signal or a compaction flag to be added to.
    pub fn last_step_mut(&mut self) -> Option<&mut LedgerStep> {
        self.steps.last_mut()
    }

    /// The children rolled up so far.
    pub fn children(&self) -> &[ChildLedger] {
        &self.children
    }

    /// Time spent working, in milliseconds.
    pub fn working_ms(&self) -> u64 {
        self.working_ms
    }

    /// Per-role totals, this run's steps plus every child's.
    pub fn totals(&self) -> BTreeMap<ModelRole, RoleTotals> {
        let mut totals: BTreeMap<ModelRole, RoleTotals> = BTreeMap::new();
        for step in self.steps.iter().chain(&self.summaries).chain(&self.asides) {
            totals.entry(step.role).or_default().add_step(step);
        }
        for child in &self.children {
            for (role, child_totals) in &child.totals {
                totals.entry(*role).or_default().add(child_totals);
            }
        }
        totals
    }

    /// Everything, over every role.
    pub fn total(&self) -> RoleTotals {
        let mut total = RoleTotals::default();
        for role_totals in self.totals().values() {
            total.add(role_totals);
        }
        total
    }

    /// The input tokens the last model call reported — the size of the context
    /// that was sent.
    pub fn last_input_tokens(&self) -> Option<u64> {
        self.steps.last().map(|s| s.usage.input_tokens)
    }

    /// This ledger as a child entry for a parent run.
    pub fn as_child(&self, run: RunId, agent: &str) -> ChildLedger {
        ChildLedger {
            run: run.to_string(),
            agent: agent.to_owned(),
            totals: self.totals(),
        }
    }
}

/// A sum where an unknown term makes the whole unknown.
fn add_cost(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    Some(a? + b?)
}

/// The share of `usage`'s input read from the cache.
pub fn cache_hit_ratio(usage: &Usage) -> Option<f64> {
    (usage.input_tokens > 0).then(|| usage.cached_input_tokens as f64 / usage.input_tokens as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(n: u32, role: ModelRole, input: u64, cached: u64, cost: Option<f64>) -> LedgerStep {
        LedgerStep {
            step: n,
            role,
            model: "m".to_owned(),
            usage: Usage {
                input_tokens: input,
                output_tokens: 10,
                cached_input_tokens: cached,
                cache_write_input_tokens: 0,
            },
            cost,
            elapsed_ms: 100,
            signals: Vec::new(),
            compacted: false,
            kind: sc_llm::ModelKind::Chat,
        }
    }

    #[test]
    fn totals_are_per_role_and_include_children() {
        let mut child = Ledger::default();
        child.record_step(step(1, ModelRole::Executor, 100, 50, Some(0.25)));

        let mut ledger = Ledger::default();
        ledger.record_step(step(1, ModelRole::Strong, 1000, 0, Some(1.0)));
        ledger.record_step(step(2, ModelRole::Executor, 200, 150, Some(0.5)));
        let child_id = RunId::new();
        ledger.record_child(child.as_child(child_id, "builder"));
        // Driving the same child again replaces its entry.
        child.record_step(step(2, ModelRole::Executor, 100, 100, Some(0.25)));
        ledger.record_child(child.as_child(child_id, "builder"));
        assert_eq!(ledger.children().len(), 1);

        let totals = ledger.totals();
        assert_eq!(totals[&ModelRole::Strong].steps, 1);
        assert_eq!(totals[&ModelRole::Executor].steps, 3);
        assert_eq!(totals[&ModelRole::Executor].cost, Some(1.0));
        assert_eq!(totals[&ModelRole::Executor].usage.input_tokens, 400);

        let total = ledger.total();
        assert_eq!(total.steps, 4);
        assert_eq!(total.cost, Some(2.0));
        assert_eq!(total.cache_hit_ratio(), Some(300.0 / 1400.0));
        assert_eq!(ledger.last_input_tokens(), Some(200));

        // A summary made while compacting counts for its role, not as a step.
        ledger.record_summary(step(3, ModelRole::Cheap, 50, 0, Some(0.1)));
        assert_eq!(ledger.steps().len(), 2);
        assert_eq!(ledger.totals()[&ModelRole::Cheap].steps, 1);
        assert_eq!(ledger.last_input_tokens(), Some(200));
    }

    #[test]
    fn one_unknown_cost_makes_the_total_unknown() {
        let mut ledger = Ledger::default();
        ledger.record_step(step(1, ModelRole::Executor, 10, 0, Some(1.0)));
        assert_eq!(ledger.total().cost, Some(1.0));
        ledger.record_step(step(2, ModelRole::Executor, 10, 0, None));
        assert_eq!(ledger.total().cost, None);
        // An empty ledger has spent a known nothing.
        assert_eq!(Ledger::default().total().cost, Some(0.0));
    }

    #[test]
    fn the_ledger_round_trips_through_json() {
        let mut ledger = Ledger::default();
        ledger.record_step(step(1, ModelRole::Cheap, 10, 0, None));
        ledger.add_working(std::time::Duration::from_millis(1500));
        let back: Ledger = serde_json::from_value(serde_json::to_value(&ledger).unwrap()).unwrap();
        assert_eq!(back, ledger);
        assert_eq!(back.working_ms(), 1500);
    }
}
