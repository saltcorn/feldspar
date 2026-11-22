//! Saving a construction's phases (TODO-app-constructor AC2.1, AC2.3).
//!
//! Both the agent's `save_phases` tool and the admin's `savePhases` endpoint
//! call [`save_phases`]. They share the same checks, so the agent and the admin
//! are allowed the same lists and get the same error messages.
//!
//! A saved list replaces the construction's phases:
//!
//! - A phase in the list with an `id` updates that existing phase. Its tasks stay.
//! - A phase in the list without an `id` is new.
//! - An existing phase missing from the list is deleted, unless it already has
//!   tasks. Then it is kept, placed before the list's phases, and the result
//!   names it. This way, work that has started is never lost.
//!
//! The save runs in one transaction that locks the phases, so a task added at
//! the same moment is never left without its phase.

use std::collections::HashSet;

use sc_catalog::Catalog;
use sc_db::Transaction;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Statement, Update, Value};
use serde::Deserialize;
use uuid::Uuid;

use crate::model::{
    Construction, ConstructionStatus, Phase, PhaseWithTasks, Requirement, spelling,
};
use crate::store::{
    self, CONSTRUCTIONS_TABLE, PHASES_TABLE, lock_phases, phase_ids_with_tasks, phase_insert,
    require_construction, tx_run,
};

/// The most phases one construction may have: v1's plans had three to six,
/// and a longer list is a sign the phases are tasks.
pub const MAX_PHASES: usize = 12;

/// The longest a phase's title may be.
pub const MAX_TITLE: usize = 200;

/// One phase as the tool and the API take it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseInput {
    /// An existing phase of the construction, updated in place; absent for a
    /// new one.
    #[serde(default)]
    pub id: Option<Uuid>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requirements: Vec<Requirement>,
}

/// What [`save_phases`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedPhases {
    pub construction: Construction,
    /// The phases now, in order, each with its tasks.
    pub phases: Vec<PhaseWithTasks>,
    /// The titles of the phases kept although the list left them out, because
    /// they have tasks.
    pub kept: Vec<String>,
}

/// Parse a phase list from JSON, naming what is wrong with it.
pub fn parse_phases(value: &serde_json::Value) -> Result<Vec<PhaseInput>> {
    let list = value.as_array().ok_or_else(|| {
        Error::invalid("`phases` must be a list of `{id?, title, description, requirements}`")
    })?;
    list.iter()
        .enumerate()
        .map(|(i, item)| {
            serde_json::from_value::<PhaseInput>(item.clone())
                .map_err(|e| Error::invalid(format!("phase {}: {e}", i + 1)))
        })
        .collect()
}

/// Replace the phases of `application`'s construction with `inputs`, in their
/// order. A construction with phases is `planned`; one emptied by the list
/// cannot be, since the list may not be empty.
pub async fn save_phases(
    catalog: &Catalog,
    application: Uuid,
    inputs: &[PhaseInput],
) -> Result<SavedPhases> {
    let construction = require_construction(catalog, application).await?;
    if construction.status == ConstructionStatus::Running {
        return Err(Error::invalid(
            "the construction is running; stop it before changing its phases",
        ));
    }
    let inputs = inputs.to_vec();
    let kept = sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| Box::pin(write_phases(tx, construction, inputs)),
    )
    .await?;

    let construction = require_construction(catalog, application).await?;
    let phases = store::phases_with_tasks(catalog, construction.id).await?;
    Ok(SavedPhases {
        construction,
        phases,
        kept: kept.into_iter().map(|p| p.title).collect(),
    })
}

/// The transactional part of [`save_phases`]: read the current phases,
/// decide which to keep, update or delete, and write the changes.
///
/// It first locks the construction's phases. Adding a task to one of them
/// ([`store::insert_task`]) then waits until this transaction ends. So a phase
/// that has no tasks when it is checked still has none when it is deleted.
///
/// Returns the phases that were kept because they have tasks.
async fn write_phases(
    tx: &mut dyn Transaction,
    construction: Construction,
    inputs: Vec<PhaseInput>,
) -> Result<Vec<Phase>> {
    let existing = lock_phases(tx, construction.id).await?;
    let inputs = validate(&inputs, &existing)?;

    // Phases the list leaves out: deleted, or kept when they have tasks.
    let listed_ids: HashSet<Uuid> = inputs.iter().filter_map(|p| p.id).collect();
    let dropped: Vec<&Phase> = existing
        .iter()
        .filter(|p| !listed_ids.contains(&p.id))
        .collect();
    let dropped_ids: Vec<Uuid> = dropped.iter().map(|p| p.id).collect();
    let with_tasks = phase_ids_with_tasks(tx, &dropped_ids).await?;
    let (kept, deleted): (Vec<&Phase>, Vec<&Phase>) = dropped
        .into_iter()
        .partition(|p| with_tasks.contains(&p.id));
    let deleted: Vec<Uuid> = deleted.into_iter().map(|p| p.id).collect();
    let kept: Vec<Phase> = kept.into_iter().cloned().collect();

    let mut statements = Vec::new();
    for id in deleted {
        statements.push(Statement::from(
            Delete::from(PHASES_TABLE).filter(Expr::col("id").eq(Expr::lit(id))),
        ));
    }
    // Kept phases first, in the order they had: they were reached before
    // anything the new list adds.
    let mut position = 0.0;
    for phase in &kept {
        position += 1.0;
        statements.push(Statement::from(
            Update::new(
                PHASES_TABLE,
                vec![Assignment::new(
                    "position",
                    Expr::Lit(Value::Float(position)),
                )],
            )
            .filter(Expr::col("id").eq(Expr::lit(phase.id))),
        ));
    }
    for input in &inputs {
        position += 1.0;
        let phase = Phase {
            id: input.id.unwrap_or_else(Uuid::new_v4),
            construction: construction.id,
            position,
            title: input.title.trim().to_owned(),
            description: input.description.trim().to_owned(),
            requirements: input.requirements.clone(),
        };
        statements.push(match input.id {
            Some(id) => Statement::from(
                Update::new(
                    PHASES_TABLE,
                    vec![
                        Assignment::new("position", Expr::Lit(Value::Float(position))),
                        Assignment::new("title", Expr::Lit(Value::Text(phase.title.clone()))),
                        Assignment::new(
                            "description",
                            Expr::Lit(Value::Text(phase.description.clone())),
                        ),
                        Assignment::new(
                            "requirements",
                            Expr::Lit(Value::Json(store::to_json(&phase.requirements)?)),
                        ),
                    ],
                )
                .filter(Expr::col("id").eq(Expr::lit(id))),
            ),
            None => Statement::from(phase_insert(&phase)?),
        });
    }
    let mut assignments = vec![Assignment::new(
        sc_catalog::COL_UPDATED_AT,
        Expr::Lit(sc_catalog::updated_now()),
    )];
    if construction.status == ConstructionStatus::Draft {
        assignments.push(Assignment::new(
            "status",
            Expr::Lit(Value::Text(spelling(ConstructionStatus::Planned))),
        ));
    }
    statements.push(Statement::from(
        Update::new(CONSTRUCTIONS_TABLE, assignments)
            .filter(Expr::col("id").eq(Expr::lit(construction.id))),
    ));
    for statement in &statements {
        tx_run(tx, statement).await?;
    }
    Ok(kept)
}

/// The phases as the agent reads them: one numbered line each, with its id,
/// status and requirements. What every `save_phases` result ends with, and
/// what the agent's session header shows.
pub fn phase_list(phases: &[PhaseWithTasks]) -> String {
    if phases.is_empty() {
        return "No phases yet.".to_owned();
    }
    let mut out = String::new();
    for (i, with_tasks) in phases.iter().enumerate() {
        let phase = &with_tasks.phase;
        out.push_str(&format!(
            "{}. {} [{}] (id {})\n",
            i + 1,
            phase.title,
            spelling(with_tasks.status()),
            phase.id
        ));
        if !phase.description.is_empty() {
            out.push_str(&format!("   {}\n", phase.description));
        }
        for requirement in &phase.requirements {
            out.push_str(&format!(
                "   - ({}) {}\n",
                spelling(requirement.priority),
                requirement.text
            ));
        }
    }
    out.trim_end().to_owned()
}

/// Refuse a list that cannot be saved, with the reason; answer it trimmed.
fn validate(inputs: &[PhaseInput], existing: &[Phase]) -> Result<Vec<PhaseInput>> {
    if inputs.is_empty() {
        return Err(Error::invalid(
            "a construction needs at least one phase; the list is empty",
        ));
    }
    if inputs.len() > MAX_PHASES {
        return Err(Error::invalid(format!(
            "{} phases are too many: at most {MAX_PHASES}. A phase delivers a usable part of \
             the application; smaller steps are its tasks",
            inputs.len()
        )));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(inputs.len());
    for (i, input) in inputs.iter().enumerate() {
        let n = i + 1;
        let title = input.title.trim();
        if title.is_empty() {
            return Err(Error::invalid(format!("phase {n} has no title")));
        }
        if title.chars().count() > MAX_TITLE {
            return Err(Error::invalid(format!(
                "phase {n}'s title is longer than {MAX_TITLE} characters; put the detail in its \
                 description"
            )));
        }
        if let Some(id) = input.id {
            if !existing.iter().any(|p| p.id == id) {
                return Err(Error::invalid(format!(
                    "phase {n} names id {id}, which is not a phase of this construction; leave \
                     `id` out for a new phase"
                )));
            }
            if !seen.insert(id) {
                return Err(Error::invalid(format!(
                    "phase {n} names id {id} a second time"
                )));
            }
        }
        let mut requirements = Vec::with_capacity(input.requirements.len());
        for (j, requirement) in input.requirements.iter().enumerate() {
            let text = requirement.text.trim();
            if text.is_empty() {
                return Err(Error::invalid(format!(
                    "phase {n}'s requirement {} has no text",
                    j + 1
                )));
            }
            requirements.push(Requirement {
                text: text.to_owned(),
                priority: requirement.priority,
            });
        }
        out.push(PhaseInput {
            id: input.id,
            title: title.to_owned(),
            description: input.description.trim().to_owned(),
            requirements,
        });
    }
    Ok(out)
}
