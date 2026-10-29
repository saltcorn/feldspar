//! Changing checklists, and who may (docs/CHECKLISTS.md §4, TODO C1.13, C1.14).
//!
//! Every change goes through here, whoever makes it — the admin API for a
//! person, the tools for an agent — so the rules below are stated once:
//!
//! - **A person may do anything.** Their steps are accepted as they write them.
//! - **An agent needs the `checklists` grant** (`none | append | full`). Under
//!   `append`, the default:
//!   1. it may create checklists and append steps to the end of any checklist —
//!      safe by construction, since a step at the end cannot change what any
//!      earlier step does or reports;
//!   2. it may edit or remove the steps **its own run** wrote, until a person
//!      accepts them — iterating on its own new check is how a check gets
//!      written;
//!   3. every other change — editing an existing step, inserting before one,
//!      reordering, changing `run_as` — is a **proposal**, stored with its
//!      reason; the old step keeps running until a person accepts it, so the
//!      checklist stays red, which is the honest state;
//!   4. it may never delete a checklist, delete an accepted step, disable a
//!      checklist, allow one on production, or point `run_as` at a user who is
//!      not a check user.
//!
//!   Under `full` the agent's changes apply directly, as a person's would —
//!   except rule 4's last three, which stay a person's under every grant: they
//!   are how a person keeps the checks honest, and an agent with narrow data
//!   grants must not be able to run `data.delete` as an admin.
//! - **Steps are never updated in place**: an edit writes a new version and
//!   points the old one at it; a deletion points it at a tombstone.
//! - **`mutates` is recomputed** from the current steps' types after every
//!   change.

use chrono::Utc;
use sc_app::Application;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::model::{
    ATTR_ALLOW_ON_PRODUCTION, Author, Checklist, Proposal, ProposalChange, Step, StepDraft,
};
use crate::registry::StepRegistry;
use crate::store;
use crate::vars;

/// The key the grant is stored under — on an `admin_copilot` agent's
/// configuration and on an MCP token's grants alike.
pub const GRANT_KEY: &str = sc_api::mcp::CHECKLISTS_GRANT;

/// What an agent may do to checklists (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecklistGrant {
    /// Nothing at all; it may still read them.
    None,
    /// Create, append, edit its own unaccepted steps; propose the rest.
    Append,
    /// Maintain checks unsupervised. Never the default.
    Full,
}

impl ChecklistGrant {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ChecklistGrant::None => "none",
            ChecklistGrant::Append => "append",
            ChecklistGrant::Full => "full",
        }
    }

    /// The grant a configuration or a token holds: `append` unless it says
    /// otherwise.
    pub fn from_attrs(attrs: &Attrs) -> ChecklistGrant {
        match attrs.get(GRANT_KEY).and_then(Json::as_str) {
            Some("none") => ChecklistGrant::None,
            Some("full") => ChecklistGrant::Full,
            _ => ChecklistGrant::Append,
        }
    }
}

/// Who is making a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editor {
    /// A person, by user id.
    Human(Uuid),
    /// An agent's run, under its grant.
    Agent {
        /// The run (see [`Author::Agent`]).
        run: String,
        /// What it may do.
        grant: ChecklistGrant,
    },
}

impl Editor {
    /// Who the change is recorded as written by.
    pub fn author(&self) -> Author {
        match self {
            Editor::Human(id) => Author::Human(*id),
            Editor::Agent { run, .. } => Author::Agent(run.clone()),
        }
    }

    /// Whether a person is making it.
    pub fn is_human(&self) -> bool {
        matches!(self, Editor::Human(_))
    }

    fn grant(&self) -> Option<ChecklistGrant> {
        match self {
            Editor::Human(_) => None,
            Editor::Agent { grant, .. } => Some(*grant),
        }
    }

    /// Whether this change applies directly: a person's, or an agent's under
    /// `full`.
    fn direct(&self) -> bool {
        matches!(self.grant(), None | Some(ChecklistGrant::Full))
    }

    /// Whether the step is one this agent's run wrote and nobody accepted.
    fn owns(&self, step: &Step) -> bool {
        match self {
            Editor::Human(_) => false,
            Editor::Agent { run, .. } => {
                !step.accepted && step.author == Author::Agent(run.clone())
            }
        }
    }

    /// Refuse an agent without at least `append`.
    fn require_append(&self, what: &str) -> Result<()> {
        if self.grant() == Some(ChecklistGrant::None) {
            return Err(refusal(what, ChecklistGrant::Append));
        }
        Ok(())
    }
}

/// The refusal an agent reads, naming the grant that is missing.
fn refusal(what: &str, needed: ChecklistGrant) -> Error {
    Error::auth(format!(
        "not permitted to {what}; nothing was changed. That needs the `{GRANT_KEY}: {}` grant \
         (an agent's `admin_copilot` settings, or an API token's grants).",
        needed.as_str()
    ))
}

/// The refusal for what only a person may do.
fn humans_only(what: &str) -> Error {
    Error::auth(format!(
        "only a person may {what}; nothing was changed. Ask the admin to do it in the \
         Checklists tab."
    ))
}

/// A checklist's settings as a caller submits them.
#[derive(Debug, Clone, PartialEq)]
pub struct ChecklistInput {
    /// Its name, unique across the instance.
    pub name: String,
    /// What it checks.
    pub description: String,
    /// The application it checks, if any.
    pub application: Option<Uuid>,
    /// Who it runs as, or nobody (an anonymous visitor).
    pub run_as: Option<Uuid>,
    /// Whether it runs with the others.
    pub enabled: bool,
    /// Its sparse settings.
    pub attributes: Attrs,
}

impl ChecklistInput {
    /// A checklist called `name` with nothing else set.
    pub fn new(name: impl Into<String>) -> ChecklistInput {
        ChecklistInput {
            name: name.into(),
            description: String::new(),
            application: None,
            run_as: None,
            enabled: true,
            attributes: Attrs::new(),
        }
    }
}

/// What an agent's change came to: applied, or stored for a person.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// It took effect.
    Applied,
    /// It waits for a person.
    Proposed(Box<Proposal>),
}

/// Create a checklist with these steps.
pub async fn create_checklist(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    input: ChecklistInput,
    steps: Vec<StepDraft>,
) -> Result<Checklist> {
    editor.require_append("create a checklist")?;
    let name = input.name.trim().to_owned();
    if name.is_empty() {
        return Err(Error::invalid("a checklist needs a name"));
    }
    if store::load_checklist_by_name(catalog, &name).await?.is_some() {
        return Err(Error::invalid(format!(
            "there is already a checklist called `{name}`; checklists are named uniquely"
        )));
    }
    if !editor.is_human() {
        if !input.enabled {
            return Err(humans_only("disable a checklist"));
        }
        if input.attributes.contains_key(ATTR_ALLOW_ON_PRODUCTION) {
            return Err(humans_only("allow a checklist on production"));
        }
    }
    check_run_as(catalog, editor, input.run_as).await?;
    check_application(catalog, input.application).await?;
    let checklist = Checklist {
        id: Uuid::new_v4(),
        name,
        description: input.description,
        application: input.application,
        run_as: input.run_as,
        enabled: input.enabled,
        mutates: false,
        attributes: input.attributes,
    };
    let application = application_of(catalog, &checklist).await?;
    for draft in &steps {
        validate_draft(registry, &checklist, application.as_ref(), draft)?;
    }
    store::write_checklist(catalog, &checklist).await?;
    let mut position = 0.0;
    for draft in steps {
        position += 1.0;
        store::insert_step(catalog, &new_step(&checklist, position, draft, editor)).await?;
    }
    refresh_mutates(catalog, registry, checklist.id).await?;
    store::require_checklist(catalog, checklist.id).await
}

/// Change a checklist's settings. A person's alone: an agent proposes a
/// `run_as` change instead ([`request_change`]).
pub async fn update_checklist(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    id: Uuid,
    input: ChecklistInput,
) -> Result<Checklist> {
    if !editor.is_human() {
        return Err(humans_only("change a checklist's settings"));
    }
    let existing = store::require_checklist(catalog, id).await?;
    let name = input.name.trim().to_owned();
    if name.is_empty() {
        return Err(Error::invalid("a checklist needs a name"));
    }
    if let Some(other) = store::load_checklist_by_name(catalog, &name).await?
        && other.id != id
    {
        return Err(Error::invalid(format!(
            "there is already a checklist called `{name}`"
        )));
    }
    check_run_as(catalog, editor, input.run_as).await?;
    check_application(catalog, input.application).await?;
    let checklist = Checklist {
        name,
        description: input.description,
        application: input.application,
        run_as: input.run_as,
        enabled: input.enabled,
        attributes: input.attributes,
        ..existing
    };
    // A step that reaches the application cannot stay on a checklist that no
    // longer has one, and a framework's step cannot stay on a checklist whose
    // application is now of another framework.
    if checklist.application != existing.application {
        let application = application_of(catalog, &checklist).await?;
        for step in store::current_steps(catalog, id).await? {
            validate_draft(registry, &checklist, application.as_ref(), &step.draft)?;
        }
    }
    store::write_checklist(catalog, &checklist).await?;
    refresh_mutates(catalog, registry, id).await?;
    store::require_checklist(catalog, id).await
}

/// Delete a checklist with its steps and proposals (its runs are kept). A
/// person's, or an agent's under `full`.
pub async fn delete_checklist(catalog: &Catalog, editor: &Editor, id: Uuid) -> Result<()> {
    if !editor.direct() {
        return Err(refusal("delete a checklist", ChecklistGrant::Full));
    }
    store::require_checklist(catalog, id).await?;
    store::delete_checklist_rows(catalog, id).await
}

/// Add steps at the end of a checklist.
pub async fn append_steps(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    checklist: Uuid,
    drafts: Vec<StepDraft>,
) -> Result<Vec<Step>> {
    editor.require_append("add steps to a checklist")?;
    let checklist = store::require_checklist(catalog, checklist).await?;
    let application = application_of(catalog, &checklist).await?;
    for draft in &drafts {
        validate_draft(registry, &checklist, application.as_ref(), draft)?;
    }
    let mut position = store::step_history(catalog, checklist.id)
        .await?
        .iter()
        .map(|s| s.position)
        .fold(0.0_f64, f64::max);
    let mut written = Vec::new();
    for draft in drafts {
        position = position.floor() + 1.0;
        let step = new_step(&checklist, position, draft, editor);
        store::insert_step(catalog, &step).await?;
        written.push(step);
    }
    refresh_mutates(catalog, registry, checklist.id).await?;
    Ok(written)
}

/// Replace a step with a new version.
pub async fn edit_step(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    step: Uuid,
    draft: StepDraft,
) -> Result<Step> {
    let old = current(catalog, step).await?;
    if !editor.direct() && !editor.owns(&old) {
        return Err(Error::auth(
            "this step is not one your run wrote and nobody has accepted, so it cannot be \
             edited directly: propose the change instead (propose_step_change), with your \
             reason, and a person will accept or reject it",
        ));
    }
    let checklist = store::require_checklist(catalog, old.checklist).await?;
    let application = application_of(catalog, &checklist).await?;
    validate_draft(registry, &checklist, application.as_ref(), &draft)?;
    let new = new_step(&checklist, old.position, draft, editor);
    store::insert_step(catalog, &new).await?;
    store::supersede(catalog, old.id, new.id).await?;
    refresh_mutates(catalog, registry, checklist.id).await?;
    Ok(new)
}

/// Delete a step: point it at a tombstone, so the deletion can be undone from
/// the history.
pub async fn remove_step(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    step: Uuid,
) -> Result<()> {
    let old = current(catalog, step).await?;
    if !editor.direct() && !editor.owns(&old) {
        return Err(if old.accepted {
            refusal("delete an accepted step", ChecklistGrant::Full)
        } else {
            refusal(
                "delete a step another run wrote",
                ChecklistGrant::Full,
            )
        });
    }
    let tombstone = Step {
        id: Uuid::new_v4(),
        deleted: true,
        author: editor.author(),
        accepted: editor.is_human(),
        created_at: Utc::now(),
        ..old.clone()
    };
    store::insert_step(catalog, &tombstone).await?;
    store::supersede(catalog, old.id, tombstone.id).await?;
    refresh_mutates(catalog, registry, old.checklist).await
}

/// Put a new step before an existing one.
pub async fn insert_step_before(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    before: Uuid,
    draft: StepDraft,
) -> Result<Step> {
    let anchor = current(catalog, before).await?;
    if !editor.direct() && !only_own_from(catalog, editor, &anchor).await? {
        return Err(Error::auth(
            "inserting a step before an existing one changes what the steps after it see, so \
             it is a proposal: use propose_step_change with your reason",
        ));
    }
    let checklist = store::require_checklist(catalog, anchor.checklist).await?;
    let application = application_of(catalog, &checklist).await?;
    validate_draft(registry, &checklist, application.as_ref(), &draft)?;
    let position = position_before(catalog, &anchor).await?;
    let step = new_step(&checklist, position, draft, editor);
    store::insert_step(catalog, &step).await?;
    refresh_mutates(catalog, registry, checklist.id).await?;
    Ok(step)
}

/// Run the current steps in this order: every step that moves gets a new
/// version at its new place (with its author and accepted state, since a move
/// is not a rewrite).
pub async fn reorder_steps(
    catalog: &Catalog,
    editor: &Editor,
    checklist: Uuid,
    order: &[Uuid],
) -> Result<()> {
    if !editor.direct() {
        return Err(Error::auth(
            "reordering changes what each step sees, so it is a proposal: use \
             propose_step_change with your reason",
        ));
    }
    let steps = store::current_steps(catalog, checklist).await?;
    let mut given: Vec<Uuid> = order.to_vec();
    given.sort();
    let mut have: Vec<Uuid> = steps.iter().map(|s| s.id).collect();
    have.sort();
    if given != have {
        return Err(Error::invalid(
            "the order should list every current step of the checklist exactly once",
        ));
    }
    for (index, id) in order.iter().enumerate() {
        let position = (index + 1) as f64;
        let Some(old) = steps.iter().find(|s| s.id == *id) else {
            continue;
        };
        if old.position == position {
            continue;
        }
        let moved = Step {
            id: Uuid::new_v4(),
            position,
            created_at: Utc::now(),
            ..old.clone()
        };
        store::insert_step(catalog, &moved).await?;
        store::supersede(catalog, old.id, moved.id).await?;
    }
    Ok(())
}

/// Mark a step seen. A person's alone.
pub async fn accept_step(catalog: &Catalog, editor: &Editor, step: Uuid) -> Result<Step> {
    if !editor.is_human() {
        return Err(humans_only("accept a step"));
    }
    let step = current(catalog, step).await?;
    store::set_accepted(catalog, step.id).await?;
    store::require_step(catalog, step.id).await
}

/// An agent's change to an existing step or to `run_as`: applied when the
/// rules allow it directly (its own unaccepted step, or `full`), stored as a
/// proposal otherwise. A person's change always applies.
pub async fn request_change(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    checklist: Uuid,
    change: ProposalChange,
    reason: &str,
) -> Result<Change> {
    editor.require_append("change a checklist")?;
    let list = store::require_checklist(catalog, checklist).await?;
    // Checked whether it applies or waits: a proposal a person accepts must be
    // one that could have been made.
    match &change {
        ProposalChange::Edit { step, draft } | ProposalChange::InsertBefore { before: step, draft } => {
            let target = current(catalog, *step).await?;
            if target.checklist != list.id {
                return Err(Error::invalid(format!(
                    "step {step} belongs to another checklist"
                )));
            }
            let application = application_of(catalog, &list).await?;
            validate_draft(registry, &list, application.as_ref(), draft)?;
        }
        ProposalChange::Reorder { order } => {
            let mut have: Vec<Uuid> = store::current_steps(catalog, list.id)
                .await?
                .iter()
                .map(|s| s.id)
                .collect();
            have.sort();
            let mut given = order.clone();
            given.sort();
            if have != given {
                return Err(Error::invalid(
                    "the order should list every current step of the checklist exactly once",
                ));
            }
        }
        ProposalChange::RunAs { user } => check_run_as(catalog, editor, *user).await?,
    }
    let direct = editor.direct()
        || match &change {
            ProposalChange::Edit { step, .. } => editor.owns(&current(catalog, *step).await?),
            ProposalChange::InsertBefore { before, .. } => {
                only_own_from(catalog, editor, &current(catalog, *before).await?).await?
            }
            _ => false,
        };
    if direct {
        apply(catalog, registry, editor, &list, change).await?;
        return Ok(Change::Applied);
    }
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(Error::invalid(
            "a proposal needs its `reason`: the person deciding reads it beside the last \
             run's failure",
        ));
    }
    let proposal = Proposal {
        id: Uuid::new_v4(),
        checklist: list.id,
        change,
        reason: reason.to_owned(),
        author: editor.author(),
        created_at: Utc::now(),
    };
    store::insert_proposal(catalog, &proposal).await?;
    Ok(Change::Proposed(Box::new(proposal)))
}

/// Accept a proposal: it takes effect (the new step version is the agent's,
/// accepted), and the proposal is gone. A person's alone.
pub async fn accept_proposal(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    id: Uuid,
) -> Result<()> {
    if !editor.is_human() {
        return Err(humans_only("accept a proposal"));
    }
    let proposal = store::load_proposal(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no proposal with id {id}")))?;
    let checklist = store::require_checklist(catalog, proposal.checklist).await?;
    if let Some(step) = proposal.change.step()
        && current(catalog, step).await.is_err()
    {
        return Err(Error::invalid(
            "the step this proposal changes has changed since it was made; reject it, and ask \
             for a new one if it is still wanted",
        ));
    }
    // Applied as the person accepting it, but the new version is recorded as
    // the agent's: it wrote it, and a person has now seen it.
    let as_proposed = AcceptedAs {
        author: proposal.author.clone(),
    };
    apply_accepted(catalog, registry, &checklist, proposal.change, &as_proposed, editor).await?;
    store::delete_proposal(catalog, id).await
}

/// Reject a proposal: it is discarded. A person's alone.
pub async fn reject_proposal(catalog: &Catalog, editor: &Editor, id: Uuid) -> Result<()> {
    if !editor.is_human() {
        return Err(humans_only("reject a proposal"));
    }
    store::load_proposal(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no proposal with id {id}")))?;
    store::delete_proposal(catalog, id).await
}

struct AcceptedAs {
    author: Author,
}

async fn apply(
    catalog: &Catalog,
    registry: &StepRegistry,
    editor: &Editor,
    checklist: &Checklist,
    change: ProposalChange,
) -> Result<()> {
    match change {
        ProposalChange::Edit { step, draft } => {
            edit_step(catalog, registry, editor, step, draft).await?;
        }
        ProposalChange::InsertBefore { before, draft } => {
            insert_step_before(catalog, registry, editor, before, draft).await?;
        }
        ProposalChange::Reorder { order } => {
            reorder_steps(catalog, editor, checklist.id, &order).await?;
        }
        ProposalChange::RunAs { user } => {
            let updated = Checklist {
                run_as: user,
                ..checklist.clone()
            };
            store::write_checklist(catalog, &updated).await?;
        }
    }
    Ok(())
}

async fn apply_accepted(
    catalog: &Catalog,
    registry: &StepRegistry,
    checklist: &Checklist,
    change: ProposalChange,
    accepted_as: &AcceptedAs,
    editor: &Editor,
) -> Result<()> {
    match change {
        ProposalChange::Edit { step, draft } => {
            let old = current(catalog, step).await?;
            let application = application_of(catalog, checklist).await?;
            validate_draft(registry, checklist, application.as_ref(), &draft)?;
            let new = Step {
                author: accepted_as.author.clone(),
                accepted: true,
                ..new_step(checklist, old.position, draft, editor)
            };
            store::insert_step(catalog, &new).await?;
            store::supersede(catalog, old.id, new.id).await?;
        }
        ProposalChange::InsertBefore { before, draft } => {
            let anchor = current(catalog, before).await?;
            let application = application_of(catalog, checklist).await?;
            validate_draft(registry, checklist, application.as_ref(), &draft)?;
            let position = position_before(catalog, &anchor).await?;
            let new = Step {
                author: accepted_as.author.clone(),
                accepted: true,
                ..new_step(checklist, position, draft, editor)
            };
            store::insert_step(catalog, &new).await?;
        }
        other => apply(catalog, registry, editor, checklist, other).await?,
    }
    refresh_mutates(catalog, registry, checklist.id).await
}

/// Where a step put before `anchor` goes: half way to the step before it, so
/// nothing is renumbered.
async fn position_before(catalog: &Catalog, anchor: &Step) -> Result<f64> {
    let previous = store::current_steps(catalog, anchor.checklist)
        .await?
        .iter()
        .map(|s| s.position)
        .filter(|p| *p < anchor.position)
        .fold(None, |best: Option<f64>, p| Some(best.map_or(p, |b| b.max(p))));
    Ok(match previous {
        Some(p) => (p + anchor.position) / 2.0,
        None => anchor.position - 1.0,
    })
}

/// The step version with this id, which must be current.
async fn current(catalog: &Catalog, step: Uuid) -> Result<Step> {
    let found = store::require_step(catalog, step).await?;
    if !found.is_current() {
        return Err(Error::invalid(format!(
            "step {step} has been replaced or deleted since; describe the checklist again for \
             its current steps"
        )));
    }
    Ok(found)
}

/// Whether every current step from `anchor` on is one this agent's run wrote
/// and nobody accepted — where inserting is as safe as appending.
async fn only_own_from(catalog: &Catalog, editor: &Editor, anchor: &Step) -> Result<bool> {
    Ok(store::current_steps(catalog, anchor.checklist)
        .await?
        .iter()
        .filter(|s| s.position >= anchor.position)
        .all(|s| editor.owns(s)))
}

fn new_step(checklist: &Checklist, position: f64, draft: StepDraft, editor: &Editor) -> Step {
    Step {
        id: Uuid::new_v4(),
        checklist: checklist.id,
        position,
        draft,
        author: editor.author(),
        accepted: editor.is_human(),
        superseded_by: None,
        deleted: false,
        created_at: Utc::now(),
    }
}

/// Check a step as it is saved: a known type, parameters its spec accepts,
/// templates that parse, an `as` a template can use, an application if the
/// step reaches one, and an application of the step's framework if the type
/// is a framework's. `application` is the checklist's, loaded.
pub fn validate_draft(
    registry: &StepRegistry,
    checklist: &Checklist,
    application: Option<&Application>,
    draft: &StepDraft,
) -> Result<()> {
    let step_type = registry.require(&draft.type_)?;
    let at = |e: Error| Error::invalid(format!("step `{}`: {e}", draft.type_));
    sc_types::validate_attrs(&step_type.config_spec(), &draft.params).map_err(at)?;
    vars::check(&Json::Object(draft.params.clone())).map_err(at)?;
    step_type
        .check(&Json::Object(draft.params.clone()))
        .map_err(at)?;
    if let Some(name) = &draft.capture {
        vars::check_capture(name).map_err(at)?;
    }
    if step_type.needs().application() && checklist.application.is_none() {
        return Err(Error::invalid(format!(
            "step `{}` checks an application, and checklist `{}` has none; give the checklist \
             its application first",
            draft.type_, checklist.name
        )));
    }
    if let Some(scope) = step_type.framework() {
        let refusal = match application {
            Some(app) => scope.refusal(&draft.type_, app),
            None => Some(format!(
                "`{}` is a step of the `{}` framework, and checklist `{}` checks no application",
                draft.type_,
                scope.label(),
                checklist.name
            )),
        };
        if let Some(refusal) = refusal {
            return Err(Error::invalid(format!(
                "{refusal}; describe_step_types lists the types this checklist may use"
            )));
        }
    }
    Ok(())
}

/// The application a checklist checks, loaded, if it checks one.
async fn application_of(catalog: &Catalog, checklist: &Checklist) -> Result<Option<Application>> {
    match checklist.application {
        None => Ok(None),
        Some(id) => sc_app::load_application(catalog, sc_app::AppId(id)).await,
    }
}

/// Refuse a `run_as` an agent may not set: anybody who is not a check user.
async fn check_run_as(catalog: &Catalog, editor: &Editor, run_as: Option<Uuid>) -> Result<()> {
    let Some(id) = run_as else {
        return Ok(());
    };
    let user = sc_auth::load_user(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no user with id {id}")))?;
    if !editor.is_human() && !user.is_check_user() {
        return Err(Error::auth(
            "a checklist an agent writes may only run as a check user, and this one is not. \
             Ask the admin to make a check user for the role (Users, \"Check user\"), or leave \
             `run_as` empty to run as an anonymous visitor.",
        ));
    }
    Ok(())
}

async fn check_application(catalog: &Catalog, application: Option<Uuid>) -> Result<()> {
    if let Some(id) = application {
        sc_app::load_application(catalog, sc_app::AppId(id))
            .await?
            .ok_or_else(|| Error::not_found(format!("no application with id {id}")))?;
    }
    Ok(())
}

/// Recompute `mutates` from the current steps' types (TODO C1.1). A type this
/// server no longer has counts as mutating: not knowing is not a reason to
/// let it run on production.
pub async fn refresh_mutates(catalog: &Catalog, registry: &StepRegistry, id: Uuid) -> Result<()> {
    let mutates = store::current_steps(catalog, id)
        .await?
        .iter()
        .any(|s| registry.get(&s.draft.type_).is_none_or(|t| t.mutates()));
    store::set_mutates(catalog, id, mutates).await
}
