//! The `_fd_model_instances` table: its schema, bootstrap, the
//! [`ModelInstance`] ⇄ row mapping, the `active` rule and the boot reap
//! (TODO §8, §15).
//!
//! Read strictly, for the reason `_fd_models` is: an instance read with half an
//! encoding would still *predict*, and a prediction encoded differently from the
//! way its fit was is confident nonsense (§6).
//!
//! The two judgements this module makes, both from §15:
//!
//! - **`status` is a column and the failure sentence is not.** Every row has a
//!   status and the list filters on it; only the rows that failed have a
//!   sentence, and a sparse value belongs in `attributes` (§9).
//! - **`active` is a column, not a nullable marker.** At most one row per model
//!   carries it, and the uniqueness is enforced here on save — see
//!   [`save_model_instance`]. A nullable "active_since" or a pointer on the
//!   model would be a second way to say the same thing, and two ways to say one
//!   thing eventually disagree.

use sc_catalog::{Catalog, DataField, Table};
use sc_db::{Row, Transaction};
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, JsonStep, OrderBy, Projection, Select, Source,
    Statement, UnOp, Update, Value,
};
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;

use crate::draws::{DRAWS_TABLE, delete_instance_draws, delete_model_draws, write_draws};
use crate::instance::{ATTR_ERROR, FitStatus, InstanceId, ModelInstance, RESTARTED};
use crate::model::ModelId;
use crate::posterior::DrawSeries;
use crate::provider::ParameterBlock;
use crate::registry::ModelRegistry;
use crate::store::{bad_column, load_model, object, optional_text, rows, structured, text};

/// Name of the model-instances table in the primary database.
pub const INSTANCES_TABLE: &str = "_fd_model_instances";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The model this is a fit of.
pub const COL_MODEL: &str = "model";
/// A short name for the instance list, or empty.
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// `fitting` | `fitted` | `failed` ([`FitStatus::as_str`]).
pub const COL_STATUS: &str = "status";
/// When the fit was started — what the list is ordered by, newest first.
pub const COL_CREATED: &str = "created";
/// Whether this is the model's active instance. At most one per model.
pub const COL_ACTIVE: &str = "active";
/// The provider's serialised fit (§10) — the big one.
pub const COL_STATE: &str = "state";
/// The provider's parameters, as a JSON array of [`ParameterBlock`]s (§7).
pub const COL_PARAMETERS: &str = "parameters";
/// The host's metrics (§7).
pub const COL_METRICS: &str = "metrics";
/// The encoding fitted with this instance (§6).
pub const COL_ENCODING: &str = "encoding";
/// The hyperparameter point this fit used — values, never lists (§11).
pub const COL_HYPERPARAMETERS: &str = "hyperparameters";
/// The sparse per-instance values column (§9): [`ATTR_ERROR`] and the grid's
/// scores.
pub const COL_ATTRIBUTES: &str = "attributes";

/// The fields of the `_fd_model_instances` table, in declaration order.
fn instance_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        // Not a foreign key: `_fd_models` is a bootstrap table like this one, and
        // the deletion rule is stated in code (`delete_model` takes its
        // instances with it) rather than in a constraint that would also have to
        // be reconciled onto every existing database.
        DataField::plain(COL_MODEL, TypeRef::Basic(BasicType::Uuid)).required(),
        // Not unique: two instances of one model may be called "with more
        // trees", and it is the id that identifies a fit.
        DataField::plain(COL_NAME, text()),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_STATUS, text()).required(),
        DataField::plain(COL_CREATED, TypeRef::Basic(BasicType::Timestamp)).required(),
        DataField::plain(COL_ACTIVE, TypeRef::Basic(BasicType::Bool)).required(),
        // `state`, `metrics` and `encoding` are JSON and NULL until the fit
        // finishes — which is a real state (§8: the row exists before the work
        // does), so they are not required.
        DataField::plain(COL_STATE, json()),
        DataField::plain(COL_PARAMETERS, json()).required(),
        DataField::plain(COL_METRICS, json()),
        DataField::plain(COL_ENCODING, json()),
        DataField::plain(COL_HYPERPARAMETERS, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure the `_fd_model_instances` table exists, creating it if absent.
pub async fn bootstrap_model_instances(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(INSTANCES_TABLE, &instance_fields())
        .await
}

/// Save an instance: insert its row, or update it in place.
///
/// **The `active` rule is enforced here.** Saving an instance with
/// [`active`](ModelInstance::active) set clears the flag on every other instance
/// of the same model, so activating instance 7 deactivates instance 3 — which is
/// what `activateModelInstance` means and what a trigger naming a model depends
/// on. Refusing the save instead would make activation a two-step dance with a
/// window in which a model has no active instance at all.
///
/// A non-[`fitted`](FitStatus::Fitted) instance **cannot** be active, and that
/// is refused rather than quietly ignored: an active instance is one a trigger
/// will predict with, and a fit that is still running or that failed has nothing
/// to predict with.
///
/// One transaction, so the row and the other instances' deactivation land
/// together.
pub async fn save_model_instance(catalog: &Catalog, instance: &ModelInstance) -> Result<()> {
    save_fitted_instance(catalog, instance, &[]).await
}

/// Save an instance **and its draws** in one transaction (Stan TODO §14): the
/// draws are written in batches, then the row — so a fitted instance always
/// has all of its draws, and a write that fails half way leaves the row as it
/// was (still `fitting`) with none of them.
///
/// Draws belong only to a fitted instance, and are refused on any other: a
/// failed fit has nothing to read, and a running one is not finished writing.
/// With no draws this is [`save_model_instance`], and the draws table need not
/// exist.
pub async fn save_fitted_instance(
    catalog: &Catalog,
    instance: &ModelInstance,
    draws: &[DrawSeries],
) -> Result<()> {
    if instance.active && !instance.status.is_usable() {
        return Err(Error::invalid(format!(
            "a model instance that is `{}` cannot be the active one: only a fitted instance \
             can answer a prediction",
            instance.status
        )));
    }
    if !draws.is_empty() {
        if instance.status != FitStatus::Fitted {
            return Err(Error::invalid(format!(
                "a model instance that is `{}` cannot store draws: only a fitted one has any",
                instance.status
            )));
        }
        if catalog.get(DRAWS_TABLE)?.is_none() {
            return Err(Error::config(format!(
                "`{DRAWS_TABLE}` does not exist, so this fit's draws have nowhere to go: \
                 the server bootstraps it at start-up"
            )));
        }
    }
    let values = instance_values(instance)?;
    let mut tx = catalog.primary().begin().await?;
    let written = async {
        write_draws_if_any(tx.as_mut(), instance.id, draws).await?;
        write_instance(tx.as_mut(), instance, values).await
    }
    .await;
    finish(tx, written).await
}

/// The draws half of [`save_fitted_instance`], skipped entirely when there are
/// none — which is every fit that is not a posterior.
async fn write_draws_if_any(
    tx: &mut dyn Transaction,
    instance: InstanceId,
    draws: &[DrawSeries],
) -> Result<()> {
    if draws.is_empty() {
        return Ok(());
    }
    write_draws(tx, instance, draws).await
}

/// Commit `tx` if `outcome` is a success and roll it back otherwise, answering
/// `outcome`.
pub(crate) async fn finish(tx: Box<dyn Transaction>, outcome: Result<()>) -> Result<()> {
    match outcome {
        Ok(()) => tx.commit().await,
        Err(e) => {
            // The rollback's own failure is not the news: the statement that
            // failed is, and a connection that cannot roll back is dropped by
            // the pool, which rolls back.
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Run a statement on `tx` and collect what it answers.
pub(crate) async fn run(tx: &mut dyn Transaction, statement: Statement) -> Result<Vec<Row>> {
    tx.query(&statement).await?.try_collect().await
}

/// Insert or update the instance row on `tx`, and keep `active` unique.
async fn write_instance(
    tx: &mut dyn Transaction,
    instance: &ModelInstance,
    values: Vec<Value>,
) -> Result<()> {
    let columns = instance_columns();
    let mut probe = Select::from(Source::table(INSTANCES_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(instance.id.0)));
    probe.columns = vec![Projection::expr(Expr::col(COL_ID))];
    if run(tx, Statement::from(probe)).await?.is_empty() {
        let insert = Insert::row(
            INSTANCES_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(tx, Statement::from(insert)).await?;
    } else {
        let assignments = columns
            .iter()
            .zip(values)
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = Update::new(INSTANCES_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(instance.id.0)));
        run(tx, Statement::from(update)).await?;
    }
    if instance.active {
        run(tx, deactivate_others(instance.model, instance.id)).await?;
    }
    Ok(())
}

/// Clear `active` on every instance of `model` except `keep`.
fn deactivate_others(model: ModelId, keep: InstanceId) -> Statement {
    let update = Update::new(
        INSTANCES_TABLE,
        vec![Assignment::new(COL_ACTIVE, Expr::lit(false))],
    )
    .filter(
        Expr::col(COL_MODEL)
            .eq(Expr::lit(model.0))
            .and(Expr::binary(
                BinOp::Ne,
                Expr::col(COL_ID),
                Expr::lit(keep.0),
            ))
            .and(Expr::col(COL_ACTIVE).eq(Expr::lit(true))),
    );
    Statement::from(update)
}

/// Load the instance with this id, if it exists.
pub async fn load_model_instance(
    catalog: &Catalog,
    id: InstanceId,
) -> Result<Option<ModelInstance>> {
    let select =
        Select::from(Source::table(INSTANCES_TABLE)).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(instance_from_row(row)?)),
        None => Ok(None),
    }
}

/// The instance with this id, or a not-found error naming it.
pub async fn require_model_instance(catalog: &Catalog, id: InstanceId) -> Result<ModelInstance> {
    load_model_instance(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no model instance with id {id}")))
}

/// Every instance of `model`, **newest first** — the instance list, which is the
/// model's history.
pub async fn list_model_instances(catalog: &Catalog, model: ModelId) -> Result<Vec<ModelInstance>> {
    let mut select = Select::from(Source::table(INSTANCES_TABLE))
        .filter(Expr::col(COL_MODEL).eq(Expr::lit(model.0)));
    select.order = vec![OrderBy::desc(Expr::col(COL_CREATED))];
    rows(catalog, select)
        .await?
        .iter()
        .map(instance_from_row)
        .collect()
}

/// The model's active instance, if it has one — what a `predict("…")` naming a
/// *model* rather than a fit resolves to (§1).
pub async fn active_model_instance(
    catalog: &Catalog,
    model: ModelId,
) -> Result<Option<ModelInstance>> {
    let select = Select::from(Source::table(INSTANCES_TABLE)).filter(
        Expr::col(COL_MODEL)
            .eq(Expr::lit(model.0))
            .and(Expr::col(COL_ACTIVE).eq(Expr::lit(true))),
    );
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(instance_from_row(row)?)),
        None => Ok(None),
    }
}

/// Delete one instance **and its draws**, in one transaction, then let its
/// provider [`discard`](crate::ModelProvider::discard) whatever the fit kept
/// outside the database. Answers whether there was one to delete.
///
/// The rows go first and the discard second, because the rows are what an
/// admin can see: an instance whose run directory could not be removed is gone
/// from every list, and the error says what was left behind. The other order
/// would leave an instance on the screen whose run directory had been deleted.
pub async fn delete_model_instance(
    catalog: &Catalog,
    registry: &ModelRegistry,
    id: InstanceId,
) -> Result<bool> {
    let Some(instance) = load_model_instance(catalog, id).await? else {
        return Ok(false);
    };
    let draws = catalog.get(DRAWS_TABLE)?.is_some();
    let mut tx = catalog.primary().begin().await?;
    let deleted = async {
        if draws {
            delete_instance_draws(tx.as_mut(), id).await?;
        }
        let delete = Delete::from(INSTANCES_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
        run(tx.as_mut(), Statement::from(delete)).await.map(drop)
    }
    .await;
    finish(tx, deleted).await?;
    discard_states(catalog, registry, instance.model, &[instance.state]).await?;
    Ok(true)
}

/// Delete every instance of `model` and their draws on `tx` — the half of
/// [`delete_model`](crate::delete_model) that is about instances, in the
/// transaction that also deletes the model row.
pub(crate) async fn delete_instances_on(
    catalog: &Catalog,
    tx: &mut dyn Transaction,
    model: ModelId,
) -> Result<()> {
    if catalog.get(DRAWS_TABLE)?.is_some() {
        delete_model_draws(tx, model).await?;
    }
    let delete = Delete::from(INSTANCES_TABLE).filter(Expr::col(COL_MODEL).eq(Expr::lit(model.0)));
    run(tx, Statement::from(delete)).await.map(drop)
}

/// Let `model`'s provider release what each of these fitted states holds
/// outside the database (Stan TODO §14), after their rows are gone.
///
/// A model whose row or provider has gone — a module uninstalled — has nobody
/// to ask, and nothing is discarded: the rows were the part that had to go.
/// Every state is tried; the first failure is reported, saying what happened
/// to the rows, because an error from a delete otherwise reads as "nothing was
/// deleted".
pub(crate) async fn discard_states(
    catalog: &Catalog,
    registry: &ModelRegistry,
    model: ModelId,
    states: &[Json],
) -> Result<()> {
    if states.iter().all(Json::is_null) {
        return Ok(());
    }
    let Some(provider) = load_model(catalog, model)
        .await
        .ok()
        .flatten()
        .and_then(|m| registry.get(m.provider.trim()))
    else {
        return Ok(());
    };
    discard_with(provider.as_ref(), states).await
}

/// [`discard_states`] once the provider is known.
pub(crate) async fn discard_with(
    provider: &dyn crate::ModelProvider,
    states: &[Json],
) -> Result<()> {
    let mut first: Option<Error> = None;
    for state in states.iter().filter(|s| !s.is_null()) {
        if let Err(e) = provider.discard(state).await {
            first.get_or_insert(e);
        }
    }
    match first {
        None => Ok(()),
        Some(e) => Err(Error::msg(format!(
            "the rows were deleted, but `{}` could not release what the fit kept outside the \
             database: {}",
            provider.name(),
            sc_error::format_chain(&e)
        ))),
    }
}

/// What [`record_fit_progress`] found on the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressWrite {
    /// The fit is still running and nobody has asked it to stop; the
    /// progress, if there was any, is on the row.
    Running,
    /// Somebody asked the fit to stop: the job should cancel it. The progress
    /// was not written.
    CancelRequested,
    /// The row is no longer `fitting` (or no longer there): nothing to report
    /// to.
    Finished,
}

/// Write a running fit's [`Progress`](crate::Progress) to its row's
/// [`ATTR_PROGRESS`](crate::ATTR_PROGRESS), and read back whether it has been
/// asked to stop (Stan TODO §13) — the one round trip the job makes each
/// second. With no progress (nothing has changed since the last write) it only
/// reads.
///
/// The attributes are a JSON object and the query language cannot merge one,
/// so this reads the row and writes the merged object back. The write is
/// guarded — `WHERE status = 'fitting' AND attributes -> 'cancel_requested'
/// IS NULL` — so it can neither undo a cancel that landed between the read and
/// the write (the next call sees it) nor touch a row the fit has already
/// finished.
pub async fn record_fit_progress(
    catalog: &Catalog,
    id: InstanceId,
    progress: Option<&crate::Progress>,
) -> Result<ProgressWrite> {
    let Some(mut instance) = load_model_instance(catalog, id).await? else {
        return Ok(ProgressWrite::Finished);
    };
    if instance.status != FitStatus::Fitting {
        return Ok(ProgressWrite::Finished);
    }
    if cancel_requested(&instance) {
        return Ok(ProgressWrite::CancelRequested);
    }
    let Some(progress) = progress else {
        return Ok(ProgressWrite::Running);
    };
    instance.attributes.insert(
        crate::ATTR_PROGRESS.to_owned(),
        serde_json::to_value(progress).map_err(|e| Error::msg(format!("progress: {e}")))?,
    );
    let update = Update::new(
        INSTANCES_TABLE,
        vec![Assignment::new(
            COL_ATTRIBUTES,
            Expr::lit(Value::Json(Json::Object(instance.attributes))),
        )],
    )
    .filter(
        Expr::col(COL_ID)
            .eq(Expr::lit(id.0))
            .and(Expr::col(COL_STATUS).eq(Expr::lit(FitStatus::Fitting.as_str())))
            .and(Expr::unary(
                UnOp::IsNull,
                Expr::Json {
                    target: Box::new(Expr::col(COL_ATTRIBUTES)),
                    path: vec![JsonStep::Field(crate::ATTR_CANCEL_REQUESTED.to_owned())],
                },
            )),
    );
    let mut tx = catalog.primary().begin().await?;
    let written = run(tx.as_mut(), Statement::from(update)).await.map(drop);
    finish(tx, written).await?;
    Ok(ProgressWrite::Running)
}

/// Whether the instance's row says somebody asked its fit to stop: the
/// attribute is there, whatever its value — exactly what the guard on
/// [`record_fit_progress`]'s write tests, so the two cannot disagree.
pub fn cancel_requested(instance: &ModelInstance) -> bool {
    instance
        .attributes
        .contains_key(crate::ATTR_CANCEL_REQUESTED)
}

/// Ask the running fit of instance `id` to stop, by setting
/// [`ATTR_CANCEL_REQUESTED`](crate::ATTR_CANCEL_REQUESTED) on its row (Stan
/// TODO §13). The row is the registry, so this works from any node: the job
/// reads it back with its next progress write and kills what it started.
///
/// Answers whether there was a running fit to ask. An instance that has
/// already finished is left alone.
pub async fn request_fit_cancel(catalog: &Catalog, id: InstanceId) -> Result<bool> {
    let instance = require_model_instance(catalog, id).await?;
    if instance.status != FitStatus::Fitting {
        return Ok(false);
    }
    let mut attributes = instance.attributes;
    attributes.insert(crate::ATTR_CANCEL_REQUESTED.to_owned(), Json::Bool(true));
    let update = Update::new(
        INSTANCES_TABLE,
        vec![Assignment::new(
            COL_ATTRIBUTES,
            Expr::lit(Value::Json(Json::Object(attributes))),
        )],
    )
    .filter(
        Expr::col(COL_ID)
            .eq(Expr::lit(id.0))
            .and(Expr::col(COL_STATUS).eq(Expr::lit(FitStatus::Fitting.as_str()))),
    );
    let mut tx = catalog.primary().begin().await?;
    let written = run(tx.as_mut(), Statement::from(update)).await.map(drop);
    finish(tx, written).await?;
    Ok(true)
}

/// Fail every instance still saying `fitting`, and answer how many there were
/// (§8).
///
/// **A fit does not survive a restart.** There is no in-memory job registry —
/// the row is the registry — so a process that dies mid-fit leaves a row saying
/// `fitting` that nothing will ever finish. Called once at boot, before anything
/// can read an instance, so nobody sees a fit that is not running described as
/// running. Making a fit durable is the workflow engine's job and would mean
/// expressing a fit as a workflow, which is a bigger claim than this milestone
/// makes.
///
/// Row by row rather than as one `UPDATE`, because the sentence goes into each
/// row's `attributes` object and merging JSON is not something the query
/// language does. At boot there are none of these rows in the ordinary case and
/// a handful in the bad one.
pub async fn reap_fitting_instances(catalog: &Catalog) -> Result<usize> {
    if catalog.get(INSTANCES_TABLE)?.is_none() {
        return Ok(0);
    }
    let select = Select::from(Source::table(INSTANCES_TABLE))
        .filter(Expr::col(COL_STATUS).eq(Expr::lit(FitStatus::Fitting.as_str())));
    let stranded: Vec<ModelInstance> = rows(catalog, select)
        .await?
        .iter()
        .map(instance_from_row)
        .collect::<Result<_>>()?;
    let count = stranded.len();
    for instance in stranded {
        save_model_instance(catalog, &instance.failed(RESTARTED)).await?;
    }
    Ok(count)
}

/// The row's columns, in the order [`instance_values`] produces them.
fn instance_columns() -> Vec<String> {
    [
        COL_ID,
        COL_MODEL,
        COL_NAME,
        COL_DESCRIPTION,
        COL_STATUS,
        COL_CREATED,
        COL_ACTIVE,
        COL_STATE,
        COL_PARAMETERS,
        COL_METRICS,
        COL_ENCODING,
        COL_HYPERPARAMETERS,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The instance serialised to its row's values, in [`instance_columns`] order.
fn instance_values(instance: &ModelInstance) -> Result<Vec<Value>> {
    let parameters = serde_json::to_value(&instance.parameters).map_err(|e| {
        Error::msg(format!(
            "a model instance's parameters could not be stored: {e}"
        ))
    })?;
    Ok(vec![
        Value::Uuid(instance.id.0),
        Value::Uuid(instance.model.0),
        Value::Text(instance.name.trim().to_owned()),
        Value::Text(instance.description.clone()),
        Value::Text(instance.status.as_str().to_owned()),
        Value::Timestamp(instance.created),
        Value::Bool(instance.active),
        json_or_null(&instance.state),
        Value::Json(parameters),
        json_or_null(&instance.metrics),
        json_or_null(&instance.encoding),
        Value::Json(Json::Object(instance.hyperparameters.clone())),
        Value::Json(Json::Object(instance.attributes.clone())),
    ])
}

/// A JSON column whose "not yet" is SQL NULL rather than the JSON `null`
/// literal — the two are indistinguishable to a reader and only one of them
/// needs a type.
fn json_or_null(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        other => Value::Json(other.clone()),
    }
}

/// Rebuild a [`ModelInstance`] from its row. Strict throughout, for the reason
/// in the module docs.
fn instance_from_row(row: &Row) -> Result<ModelInstance> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => InstanceId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let at = |e: String| Error::invalid(format!("model instance {id}: {e}"));

    let model = match row.get(COL_MODEL) {
        Some(Value::Uuid(u)) => ModelId(*u),
        other => return Err(at(bad_column(COL_MODEL, "a uuid", other).to_string())),
    };
    let status = FitStatus::parse(&text(row, COL_STATUS).map_err(|e| at(e.to_string()))?)
        .map_err(|e| at(e.to_string()))?;
    let created = match row.get(COL_CREATED) {
        Some(Value::Timestamp(t)) => *t,
        other => return Err(at(bad_column(COL_CREATED, "a timestamp", other).to_string())),
    };
    let active = match row.get(COL_ACTIVE) {
        Some(Value::Bool(b)) => *b,
        other => return Err(at(bad_column(COL_ACTIVE, "a boolean", other).to_string())),
    };
    let parameters: Vec<ParameterBlock> = match row.get(COL_PARAMETERS) {
        Some(Value::Json(Json::Array(_))) => {
            structured(row, COL_PARAMETERS).map_err(|e| at(e.to_string()))?
        }
        Some(Value::Json(Json::Null)) | Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            return Err(at(format!("{COL_PARAMETERS} should be a json array")));
        }
    };

    Ok(ModelInstance {
        id,
        model,
        name: optional_text(row, COL_NAME).map_err(|e| at(e.to_string()))?,
        description: optional_text(row, COL_DESCRIPTION).map_err(|e| at(e.to_string()))?,
        status,
        created,
        active,
        state: json_column(row, COL_STATE).map_err(|e| at(e.to_string()))?,
        parameters,
        metrics: json_column(row, COL_METRICS).map_err(|e| at(e.to_string()))?,
        encoding: json_column(row, COL_ENCODING).map_err(|e| at(e.to_string()))?,
        hyperparameters: object(row, COL_HYPERPARAMETERS).map_err(|e| at(e.to_string()))?,
        attributes: object(row, COL_ATTRIBUTES).map_err(|e| at(e.to_string()))?,
    })
}

/// A nullable JSON column: SQL NULL reads as [`Json::Null`], which is what
/// "the fit has not written this yet" means in memory.
fn json_column(row: &Row, column: &str) -> Result<Json> {
    match row.get(column) {
        Some(Value::Json(value)) => Ok(value.clone()),
        Some(Value::Null) | None => Ok(Json::Null),
        other => Err(bad_column(column, "json", other)),
    }
}

/// An instance that finished, with the state and parameters a fit produced.
///
/// A convenience for the fit runner (Phase 3) and for tests: it is the one
/// transition the whole of §8 is about, and spelling it out here keeps the
/// "status, state and parameters move together" rule in one place rather than at
/// every call site.
pub fn fitted(
    mut instance: ModelInstance,
    state: Json,
    parameters: Vec<ParameterBlock>,
) -> ModelInstance {
    instance.status = FitStatus::Fitted;
    instance.state = state;
    instance.parameters = parameters;
    instance.attributes.remove(ATTR_ERROR);
    instance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_15_columns_with_the_judgements_it_asks_for() {
        let fields = instance_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        assert!(by_name(COL_ID).primary_key);
        assert!(by_name(COL_MODEL).required);
        // `status` and `active` are columns: every row has a status, and the
        // uniqueness of `active` is enforced against it.
        assert!(by_name(COL_STATUS).required);
        assert!(by_name(COL_ACTIVE).required);
        assert_eq!(
            by_name(COL_ACTIVE).base.type_,
            TypeRef::Basic(BasicType::Bool)
        );
        // The fit's output is not required, because the row exists before the
        // fit does (§8).
        assert!(!by_name(COL_STATE).required);
        assert!(!by_name(COL_METRICS).required);
        assert!(!by_name(COL_ENCODING).required);
        // A name is optional — two instances of one model may share one.
        assert!(!by_name(COL_NAME).required && !by_name(COL_NAME).unique);
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let instance = ModelInstance::starting(ModelId::new());
        assert_eq!(
            instance_columns().len(),
            instance_values(&instance).unwrap().len()
        );
        let declared: Vec<String> = instance_fields()
            .iter()
            .map(|f| f.base.name.clone())
            .collect();
        assert_eq!(instance_columns(), declared);
    }

    #[test]
    fn a_fit_that_has_not_finished_stores_sql_null_and_not_a_json_null() {
        let values = instance_values(&ModelInstance::starting(ModelId::new())).unwrap();
        assert_eq!(values[7], Value::Null); // state
        assert_eq!(values[9], Value::Null); // metrics
        assert_eq!(values[10], Value::Null); // encoding
        assert_eq!(values[8], Value::Json(Json::Array(Vec::new()))); // parameters
    }
}
