//! The `_fd_streams` table: its schema, bootstrap, and the [`Stream`] ⇄ row
//! mapping (TODO §5; design §9).
//!
//! §9's rule applies here with nothing to argue about. A stream, like a trigger
//! or a model, has **nothing to introspect it from**: no column in
//! `information_schema` says "subscribe to `house/boiler/#` on this broker and
//! read each payload as an object with a `temperature` float in it". So its row
//! *is* its definition — this is not an overlay, and without the row there is
//! no stream at all.
//!
//! **Reading is strict**, as `load_model` and `load_trigger` are: a
//! column that is missing or of the wrong shape is an [`Error::invalid`] naming
//! the stream and the column, never a silently defaulted field. The failure
//! that prevents is specific to this entity and worse than a form that will not
//! open: a stream read with half a configuration would *connect anyway*, to the
//! wrong broker or the wrong topic, and deliver elements that look exactly like
//! the right ones.
//!
//! The judgements §5 asks for, made out loud:
//!
//! - **`element_type` is not a column.** It is a pure function of `provider` +
//!   `configuration` ([`StreamProvider::element_type`](crate::StreamProvider::element_type)),
//!   and a stored copy would be a second answer that drifts the day a
//!   provider's declaration changes. Computed on read, cached on the running
//!   stream. See [`stream`](crate::stream)'s module docs.
//! - **`min_role` is nullable and NULL is the restrictive end** — admin-only,
//!   the way a trigger's is. A flow nobody has thought about the access of is
//!   not public.
//! - **`enabled` is an attribute, not a column**, because §9 says sparse: a
//!   stream that has never been switched off carries no key saying so, and the
//!   supervisor reads [`Stream::is_enabled`].
//!
//! What is *not* here: validation (see [`validate`](crate::validate), which
//! [`save_stream`] calls) and the secret round trip (see
//! [`secrets`](crate::secrets), whose inward half [`save_stream`] calls too).
//!
//! [`Error::invalid`]: sc_error::Error::invalid

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;

use crate::registry::StreamRegistry;
use crate::secrets::restore_secrets;
use crate::stream::{Stream, StreamId};
use crate::validate::validate_stream;

/// Name of the streams table in the primary database.
pub const STREAMS_TABLE: &str = "_fd_streams";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The stream's unique name — what a trigger's channel, an application's
/// `StreamRef` and the observe socket's path segment all name.
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// The registered stream-provider name.
pub const COL_PROVIDER: &str = "provider";
/// The provider's settings (JSON object); secrets stored as given.
pub const COL_CONFIGURATION: &str = "configuration";
/// The role floor for observing this stream through an application, or NULL for
/// admin-only.
pub const COL_MIN_ROLE: &str = "min_role";
/// The sparse per-stream values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// The fields of the `_fd_streams` table, in declaration order.
///
/// `name` carries the `UNIQUE` constraint for the reason a trigger's and a
/// model's do: it is the key everything resolves through — a trigger's channel,
/// an app's `StreamRef`, a socket path — so two streams claiming one name is
/// not a state the system can serve. The database is the authority, because two
/// admins saving concurrently cannot see each other's transaction.
fn stream_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_PROVIDER, text()).required(),
        // Required *as a column*: an empty configuration is `{}`, which is a
        // real value a provider with no settings has, and NULL is not.
        DataField::plain(COL_CONFIGURATION, TypeRef::Basic(BasicType::Json)).required(),
        // Nullable, and NULL is admin-only rather than "unset" — see the module
        // docs.
        DataField::plain(COL_MIN_ROLE, TypeRef::Basic(BasicType::Int)),
        DataField::plain(COL_ATTRIBUTES, TypeRef::Basic(BasicType::Json)).required(),
        // When it was last saved (`sc_catalog::edited`): bookkeeping for the
        // entities list, written by every save and read by nothing else.
        sc_catalog::updated_at_field(),
    ]
}

/// Ensure the `_fd_streams` table exists, creating it if absent, and return it.
///
/// Idempotent, and safe against a database that has never seen Saltcorn — the
/// same contract as `bootstrap_triggers` and `bootstrap_models`. Called from
/// `install_streams` at boot (task 5.3), which is where `bootstrap_models` is
/// called from and for its reason: a stream is no use to a `feldspar` command
/// that is not serving, so the table is created by the thing that would
/// subscribe rather than by every invocation of the binary.
pub async fn bootstrap_streams(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(STREAMS_TABLE, &stream_fields())
        .await
}

/// Save a stream: insert its row, or update it in place if a row with its
/// [`StreamId`] already exists.
///
/// Two things happen before the write, in this order, and the order is the
/// point.
///
/// 1. **The secrets are restored** ([`restore_secrets`]): wherever the caller
///    sent the redaction sentinel back for a `secret` setting, the stored value
///    takes its place. It is here, in the one door every write goes through,
///    rather than in the handler, because the sentinel must not reach the
///    provider's own `validate` either — see [`secrets`](crate::secrets)'s
///    module docs.
/// 2. **[`validate_stream`] runs**, for `save_model`'s reason and then some: a
///    stream that could never work — an unknown provider, a setting the
///    provider does not declare, a configuration whose element type cannot be
///    computed — is refused while the admin is still looking at the form. A
///    model that fails validation does not fit and a trigger that fails does
///    not fire, but a stream that is wrong *connects anyway* and throws away
///    every payload a healthy broker sends it.
pub async fn save_stream(
    catalog: &Catalog,
    registry: &StreamRegistry,
    stream: &Stream,
) -> Result<()> {
    // Read once: the row that is there decides both what the sentinel stands
    // for and whether this is an insert or an update.
    let stored = load_stream(catalog, stream.id).await?;
    let stream = &restore_secrets(registry, stored.as_ref(), stream);
    validate_stream(catalog, registry, stream).await?;

    let columns = stream_columns();
    let values = stream_values(stream);

    if stored.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = Update::new(STREAMS_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(stream.id.0)));
        exec(catalog, Statement::from(update)).await
    } else {
        let insert = Insert::row(
            STREAMS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        exec(catalog, Statement::from(insert)).await
    }
}

/// Load the stream with this id, if it exists.
pub async fn load_stream(catalog: &Catalog, id: StreamId) -> Result<Option<Stream>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the stream named `name`, if any — the lookup a trigger's channel and an
/// application's `StreamRef` resolve through.
pub async fn load_stream_by_name(catalog: &Catalog, name: &str) -> Result<Option<Stream>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name.trim()))).await
}

/// The stream named `name`, or a not-found error naming it.
pub async fn require_stream(catalog: &Catalog, name: &str) -> Result<Stream> {
    load_stream_by_name(catalog, name)
        .await?
        .ok_or_else(|| Error::not_found(format!("no stream named `{name}`")))
}

/// Every stored stream, ordered by name — what the Streams list shows and what
/// the supervisor's `reload` diffs against.
///
/// Every *stored* one, including those that no longer validate and those that
/// are disabled: a stream whose provider came from a module that has been
/// uninstalled stays listed and editable, because editing it is the repair, and
/// a disabled one is exactly what the admin switched off rather than deleted.
/// Deciding which of them to subscribe to is the supervisor's job, not this
/// one's.
pub async fn list_streams(catalog: &Catalog) -> Result<Vec<Stream>> {
    let select = Select::from(Source::table(STREAMS_TABLE));
    let mut out: Vec<Stream> = rows(catalog, select)
        .await?
        .iter()
        .map(stream_from_row)
        .collect::<Result<_>>()?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Delete a stream, returning whether one was there to delete.
///
/// **Nothing goes with it**, and there is nothing that could: an element is not
/// stored (§4), so a deleted stream leaves no rows behind. What it does leave
/// is a running subscription, which the supervisor's next `reload` stops — the
/// caller (task 6.2's handler) calls it, for the same reason saving one does.
///
/// ## The refusal
///
/// Refused while `referents` is non-empty, **naming them** — the refusal
/// `delete_llm_model` already makes, and for its reason. A trigger holds its
/// stream as a `channel`, which is a *name*, not a foreign key: nothing in the
/// database stops the row going, and what an admin would be left with is a
/// trigger that has silently stopped firing and no way to see why. So the
/// deletion is refused with the referents in the sentence, and the admin can
/// choose — delete the trigger, or repoint it.
///
/// **The caller passes the referents in.** A trigger lives in `sc-action`,
/// which this crate must not depend on (§2: `sc-stream` knows nothing about
/// triggers), so the check cannot be made here. Task 6.2's handler, which can
/// see both, collects the triggers whose channel is this stream's name and
/// hands them over as strings — exactly the arrangement `delete_llm_model` and
/// `delete_file_store` use for agents and applications.
pub async fn delete_stream(catalog: &Catalog, id: StreamId, referents: &[String]) -> Result<bool> {
    let Some(stream) = load_stream(catalog, id).await? else {
        // Nothing to delete, so nothing can be referencing it — and reporting
        // "already gone" beats reporting a reference to a stream that is not
        // there.
        return Ok(false);
    };
    if !referents.is_empty() {
        return Err(Error::invalid(format!(
            "stream `{}` is still used by {}; remove those references before deleting it",
            stream.name,
            referents.join(", ")
        )));
    }
    let delete = Delete::from(STREAMS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    exec(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// How a trigger names a stream in a deletion refusal, so every caller phrases
/// it the same way.
///
/// The referents [`delete_stream`] lists are prose written by the caller, and
/// this is the phrasing: ``trigger `store_temp` ``. One function rather than a
/// `format!` at each call site, because the admin reads these next to each
/// other.
pub fn trigger_referent(trigger_name: &str) -> String {
    format!("trigger `{trigger_name}`")
}

/// The row's columns, in the order [`stream_values`] produces them.
fn stream_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_PROVIDER,
        COL_CONFIGURATION,
        COL_MIN_ROLE,
        COL_ATTRIBUTES,
        sc_catalog::COL_UPDATED_AT,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The stream serialised to its row's values, in [`stream_columns`] order.
fn stream_values(stream: &Stream) -> Vec<Value> {
    vec![
        Value::Uuid(stream.id.0),
        Value::Text(stream.name.trim().to_owned()),
        Value::Text(stream.description.clone()),
        Value::Text(stream.provider.trim().to_owned()),
        Value::Json(Json::Object(stream.configuration.clone())),
        match stream.min_role {
            Some(role) => Value::Int(i64::from(role)),
            None => Value::Null,
        },
        Value::Json(Json::Object(stream.attributes.clone())),
        // Every save is an edit (`sc_catalog::edited`).
        sc_catalog::updated_now(),
    ]
}

/// Rebuild a [`Stream`] from its `_fd_streams` row. The strictness note in the
/// module docs applies throughout.
fn stream_from_row(row: &Row) -> Result<Stream> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => StreamId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let name = text(row, COL_NAME)?;
    let at = |e: Error| Error::invalid(format!("stream `{name}`: {e}"));

    Ok(Stream {
        id,
        name: name.clone(),
        description: optional_text(row, COL_DESCRIPTION)?,
        provider: text(row, COL_PROVIDER).map_err(at)?,
        configuration: object(row, COL_CONFIGURATION).map_err(at)?,
        min_role: min_role(row).map_err(at)?,
        attributes: object(row, COL_ATTRIBUTES).map_err(at)?,
    })
}

/// The role floor. A value outside 1–100 is **not** clamped: roles are a fixed
/// scale, and reading a corrupt one as the nearest valid role would quietly
/// change who may observe the flow. (`_fd_triggers` makes the same argument for
/// the same column.)
fn min_role(row: &Row) -> Result<Option<u8>> {
    match row.get(COL_MIN_ROLE) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Int(i)) => Ok(Some(
            u8::try_from(*i)
                .ok()
                .filter(|r| (1..=100).contains(r))
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "{STREAMS_TABLE}.{COL_MIN_ROLE} should be a role between 1 and 100, \
                         got {i}"
                    ))
                })?,
        )),
        other => Err(bad_column(COL_MIN_ROLE, "an integer role", other)),
    }
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A text column whose NULL means "none given".
fn optional_text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        Some(Value::Null) | None => Ok(String::new()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(other)) => Err(Error::invalid(format!(
            "{column} should be a json object, got {}",
            kind_of(other)
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

/// What a JSON value is, in a word, for an error message.
fn kind_of(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Load the single stream matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<Stream>> {
    let select = Select::from(Source::table(STREAMS_TABLE)).filter(filter);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(stream_from_row(row)?)),
        None => Ok(None),
    }
}

/// Run a statement that returns no rows of interest.
async fn exec(catalog: &Catalog, statement: Statement) -> Result<()> {
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
    use std::sync::Arc;

    /// A row of exactly the columns [`stream_columns`] names, so a test can
    /// damage one of them and assert what reading it says.
    fn row(values: Vec<Value>) -> Row {
        Row::new(Arc::new(stream_columns()), values).unwrap()
    }

    fn boiler() -> Stream {
        Stream::with_id(StreamId(uuid::Uuid::nil()), "boiler", "mqtt")
            .description("the boiler's temperature")
            .config("topic", "house/boiler/#")
            .min_role(40)
            .enabled(false)
    }

    #[test]
    fn schema_has_the_section_5_columns() {
        let fields = stream_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required && by_name(COL_NAME).unique);
        assert!(!by_name(COL_DESCRIPTION).required);
        assert!(by_name(COL_PROVIDER).required);
        assert!(by_name(COL_CONFIGURATION).required);
        assert!(by_name(COL_ATTRIBUTES).required);
        // NULL is admin-only, which is a value the column has to be able to
        // hold.
        assert!(!by_name(COL_MIN_ROLE).required);
        // The element type is a function of the provider and the configuration,
        // never a column (§5).
        assert!(fields.iter().all(|f| f.base.name != "element_type"));
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        assert_eq!(stream_columns().len(), stream_values(&boiler()).len());
        let declared: Vec<String> = stream_fields()
            .iter()
            .map(|f| f.base.name.clone())
            .collect();
        assert_eq!(stream_columns(), declared);
    }

    #[test]
    fn a_stream_round_trips_through_its_row() {
        let stream = boiler();
        let read = stream_from_row(&row(stream_values(&stream))).unwrap();
        assert_eq!(read, stream);
        assert!(!read.is_enabled());
        assert_eq!(read.min_role, Some(40));
        assert_eq!(read.configuration["topic"], Json::from("house/boiler/#"));
    }

    #[test]
    fn an_admin_only_stream_stores_a_null_min_role_and_reads_back_as_none() {
        let stream = Stream::with_id(StreamId(uuid::Uuid::nil()), "boiler", "mqtt");
        let values = stream_values(&stream);
        assert_eq!(values[5], Value::Null);
        let read = stream_from_row(&row(values)).unwrap();
        assert_eq!(read.min_role, None);
        // And an untouched stream is enabled with no attribute saying so.
        assert!(read.is_enabled());
        assert!(read.attributes.is_empty());
    }

    #[test]
    fn a_damaged_row_is_refused_naming_the_stream_and_the_column() {
        // A configuration that is not an object: the stream would otherwise
        // connect with no settings at all.
        let mut values = stream_values(&boiler());
        values[4] = Value::Json(Json::from("house/boiler/#"));
        let err = stream_from_row(&row(values)).unwrap_err().to_string();
        assert!(
            err.contains("`boiler`") && err.contains(COL_CONFIGURATION) && err.contains("a string"),
            "{err}"
        );

        // A role outside the scale is not clamped to the nearest one.
        let mut values = stream_values(&boiler());
        values[5] = Value::Int(140);
        let err = stream_from_row(&row(values)).unwrap_err().to_string();
        assert!(
            err.contains("`boiler`") && err.contains("between 1 and 100") && err.contains("140"),
            "{err}"
        );

        // A missing provider is not an empty provider.
        let mut values = stream_values(&boiler());
        values[3] = Value::Null;
        let err = stream_from_row(&row(values)).unwrap_err().to_string();
        assert!(
            err.contains("`boiler`") && err.contains(COL_PROVIDER),
            "{err}"
        );

        // And a row with no id column at all is named rather than defaulted.
        let columns = Arc::new(vec![COL_NAME.to_owned()]);
        let row = Row::new(columns, vec![Value::Text("boiler".into())]).unwrap();
        let err = stream_from_row(&row).unwrap_err().to_string();
        assert!(err.contains(COL_ID), "{err}");
    }

    #[test]
    fn a_null_description_is_none_given_rather_than_a_broken_row() {
        let mut values = stream_values(&boiler());
        values[2] = Value::Null;
        let read = stream_from_row(&row(values)).unwrap();
        assert_eq!(read.description, "");
    }
}
