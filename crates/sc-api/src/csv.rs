//! A table's rows in and out as CSV, and a **new table** from one (design
//! §13.1).
//!
//! The one bulk shape an admin already has a tool for: a spreadsheet exports a
//! CSV, and a table's rows are a CSV. This module is the translation on both
//! sides of that — and, since a spreadsheet is also how a table often *starts*,
//! the deduction of a schema from a file's header and contents.
//!
//! Everything else is **nothing else**: an import goes row by row through
//! [`rows::create_row_ctx`]/[`rows::update_row_ctx`], the same functions the row
//! endpoints and an application's REST provider write through, so a CSV write is
//! held to every rule an ordinary write is: type coercion, rich-type attributes,
//! `File` field paths, ownership, RLS, and the insert triggers that fire
//! afterwards. A bulk loader that went straight to `INSERT` would be a second
//! write path with none of that, which is the thing this codebase does not do.
//! Creating a table goes through [`schema_edit::apply`] for the same reason.
//!
//! **Import is not a transaction.** Each row stands or falls on its own and a
//! failure is reported with its line number rather than discarding the rows that
//! did work — a 5000-line export with three bad dates in it is a file to fix
//! three lines of, not a file to be refused whole. The caller sees both numbers.
//! Creating a table from a file is the one exception, and deliberately: a table
//! whose first import was half-refused is not a table anybody wanted, so the
//! whole thing is dropped and the errors reported (see [`create_table_from_csv`]).
//!
//! ## What the header may say
//!
//! A header cell finds its field by name, by label, or by the name its label
//! would make (`Item Name` → `item_name`) — so a file exported from a
//! spreadsheet, where the columns are titled for people, imports without being
//! edited first. A header naming **nothing** in the table is *ignored*: the
//! wrong-file mistake is caught by the check that every required field is
//! covered, and refusing a file for carrying a column the table happens not to
//! want would refuse most real exports. This matches Saltcorn 1.
//!
//! ## The primary key
//!
//! A file that names the primary key **upserts**: a row whose key is already
//! there is updated, one whose key is new is inserted with that key, and a blank
//! key cell is an insert with a key the database assigns. That is what makes a
//! CSV a round trip rather than a one-way duplication — an admin edits the
//! export and imports it back. The identity sequence is moved past the largest
//! key written afterwards, or the next ordinary insert would collide with a row
//! the import placed.
//!
//! The wire shape is a **string**, not bytes: CSV is text, the endpoint model is
//! JSON (see [`crate::endpoint`]), and a table small enough for an admin to
//! round-trip through a spreadsheet is small enough to cross as one.

use std::collections::{HashMap, HashSet};

use sc_catalog::{CallerContext, Catalog, DataFieldKind, SharedTx, Table};
use sc_error::{Error, Result};
use sc_query::Expr;
use sc_types::BasicType;
use serde_json::{Map, Value as Json};

use crate::rows;
use crate::schema_edit;

/// The header cell that means "this column is the table's primary key" when a
/// table is created from a file.
///
/// A convention, and Saltcorn 1's: a CSV has no way to say which column is the
/// key, and `id` is what an export of a keyed table calls it. Any other name
/// makes an ordinary field, and a file with no `id` column makes a table with no
/// key — which is a state, not a failure (GOALS: the admin creates key fields
/// like any other field).
const PK_COLUMN: &str = "id";

/// What an import did: the rows that went in, the rows that were replaced, and
/// the ones that did not with the reason and the line each was on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportOutcome {
    /// How many rows were inserted.
    pub inserted: usize,
    /// How many named a primary key that was already there and replaced it.
    pub updated: usize,
    /// One message per rejected row, each naming its line in the file.
    pub errors: Vec<String>,
}

/// Every row of `table` as a CSV document: a header line of column names, then
/// one line per row.
///
/// The columns are the table's **stored** fields in declaration order.
/// Calculated fields are left out on purpose: they are computed on read and
/// refused on write, so putting them in the export would produce a file that
/// cannot be imported back — and a round trip is what an export is for.
pub async fn export_table(
    catalog: &Catalog,
    table: &Table,
    context: Option<&CallerContext>,
) -> Result<String> {
    let columns: Vec<&str> = stored_columns(table);
    let rows = rows::list_rows_ctx(catalog, table, context).await?;
    let Json::Array(rows) = rows else {
        return Err(Error::msg("the row layer did not return an array"));
    };

    let mut writer = ::csv::Writer::from_writer(Vec::new());
    writer
        .write_record(&columns)
        .map_err(|e| Error::msg(format!("could not write the CSV header: {e}")))?;
    for row in &rows {
        let record: Vec<String> = columns.iter().map(|c| cell(row.get(*c))).collect();
        writer
            .write_record(&record)
            .map_err(|e| Error::msg(format!("could not write a CSV row: {e}")))?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|e| Error::msg(format!("could not finish the CSV: {e}")))?;
    String::from_utf8(bytes).map_err(|e| Error::msg(format!("the CSV was not valid UTF-8: {e}")))
}

/// Write the rows of a CSV document into `table`, one row at a time.
///
/// The header is resolved to fields as the module docs describe: by name, by
/// label, or by the name a label would make, with anything unmatched ignored.
/// What is *not* ignored is a **required** field no column covers — that is the
/// wrong file, it is refused before a row is written, and the message names the
/// field.
///
/// A blank cell is `null`, not the empty string: a spreadsheet has no way to
/// write "absent" other than by leaving the cell empty, and a `NOT NULL` column
/// will say so itself. The one exception is a text column, where the empty
/// string is a value a user may well have meant.
///
/// # One transaction, one savepoint per row
///
/// The whole import runs inside a single transaction with **`SET CONSTRAINTS
/// ALL DEFERRED`**, and each row inside a savepoint of its own. That is two
/// properties at once:
///
///   - a row may reference another row of the same file that has not arrived
///     yet — a self-joining `project` CSV listing a child before its parent —
///     because the foreign keys are checked at the commit, by which time every
///     row is there. This is what the `DEFERRABLE` on every generated reference
///     is for;
///   - a row that will not go in still does not take the others down with it:
///     the savepoint rolls that row back and the import carries on, so the two
///     numbers this returns are still "what landed" and "what did not".
///
/// The one thing that cannot be reported per line is a **deferred** violation —
/// a key that matches nothing anywhere in the file. Postgres raises it at the
/// commit, when the statement that caused it is long past, so it fails the
/// import as a whole and says so.
pub async fn import_table(
    catalog: &Catalog,
    table: &Table,
    document: &str,
    context: Option<&CallerContext>,
) -> Result<ImportOutcome> {
    // `flexible`: a row with a cell too few is a row with a blank at the end,
    // and a trailing comma is a cell too many. Both are things spreadsheets
    // write, and neither is worth refusing a line over.
    let mut reader = ::csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(document.as_bytes());
    let header: Vec<String> = reader
        .headers()
        .map_err(|e| Error::invalid(format!("could not read the CSV header: {e}")))?
        .iter()
        .map(|h| h.trim().to_owned())
        .collect();
    if header.is_empty() {
        return Err(Error::invalid("the CSV has no header row"));
    }
    let columns = map_header(table, &header)?;

    let pk = rows::single_pk(table).ok();
    let mut keys_seen: HashSet<String> = HashSet::new();
    let mut summaries = SummaryLookups::default();
    let mut outcome = ImportOutcome::default();
    let mut wrote_keys = false;

    // On the database that hosts the table, not on the primary: a table created
    // on a connection takes its rows in the same database its columns are in.
    let driver = catalog.driver_for(table)?;
    // Shared rather than held: the import makes every statement itself, but the
    // *events* its writes raise reach triggers that write too, and those writes
    // belong in this transaction as much as the import's own (§10.3, decision 6).
    // The caller travels with each statement — `SharedTx::run` applies its GUCs —
    // so an RLS table's policies see the importer on the import's writes and the
    // trigger on the trigger's.
    let tx = SharedTx::begin_on(&driver, table.database.clone());
    // Rows may reference rows that arrive later in the file; the keys are all
    // checked at commit.
    tx.defer_constraints().await?;
    let executor = rows::Executor::Transaction(tx.clone());

    for (index, record) in reader.records().enumerate() {
        // Line numbers as a spreadsheet counts them: the header is line 1, so
        // the first data row is line 2. An error an admin cannot locate in the
        // file is an error they cannot fix.
        let line = index + 2;
        let record = match record {
            Ok(record) => record,
            Err(e) => {
                outcome.errors.push(format!("line {line}: {e}"));
                continue;
            }
        };
        let mut body = Map::new();
        for (position, column) in columns.iter().enumerate() {
            let Some(name) = column else { continue };
            let raw = record.get(position).unwrap_or("");
            body.insert(name.clone(), field_json(table, name, raw));
        }

        // From here on the row touches the database, so it gets a savepoint: a
        // failed statement poisons a Postgres transaction, and without one the
        // next row would fail with "current transaction is aborted" rather than
        // with anything about itself.
        tx.batch(&format!("SAVEPOINT {ROW_SAVEPOINT}")).await?;
        let result = write_row(
            catalog,
            table,
            pk.as_deref(),
            body,
            context,
            &mut summaries,
            &mut keys_seen,
            &executor,
        )
        .await;
        match result {
            Ok(written) => {
                match written {
                    Written::Inserted => outcome.inserted += 1,
                    Written::InsertedWithKey => {
                        outcome.inserted += 1;
                        wrote_keys = true;
                    }
                    Written::Updated => {
                        outcome.updated += 1;
                        wrote_keys = true;
                    }
                }
                tx.batch(&format!("RELEASE SAVEPOINT {ROW_SAVEPOINT}"))
                    .await?;
            }
            Err(e) => {
                outcome.errors.push(format!("line {line}: {}", e.causes()));
                tx.batch(&format!("ROLLBACK TO SAVEPOINT {ROW_SAVEPOINT}"))
                    .await?;
            }
        }
    }

    if outcome.inserted + outcome.updated == 0 {
        // Nothing to keep. Rolling back rather than committing an empty
        // transaction is the same outcome and says what happened.
        let _ = tx.rollback().await;
        return Ok(outcome);
    }
    tx.commit().await.map_err(|e| {
        Error::invalid(format!(
            "the import was rolled back when it was committed: {}. \
             A reference that matches no row anywhere in the file fails here rather \
             than on its own line, because that is when the database checks it.",
            e.causes()
        ))
    })?;

    // A key the file chose is a key the sequence has not issued. Leaving it
    // behind would make the *next* ordinary insert collide with a row this
    // import placed — which would look like a bug in the row editor, not in the
    // import that caused it.
    if wrote_keys && let Some(pk) = pk.as_deref() {
        advance_identity_sequence(catalog, table, pk).await?;
    }
    Ok(outcome)
}

/// The savepoint each row is written inside. Structural, never user data.
const ROW_SAVEPOINT: &str = "sc_csv_row";

/// Write one row of the file: resolve its summary-valued keys, decide whether
/// its primary key makes it an insert or a replacement, and do it.
///
/// Split out of [`import_table`] so that everything which can fail for *this
/// row* is one `Result` the savepoint can be wound back on — including the
/// lookups, which read through the same transaction and so can see the rows
/// earlier lines put there.
#[allow(clippy::too_many_arguments)]
async fn write_row(
    catalog: &Catalog,
    table: &Table,
    pk: Option<&str>,
    mut body: Map<String, Json>,
    context: Option<&CallerContext>,
    summaries: &mut SummaryLookups,
    keys_seen: &mut HashSet<String>,
    executor: &rows::Executor,
) -> Result<Written> {
    summaries
        .resolve(catalog, table, &mut body, context, executor)
        .await?;

    // The primary key decides which write this is. An empty cell is a plain
    // insert: the export of a table whose keys the admin does not care about is
    // imported by clearing the column, and that has to keep working.
    let key = pk.and_then(|pk| match body.get(pk) {
        Some(Json::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Json::Number(n)) => Some(n.to_string()),
        _ => None,
    });
    if let (Some(pk), None) = (pk, key.as_ref()) {
        body.remove(pk);
    }
    if body.is_empty() {
        return Err(Error::invalid("the row has no values"));
    }

    let Some(key) = key else {
        rows::create_row_in(catalog, table, &Json::Object(body), context, executor).await?;
        return Ok(Written::Inserted);
    };
    if !keys_seen.insert(key.clone()) {
        return Err(Error::invalid(format!(
            "the primary key {key} appears more than once in the file"
        )));
    }
    write_by_key(catalog, table, &key, body, context, executor).await
}

/// Create a table from a CSV document and fill it with the document's rows.
///
/// The fields are **deduced**: one per header cell, named as the header's label
/// would name it, typed by what every non-empty cell in that column turns out to
/// be, and `NOT NULL` when the column has no empty cell at all. A column called
/// `id` is a field like the others, and additionally the **primary key** — of
/// whatever type its values turn out to be, an integer or a UUID or text. No
/// table is created with a key it did not declare (GOALS), so a file with no
/// `id` column makes a table with no primary key, which the field list says in
/// red until the admin adds one.
///
/// **All or nothing.** Unlike an import into a table that already exists, a
/// rejected row here drops the whole table and reports the errors: the schema
/// was deduced from this very file, so a row it refuses means the deduction was
/// wrong, and a half-filled table nobody asked for is worse than no table.
/// `database` names where to create it: empty (or `primary`) for Saltcorn's own
/// database, otherwise a connected database connection (§5.0). The rows are
/// imported through the table's own provider, which routes to the same database
/// the create landed in, so nothing else here has to know which one it was.
pub async fn create_table_from_csv(
    catalog: &Catalog,
    name: &str,
    database: &str,
    document: &str,
    context: Option<&CallerContext>,
) -> Result<(Table, ImportOutcome)> {
    let columns = plan_columns(document)?;
    let fields: Vec<schema_edit::FieldSpec> = columns
        .iter()
        .map(|c| schema_edit::FieldSpec {
            name: c.name.clone(),
            type_name: c.type_name.clone(),
            label: c.label.clone(),
            required: c.required,
            primary_key: c.is_primary_key,
            ..schema_edit::FieldSpec::default()
        })
        .collect();
    if fields.is_empty() {
        return Err(Error::invalid("the CSV has no columns to make fields from"));
    }
    schema_edit::apply(
        catalog,
        &[schema_edit::Operation::CreateTable {
            name: name.trim().to_owned(),
            database: database.trim().to_owned(),
            settings: schema_edit::TableSettings::default(),
            fields,
        }],
        &schema_edit::ApplyOptions::default(),
    )
    .await?;

    let table = catalog.require(name.trim())?;
    let outcome = match import_table(catalog, &table, document, context).await {
        Ok(outcome) if outcome.errors.is_empty() => outcome,
        other => {
            // The table was made for this file. If the file will not go into it,
            // the table was a mistake — so it goes, and the caller is told why
            // rather than being left to find an empty table and wonder.
            let _ = schema_edit::apply(
                catalog,
                &[schema_edit::Operation::DropTable {
                    table: table.name.clone(),
                }],
                &schema_edit::ApplyOptions::default(),
            )
            .await;
            return match other {
                Err(e) => Err(e),
                Ok(outcome) => Err(Error::invalid(format!(
                    "the rows could not be imported, so `{}` was not created: {}",
                    table.name,
                    outcome.errors.join("; ")
                ))),
            };
        }
    };
    Ok((catalog.require(&table.name)?, outcome))
}

/// One column of a CSV as [`create_table_from_csv`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvColumn {
    /// The header cell, as written.
    pub header: String,
    /// The field name it becomes.
    pub name: String,
    /// The human label it keeps.
    pub label: String,
    /// The basic type deduced from the column's values.
    pub type_name: String,
    /// Whether every cell in the column has a value.
    pub required: bool,
    /// Whether this column is the table's primary key (a column called `id`)
    /// rather than a field of its own.
    pub is_primary_key: bool,
}

/// Read a whole CSV and decide what its columns are: their names, labels, types
/// and whether they are complete.
///
/// The **whole** document is read, not a sample of it: a type deduced from the
/// first 500 lines is a type the 501st can contradict, and the file has already
/// crossed the wire as a string. A duplicated column is dropped rather than
/// refused (the second `cost` is ignored); a header that cannot make an
/// identifier at all is refused, because that is a file with a problem in it.
pub fn plan_columns(document: &str) -> Result<Vec<CsvColumn>> {
    let mut reader = ::csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(document.as_bytes());
    let header: Vec<String> = reader
        .headers()
        .map_err(|e| Error::invalid(format!("could not read the CSV header: {e}")))?
        .iter()
        .map(|h| h.trim().to_owned())
        .collect();
    if header.is_empty() {
        return Err(Error::invalid("the CSV has no header row"));
    }

    let mut cells: Vec<Vec<String>> = vec![Vec::new(); header.len()];
    for record in reader.records() {
        let record = record.map_err(|e| Error::invalid(format!("could not read the CSV: {e}")))?;
        for (position, column) in cells.iter_mut().enumerate() {
            column.push(record.get(position).unwrap_or("").trim().to_owned());
        }
    }

    let mut columns: Vec<CsvColumn> = Vec::with_capacity(header.len());
    for (position, head) in header.iter().enumerate() {
        let name = label_to_name(head);
        if name.is_empty() {
            return Err(Error::invalid(format!(
                "`{head}` cannot be a column name — use A-Z, a-z, 0-9 and _ only"
            )));
        }
        // The second column of a name is dropped, not refused: a spreadsheet
        // with two `cost` columns is a spreadsheet, and the first one is the
        // answer.
        if columns.iter().any(|c| c.name == name) {
            continue;
        }
        let values = &cells[position];
        let filled: Vec<&str> = values
            .iter()
            .map(String::as_str)
            .filter(|v| !v.is_empty())
            .collect();
        let required = filled.len() == values.len();
        let basic = detect_type(&filled);
        // A column called `id` is the table's key, of whatever type its values
        // are: whole numbers make an identity key, UUIDs a `uuid` one, anything
        // else a key the file supplies. A key column with a gap in it is the one
        // thing no type can rescue — a primary key cannot be null.
        let is_primary_key = name == PK_COLUMN;
        if is_primary_key && !required {
            return Err(Error::invalid(
                "a column called `id` becomes the table's primary key, so it must \
                 have a value in every row",
            ));
        }
        columns.push(CsvColumn {
            header: head.clone(),
            name,
            label: header_label(head),
            type_name: basic.name().to_owned(),
            required,
            is_primary_key,
        });
    }
    Ok(columns)
}

/// The table's stored fields in declaration order — everything an import may
/// write, which is what an export should contain.
fn stored_columns(table: &Table) -> Vec<&str> {
    table
        .fields
        .iter()
        .filter(|f| !f.is_calc())
        .map(|f| f.base.name.as_str())
        .collect()
}

/// Which field each header cell writes, or `None` for a cell nothing is written
/// from.
///
/// Refuses two things and ignores everything else: a header naming a
/// **calculated** field (which is computed on read and refused on write, so a
/// file carrying one is a file that will not import), and a **required** field
/// no header covers (which is the wrong file).
fn map_header(table: &Table, header: &[String]) -> Result<Vec<Option<String>>> {
    let mut columns: Vec<Option<String>> = Vec::with_capacity(header.len());
    let mut matched: HashSet<String> = HashSet::new();
    for head in header {
        let by_label_name = label_to_name(head);
        let field = table
            .fields
            .iter()
            .find(|f| f.base.name == *head)
            .or_else(|| table.fields.iter().find(|f| f.base.label == *head))
            .or_else(|| table.fields.iter().find(|f| f.base.name == by_label_name));
        match field {
            Some(field) if field.is_calc() => {
                return Err(Error::invalid(format!(
                    "`{head}` is a calculated field and cannot be written"
                )));
            }
            Some(field) => {
                // The second column naming one field is ignored, as a
                // duplicated header is when a table is created from the file.
                let first = matched.insert(field.base.name.clone());
                columns.push(first.then(|| field.base.name.clone()));
            }
            None => columns.push(None),
        }
    }
    for field in &table.fields {
        if field.required
            && !field.primary_key
            && !field.is_calc()
            && !matched.contains(&field.base.name)
        {
            return Err(Error::invalid(format!(
                "`{}` has a required field `{}` that no column of the CSV supplies",
                table.name, field.base.label
            )));
        }
    }
    Ok(columns)
}

/// Which of the two writes an upsert turned out to be.
enum Written {
    Inserted,
    /// Inserted carrying the key the file gave it — the case the identity
    /// sequence has to be caught up with afterwards.
    InsertedWithKey,
    Updated,
}

/// Write a row whose primary key the file chose: replacing the row that has that
/// key, or inserting one that carries it.
async fn write_by_key(
    catalog: &Catalog,
    table: &Table,
    key: &str,
    body: Map<String, Json>,
    context: Option<&CallerContext>,
    executor: &rows::Executor,
) -> Result<Written> {
    if row_exists(catalog, table, key, context, executor).await? {
        // `update_row_in` ignores the key in the body — it addresses the row —
        // so the column stays as the file has it either way.
        rows::update_row_in(catalog, table, key, &Json::Object(body), context, executor).await?;
        Ok(Written::Updated)
    } else {
        rows::create_row_in(catalog, table, &Json::Object(body), context, executor).await?;
        Ok(Written::InsertedWithKey)
    }
}

/// Whether `table` already has a row with this primary key.
///
/// Read on the import's own transaction, so a key an earlier line inserted
/// counts as present — otherwise the file's second mention of a key would insert
/// a duplicate rather than replacing it.
async fn row_exists(
    catalog: &Catalog,
    table: &Table,
    key: &str,
    context: Option<&CallerContext>,
    executor: &rows::Executor,
) -> Result<bool> {
    let pk = rows::single_pk(table)?;
    let value = rows::column_value(table, &pk, &Json::String(key.to_owned()))?;
    let filter = Expr::col(&pk).eq(Expr::lit(value));
    let found = rows::select_values_in(catalog, table, Some(filter), context, executor).await?;
    Ok(!found.is_empty())
}

/// Move the identity sequence behind `table`'s primary key past the largest key
/// in the table.
///
/// Raw SQL, and the only raw SQL outside the row-level-security policies: there
/// is no schema *change* here to express as a [`SchemaChange`](sc_catalog::SchemaChange),
/// only a sequence to catch up with rows that were just written. A column with
/// no sequence behind it (a key that is not an identity column) is left alone
/// rather than being an error, which is what `pg_get_serial_sequence` returning
/// null means.
pub(crate) async fn advance_identity_sequence(
    catalog: &Catalog,
    table: &Table,
    pk: &str,
) -> Result<()> {
    // Only where there is a sequence to advance. A backend that numbers a key
    // from the table itself (SQLite's rowid) is already past the rows that were
    // just written, and this statement is Postgres's own dialect — sending it
    // there would fail an import that had already succeeded.
    if !catalog.driver_for(table)?.capabilities().identity_sequences {
        return Ok(());
    }
    let table_lit = table.name.replace('\'', "''").replace('"', "\"\"");
    let pk_lit = pk.replace('\'', "''");
    // The table name reaches `pg_get_serial_sequence` **quoted**: it parses its
    // argument as SQL would, so an unquoted `Invoice` would be folded to
    // `invoice` and found to be no table at all. `format`'s `%I` below does the
    // same job for the two names it interpolates.
    let sql = format!(
        "DO $sc$ DECLARE seq text; largest bigint; BEGIN \
           seq := pg_get_serial_sequence('\"{table_lit}\"', '{pk_lit}'); \
           IF seq IS NULL THEN RETURN; END IF; \
           EXECUTE format('SELECT max(%I) FROM %I', '{pk_lit}', '{table_lit}') INTO largest; \
           IF largest IS NOT NULL THEN PERFORM setval(seq, largest); END IF; \
         END $sc$;"
    );
    catalog
        .apply_schema_batch_in(&table.database, &[sc_catalog::SchemaStep::Sql(sql)])
        .await
}

/// The `Key` fields whose cells may be summary values rather than keys, and what
/// the ones seen so far resolved to.
///
/// A cache because a CSV of five thousand time entries names the same dozen
/// matters over and over, and each distinct name is one read.
#[derive(Default)]
struct SummaryLookups {
    resolved: HashMap<(String, String), Option<Json>>,
}

impl SummaryLookups {
    /// Replace any cell of a `Key` field that is a **summary value** with the key
    /// it stands for.
    ///
    /// A `Key` column in a spreadsheet is usually the thing a person can read —
    /// the author's name, not the author's id — so a value the key's own type
    /// cannot take is looked up in the target table by its summary field. A
    /// value that *is* a key is left alone, and a summary that matches nothing is
    /// the row's error, naming both the value and where it was looked for.
    async fn resolve(
        &mut self,
        catalog: &Catalog,
        table: &Table,
        body: &mut Map<String, Json>,
        context: Option<&CallerContext>,
        executor: &rows::Executor,
    ) -> Result<()> {
        for field in &table.fields {
            let DataFieldKind::Key {
                target_table,
                target_field,
                summary_field: Some(summary),
            } = &field.kind
            else {
                continue;
            };
            let Some(Json::String(text)) = body.get(&field.base.name) else {
                continue;
            };
            let text = text.clone();
            // A cell the key's own column can take is a key, not a summary.
            if rows::column_value(table, &field.base.name, &Json::String(text.clone())).is_ok() {
                continue;
            }
            let cache_key = (field.base.name.clone(), text.clone());
            let found = match self.resolved.get(&cache_key) {
                Some(found) => found.clone(),
                None => {
                    let target = catalog.require(&target_table.0)?;
                    let found = summary_key(
                        catalog,
                        &target,
                        &summary.0,
                        &target_field.0,
                        &text,
                        context,
                        executor,
                    )
                    .await?;
                    // Only a *hit* is cached. A miss may be a row a later line
                    // of this very file inserts — a self-joining table whose
                    // parent is named before it exists — and caching that would
                    // make the file's order matter again, which is the thing the
                    // deferred constraints are here to stop mattering.
                    if found.is_some() {
                        self.resolved.insert(cache_key, found.clone());
                    }
                    found
                }
            };
            match found {
                Some(key) => {
                    body.insert(field.base.name.clone(), key);
                }
                None => {
                    return Err(Error::invalid(format!(
                        "in field `{}` the value \"{text}\" is not matched by any row of \
                         `{}`.`{}`",
                        field.base.name, target_table.0, summary.0
                    )));
                }
            }
        }
        Ok(())
    }
}

/// The key of the row of `target` whose summary field holds `text`, if there is
/// one.
#[allow(clippy::too_many_arguments)]
async fn summary_key(
    catalog: &Catalog,
    target: &Table,
    summary: &str,
    target_field: &str,
    text: &str,
    context: Option<&CallerContext>,
    executor: &rows::Executor,
) -> Result<Option<Json>> {
    let Ok(value) = rows::column_value(target, summary, &Json::String(text.to_owned())) else {
        return Ok(None);
    };
    let filter = Expr::col(summary).eq(Expr::lit(value));
    let found = rows::select_values_in(catalog, target, Some(filter), context, executor).await?;
    Ok(found
        .first()
        .and_then(|row| row.get(target_field))
        .map(sc_types::value_to_json))
}

/// One JSON value as a CSV cell.
///
/// A string goes out as itself rather than as its JSON spelling — a quoted,
/// escaped `"hello"` in a spreadsheet cell is not what anyone wants — and a
/// null is the empty cell [`field_json`] reads back as null. Everything else
/// (numbers, booleans, and a `json`/`jsonb` column's object) takes its JSON
/// form, which is the one spelling that survives the round trip.
fn cell(value: Option<&Json>) -> String {
    match value {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// A raw CSV cell as the JSON the row layer should coerce for this column.
///
/// Nearly everything is handed over as a **string** and coerced by the field's
/// own type ([`sc_types::json_to_value`] parses text into ints, floats,
/// decimals, dates, times, timestamps and UUIDs), so the CSV path inherits the
/// same coercion rules as every other write instead of restating them. The two
/// exceptions are the types where a string is not coercible and CSV has no
/// other way to spell the value: a boolean and a JSON column.
///
/// The cell is **trimmed** first. `Joe Celko, 856` is how a person writes a CSV
/// row, and ` 856` is not an integer to anything that parses one.
fn field_json(table: &Table, column: &str, raw: &str) -> Json {
    let basic = table
        .field(column)
        .and_then(|f| f.base.type_.as_basic().cloned());
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // The empty cell is absence — except for text, where it is the empty
        // string a user may have typed on purpose.
        return match basic {
            Some(BasicType::Text) => Json::String(String::new()),
            _ => Json::Null,
        };
    }
    match basic {
        Some(BasicType::Bool) => match parse_bool(trimmed) {
            Some(b) => Json::Bool(b),
            // Not a boolean this understands: hand the text on so the row
            // layer refuses it by name, on its line, like any other bad cell.
            None => Json::String(trimmed.to_owned()),
        },
        Some(BasicType::Json) => {
            serde_json::from_str(trimmed).unwrap_or_else(|_| Json::String(trimmed.to_owned()))
        }
        _ => Json::String(trimmed.to_owned()),
    }
}

/// The words a CSV spells a boolean with, as Saltcorn 1's `csv_bool_values`
/// setting lists them, plus the `1`/`0` a database exports.
fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "on" | "1" => Some(true),
        "false" | "f" | "no" | "n" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Whether every value in the column is one of the boolean **words** — `1` and
/// `0` excluded, so that a column of ones and zeroes is the integer column it
/// almost always is.
fn all_bool_words(values: &[&str]) -> bool {
    values.iter().all(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "true" | "t" | "yes" | "y" | "on" | "false" | "f" | "no" | "n" | "off"
        )
    })
}

/// The type a column of these (non-empty, trimmed) values is.
///
/// Ordered from the narrowest reading to the widest, ending at text, which
/// anything is. A column with nothing in it at all is text: there is no evidence
/// for anything narrower, and text is the type that will take whatever arrives
/// later.
fn detect_type(values: &[&str]) -> BasicType {
    if values.is_empty() {
        return BasicType::Text;
    }
    if all_bool_words(values) {
        return BasicType::Bool;
    }
    // Unlike Saltcorn 1 there is no large-integer escape to text: `int` here is
    // a 64-bit column, so an integer that overflowed Saltcorn 1's `int4` is
    // simply an integer.
    if values.iter().all(|v| v.parse::<i64>().is_ok()) {
        return BasicType::Int;
    }
    if values.iter().all(|v| v.parse::<f64>().is_ok()) {
        return BasicType::Float;
    }
    if values
        .iter()
        .all(|v| v.parse::<chrono::NaiveDate>().is_ok())
    {
        return BasicType::Date;
    }
    if values
        .iter()
        .all(|v| chrono::DateTime::parse_from_rfc3339(v).is_ok())
    {
        return BasicType::Timestamp;
    }
    if values.iter().all(|v| v.parse::<uuid::Uuid>().is_ok()) {
        return BasicType::Uuid;
    }
    if values.iter().all(|v| {
        (v.starts_with('{') || v.starts_with('[')) && serde_json::from_str::<Json>(v).is_ok()
    }) {
        return BasicType::Json;
    }
    BasicType::Text
}

/// The field name a header cell makes: lower case, spaces and dashes as
/// underscores, punctuation dropped.
///
/// Saltcorn 1's `Field.labelToName`, which is what makes `Item Name` and
/// `Item_Name` the same column — an admin importing the same data twice from two
/// spreadsheets must not get two tables' worth of fields out of it.
pub(crate) fn label_to_name(label: &str) -> String {
    let mut name = String::with_capacity(label.len());
    for c in label.trim().chars() {
        match c {
            ' ' | '-' => name.push('_'),
            '_' => name.push('_'),
            c if c.is_ascii_alphanumeric() => name.push(c.to_ascii_lowercase()),
            // Everything else — punctuation, quotes, brackets — is dropped, so a
            // header of nothing but punctuation makes no name at all and is
            // refused by the caller.
            _ => {}
        }
    }
    // A leading digit cannot start an identifier.
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

/// The label a header cell keeps: itself, with underscores as spaces and the
/// first letter capitalised. `Item_Name` and `item name` both read as
/// `Item Name` on a form.
pub(crate) fn header_label(header: &str) -> String {
    let spaced = header.trim().replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::testing::{id_field, table_of, typed_field};

    fn table() -> Table {
        table_of(
            "books",
            vec![
                id_field(),
                typed_field("title", BasicType::Text),
                typed_field("in_print", BasicType::Bool),
                typed_field("meta", BasicType::Json),
            ],
        )
    }

    #[test]
    fn the_columns_are_the_stored_fields_and_never_a_calculated_one() {
        let mut t = table();
        let mut calc = typed_field("doubled", BasicType::Int);
        calc.kind = sc_catalog::DataFieldKind::Calc {
            expression: "id * 2".to_owned(),
        };
        t.fields.push(calc);
        // A calc field is refused on write, so an export carrying it would
        // produce a file that could not be imported back.
        assert_eq!(stored_columns(&t), vec!["id", "title", "in_print", "meta"]);
    }

    #[test]
    fn a_cell_is_the_string_itself_and_the_json_spelling_of_anything_else() {
        assert_eq!(cell(None), "");
        assert_eq!(cell(Some(&Json::Null)), "");
        assert_eq!(cell(Some(&Json::String("a, b".to_owned()))), "a, b");
        assert_eq!(cell(Some(&serde_json::json!(3))), "3");
        assert_eq!(cell(Some(&serde_json::json!(true))), "true");
        assert_eq!(cell(Some(&serde_json::json!({"a": 1}))), r#"{"a":1}"#);
    }

    #[test]
    fn a_blank_cell_is_null_except_in_a_text_column() {
        let t = table();
        assert_eq!(field_json(&t, "id", ""), Json::Null);
        assert_eq!(field_json(&t, "title", ""), Json::String(String::new()));
        // Whitespace is blank too: a spreadsheet's " " is not a page count.
        assert_eq!(field_json(&t, "id", "  "), Json::Null);
    }

    #[test]
    fn booleans_and_json_are_parsed_and_everything_else_is_trimmed_text() {
        let t = table();
        assert_eq!(field_json(&t, "in_print", "yes"), Json::Bool(true));
        assert_eq!(field_json(&t, "in_print", "FALSE"), Json::Bool(false));
        // Not a boolean: handed on as text so the row layer names the field.
        assert_eq!(
            field_json(&t, "in_print", "perhaps"),
            Json::String("perhaps".to_owned())
        );
        assert_eq!(
            field_json(&t, "meta", r#"{"a": 1}"#),
            serde_json::json!({"a": 1})
        );
        // An int stays text; `json_to_value` parses it against the column — and
        // the space after the comma a person types is not part of the number.
        assert_eq!(field_json(&t, "id", " 7"), Json::String("7".to_owned()));
    }

    #[test]
    fn a_header_finds_its_field_by_name_by_label_or_by_the_name_a_label_makes() {
        let mut t = table();
        t.fields[1].base.label = "The Title".to_owned();
        let header: Vec<String> = ["The Title", "In Print", "isbn"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mapped = map_header(&t, &header).expect("no required field is missing");
        assert_eq!(
            mapped,
            vec![
                Some("title".to_owned()),
                // `In Print` names no field, but the name its label makes does.
                Some("in_print".to_owned()),
                // A column the table knows nothing about is ignored, not
                // refused: real exports carry columns nobody asked for.
                None,
            ]
        );
    }

    #[test]
    fn a_required_field_no_column_supplies_is_the_wrong_file() {
        let mut t = table();
        t.fields[1].required = true;
        let header = vec!["in_print".to_owned()];
        let e = map_header(&t, &header).expect_err("title is required and absent");
        assert!(e.causes().contains("title"), "message: {}", e.causes());

        // The primary key is exempt: the database supplies it.
        let header = vec!["title".to_owned(), "in_print".to_owned()];
        assert!(map_header(&t, &header).is_ok());
    }

    #[test]
    fn a_calculated_field_in_the_header_is_refused_by_name() {
        let mut t = table();
        let mut calc = typed_field("doubled", BasicType::Int);
        calc.kind = sc_catalog::DataFieldKind::Calc {
            expression: "id * 2".to_owned(),
        };
        t.fields.push(calc);
        let header = vec!["title".to_owned(), "doubled".to_owned()];
        let e = map_header(&t, &header).expect_err("a calc field cannot be written");
        assert!(e.causes().contains("doubled"), "message: {}", e.causes());
    }

    #[test]
    fn the_second_column_naming_one_field_is_ignored() {
        let t = table();
        let header = vec!["title".to_owned(), "Title".to_owned()];
        assert_eq!(
            map_header(&t, &header).expect("a repeat is not an error"),
            vec![Some("title".to_owned()), None]
        );
    }

    #[test]
    fn a_column_is_typed_by_everything_in_it() {
        assert_eq!(detect_type(&["f", "t", "yes"]), BasicType::Bool);
        assert_eq!(detect_type(&["5", "4"]), BasicType::Int);
        // Saltcorn 1 made this text, because its Integer was 32 bits.
        assert_eq!(detect_type(&["1", "4084787842"]), BasicType::Int);
        assert_eq!(detect_type(&["0.5", "2"]), BasicType::Float);
        assert_eq!(detect_type(&["2011-03-15", "2012-08-13"]), BasicType::Date);
        assert_eq!(
            detect_type(&["179f7e88-ae48-495e-a080-68c471fac2ac"]),
            BasicType::Uuid
        );
        assert_eq!(detect_type(&[r#"{"foo":5}"#, "[7]"]), BasicType::Json);
        assert_eq!(detect_type(&["Book", "Pencil"]), BasicType::Text);
        // Nothing to go on: text takes whatever arrives later.
        assert_eq!(detect_type(&[]), BasicType::Text);
        // One value that is not a number makes the whole column text.
        assert_eq!(detect_type(&["5", "ITILA"]), BasicType::Text);
    }

    #[test]
    fn a_header_makes_a_name_and_keeps_a_label() {
        assert_eq!(label_to_name("Item Name"), "item_name");
        assert_eq!(label_to_name("Item_Name"), "item_name");
        assert_eq!(label_to_name("cost ($)"), "cost_");
        assert_eq!(label_to_name("!"), "");
        assert_eq!(label_to_name("2020"), "_2020");
        assert_eq!(header_label("Item_Name"), "Item Name");
        assert_eq!(header_label("cost"), "Cost");
    }

    #[test]
    fn the_plan_names_types_and_completeness_of_every_column() {
        let document = "item,cost,count, vatable\nBook, 5,4, f\nPencil, 0.5,, t\n";
        let columns = plan_columns(document).expect("a plain CSV plans");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["item", "cost", "count", "vatable"]);
        let types: Vec<&str> = columns.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(types, vec!["text", "float", "int", "bool"]);
        // `count` has a gap, so it is not NOT NULL; the others have none.
        let required: Vec<bool> = columns.iter().map(|c| c.required).collect();
        assert_eq!(required, vec![true, true, false, true]);
        assert!(columns.iter().all(|c| !c.is_primary_key));
    }

    #[test]
    fn a_duplicated_header_is_dropped_and_an_unnameable_one_is_refused() {
        let plan = plan_columns("item,cost,cost,vatable\nBook,5,4,f\n").expect("planned");
        assert_eq!(plan.len(), 3);
        let e = plan_columns("item,cost,!,vatable\nBook,5,4,f\n").expect_err("`!` is no name");
        assert!(e.causes().contains('!'), "message: {}", e.causes());
    }

    #[test]
    fn an_id_column_becomes_the_primary_key_of_whatever_type_it_holds() {
        let plan = plan_columns("id,cost\n1,5\n2,0.5\n").expect("planned");
        assert!(plan[0].is_primary_key);
        assert_eq!(plan[0].type_name, "int");

        // A UUID key and a text key are keys too: the key is a field like any
        // other, so its type is the one its values have (GOALS).
        let plan = plan_columns(
            "id,cost\n179f7e88-ae48-495e-a080-68c471fac2ac,5\nd1403829-cc1e-49b5-bcdc-488973e640ba,2\n",
        )
        .expect("planned");
        assert!(plan[0].is_primary_key);
        assert_eq!(plan[0].type_name, "uuid");

        let plan = plan_columns("id,cost\nBook,5\nPencil,0.5\n").expect("planned");
        assert!(plan[0].is_primary_key);
        assert_eq!(plan[0].type_name, "text");

        // The one thing no type rescues: a key cannot be null.
        let e = plan_columns("id,cost\n1,5\n,0.5\n").expect_err("a gap in the key");
        assert!(e.causes().contains("every row"), "message: {}", e.causes());
    }
}
