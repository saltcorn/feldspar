//! The `_fd_triggers` table: its schema, bootstrap, and the [`Trigger`] ⇄ row
//! mapping (design §9, §10.2).
//!
//! A trigger, like an application or a file store, has **nothing to introspect
//! it from**: no column in `information_schema` says "when a row lands in
//! `books`, send a webhook". So by §9's rule its row *is* its definition — this
//! is not an overlay, and without the row there is no trigger at all.
//!
//! **Reading is strict**, as it is for those two: a column that is missing or of
//! the wrong shape is an [`Error::invalid`] naming the trigger and the column,
//! never a silently defaulted field. A half-understood trigger is one that would
//! fire the wrong action, or fire on the wrong event, and the admin needs telling.
//!
//! What is *not* here: validation (see [`validate`](crate::validate), which
//! [`save_trigger`] calls) and caching (see [`Triggers`](crate::Triggers)).

use chrono::{DateTime, Utc};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;

use crate::event::EventKind;
use crate::trigger::{Trigger, TriggerBody, TriggerId};

/// Name of the triggers table in the primary database.
pub const TRIGGERS_TABLE: &str = "_fd_triggers";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The trigger's unique name — the key an app, an API path and a Run button use.
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// The event that fires it, as [`EventKind::as_str`].
///
/// Named `event` rather than v1's `when_trigger`: `when` is a SQL keyword (the
/// renderer would quote it, but a column nobody has to think twice about is
/// better), and this *is* the event.
pub const COL_EVENT: &str = "event";
/// The channel — the table name for a table event, NULL otherwise.
pub const COL_CHANNEL: &str = "channel";
/// The "only if" formula, or NULL for "always".
pub const COL_ONLY_IF: &str = "only_if";
/// Which engine runs this trigger: `action` or `workflow`
/// ([`TriggerBody::as_str`]).
///
/// The discriminator is a column of its own rather than "an action name means an
/// action": a workflow body has nothing to put in `action`, and inferring the
/// body from the absence of a value would make a half-written row read as a
/// workflow.
pub const COL_BODY: &str = "body";
/// The registered action name — NULL for a workflow body.
pub const COL_ACTION: &str = "action";
/// The action's configuration (JSON object).
pub const COL_CONFIGURATION: &str = "configuration";
/// The role floor for running this trigger through an API, or NULL for
/// admin-only.
pub const COL_MIN_ROLE: &str = "min_role";
/// The sparse per-trigger values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";
/// When the scheduler last fired this trigger, or NULL for one it never has.
///
/// A column rather than an attribute, and one [`save_trigger`] deliberately does
/// **not** write: it is the scheduler's bookkeeping, not part of the definition
/// the admin edits, so an edit at 3pm must not be able to claim the daily job ran
/// at 3pm (or that it never ran). Only [`record_trigger_run`] writes it.
///
/// It arrived after `_fd_triggers` did, so it is nullable and reaches existing
/// databases through the additive bootstrap (`Catalog::bootstrap_table`).
pub const COL_LAST_RUN_AT: &str = "last_run_at";

/// The fields of the `_fd_triggers` table, in declaration order.
///
/// `name` carries the `UNIQUE` constraint for the same reason an application's
/// `subdomain` and a file store's `name` do: it is the key everything resolves
/// through, so two triggers claiming one name is not a state the system can
/// serve. The database is the authority, because two admins saving concurrently
/// cannot see each other's transaction.
fn trigger_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_EVENT, text()).required(),
        // Nullable: a non-table event has no channel, and that is a real state
        // rather than an empty string.
        DataField::plain(COL_CHANNEL, text()),
        DataField::plain(COL_ONLY_IF, text()),
        DataField::plain(COL_BODY, text()).required(),
        // Nullable since §10.3: a workflow's body is its steps, which live in
        // `_fd_workflow_versions`, so there is no action to name here.
        DataField::plain(COL_ACTION, text()),
        DataField::plain(COL_CONFIGURATION, json()).required(),
        DataField::plain(COL_MIN_ROLE, int()),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
        // Nullable, and necessarily so: it is reconciled onto tables that
        // already have rows, and "never run" is its honest value for those.
        DataField::plain(COL_LAST_RUN_AT, TypeRef::Basic(BasicType::Timestamp)),
        // When it was last saved (`sc_catalog::edited`): bookkeeping for the
        // entities list, written by every save and read by nothing else.
        sc_catalog::updated_at_field(),
    ]
}

/// Ensure the `_fd_triggers` table exists, creating it if absent, and return it.
///
/// Idempotent, and safe against a database that has never seen Saltcorn — the
/// same contract as `sc_app::bootstrap` and `bootstrap_file_stores`. Call once at
/// startup, after the [`Catalog`] is initialised and before loading triggers. An
/// existing table is reconciled additively
/// ([`bootstrap_table`](sc_catalog::Catalog::bootstrap_table)), which is how
/// [`last_run_at`](COL_LAST_RUN_AT) reaches a database that already has triggers
/// in it.
pub async fn bootstrap_triggers(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(TRIGGERS_TABLE, &trigger_fields())
        .await
}

/// Save a trigger: insert its row, or update it in place if a row with its
/// [`TriggerId`] already exists.
///
/// Validation runs **first** ([`validate_trigger`](crate::validate_trigger)), so
/// a trigger that could never fire correctly is refused while the admin is still
/// looking at the form rather than discovered when the event happens — the same
/// argument `save_file_store` makes for a backend's settings.
///
/// The name clash is checked here so the admin gets an error naming the trigger
/// that already holds the name, rather than a raw constraint violation; the
/// database's `UNIQUE` remains the authority.
pub async fn save_trigger(
    catalog: &Catalog,
    registry: &crate::ActionRegistry,
    trigger: &Trigger,
) -> Result<()> {
    crate::validate_trigger(catalog, registry, trigger).await?;

    let name = trigger.name.trim();
    if let Some(other) = load_trigger_by_name(catalog, name).await?
        && other.id != trigger.id
    {
        return Err(Error::invalid(format!(
            "trigger name `{name}` is already used; \
             each trigger is referenced by its own name"
        )));
    }

    let columns = trigger_columns();
    let values = trigger_values(trigger);

    if load_trigger(catalog, trigger.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            // The id is the row's identity, not something to reassign.
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = sc_query::Update::new(TRIGGERS_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(trigger.id.0)));
        run(catalog, Statement::from(update)).await
    } else {
        let insert = Insert::row(
            TRIGGERS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await
    }
}

/// Record that the scheduler fired `id` at `at` — the only writer of
/// [`last_run_at`](COL_LAST_RUN_AT).
///
/// One targeted `UPDATE` of one column, rather than [`save_trigger`] with a
/// mutated trigger, for two reasons: it cannot lose an edit an admin made while
/// the run was in flight, and it does not re-validate a trigger that has just
/// demonstrably run.
///
/// This is what makes a run missed while the server was down fire **once** at the
/// next start rather than being lost or repeated per missed period — the whole
/// purpose of persisting it.
pub async fn record_trigger_run(catalog: &Catalog, id: TriggerId, at: DateTime<Utc>) -> Result<()> {
    let update = sc_query::Update::new(
        TRIGGERS_TABLE,
        vec![Assignment::new(
            COL_LAST_RUN_AT.to_owned(),
            Expr::Lit(Value::Timestamp(at)),
        )],
    )
    .filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(update)).await
}

/// Record that `id` was edited now without rewriting its row — for an edit
/// that lives elsewhere, a workflow body's new version (§10.3), which is still
/// an edit of the trigger as far as the entities list is concerned.
pub async fn touch_trigger(catalog: &Catalog, id: TriggerId) -> Result<()> {
    if catalog.get(TRIGGERS_TABLE)?.is_none() {
        return Ok(());
    }
    let update = sc_query::Update::new(
        TRIGGERS_TABLE,
        vec![Assignment::new(
            sc_catalog::COL_UPDATED_AT.to_owned(),
            Expr::Lit(sc_catalog::updated_now()),
        )],
    )
    .filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(update)).await
}

/// Load the trigger with this id, if it exists.
pub async fn load_trigger(catalog: &Catalog, id: TriggerId) -> Result<Option<Trigger>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the trigger named `name`, if any — the lookup a direct run (the admin's
/// Run button, an API call) resolves through.
pub async fn load_trigger_by_name(catalog: &Catalog, name: &str) -> Result<Option<Trigger>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// Every stored trigger, ordered by name — what the cache loads at boot and what
/// the admin UI lists.
pub async fn list_triggers(catalog: &Catalog) -> Result<Vec<Trigger>> {
    let select = Select::from(Source::table(TRIGGERS_TABLE));
    let mut out: Vec<Trigger> = rows(catalog, select)
        .await?
        .iter()
        .map(trigger_from_row)
        .collect::<Result<_>>()?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Delete a trigger, returning whether one was there to delete.
///
/// Nothing else is touched: a trigger's *effects* are rows in other tables, and
/// removing the trigger does not unmake them. Unlike a file store there is no
/// reference check — an application exposing this trigger (Phase 7) fails to
/// mount with the name in the error, which is the report the admin needs, and
/// blocking the delete would leave them unable to remove a trigger they no longer
/// want.
pub async fn delete_trigger(catalog: &Catalog, id: TriggerId) -> Result<bool> {
    if load_trigger(catalog, id).await?.is_none() {
        return Ok(false);
    }
    let delete = Delete::from(TRIGGERS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The row's columns, in the order [`trigger_values`] produces them.
fn trigger_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_EVENT,
        COL_CHANNEL,
        COL_ONLY_IF,
        COL_BODY,
        COL_ACTION,
        COL_CONFIGURATION,
        COL_MIN_ROLE,
        COL_ATTRIBUTES,
        sc_catalog::COL_UPDATED_AT,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The trigger serialised to its row's values, in [`trigger_columns`] order.
fn trigger_values(trigger: &Trigger) -> Vec<Value> {
    vec![
        Value::Uuid(trigger.id.0),
        Value::Text(trigger.name.trim().to_owned()),
        Value::Text(trigger.description.clone()),
        Value::Text(trigger.when.as_str().to_owned()),
        text_or_null(trigger.channel.as_deref()),
        text_or_null(trigger.only_if.as_deref()),
        Value::Text(trigger.body.as_str().to_owned()),
        text_or_null(trigger.action()),
        Value::Json(Json::Object(
            trigger.configuration().cloned().unwrap_or_default(),
        )),
        match trigger.min_role {
            Some(role) => Value::Int(i64::from(role)),
            None => Value::Null,
        },
        Value::Json(Json::Object(trigger.attributes.clone())),
        // Every save is an edit (`sc_catalog::edited`).
        sc_catalog::updated_now(),
    ]
}

/// A trimmed optional string as a value: `None` and `""` are both NULL, so
/// "cleared in the form" and "never set" store identically.
fn text_or_null(value: Option<&str>) -> Value {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Value::Text(s.to_owned()),
        None => Value::Null,
    }
}

/// Rebuild a [`Trigger`] from its `_fd_triggers` row. The strictness note in the
/// module docs applies throughout.
fn trigger_from_row(row: &Row) -> Result<Trigger> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => TriggerId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let name = text(row, COL_NAME)?;

    // An unknown event kind is a corrupt or downgraded row: reported naming the
    // trigger, never mapped to some default event that would fire at the wrong
    // time.
    let when = EventKind::parse(&text(row, COL_EVENT)?)
        .map_err(|e| Error::invalid(format!("trigger `{name}`: {e}")))?;

    // A NULL description is "none given", not a broken row.
    let description = match row.get(COL_DESCRIPTION) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad_column(COL_DESCRIPTION, "text", other)),
    };

    // A value outside 1–100 is not clamped: roles are a fixed scale, and reading
    // a corrupt one as the nearest valid role would quietly change who may run
    // the trigger. (`file_stores` makes the same argument for `min_role`.)
    let min_role = match row.get(COL_MIN_ROLE) {
        Some(Value::Null) | None => None,
        Some(Value::Int(i)) => Some(
            u8::try_from(*i)
                .ok()
                .filter(|r| (1..=100).contains(r))
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "{TRIGGERS_TABLE}.{COL_MIN_ROLE} should be a role between 1 and 100, \
                         got {i}"
                    ))
                })?,
        ),
        other => return Err(bad_column(COL_MIN_ROLE, "an integer role", other)),
    };

    // Strict in both directions (`TriggerBody::parse`): the discriminator, the
    // action name and the configuration have to agree, and a row where they do
    // not is reported naming the trigger rather than run as whichever half was
    // read first.
    let body = TriggerBody::parse(
        &text(row, COL_BODY)?,
        optional_text(row, COL_ACTION)?.as_deref(),
        object(row, COL_CONFIGURATION)?,
    )
    .map_err(|e| Error::invalid(format!("trigger `{name}`: {e}")))?;

    Ok(Trigger {
        id,
        name,
        description,
        when,
        channel: optional_text(row, COL_CHANNEL)?,
        only_if: optional_text(row, COL_ONLY_IF)?,
        body,
        min_role,
        attributes: object(row, COL_ATTRIBUTES)?,
        last_run_at: last_run_at(row)?,
    })
}

/// The last-run instant: NULL, or a column the additive bootstrap has not added
/// yet, both mean "never run".
fn last_run_at(row: &Row) -> Result<Option<DateTime<Utc>>> {
    match row.get(COL_LAST_RUN_AT) {
        Some(Value::Timestamp(t)) => Ok(Some(*t)),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(COL_LAST_RUN_AT, "a timestamp", other)),
    }
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A nullable text column: NULL and empty both read as `None`.
fn optional_text(row: &Row, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) if t.trim().is_empty() => Ok(None),
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{TRIGGERS_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{TRIGGERS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Load the single trigger matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<Trigger>> {
    let select = Select::from(Source::table(TRIGGERS_TABLE)).filter(filter);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(trigger_from_row(row)?)),
        None => Ok(None),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = trigger_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        // §9: every system metadata table MUST have id (UUID), name,
        // description, attributes (JSON object).
        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required && by_name(COL_NAME).unique);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
        // A description is optional; NULL reads back as "".
        assert!(!by_name(COL_DESCRIPTION).required);
        // The event and the body are what a trigger *is*; neither is optional.
        assert!(by_name(COL_EVENT).required && by_name(COL_BODY).required);
        // The action is not: a workflow body has none (§10.3).
        assert!(!by_name(COL_ACTION).required);
        // A channel-less event and an always-run trigger are real states.
        assert!(!by_name(COL_CHANNEL).required && !by_name(COL_ONLY_IF).required);
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let t = Trigger::new("t", EventKind::Insert, "insert_row").on("books");
        assert_eq!(trigger_columns().len(), trigger_values(&t).len());
        // Every declared field is a written column **except** `last_run_at`,
        // which only the scheduler writes — so an admin's save cannot silently
        // claim a scheduled trigger ran, or that it never did.
        let written: Vec<String> = trigger_columns();
        let declared: Vec<String> = trigger_fields()
            .iter()
            .map(|f| f.base.name.clone())
            .collect();
        assert_eq!(written.len() + 1, declared.len());
        assert!(!written.contains(&COL_LAST_RUN_AT.to_owned()));
        for column in &written {
            assert!(declared.contains(column), "{column} is not a column");
        }
    }

    #[test]
    fn a_workflow_body_stores_its_discriminator_and_a_null_action() {
        let workflow = Trigger::workflow("approve", EventKind::Insert).on("orders");
        let values = trigger_values(&workflow);
        let at = |col: &str| {
            trigger_columns()
                .iter()
                .position(|c| c == col)
                .map(|i| values[i].clone())
                .unwrap()
        };
        assert_eq!(at(COL_BODY), Value::Text("workflow".to_owned()));
        assert_eq!(at(COL_ACTION), Value::Null);
        // The configuration column is NOT NULL, and an empty object is the
        // honest value for a body that has none.
        assert_eq!(
            at(COL_CONFIGURATION),
            Value::Json(Json::Object(Attrs::new()))
        );

        let action = Trigger::new("audit", EventKind::Insert, "insert_row").on("books");
        let values = trigger_values(&action);
        let at = |col: &str| {
            trigger_columns()
                .iter()
                .position(|c| c == col)
                .map(|i| values[i].clone())
                .unwrap()
        };
        assert_eq!(at(COL_BODY), Value::Text("action".to_owned()));
        assert_eq!(at(COL_ACTION), Value::Text("insert_row".to_owned()));
    }

    #[test]
    fn a_cleared_optional_field_stores_as_null() {
        // "" and whitespace come from a form the admin emptied; both must store
        // as NULL so "cleared" and "never set" read back identically.
        assert_eq!(text_or_null(Some("")), Value::Null);
        assert_eq!(text_or_null(Some("   ")), Value::Null);
        assert_eq!(text_or_null(None), Value::Null);
        assert_eq!(
            text_or_null(Some(" books ")),
            Value::Text("books".to_owned())
        );
    }
}
