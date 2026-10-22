//! Rendering and applying a [`SchemaChange`] against SQLite.
//!
//! Most changes are one statement, as they are for Postgres. Three things make
//! SQLite different, and each is handled here rather than pushed up to callers
//! who would then have to know which backend they were talking to:
//!
//! - **`ALTER TABLE` does very little.** SQLite can add and drop a column and
//!   rename; it cannot alter a column, add or drop a primary key, or add a
//!   `UNIQUE` column. The documented way round it is to *rebuild* the table —
//!   create the new shape, copy the rows, drop the old, rename — and that is
//!   what [`SchemaChange::SetPrimaryKey`] and
//!   [`SchemaChange::SetColumnGenerator`] do here. A rebuild needs to know the
//!   table's current shape, so unlike every other change it cannot be rendered
//!   from the change alone; [`render`] says so rather than emitting something
//!   that would not run.
//! - **There is no `COMMENT ON`.** A constraint's error message and a row
//!   constraint's formula ride in an object's comment (see
//!   [`PhysicalConstraint::comment`](sc_db::PhysicalConstraint)), which SQLite
//!   has nowhere to put — so this driver keeps them in a table of its own,
//!   [`COMMENTS_TABLE`], written by the same change and read back by
//!   introspection. A side table is not a hiding place here: it is the only
//!   place, and it is dropped and rewritten with the object it describes.
//! - **A default is Postgres-flavoured SQL.** `gen_random_uuid()` is what the
//!   schema editor asks a uuid key to fill itself in with, and SQLite has no
//!   such function. The handful of defaults Saltcorn itself generates are
//!   translated ([`translate_default`]); anything else passes through and SQLite
//!   judges it.
//!
//! Type names pass through **verbatim** — `int8`, `jsonb`, `timestamptz` and all
//! — because SQLite keeps the declared name and reports it back, which is what
//! lets [`crate::value`] read a value as the thing it was written as. The one
//! exception is an identity column, which must be spelled exactly `INTEGER` to
//! be the rowid alias SQLite numbers itself.

use rusqlite::Connection;
use sc_db::{ColumnDef, ColumnGenerator, CommentTarget, IndexOn, SchemaChange};
use sc_error::{Error, Result};
use sc_query::SqlDialect;

use crate::dialect::SqliteDialect;
use crate::introspect::{self, COMMENTS_TABLE, RawColumn};

/// The savepoint a multi-statement change runs inside.
///
/// A savepoint rather than a transaction because this is reached both from
/// [`SqliteDriver::apply_schema`](crate::SqliteDriver::apply_schema), which is
/// outside one, and from a [`Transaction`](sc_db::Transaction), which is inside
/// one — and a savepoint is the one form that means "all of this or none of it"
/// in both places.
const SAVEPOINT: &str = "sc_schema_change";

/// Render `change` as the SQLite DDL [`apply`] would run, without running it.
///
/// The `;`-joined statements, for the changes that can be known without looking
/// at the database. A table rebuild cannot: it is written out of the table's
/// *current* columns, so rendering it from the change alone would be a guess.
pub(crate) fn render(dialect: &SqliteDialect, change: &SchemaChange) -> Result<String> {
    match statements(dialect, change)? {
        Some(sql) => Ok(sql.join("; ")),
        None => Err(Error::database(format!(
            "SQLite has no `ALTER TABLE` for this change ({}), so it is applied by \
             rebuilding the table — which is written from the table's current \
             columns and cannot be rendered without them",
            change_name(change)
        ))),
    }
}

/// Apply `change`, rebuilding the table where SQLite has no statement for what
/// was asked.
pub(crate) fn apply(
    conn: &Connection,
    dialect: &SqliteDialect,
    change: &SchemaChange,
) -> Result<()> {
    match statements(dialect, change)? {
        Some(statements) => run_all(conn, &statements, false),
        // Only a rebuild can leave a key pointing at nothing — it is the one
        // change that drops the table other tables reference — and
        // `foreign_key_check` scans the whole database, so it is asked there and
        // not after every two-statement `ALTER`.
        None => run_all(conn, &rebuild_statements(conn, dialect, change)?, true),
    }
}

/// The statements a change becomes, or `None` when it needs the table rebuilt.
fn statements(dialect: &SqliteDialect, change: &SchemaChange) -> Result<Option<Vec<String>>> {
    let statements = match change {
        SchemaChange::CreateTable {
            name,
            columns,
            primary_key,
            // No unlogged tables in SQLite, and the capability is not
            // advertised, so a change that asks for one gets an ordinary table —
            // which the flag's own documentation says is the contract.
            unlogged: _,
        } => {
            for pk in primary_key {
                if !columns.iter().any(|c| &c.name == pk) {
                    return Err(Error::invalid(format!(
                        "primary key column `{pk}` is not a column of table `{name}`"
                    )));
                }
            }
            vec![create_table(dialect, name, columns, primary_key)?]
        }
        SchemaChange::DropTable { name, if_exists } => {
            vec![format!(
                "DROP TABLE {}{}",
                if *if_exists { "IF EXISTS " } else { "" },
                dialect.quote_ident(name)
            )]
        }
        SchemaChange::AddColumn { table, column } => {
            if matches!(column.generated, Some(ColumnGenerator::Identity)) {
                return Err(Error::invalid(format!(
                    "column `{}` cannot be added to `{table}` as an identity column: \
                     SQLite numbers only an `INTEGER PRIMARY KEY`, which a column \
                     added to an existing table cannot become",
                    column.name
                )));
            }
            // `ADD COLUMN` refuses a `UNIQUE` column, so the uniqueness arrives
            // as the index it would have created anyway — and under the name
            // Postgres would have given it, so a later drop finds it.
            let added = ColumnDef {
                unique: false,
                ..column.clone()
            };
            let mut out = vec![format!(
                "ALTER TABLE {} ADD COLUMN {}",
                dialect.quote_ident(table),
                column_def(dialect, &added, false)
            )];
            if column.unique {
                out.push(format!(
                    "CREATE UNIQUE INDEX {} ON {} ({})",
                    dialect.quote_ident(&format!("{table}_{}_key", column.name)),
                    dialect.quote_ident(table),
                    dialect.quote_ident(&column.name)
                ));
            }
            out
        }
        SchemaChange::DropColumn {
            table,
            column,
            if_exists: _,
        } => {
            // No `IF EXISTS` in SQLite's `DROP COLUMN`; `apply` checks instead,
            // which is why the flag is not rendered here.
            vec![format!(
                "ALTER TABLE {} DROP COLUMN {}",
                dialect.quote_ident(table),
                dialect.quote_ident(column)
            )]
        }
        // SQLite (3.25 on) rewrites the indexes, triggers and views that name
        // the column too.
        SchemaChange::RenameColumn { table, from, to } => {
            vec![format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                dialect.quote_ident(table),
                dialect.quote_ident(from),
                dialect.quote_ident(to)
            )]
        }
        // All of these change a column's declaration, which SQLite cannot do.
        SchemaChange::SetPrimaryKey { .. }
        | SchemaChange::SetColumnGenerator { .. }
        | SchemaChange::SetColumnNullable { .. }
        | SchemaChange::SetColumnReference { .. } => {
            return Ok(None);
        }
        SchemaChange::AddUniqueConstraint {
            table,
            name,
            columns,
        } => {
            if columns.is_empty() {
                return Err(Error::invalid(format!(
                    "unique constraint `{name}` on `{table}` names no columns"
                )));
            }
            // A unique *index* rather than a table constraint, because SQLite
            // has no `ADD CONSTRAINT` — and the two are the same thing to it:
            // a `UNIQUE (…)` in a `CREATE TABLE` is stored as a unique index.
            // Introspection reports both as `Unique`, and this one can be
            // dropped by name, which a table constraint could not be.
            vec![format!(
                "CREATE UNIQUE INDEX {} ON {} ({})",
                dialect.quote_ident(name),
                dialect.quote_ident(table),
                quoted_list(dialect, columns)
            )]
        }
        SchemaChange::DropConstraint {
            table: _,
            name,
            if_exists,
        } => {
            vec![format!(
                "DROP INDEX {}{}",
                if *if_exists { "IF EXISTS " } else { "" },
                dialect.quote_ident(name)
            )]
        }
        SchemaChange::CreateIndex {
            table,
            name,
            on,
            // SQLite has one index kind, so there is no `USING` to render and
            // nothing is lost by ignoring a method the caller asked for.
            method: _,
        } => {
            let target = match on {
                IndexOn::Columns(columns) => {
                    if columns.is_empty() {
                        return Err(Error::invalid(format!(
                            "index `{name}` on `{table}` names no columns"
                        )));
                    }
                    quoted_list(dialect, columns)
                }
                IndexOn::Expression(expr) => format!("({expr})"),
            };
            vec![format!(
                "CREATE INDEX {} ON {} ({target})",
                dialect.quote_ident(name),
                dialect.quote_ident(table),
            )]
        }
        SchemaChange::DropIndex { name, if_exists } => {
            vec![format!(
                "DROP INDEX {}{}",
                if *if_exists { "IF EXISTS " } else { "" },
                dialect.quote_ident(name)
            )]
        }
        SchemaChange::SetComment { target, comment } => {
            let (kind, table, name) = match target {
                // A constraint *is* an index here (see `AddUniqueConstraint`),
                // and an index is named in its own right — SQLite index names
                // are unique across the database — so both are keyed without a
                // table. Keying the constraint by its table instead would store
                // a comment under one key and read it back under another.
                CommentTarget::Constraint { name, .. } | CommentTarget::Index { name } => {
                    ("index", "", name)
                }
                CommentTarget::Trigger { table, name } => ("trigger", table.as_str(), name),
            };
            let mut out = vec![create_comments_table()];
            match comment {
                Some(text) => out.push(format!(
                    "INSERT INTO {t} (kind, \"table\", name, comment) VALUES ({}, {}, {}, {}) \
                     ON CONFLICT (kind, \"table\", name) DO UPDATE SET comment = excluded.comment",
                    dialect.quote_literal(kind),
                    dialect.quote_literal(table),
                    dialect.quote_literal(name),
                    dialect.quote_literal(text),
                    t = dialect.quote_ident(COMMENTS_TABLE),
                )),
                // Removing a comment removes the row: an empty comment would
                // read back as "somebody commented this and said nothing".
                None => out.push(format!(
                    "DELETE FROM {t} WHERE kind = {} AND \"table\" = {} AND name = {}",
                    dialect.quote_literal(kind),
                    dialect.quote_literal(table),
                    dialect.quote_literal(name),
                    t = dialect.quote_ident(COMMENTS_TABLE),
                )),
            }
            out
        }
    };
    Ok(Some(statements))
}

/// A `CREATE TABLE`, with the primary key declared where SQLite needs it.
fn create_table(
    dialect: &SqliteDialect,
    name: &str,
    columns: &[ColumnDef],
    primary_key: &[String],
) -> Result<String> {
    // An identity column is the rowid alias, which is declared **on the column**
    // (`"id" INTEGER PRIMARY KEY`) and nowhere else: a table-level `PRIMARY KEY
    // ("id")` beside it would be an ordinary key over an ordinary integer, and
    // SQLite would stop numbering it.
    let identity: Vec<&ColumnDef> = columns
        .iter()
        .filter(|c| matches!(c.generated, Some(ColumnGenerator::Identity)))
        .collect();
    let rowid_alias = match identity.as_slice() {
        [] => None,
        [column] if primary_key == [column.name.clone()] => Some(column.name.clone()),
        [column] => {
            return Err(Error::invalid(format!(
                "column `{}` of table `{name}` numbers itself, which SQLite can only do \
                 for a single-column integer primary key — but the key here is ({})",
                column.name,
                if primary_key.is_empty() {
                    "none".to_owned()
                } else {
                    primary_key.join(", ")
                }
            )));
        }
        _ => {
            return Err(Error::invalid(format!(
                "table `{name}` has more than one column that numbers itself; \
                 SQLite numbers only the single integer primary key"
            )));
        }
    };

    let mut items: Vec<String> = columns
        .iter()
        .map(|c| {
            let is_rowid = rowid_alias.as_deref() == Some(c.name.as_str());
            // A primary-key column is `NOT NULL`, which SQLite does *not* imply
            // — a legacy quirk that would otherwise let a null into a key the
            // admin declared required.
            let def = if primary_key.contains(&c.name) && !is_rowid && c.nullable {
                ColumnDef {
                    nullable: false,
                    ..c.clone()
                }
            } else {
                c.clone()
            };
            column_def(dialect, &def, is_rowid)
        })
        .collect();
    if !primary_key.is_empty() && rowid_alias.is_none() {
        items.push(format!(
            "PRIMARY KEY ({})",
            quoted_list(dialect, primary_key)
        ));
    }
    Ok(format!(
        "CREATE TABLE {} ({})",
        dialect.quote_ident(name),
        items.join(", ")
    ))
}

/// Render one column definition:
/// `"name" type [PRIMARY KEY] [NOT NULL] [DEFAULT …] [UNIQUE] [REFERENCES "t"("c")]`.
///
/// `rowid_alias` renders the identity form — `"id" INTEGER PRIMARY KEY` — which
/// is a declaration SQLite recognises rather than a type plus a constraint.
fn column_def(dialect: &SqliteDialect, col: &ColumnDef, rowid_alias: bool) -> String {
    let mut s = dialect.quote_ident(&col.name);
    s.push(' ');
    if rowid_alias {
        s.push_str("INTEGER PRIMARY KEY");
    } else {
        s.push_str(&col.sql_type);
        if !col.nullable {
            s.push_str(" NOT NULL");
        }
        if let Some(ColumnGenerator::Default(sql)) = &col.generated {
            s.push_str(&default_clause(&translate_default(sql)));
        }
        if col.unique {
            s.push_str(" UNIQUE");
        }
    }
    if let Some(target) = &col.references {
        s.push_str(" REFERENCES ");
        s.push_str(&dialect.quote_ident(&target.table));
        s.push_str(" (");
        s.push_str(&dialect.quote_ident(&target.column));
        s.push(')');
        // Deferrable, checked immediately by default — as the Postgres driver
        // declares its keys, and for the same reason: a CSV import of a
        // self-referencing table says `PRAGMA defer_foreign_keys` and the parent
        // three lines further down is there by the time the transaction commits.
        s.push_str(" DEFERRABLE INITIALLY IMMEDIATE");
    }
    s
}

/// A `DEFAULT` clause, parenthesised exactly when it has to be.
///
/// SQLite requires an expression default to be in parentheses and refuses a
/// parenthesised one in `ALTER TABLE ADD COLUMN`, where only a constant is
/// allowed — so the two cases are told apart rather than one form used for both.
/// A literal (a number, a quoted string, `TRUE`, `NULL`, `CURRENT_TIMESTAMP`)
/// goes out bare and can therefore be added to an existing table; anything else
/// is an expression, which is parenthesised and which SQLite will refuse to add
/// to an existing table — correctly, and with its own message.
fn default_clause(default: &str) -> String {
    let trimmed = default.trim();
    let literal = trimmed.starts_with('\'')
        || trimmed.starts_with('"')
        || trimmed.parse::<f64>().is_ok()
        || matches!(
            trimmed.to_ascii_uppercase().as_str(),
            "NULL" | "TRUE" | "FALSE" | "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME"
        );
    if literal {
        format!(" DEFAULT {trimmed}")
    } else {
        format!(" DEFAULT ({trimmed})")
    }
}

/// A Postgres-flavoured column default in SQLite's own spelling.
///
/// Only the handful Saltcorn itself generates are translated — a uuid key's
/// `gen_random_uuid()` above all, which is what the schema editor gives every
/// uuid primary key. Anything else is passed through: it may well be valid
/// SQLite, and a driver that refused what it did not recognise would refuse a
/// default an admin typed correctly.
fn translate_default(sql: &str) -> String {
    match sql.trim().to_ascii_lowercase().as_str() {
        // A v4 uuid out of SQLite's own randomness: four random blobs, with the
        // version nibble (`4`) and the variant nibble (`8`–`b`) set by hand.
        "gen_random_uuid()" | "uuid_generate_v4()" => concat!(
            "lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' || ",
            "substr(lower(hex(randomblob(2))), 2) || '-' || ",
            "substr('89ab', abs(random()) % 4 + 1, 1) || ",
            "substr(lower(hex(randomblob(2))), 2) || '-' || lower(hex(randomblob(6)))"
        )
        .to_owned(),
        // The format `crate::value` writes a timestamp in, so a default and a
        // written value are the same shape and sort together.
        "now()" | "current_timestamp" | "current_timestamp()" => {
            "strftime('%Y-%m-%dT%H:%M:%f', 'now') || 'Z'".to_owned()
        }
        _ => sql.to_owned(),
    }
}

/// The statements that rebuild a table into the shape `change` asks for.
///
/// The recipe SQLite documents for the schema changes `ALTER TABLE` cannot make:
/// create the new shape under a temporary name, copy every row, drop the
/// original, rename. Everything attached to the table — its indexes and its
/// triggers — goes with the drop, so it is recreated afterwards from the
/// statements SQLite kept.
///
/// Foreign keys are **deferred** rather than switched off: `PRAGMA
/// defer_foreign_keys` holds only to the end of the transaction, which is the
/// savepoint [`run_all`] opens, so the window in which a dropped table's
/// references dangle closes automatically — and is checked before it does.
fn rebuild_statements(
    conn: &Connection,
    dialect: &SqliteDialect,
    change: &SchemaChange,
) -> Result<Vec<String>> {
    let (table, mut columns, primary_key) = match change {
        SchemaChange::SetPrimaryKey { table, columns } => {
            let existing = introspect::columns(conn, table)?;
            for wanted in columns {
                if !existing.iter().any(|c| &c.name == wanted) {
                    return Err(Error::invalid(format!(
                        "primary key column `{wanted}` is not a column of table `{table}`"
                    )));
                }
            }
            (table, existing, columns.clone())
        }
        SchemaChange::SetColumnGenerator {
            table,
            column,
            generator,
        } => {
            let mut existing = introspect::columns(conn, table)?;
            let key = introspect::primary_key(conn, table)?;
            let target = existing
                .iter_mut()
                .find(|c| &c.name == column)
                .ok_or_else(|| {
                    Error::invalid(format!("table `{table}` has no column `{column}`"))
                })?;
            match generator {
                Some(ColumnGenerator::Identity) => {
                    if key != [column.clone()] {
                        return Err(Error::invalid(format!(
                            "column `{column}` of `{table}` cannot number itself: SQLite \
                             numbers only a single-column integer primary key, and the \
                             key here is ({})",
                            if key.is_empty() {
                                "none".to_owned()
                            } else {
                                key.join(", ")
                            }
                        )));
                    }
                    // The rowid alias is a *declared type*, so becoming one is a
                    // change of type as well as of generator.
                    target.decl_type = "INTEGER".to_owned();
                    target.default_sql = None;
                }
                Some(ColumnGenerator::Default(sql)) => {
                    target.default_sql = Some(translate_default(sql));
                }
                None => target.default_sql = None,
            }
            (table, existing, key)
        }
        SchemaChange::SetColumnNullable {
            table,
            column,
            nullable,
        } => {
            let mut existing = introspect::columns(conn, table)?;
            let key = introspect::primary_key(conn, table)?;
            // Refused rather than quietly undone by the key's `NOT NULL` below,
            // which is what the rebuild would otherwise do with it.
            if *nullable && key.contains(column) {
                return Err(Error::invalid(format!(
                    "column `{column}` of `{table}` is part of the primary key, \
                     which never accepts nulls"
                )));
            }
            let target = existing
                .iter_mut()
                .find(|c| &c.name == column)
                .ok_or_else(|| {
                    Error::invalid(format!("table `{table}` has no column `{column}`"))
                })?;
            // A row holding a null fails the copy into the new shape, which
            // rolls the rebuild back: the column is made `NOT NULL` only over
            // data that already is.
            target.not_null = !nullable;
            (table, existing, key)
        }
        SchemaChange::SetColumnReference { table, column, .. } => {
            let existing = introspect::columns(conn, table)?;
            if !existing.iter().any(|c| &c.name == column) {
                return Err(Error::invalid(format!(
                    "table `{table}` has no column `{column}`"
                )));
            }
            // The columns stay as they are; what changes is the foreign keys,
            // which are written out below.
            let key = introspect::primary_key(conn, table)?;
            (table, existing, key)
        }
        other => {
            return Err(Error::database(format!(
                "{} does not need a table rebuild",
                change_name(other)
            )));
        }
    };

    // Every primary-key column is `NOT NULL`, which is what the key means and
    // what SQLite would otherwise not imply.
    for column in &mut columns {
        if primary_key.contains(&column.name) {
            column.not_null = true;
        }
    }

    let temporary = format!("{table}__sc_rebuild");
    let mut foreign_keys = introspect::foreign_keys(conn, table)?;
    if let SchemaChange::SetColumnReference {
        column, references, ..
    } = change
    {
        // Every single-column key on the column goes, and the new one — if
        // there is one — takes its place.
        foreign_keys.retain(|key| key.columns != [column.clone()]);
        if let Some(target) = references {
            foreign_keys.push(sc_db::ForeignKey {
                columns: vec![column.clone()],
                referenced_table: target.table.clone(),
                referenced_columns: vec![target.column.clone()],
            });
        }
    }
    let indexes = introspect::indexes(conn, table)?;
    let rowid_alias = (primary_key.len() == 1
        && columns
            .iter()
            .any(|c| c.name == primary_key[0] && c.decl_type.eq_ignore_ascii_case("integer")))
    .then(|| primary_key[0].clone());

    let mut items: Vec<String> = columns
        .iter()
        .map(|c| raw_column_def(dialect, c, rowid_alias.as_deref() == Some(c.name.as_str())))
        .collect();
    if !primary_key.is_empty() && rowid_alias.is_none() {
        items.push(format!(
            "PRIMARY KEY ({})",
            quoted_list(dialect, &primary_key)
        ));
    }
    for index in indexes.iter().filter(|i| i.is_constraint_index()) {
        // A `UNIQUE` that lives in the table declaration has to be written into
        // the new declaration; one that is its own `CREATE UNIQUE INDEX`
        // statement is recreated afterwards, with the rest of the indexes.
        if index.origin == "u" {
            let cols: Vec<String> = index.columns.iter().flatten().cloned().collect();
            if !cols.is_empty() {
                items.push(format!("UNIQUE ({})", quoted_list(dialect, &cols)));
            }
        }
    }
    for key in &foreign_keys {
        items.push(format!(
            "FOREIGN KEY ({}) REFERENCES {} ({}) DEFERRABLE INITIALLY IMMEDIATE",
            quoted_list(dialect, &key.columns),
            dialect.quote_ident(&key.referenced_table),
            quoted_list(dialect, &key.referenced_columns),
        ));
    }

    let names = quoted_list(
        dialect,
        &columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
    );
    let mut statements = vec![
        "PRAGMA defer_foreign_keys = ON".to_owned(),
        format!(
            "CREATE TABLE {} ({})",
            dialect.quote_ident(&temporary),
            items.join(", ")
        ),
        format!(
            "INSERT INTO {} ({names}) SELECT {names} FROM {}",
            dialect.quote_ident(&temporary),
            dialect.quote_ident(table)
        ),
        format!("DROP TABLE {}", dialect.quote_ident(table)),
        format!(
            "ALTER TABLE {} RENAME TO {}",
            dialect.quote_ident(&temporary),
            dialect.quote_ident(table)
        ),
    ];
    // The indexes and triggers the drop took with it, put back exactly as they
    // were written.
    statements.extend(introspect::objects_sql(conn, "index", table)?);
    statements.extend(introspect::objects_sql(conn, "trigger", table)?);
    Ok(statements)
}

/// A column of a table being rebuilt, rendered from what it currently is.
fn raw_column_def(dialect: &SqliteDialect, col: &RawColumn, rowid_alias: bool) -> String {
    let mut s = dialect.quote_ident(&col.name);
    s.push(' ');
    if rowid_alias {
        s.push_str("INTEGER PRIMARY KEY");
        return s;
    }
    // A column declared without a type keeps none: SQLite allows it, and
    // inventing one would change how its values are read back.
    if !col.decl_type.is_empty() {
        s.push_str(&col.decl_type);
    }
    if col.not_null {
        s.push_str(" NOT NULL");
    }
    if let Some(default) = &col.default_sql {
        s.push_str(&default_clause(default));
    }
    s
}

/// The `CREATE TABLE` for the comment side table, `IF NOT EXISTS` so setting a
/// comment on a database that has never had one is one change rather than two.
fn create_comments_table() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS \"{COMMENTS_TABLE}\" (\
         kind TEXT NOT NULL, \"table\" TEXT NOT NULL, name TEXT NOT NULL, comment TEXT, \
         PRIMARY KEY (kind, \"table\", name))"
    )
}

/// Run every statement, all of them or none.
///
/// A single statement runs on its own — the savepoint would be pure overhead,
/// and a statement is already atomic. More than one is wrapped, so a rebuild
/// that fails half way leaves the table it was rebuilding exactly as it was.
fn run_all(conn: &Connection, statements: &[String], check_foreign_keys: bool) -> Result<()> {
    if statements.len() == 1 {
        return execute(conn, &statements[0]);
    }
    execute(conn, &format!("SAVEPOINT {SAVEPOINT}"))?;
    let mut result = Ok(());
    for sql in statements {
        result = execute(conn, sql);
        if result.is_err() {
            break;
        }
    }
    if result.is_ok() && check_foreign_keys {
        // Deferred foreign keys are checked at the end of the transaction, and
        // a rebuild's savepoint may be released inside a longer one — so the
        // check is made here, where the table has just been put back, rather
        // than left to a commit that might be minutes away.
        result = foreign_key_check(conn);
    }
    match result {
        Ok(()) => execute(conn, &format!("RELEASE {SAVEPOINT}")),
        Err(e) => {
            // Roll the savepoint back and release it, so the connection is not
            // left holding one. Both are best-effort: the original failure is
            // what the caller needs to see.
            let _ = execute(conn, &format!("ROLLBACK TO {SAVEPOINT}"));
            let _ = execute(conn, &format!("RELEASE {SAVEPOINT}"));
            Err(e)
        }
    }
}

/// Refuse to leave a foreign key pointing at nothing.
fn foreign_key_check(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA foreign_key_check")
        .map_err(|e| Error::database(format!("checking foreign keys: {e}")))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| Error::database(format!("checking foreign keys: {e}")))?;
    let broken = rows
        .next()
        .map_err(|e| Error::database(format!("checking foreign keys: {e}")))?
        .map(|row| row.get::<_, Option<String>>(0).unwrap_or_default());
    match broken {
        None => Ok(()),
        Some(table) => Err(Error::database(format!(
            "the change would leave a foreign key pointing at nothing (in table `{}`)",
            table.unwrap_or_else(|| "?".to_owned())
        ))),
    }
}

/// Run one statement, reporting a failure with the SQL that caused it.
fn execute(conn: &Connection, sql: &str) -> Result<()> {
    sc_log::log_sql(sql, crate::exec::NO_BINDS);
    conn.execute_batch(sql).map_err(|e| {
        Error::database(format!(
            "apply_schema failed: {}\n  sql: {sql}",
            crate::exec::db_error(&e)
        ))
    })
}

/// A comma-separated list of quoted identifiers.
fn quoted_list(dialect: &SqliteDialect, items: &[String]) -> String {
    items
        .iter()
        .map(|c| dialect.quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A change's name, for an error that has to say which one it is about.
fn change_name(change: &SchemaChange) -> &'static str {
    match change {
        SchemaChange::CreateTable { .. } => "create table",
        SchemaChange::DropTable { .. } => "drop table",
        SchemaChange::AddColumn { .. } => "add column",
        SchemaChange::DropColumn { .. } => "drop column",
        SchemaChange::RenameColumn { .. } => "rename column",
        SchemaChange::SetPrimaryKey { .. } => "set primary key",
        SchemaChange::SetColumnGenerator { .. } => "set column generator",
        SchemaChange::SetColumnNullable { .. } => "set column nullable",
        SchemaChange::SetColumnReference { .. } => "set column reference",
        SchemaChange::AddUniqueConstraint { .. } => "add unique constraint",
        SchemaChange::DropConstraint { .. } => "drop constraint",
        SchemaChange::CreateIndex { .. } => "create index",
        SchemaChange::DropIndex { .. } => "drop index",
        SchemaChange::SetComment { .. } => "set comment",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_change(change: &SchemaChange) -> String {
        render(&SqliteDialect, change).expect("render")
    }

    #[test]
    fn create_table_emits_exactly_the_columns_given() {
        let sql = render_change(&SchemaChange::CreateTable {
            name: "member".into(),
            columns: vec![
                ColumnDef::new("org", "int8").not_null(),
                ColumnDef::new("user_id", "int8").not_null(),
                ColumnDef::new("email", "text").unique(),
            ],
            primary_key: vec!["org".into(), "user_id".into()],
            unlogged: true,
        });
        assert_eq!(
            sql,
            "CREATE TABLE \"member\" (\"org\" int8 NOT NULL, \"user_id\" int8 NOT NULL, \
             \"email\" text UNIQUE, PRIMARY KEY (\"org\", \"user_id\"))"
        );
        // No invented id column, and `unlogged` is silently an ordinary table.
        assert!(!sql.contains("\"id\""));
        assert!(!sql.to_uppercase().contains("UNLOGGED"));
    }

    #[test]
    fn an_identity_key_becomes_the_rowid_alias() {
        let sql = render_change(&SchemaChange::CreateTable {
            name: "book".into(),
            columns: vec![
                ColumnDef::new("id", "int8").not_null().identity(),
                ColumnDef::new("title", "text"),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        });
        // `INTEGER PRIMARY KEY`, spelled exactly so — and no second, table-level
        // key declaration, which would stop SQLite numbering it.
        assert_eq!(
            sql,
            "CREATE TABLE \"book\" (\"id\" INTEGER PRIMARY KEY, \"title\" text)"
        );
    }

    #[test]
    fn an_identity_column_that_is_not_the_whole_key_is_refused() {
        let err = render(
            &SqliteDialect,
            &SchemaChange::CreateTable {
                name: "member".into(),
                columns: vec![
                    ColumnDef::new("org", "int8").not_null(),
                    ColumnDef::new("seq", "int8").not_null().identity(),
                ],
                primary_key: vec!["org".into(), "seq".into()],
                unlogged: false,
            },
        )
        .expect_err("SQLite cannot number part of a composite key");
        assert!(format!("{err}").contains("single-column integer primary key"));
    }

    #[test]
    fn a_uuid_key_gets_a_default_sqlite_can_actually_run() {
        let sql = render_change(&SchemaChange::CreateTable {
            name: "app".into(),
            columns: vec![
                ColumnDef::new("id", "uuid")
                    .not_null()
                    .default("gen_random_uuid()"),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        });
        assert!(!sql.contains("gen_random_uuid"), "{sql}");
        assert!(sql.contains("randomblob"), "{sql}");
    }

    #[test]
    fn a_key_column_is_not_null_even_when_the_change_forgot_to_say_so() {
        let sql = render_change(&SchemaChange::CreateTable {
            name: "t".into(),
            columns: vec![ColumnDef::new("k", "text")],
            primary_key: vec!["k".into()],
            unlogged: false,
        });
        assert_eq!(
            sql,
            "CREATE TABLE \"t\" (\"k\" text NOT NULL, PRIMARY KEY (\"k\"))"
        );
    }

    #[test]
    fn a_unique_column_added_later_becomes_the_index_sqlite_will_accept() {
        let sql = render_change(&SchemaChange::AddColumn {
            table: "book".into(),
            column: ColumnDef::new("isbn", "text").unique(),
        });
        assert_eq!(
            sql,
            "ALTER TABLE \"book\" ADD COLUMN \"isbn\" text; \
             CREATE UNIQUE INDEX \"book_isbn_key\" ON \"book\" (\"isbn\")"
        );
    }

    #[test]
    fn a_unique_constraint_is_a_unique_index_under_the_name_it_was_given() {
        let sql = render_change(&SchemaChange::AddUniqueConstraint {
            table: "book".into(),
            name: "book_title_author".into(),
            columns: vec!["title".into(), "author".into()],
        });
        assert_eq!(
            sql,
            "CREATE UNIQUE INDEX \"book_title_author\" ON \"book\" (\"title\", \"author\")"
        );
        // …and dropped by that name, which is what makes the pair usable.
        assert_eq!(
            render_change(&SchemaChange::DropConstraint {
                table: "book".into(),
                name: "book_title_author".into(),
                if_exists: true,
            }),
            "DROP INDEX IF EXISTS \"book_title_author\""
        );
    }

    #[test]
    fn a_comment_is_stored_in_the_drivers_own_table() {
        let sql = render_change(&SchemaChange::SetComment {
            target: CommentTarget::Constraint {
                table: "book".into(),
                name: "book_isbn_key".into(),
            },
            comment: Some("that ISBN is already used".into()),
        });
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS \"_fd_object_comments\""));
        assert!(sql.contains("that ISBN is already used"));
        assert!(sql.contains("ON CONFLICT"));

        let removed = render_change(&SchemaChange::SetComment {
            target: CommentTarget::Index {
                name: "book_isbn_key".into(),
            },
            comment: None,
        });
        assert!(removed.contains("DELETE FROM \"_fd_object_comments\""));
    }

    #[test]
    fn a_change_that_needs_a_rebuild_says_so_rather_than_rendering_a_guess() {
        let err = render(
            &SqliteDialect,
            &SchemaChange::SetPrimaryKey {
                table: "book".into(),
                columns: vec!["id".into()],
            },
        )
        .expect_err("a rebuild cannot be rendered from the change alone");
        assert!(format!("{err}").contains("rebuilding the table"));
    }

    #[test]
    fn a_column_is_renamed_in_place() {
        assert_eq!(
            render_change(&SchemaChange::RenameColumn {
                table: "member".into(),
                from: "note".into(),
                to: "remark".into(),
            }),
            "ALTER TABLE \"member\" RENAME COLUMN \"note\" TO \"remark\""
        );
    }
}
