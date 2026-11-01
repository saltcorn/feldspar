//! Getting a public dataset: download its files, create its tables, write its
//! rows and create the Analytics dataset on its main table.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use sc_catalog::{Catalog, DataFieldKind, FieldId, SharedTx, Unchecked};
use sc_dataset::{DatasetDef, DatasetId};
use sc_error::{Error, Result};
use sc_types::BasicType;
use serde::Serialize;
use serde_json::{Map, Value as Json};

use super::read::{self, Record};
use super::{FieldDef, PublicDataset, TableDef, find};
use crate::schema_edit;
use sc_access::rows;

/// How the files are downloaded: the server's HTTP client, or a test's
/// fixtures.
#[async_trait]
pub trait Fetch: Send + Sync {
    /// The bytes at `url`, or why they could not be had.
    async fn fetch(&self, url: &str) -> Result<Vec<u8>>;
}

/// What an install is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressStage {
    /// Downloading file `done + 1` of `total`.
    Downloading,
    /// Creating the tables.
    Creating,
    /// Writing the rows of `table`: `done` of about `total`.
    Importing,
    /// Creating the dataset.
    Finishing,
}

/// How far an install has got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Progress {
    /// What it is doing.
    pub stage: ProgressStage,
    /// The file being downloaded, or the table being written.
    pub subject: String,
    /// How much of it is done.
    pub done: u64,
    /// Out of about how much.
    pub total: u64,
}

/// What an install made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The dataset on the main table.
    pub dataset_id: DatasetId,
    /// Its name.
    pub dataset_name: String,
    /// The tables, created now or found already there.
    pub tables: Vec<String>,
    /// The rows written, 0 when the tables were already there.
    pub rows: u64,
}

/// How often the row count is reported.
const REPORT_EVERY: u64 = 1000;

/// Get the public dataset `key`: download its files, create its tables and
/// fill them, and create a dataset on its main table, named after it.
///
/// **All or nothing.** Every file is downloaded before anything is created;
/// the rows are written in one transaction; and a row that will not go in
/// drops the tables again, with the error naming the table, the row and the
/// field. When the tables are already there (it was got before) nothing is
/// downloaded: the dataset on the main table is answered, or made if it was
/// deleted since. A table of one of its names that is *not* all of its tables
/// is in the way, and refused rather than written into.
pub async fn install(
    catalog: &Catalog,
    key: &str,
    fetch: &dyn Fetch,
    progress: &(dyn Fn(Progress) + Send + Sync),
    context: Unchecked<'_>,
) -> Result<Installed> {
    let entry = find(key)?;
    let present: Vec<&str> = entry
        .tables
        .iter()
        .filter(|t| matches!(catalog.get(&t.name), Ok(Some(_))))
        .map(|t| t.name.as_str())
        .collect();
    let tables: Vec<String> = entry.tables.iter().map(|t| t.name.clone()).collect();
    if present.len() == entry.tables.len() {
        let (dataset_id, dataset_name) = dataset_for(catalog, entry).await?;
        return Ok(Installed {
            dataset_id,
            dataset_name,
            tables,
            rows: 0,
        });
    }
    if !present.is_empty() {
        return Err(Error::invalid(format!(
            "`{}` cannot be got here: it creates {}, and {} already exists",
            entry.title,
            quoted(&entry.table_names()),
            quoted(&present)
        )));
    }
    let spatial = catalog.primary_info().spatial();
    if entry.needs_spatial()
        && let Err(reason) = spatial.require()
    {
        return Err(Error::invalid(format!(
            "`{}` needs PostGIS to store its geometry: {reason}",
            entry.title
        )));
    }
    let plans: Vec<TablePlan<'_>> = entry
        .tables
        .iter()
        .map(|t| TablePlan::new(entry, t, spatial.is_available()))
        .collect::<Result<_>>()?;

    // Every file, before anything is created.
    let mut urls: Vec<&str> = Vec::new();
    for source in entry.tables.iter().flat_map(|t| &t.sources) {
        if !urls.contains(&source.url.as_str()) {
            urls.push(&source.url);
        }
    }
    let mut files: HashMap<&str, Vec<u8>> = HashMap::new();
    for (i, url) in urls.iter().enumerate() {
        progress(Progress {
            stage: ProgressStage::Downloading,
            subject: file_name(url).to_owned(),
            done: i as u64,
            total: urls.len() as u64,
        });
        let bytes = fetch.fetch(url).await.map_err(|e| {
            Error::invalid(format!(
                "{url} could not be downloaded: {}",
                e.message_and_causes()
            ))
        })?;
        files.insert(url, bytes);
    }

    progress(Progress {
        stage: ProgressStage::Creating,
        subject: entry.title.clone(),
        done: 0,
        total: plans.len() as u64,
    });
    create_tables(catalog, entry, &plans).await?;

    let written = match fill(catalog, &plans, &files, progress, context).await {
        Ok(written) => written,
        Err(e) => {
            // The import failed either way; what the sentence must not do is
            // say the half-filled tables are gone when the drop failed too.
            return Err(Error::invalid(match drop_tables(catalog, entry).await {
                Ok(()) => format!(
                    "`{}` could not be imported, so its tables were not kept: {}",
                    entry.title,
                    e.message_and_causes()
                ),
                Err(cleanup) => format!(
                    "`{}` could not be imported: {}; and its partly imported tables could \
                     not be dropped, so drop them by hand: {}",
                    entry.title,
                    e.message_and_causes(),
                    cleanup.message_and_causes()
                ),
            }));
        }
    };

    progress(Progress {
        stage: ProgressStage::Finishing,
        subject: entry.title.clone(),
        done: 0,
        total: 1,
    });
    for plan in &plans {
        if plan.key_from_file
            && let Some(pk) = plan.table.primary_key()
        {
            let table = catalog.require(&plan.table.name)?;
            crate::csv::advance_identity_sequence(catalog, &table, &pk.name).await?;
        }
    }
    let (dataset_id, dataset_name) = dataset_for(catalog, entry).await?;
    Ok(Installed {
        dataset_id,
        dataset_name,
        tables,
        rows: written,
    })
}

/// One table, ready to be created and filled.
struct TablePlan<'a> {
    table: &'a TableDef,
    /// The fields kept here, each with the type its values are read as (a
    /// key's is its target's key's).
    fields: Vec<(&'a FieldDef, BasicType)>,
    /// Whether the key is a whole number the file supplies, so the identity
    /// sequence behind it has to be moved past the largest afterwards.
    key_from_file: bool,
    /// The field that points at this same table, if one does: its rows are
    /// written parents first.
    self_reference: Option<&'a FieldDef>,
}

impl<'a> TablePlan<'a> {
    fn new(entry: &'a PublicDataset, table: &'a TableDef, spatial: bool) -> Result<TablePlan<'a>> {
        let mut fields = Vec::with_capacity(table.fields.len());
        for field in &table.fields {
            let ty = if field.is_key() {
                field
                    .references
                    .as_deref()
                    .and_then(|t| entry.table(t))
                    .and_then(TableDef::primary_key)
                    .and_then(FieldDef::basic_type)
                    .ok_or_else(|| Error::msg(format!("`{}` has no target key", field.name)))?
            } else {
                field.basic_type().unwrap_or(BasicType::Text)
            };
            // A point made from coordinates is left out where geometry cannot
            // be stored; the coordinates are columns of their own.
            if matches!(ty, BasicType::Geometry(_)) && !spatial && !field.is_key() {
                continue;
            }
            fields.push((field, ty));
        }
        Ok(TablePlan {
            table,
            key_from_file: table
                .primary_key()
                .is_some_and(|pk| pk.basic_type() == Some(BasicType::Int)),
            self_reference: table
                .fields
                .iter()
                .find(|f| f.references.as_deref() == Some(table.name.as_str())),
            fields,
        })
    }
}

/// Create every table in one schema batch: a reference to a table made
/// earlier in the batch resolves, and one to the table itself is added once
/// the table is there.
async fn create_tables(
    catalog: &Catalog,
    entry: &PublicDataset,
    plans: &[TablePlan<'_>],
) -> Result<()> {
    let mut operations = Vec::new();
    let mut later = Vec::new();
    for plan in plans {
        let mut fields = Vec::with_capacity(plan.fields.len() + 1);
        if plan.table.primary_key().is_none() {
            fields.push(schema_edit::FieldSpec {
                name: "id".to_owned(),
                type_name: BasicType::Int.name().to_owned(),
                label: "ID".to_owned(),
                required: true,
                primary_key: true,
                ..schema_edit::FieldSpec::default()
            });
        }
        for (field, _) in &plan.fields {
            let spec = field_spec(entry, field);
            if plan.self_reference.is_some_and(|f| f.name == field.name) {
                later.push(schema_edit::Operation::AddField {
                    table: plan.table.name.clone(),
                    field: spec,
                });
            } else {
                fields.push(spec);
            }
        }
        operations.push(schema_edit::Operation::CreateTable {
            name: plan.table.name.clone(),
            database: String::new(),
            settings: schema_edit::TableSettings {
                description: Some(entry.long_description()),
                ..schema_edit::TableSettings::default()
            },
            fields,
        });
    }
    operations.extend(later);
    schema_edit::apply(catalog, &operations, &schema_edit::ApplyOptions::default()).await?;
    Ok(())
}

/// The schema editor's description of a catalogue field.
fn field_spec(entry: &PublicDataset, field: &FieldDef) -> schema_edit::FieldSpec {
    let kind = match (&field.references, field.is_key()) {
        (Some(target), true) => DataFieldKind::Key {
            target_table: target.clone(),
            target_field: FieldId(String::new()),
            summary_field: entry
                .table(target)
                .and_then(summary_field)
                .map(|f| FieldId(f.to_owned())),
        },
        _ => DataFieldKind::Plain,
    };
    schema_edit::FieldSpec {
        name: field.name.clone(),
        type_name: if field.is_key() {
            String::new()
        } else {
            field.type_name.clone()
        },
        label: field
            .label
            .clone()
            .unwrap_or_else(|| crate::csv::header_label(&field.name)),
        required: field.required,
        primary_key: field.primary_key,
        kind,
        ..schema_edit::FieldSpec::default()
    }
}

/// The field a reference to `table` is shown by: its name, when it has one
/// that is not its key.
fn summary_field(table: &TableDef) -> Option<&str> {
    ["name", "company_name", "last_name", "title"]
        .into_iter()
        .find(|n| table.field(n).is_some_and(|f| !f.primary_key))
}

/// Write every table's rows in one transaction; answers how many.
async fn fill(
    catalog: &Catalog,
    plans: &[TablePlan<'_>],
    files: &HashMap<&str, Vec<u8>>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    context: Unchecked<'_>,
) -> Result<u64> {
    let first = catalog.require(&plans[0].table.name)?;
    let driver = catalog.driver_for(&first, context)?;
    let tx = SharedTx::begin_on(&driver, first.database.clone());
    let executor = rows::Executor::Transaction(tx.clone());
    // The keys written so far of each table some reference may find missing.
    let wanted: HashSet<&str> = plans
        .iter()
        .flat_map(|p| &p.fields)
        .filter(|(f, _)| f.null_if_missing())
        .filter_map(|(f, _)| f.references.as_deref())
        .collect();
    let mut keys: HashMap<String, HashSet<String>> = HashMap::new();
    let mut written = 0;
    for plan in plans {
        match fill_table(catalog, plan, files, &keys, progress, context, &executor).await {
            Ok((count, table_keys)) => {
                written += count;
                if wanted.contains(plan.table.name.as_str()) {
                    keys.insert(plan.table.name.clone(), table_keys);
                }
            }
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(Error::invalid(format!(
                    "`{}`: {}",
                    plan.table.name,
                    e.message_and_causes()
                )));
            }
        }
    }
    tx.commit().await?;
    Ok(written)
}

/// Write one table's rows; answers how many, and the keys written.
async fn fill_table(
    catalog: &Catalog,
    plan: &TablePlan<'_>,
    files: &HashMap<&str, Vec<u8>>,
    keys: &HashMap<String, HashSet<String>>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    context: Unchecked<'_>,
    executor: &rows::Executor,
) -> Result<(u64, HashSet<String>)> {
    let table = catalog.require(&plan.table.name)?;
    let pk = plan.table.primary_key().map(|f| f.name.as_str());
    let mut seen: HashSet<String> = HashSet::new();
    // A table that references itself is read whole and written parents first;
    // any other is written as it is read.
    let mut held: Vec<(u64, Map<String, Json>)> = Vec::new();
    let mut written = 0;
    let report = |done: u64| {
        progress(Progress {
            stage: ProgressStage::Importing,
            subject: plan.table.name.clone(),
            done,
            total: plan.table.rows,
        })
    };
    report(0);
    for source in &plan.table.sources {
        let bytes = files
            .get(source.url.as_str())
            .ok_or_else(|| Error::msg(format!("{} was not downloaded", source.url)))?;
        let text = read::decode(bytes);
        let mut reader = read::Reader::new(source, &text)?;
        loop {
            let next = reader.next_with(|n, record| {
                let body = row_of(plan, record, &source.na, keys).map_err(|e| at_row(n, e))?;
                Ok((n, body))
            });
            let (n, body) = match next {
                None => break,
                Some(outcome) => outcome?,
            };
            let Some(body) = body else { continue };
            if let Some(pk) = pk {
                let key = key_text(&body[pk]);
                if !seen.insert(key.clone()) {
                    if plan.table.distinct {
                        continue;
                    }
                    return Err(at_row(
                        n,
                        Error::invalid(format!("`{pk}` {key} is there twice")),
                    ));
                }
            }
            if plan.self_reference.is_some() {
                held.push((n, body));
                continue;
            }
            rows::create_row_in(catalog, &table, &Json::Object(body), context, executor)
                .await
                .map_err(|e| at_row(n, e))?;
            written += 1;
            if written % REPORT_EVERY == 0 {
                report(written);
            }
        }
    }
    if let (Some(parent), Some(pk)) = (plan.self_reference, pk) {
        for (n, body) in parents_first(held, pk, &parent.name)? {
            rows::create_row_in(catalog, &table, &Json::Object(body), context, executor)
                .await
                .map_err(|e| at_row(n, e))?;
            written += 1;
            if written % REPORT_EVERY == 0 {
                report(written);
            }
        }
    }
    report(written);
    Ok((written, seen))
}

/// The row the record makes, or `None` for a record with no value at all.
/// A reference to a row that is not there, where the field allows it, is
/// written as none and the row kept.
fn row_of(
    plan: &TablePlan<'_>,
    record: &Record<'_>,
    source_na: &[String],
    keys: &HashMap<String, HashSet<String>>,
) -> Result<Option<Map<String, Json>>> {
    let mut body = Map::new();
    for (field, ty) in &plan.fields {
        let mut value = read::value(field, ty, record, source_na)?;
        if value.is_null() && field.required {
            return Err(Error::invalid(format!("`{}` has no value", field.name)));
        }
        if field.null_if_missing()
            && !value.is_null()
            && let Some(known) = field.references.as_deref().and_then(|t| keys.get(t))
            && !known.contains(&key_text(&value))
        {
            value = Json::Null;
        }
        body.insert(field.name.clone(), value);
    }
    Ok((!body.values().all(Json::is_null)).then_some(body))
}

/// A key's value as text, for comparing keys of any type.
fn key_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The rows of a table that references itself, ordered so that each row's
/// parent is written before it.
fn parents_first(
    rows: Vec<(u64, Map<String, Json>)>,
    pk: &str,
    parent: &str,
) -> Result<Vec<(u64, Map<String, Json>)>> {
    let mut placed: HashSet<String> = HashSet::new();
    let mut pending = rows;
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let before = pending.len();
        let (ready, waiting): (Vec<_>, Vec<_>) = pending.into_iter().partition(|(_, row)| {
            row.get(parent)
                .filter(|p| !p.is_null())
                .is_none_or(|p| placed.contains(&key_text(p)))
        });
        for (n, row) in ready {
            placed.insert(key_text(&row[pk]));
            ordered.push((n, row));
        }
        if waiting.len() == before {
            let (n, row) = &waiting[0];
            return Err(at_row(
                *n,
                Error::invalid(format!(
                    "`{parent}` names {}, which is not a row of the table",
                    key_text(&row[parent])
                )),
            ));
        }
        pending = waiting;
    }
    Ok(ordered)
}

/// The dataset on `entry`'s main table: one already there, or a new one named
/// after the entry (with a number when that name is taken).
async fn dataset_for(catalog: &Catalog, entry: &PublicDataset) -> Result<(DatasetId, String)> {
    let library = sc_dataset::load_library(catalog).await?;
    if let Some(found) = library.defs().find(
        |d| matches!(&d.base, sc_dataset::Base::Table { table } if *table == entry.dataset_table),
    ) {
        return Ok((found.id, found.name.clone()));
    }
    let taken: HashSet<&str> = library.defs().map(|d| d.name.as_str()).collect();
    let name = std::iter::once(entry.title.clone())
        .chain((2..).map(|i| format!("{} ({i})", entry.title)))
        .find(|n| !taken.contains(n.as_str()))
        .unwrap_or_else(|| entry.title.clone());
    let mut def = DatasetDef::over_table(&name, &entry.dataset_table);
    def.description = entry.dataset_description();
    sc_dataset::save_dataset(catalog, &def).await?;
    Ok((def.id, name))
}

/// Drop whatever of `entry`'s tables exist, those referenced last.
async fn drop_tables(catalog: &Catalog, entry: &PublicDataset) -> Result<()> {
    let operations: Vec<_> = entry
        .tables
        .iter()
        .rev()
        .filter(|t| matches!(catalog.get(&t.name), Ok(Some(_))))
        .map(|t| schema_edit::Operation::DropTable {
            table: t.name.clone(),
        })
        .collect();
    if operations.is_empty() {
        return Ok(());
    }
    schema_edit::apply(catalog, &operations, &schema_edit::ApplyOptions::default()).await?;
    Ok(())
}

fn at_row(n: u64, e: Error) -> Error {
    Error::invalid(format!("row {n}: {}", e.message_and_causes()))
}

fn file_name(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

fn quoted(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: i64, parent: Option<i64>) -> (u64, Map<String, Json>) {
        let body = json!({ "id": id, "parent": parent });
        (id as u64, body.as_object().cloned().unwrap_or_default())
    }

    #[test]
    fn a_tree_is_written_parents_first_and_a_dangling_parent_is_named() {
        let ordered = parents_first(
            vec![
                row(3, Some(2)),
                row(1, None),
                row(2, Some(1)),
                row(4, Some(1)),
            ],
            "id",
            "parent",
        )
        .unwrap();
        let ids: Vec<i64> = ordered
            .iter()
            .map(|(_, r)| r["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2, 4, 3]);

        let e = parents_first(vec![row(1, None), row(2, Some(9))], "id", "parent")
            .unwrap_err()
            .to_string();
        assert!(e.contains("row 2") && e.contains("9"), "{e}");
    }
}
