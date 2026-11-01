//! A coding run read from outside it (TODO 10.5): the plan a planner run holds,
//! and the diff of everything the run and its sessions changed.
//!
//! This is what the admin API serves beside a run, so it works from the stored
//! rows alone — the run, the runs it delegated to, and the agents they were of
//! — and never from a live loop.
//!
//! **Children are included.** A planned run changes nothing itself: its
//! features' sessions do, each in a run of its own. So the diff walks the run's
//! children (the ledger's rolled-up entries, and the session runs its plan
//! records, which include one that is still running), and folds each `coding`
//! instance's change ledger into one per scope, in the order the runs were
//! created. The earliest pre-image of a path wins, which is right for sessions
//! that follow one another; a parent that edited a path *after* its child did
//! would show the child's pre-image instead, and no run does that today.

use std::collections::BTreeMap;

use sc_agent::{Agent, AgentLoop, Run, RunId, load_agent_by_name, load_run, trait_state_key};
use sc_catalog::Catalog;
use sc_error::Result;
use uuid::Uuid;

use super::ledger::{Ledger, RunDiff, diff_ledger};
use super::plan::Plan;
use super::state::CodingState;
use crate::files::{FileScope, configured_scope};

/// The trait whose state this reads.
const CODING: &str = "coding";

/// The deepest delegation followed: an agent's own sessions go two deep
/// (a feature's session, and its `explore`), and a `subagent` chain is capped
/// by its own setting below this.
const MAX_DEPTH: usize = 8;

/// The plan a planner run holds: the first `coding` instance's that has one.
pub fn run_plan(state: &AgentLoop) -> Option<Plan> {
    state
        .trait_states()
        .filter(|(key, _)| key.ends_with(&format!(":{CODING}")))
        .find_map(|(_, json)| CodingState::load(json).plan)
}

/// One scope's changes over a run and its children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDiff {
    /// The store and directory the changes are in.
    pub scope: FileScope,
    /// What changed, against what the store holds now.
    pub diff: RunDiff,
}

/// Everything `run` and the runs it delegated to changed, one entry per
/// `coding` scope that changed anything, and the ids of the runs looked at.
///
/// A child run that has since been deleted is skipped, and so is a run whose
/// agent has been deleted: the scope its ledger's paths are in is that agent's
/// configuration, and without it the paths cannot be placed.
pub async fn agent_run_diff(catalog: &Catalog, run: &Run) -> Result<(Vec<ScopeDiff>, Vec<RunId>)> {
    let runs = run_tree(catalog, run).await?;

    let mut agents: BTreeMap<String, Option<Agent>> = BTreeMap::new();
    let mut ledgers: Vec<(FileScope, Ledger)> = Vec::new();
    for (run, state) in &runs {
        if !agents.contains_key(&run.subject) {
            let agent = load_agent_by_name(catalog, &run.subject).await?;
            agents.insert(run.subject.clone(), agent);
        }
        let Some(agent) = agents.get(&run.subject).and_then(Option::as_ref) else {
            continue;
        };
        for (index, enabled) in agent.traits.iter().enumerate() {
            if enabled.trait_ != CODING {
                continue;
            }
            let Some(json) = state.trait_state(&trait_state_key(index, CODING)) else {
                continue;
            };
            let Ok(scope) = configured_scope(&enabled.config) else {
                continue;
            };
            let ledger = CodingState::load(json).ledger;
            match ledgers.iter_mut().find(|(s, _)| *s == scope) {
                Some((_, into)) => into.absorb(&ledger),
                None => ledgers.push((scope, ledger)),
            }
        }
    }

    let mut out = Vec::new();
    for (scope, ledger) in ledgers {
        if ledger.is_empty() {
            continue;
        }
        let (store, _) = scope.connect(catalog).await?;
        let diff = diff_ledger(&scope, store.as_ref(), &ledger).await?;
        if !diff.is_empty() || !diff.moves.is_empty() {
            out.push(ScopeDiff { scope, diff });
        }
    }
    Ok((out, runs.iter().map(|(run, _)| run.id).collect()))
}

/// `run` and every run it delegated to, **oldest first**: the ledger's rolled-up
/// children and the session runs its plan records, each with its loop state.
///
/// What "a run" means for anything that has to add a planned run up — the diff
/// below, and the eval harness's metrics (TODO §13) — since a planner run's own
/// ledger holds only what the planner itself spent.
pub async fn run_tree(catalog: &Catalog, run: &Run) -> Result<Vec<(Run, AgentLoop)>> {
    let mut runs: Vec<(Run, AgentLoop)> = Vec::new();
    collect(catalog, run.clone(), 0, &mut runs).await?;
    runs.sort_by_key(|(run, _)| run.created_at);
    Ok(runs)
}

/// `run` and its descendants, depth first.
async fn collect(
    catalog: &Catalog,
    run: Run,
    depth: usize,
    into: &mut Vec<(Run, AgentLoop)>,
) -> Result<()> {
    if into.iter().any(|(seen, _)| seen.id == run.id) {
        return Ok(());
    }
    let state = run.agent_loop()?;
    let mut children: Vec<String> = state
        .ledger()
        .children()
        .iter()
        .map(|child| child.run.clone())
        .collect();
    // A session still running has not been rolled up yet, but its plan entry
    // was saved before it started.
    if let Some(plan) = run_plan(&state) {
        for feature in &plan.features {
            children.extend(feature.runs.iter().cloned());
        }
    }
    into.push((run, state));
    if depth >= MAX_DEPTH {
        return Ok(());
    }
    for child in children {
        let Ok(id) = Uuid::parse_str(&child) else {
            continue;
        };
        if let Some(child) = load_run(catalog, RunId(id)).await? {
            Box::pin(collect(catalog, child, depth + 1, into)).await?;
        }
    }
    Ok(())
}
