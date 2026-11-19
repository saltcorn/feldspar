//! How far a run in progress has got, for a screen to show while it waits.
//!
//! A run is long — minutes for a checklist that drives a browser — so a caller
//! that wants to show it moving gives the run a sink
//! ([`CheckEnv::with_progress`](crate::CheckEnv::with_progress)), and the
//! runner hands it an [`Event`] as each checklist and step begins and ends.
//! The admin's run socket is the caller that does: it runs the checklist on
//! the connection's own node and pushes a [`Progress`] after every event, so
//! there is nothing to poll and nothing to share between nodes.
//!
//! A run given no sink (the CLI, agents, schedules, a build's checks) reports
//! nothing, and costs nothing.

use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::model::{Outcome, StepResult};

/// What the runner reports, in the order it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The request runs `total` checklists (only when it runs several).
    ChecklistsKnown { total: usize },
    /// A checklist's run has begun.
    ChecklistStarted { name: String },
    /// The checklist's steps are read: `total` of them.
    StepsKnown { total: usize },
    /// Step `index` (0-based) has begun.
    StepStarted {
        index: usize,
        label: String,
        type_: String,
    },
    /// A step has a result (run or skipped).
    StepFinished {
        label: String,
        type_: String,
        outcome: Outcome,
    },
    /// The checklist's run is recorded as `run`.
    ChecklistFinished { run: Uuid },
}

impl Event {
    pub(crate) fn step_finished(result: &StepResult) -> Event {
        Event::StepFinished {
            label: result.label.clone(),
            type_: result.type_.clone(),
            outcome: result.outcome,
        }
    }
}

/// Where a run's [`Event`]s go. Called inline from the runner, so it must not
/// block: hand the event on (a channel) and return.
pub type Sink<'a> = &'a (dyn Fn(Event) + Send + Sync);

/// One finished step, as the progress shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Done {
    label: String,
    type_: String,
    outcome: Outcome,
}

/// Everything the events so far say, folded: what a screen shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    checklists_total: Option<usize>,
    checklists_done: usize,
    checklist: Option<String>,
    total: Option<usize>,
    current: Option<(usize, String, String)>,
    done: Vec<Done>,
}

impl Progress {
    /// Fold in the next event.
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::ChecklistsKnown { total } => self.checklists_total = Some(total),
            Event::ChecklistStarted { name } => {
                self.checklist = Some(name);
                self.total = None;
                self.current = None;
                self.done.clear();
            }
            Event::StepsKnown { total } => self.total = Some(total),
            Event::StepStarted {
                index,
                label,
                type_,
            } => self.current = Some((index, label, type_)),
            Event::StepFinished {
                label,
                type_,
                outcome,
            } => {
                self.current = None;
                self.done.push(Done {
                    label,
                    type_,
                    outcome,
                });
            }
            Event::ChecklistFinished { .. } => {
                self.current = None;
                self.checklists_done += 1;
            }
        }
    }

    /// As JSON: `checklist` the one running, `checklists_done`/`_total` the
    /// count when the request runs several, `total` its step count, `step`
    /// the running step's 1-based number with its `label` and `type`, and
    /// `steps` what the finished ones did.
    pub fn to_json(&self) -> Json {
        let (step, label, type_) = match &self.current {
            Some((index, label, type_)) => (
                Json::from(index + 1),
                Json::from(label.clone()),
                Json::from(type_.clone()),
            ),
            None => (Json::Null, Json::Null, Json::Null),
        };
        json!({
            "checklist": self.checklist,
            "checklists_done": self.checklists_done,
            "checklists_total": self.checklists_total,
            "total": self.total,
            "step": step,
            "label": label,
            "type": type_,
            "steps": self.done.iter().map(|d| json!({
                "label": d.label,
                "type": d.type_,
                "outcome": d.outcome.as_str(),
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_events_fold_into_what_a_screen_shows() {
        let mut p = Progress::default();
        p.apply(Event::ChecklistsKnown { total: 2 });
        p.apply(Event::ChecklistStarted { name: "A".into() });
        p.apply(Event::StepsKnown { total: 2 });
        p.apply(Event::StepStarted {
            index: 0,
            label: "Look".into(),
            type_: "core.manual".into(),
        });
        let j = p.to_json();
        assert_eq!(j["step"], json!(1));
        assert_eq!(j["label"], json!("Look"));
        p.apply(Event::StepFinished {
            label: "Look".into(),
            type_: "core.manual".into(),
            outcome: Outcome::Pass,
        });
        let j = p.to_json();
        assert_eq!(j["step"], Json::Null);
        assert_eq!(j["steps"][0]["outcome"], json!("pass"));

        // The next checklist starts with a clean slate but keeps the count.
        p.apply(Event::ChecklistFinished { run: Uuid::nil() });
        p.apply(Event::ChecklistStarted { name: "B".into() });
        let j = p.to_json();
        assert_eq!(j["checklist"], json!("B"));
        assert_eq!(j["checklists_done"], json!(1));
        assert_eq!(j["checklists_total"], json!(2));
        assert_eq!(j["total"], Json::Null);
        assert_eq!(j["steps"], json!([]));
    }
}
