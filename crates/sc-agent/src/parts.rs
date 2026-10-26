//! `_fd_run_parts`: where the display parts a run's tools showed the person are
//! kept (TODO-agent-skills AS6.3).
//!
//! A part is stored apart from the run's context so that the transcript — which
//! is read on every step and sent, in part, on every request — carries only a
//! [`DisplayRef`] to it. An image is a few hundred kilobytes the model never
//! reads; the chat surfaces fetch it by id when they draw it.
//!
//! A part belongs to its run: it is read through the run (the surfaces check
//! the run's owner first) and deleted with it.

use chrono::Utc;
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::interact::{DisplayPart, DisplayRef};
use crate::run::RunId;

/// Name of the display parts table in the primary database.
pub const RUN_PARTS_TABLE: &str = "_fd_run_parts";

const COL_ID: &str = "id";
const COL_RUN: &str = "run_id";
const COL_KIND: &str = "kind";
const COL_MIME: &str = "mime_type";
const COL_SUMMARY: &str = "summary";
/// The rows of a rows part, or an image's or a clip's name.
const COL_DATA: &str = "data";
/// The bytes of an image or a clip; NULL for rows.
const COL_CONTENT: &str = "content";
const COL_CREATED_AT: &str = "created_at";

fn fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain(COL_RUN, TypeRef::Basic(BasicType::Uuid)).required(),
        DataField::plain(COL_KIND, text()).required(),
        DataField::plain(COL_MIME, text()).required(),
        DataField::plain(COL_SUMMARY, text()),
        DataField::plain(COL_DATA, TypeRef::Basic(BasicType::Json)).required(),
        DataField::plain(COL_CONTENT, TypeRef::Basic(BasicType::Bytes)),
        DataField::plain(COL_CREATED_AT, TypeRef::Basic(BasicType::Timestamp)).required(),
    ]
}

/// Ensure `_fd_run_parts` exists. Called by [`bootstrap_runs`](crate::bootstrap_runs).
pub async fn bootstrap_run_parts(catalog: &Catalog) -> Result<Table> {
    catalog.bootstrap_table(RUN_PARTS_TABLE, &fields()).await
}

/// A display part as it was stored.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredPart {
    /// Its reference.
    pub reference: DisplayRef,
    /// The run it belongs to.
    pub run: RunId,
    /// The rows of a rows part (`{ title, view, columns, rows }`), or a file's
    /// `{ name }`.
    pub data: Json,
    /// The bytes of an image or a clip.
    pub content: Option<Vec<u8>>,
}

/// Store `part` for `run`, answering the reference the run keeps.
pub async fn save_display_part(
    catalog: &Catalog,
    run: RunId,
    part: &DisplayPart,
) -> Result<DisplayRef> {
    part.check()?;
    let reference = DisplayRef {
        id: Uuid::new_v4(),
        kind: part.kind().to_owned(),
        mime_type: part.mime_type().to_owned(),
        summary: part.summary(),
    };
    let insert = Insert::row(
        RUN_PARTS_TABLE,
        [
            COL_ID,
            COL_RUN,
            COL_KIND,
            COL_MIME,
            COL_SUMMARY,
            COL_DATA,
            COL_CONTENT,
            COL_CREATED_AT,
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect(),
        vec![
            Expr::Lit(Value::Uuid(reference.id)),
            Expr::Lit(Value::Uuid(run.0)),
            Expr::Lit(Value::Text(reference.kind.clone())),
            Expr::Lit(Value::Text(reference.mime_type.clone())),
            Expr::Lit(Value::Text(reference.summary.clone())),
            Expr::Lit(Value::Json(part.stored_data())),
            Expr::Lit(match part.bytes() {
                Some(bytes) => Value::Bytes(bytes.to_vec()),
                None => Value::Null,
            }),
            Expr::Lit(Value::Timestamp(Utc::now())),
        ],
    );
    exec(catalog, Statement::from(insert)).await?;
    Ok(reference)
}

/// The part `id` of `run`, if there is one. A part of another run is not found:
/// a caller allowed to read one run cannot reach another's parts by id.
pub async fn load_display_part(
    catalog: &Catalog,
    run: RunId,
    id: Uuid,
) -> Result<Option<StoredPart>> {
    let select = Select::from(Source::table(RUN_PARTS_TABLE))
        .filter(
            Expr::col(COL_ID)
                .eq(Expr::lit(id))
                .and(Expr::col(COL_RUN).eq(Expr::lit(run.0))),
        )
        .limit(1);
    let rows: Vec<Row> = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    rows.first().map(from_row).transpose()
}

/// Delete every part of `run`. Called when the run is deleted.
pub async fn delete_display_parts(catalog: &Catalog, run: RunId) -> Result<()> {
    if catalog.get(RUN_PARTS_TABLE)?.is_none() {
        return Ok(());
    }
    let delete = Delete::from(RUN_PARTS_TABLE).filter(Expr::col(COL_RUN).eq(Expr::lit(run.0)));
    exec(catalog, Statement::from(delete)).await
}

fn from_row(row: &Row) -> Result<StoredPart> {
    let text = |column: &str| match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        Some(Value::Null) | None if column == COL_SUMMARY => Ok(String::new()),
        _ => Err(bad(column)),
    };
    let uuid = |column: &str| match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        _ => Err(bad(column)),
    };
    Ok(StoredPart {
        reference: DisplayRef {
            id: uuid(COL_ID)?,
            kind: text(COL_KIND)?,
            mime_type: text(COL_MIME)?,
            summary: text(COL_SUMMARY)?,
        },
        run: RunId(uuid(COL_RUN)?),
        data: match row.get(COL_DATA) {
            Some(Value::Json(j)) => j.clone(),
            _ => return Err(bad(COL_DATA)),
        },
        content: match row.get(COL_CONTENT) {
            Some(Value::Bytes(b)) => Some(b.clone()),
            _ => None,
        },
    })
}

fn bad(column: &str) -> Error {
    Error::invalid(format!(
        "{RUN_PARTS_TABLE}.{column} is missing or of the wrong type"
    ))
}

async fn exec(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}
