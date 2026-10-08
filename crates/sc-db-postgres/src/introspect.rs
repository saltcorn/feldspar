//! Live-schema introspection: read the connected database's tables straight from
//! the catalog into [`PhysicalTable`]s.
//!
//! Per the goals there is no discovery/registration step — everything a
//! connection can see is returned here (technical design §5). We read all base
//! tables in non-system schemas.
//!
//! Columns and their types come from `information_schema`; primary keys and
//! foreign keys come from `pg_catalog` instead, because the goals require
//! **composite primary keys** and **foreign keys to non-primary-key columns**,
//! and only the catalog's `conkey`/`confkey` attribute-number arrays give the
//! key columns in the correct order (the `information_schema` constraint views
//! lose that ordering for composite keys). `unnest(… ) WITH ORDINALITY`
//! preserves it.

use std::collections::BTreeMap;

use sc_db::{
    Column, ColumnGenerator, ForeignKey, PhysicalConstraint, PhysicalConstraintKind, PhysicalTable,
};
use sc_error::{Error, Result};
use tokio_postgres::{Client, Row};

/// All user base tables, keyed later by `(schema, name)`.
///
/// A table an **extension** owns is not a user table, so it is left out:
/// PostGIS's `spatial_ref_sys` sits in `public` beside the user's tables, and
/// offering it as one would put four thousand coordinate systems in the admin's
/// table list (analytics TODO A5.1).
const TABLES_SQL: &str = "\
    SELECT t.table_schema, t.table_name \
    FROM information_schema.tables t \
    WHERE t.table_type = 'BASE TABLE' \
      AND t.table_schema NOT IN ('pg_catalog', 'information_schema') \
      AND NOT EXISTS ( \
        SELECT 1 FROM pg_depend d \
        JOIN pg_class c ON c.oid = d.objid \
        JOIN pg_namespace n ON n.oid = c.relnamespace \
        WHERE d.classid = 'pg_class'::regclass AND d.deptype = 'e' \
          AND n.nspname = t.table_schema AND c.relname = t.table_name) \
    ORDER BY t.table_schema, t.table_name";

/// Every column of every user table, in declaration order. `udt_name` is the
/// backend's own type name (`int8`, `text`, `timestamptz`, …), matching what
/// `apply_schema` emits, rather than the friendlier `data_type`.
///
/// `is_identity` as well as `column_default`, because an identity column has no
/// default to report — the two are separate spellings of the one fact that the
/// database fills the column in, and reading only the first would make every
/// identity key look like a key somebody has to type.
///
/// A PostGIS column is the exception: `udt_name` says only `geometry`, and the
/// kind and coordinate system are in its type modifier, so `format_type` spells
/// it out (`geometry(Point,4326)`) — what `apply_schema` emits for one
/// (analytics TODO A5.1).
const COLUMNS_SQL: &str = "\
    SELECT c.table_schema, c.table_name, c.column_name, \
           CASE WHEN c.udt_name = 'geometry' THEN ( \
             SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             JOIN pg_class t ON t.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = c.table_schema AND t.relname = c.table_name \
               AND a.attname = c.column_name) \
           ELSE c.udt_name::text END AS udt_name, \
           c.is_nullable, c.column_default, c.is_identity \
    FROM information_schema.columns c \
    WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') \
    ORDER BY c.table_schema, c.table_name, c.ordinal_position";

/// Primary-key columns, one row per key column, ordered within each key so a
/// composite key is reassembled correctly.
const PK_SQL: &str = "\
    SELECT n.nspname, t.relname, a.attname \
    FROM pg_constraint c \
    JOIN pg_class t ON t.oid = c.conrelid \
    JOIN pg_namespace n ON n.oid = t.relnamespace \
    JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON true \
    JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
    WHERE c.contype = 'p' \
      AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
    ORDER BY n.nspname, t.relname, k.ord";

/// Foreign-key columns, one row per constrained column, pairing each local
/// column with the referenced column it points at (which need not be a primary
/// key). Ordered within each constraint.
const FK_SQL: &str = "\
    SELECT n.nspname, t.relname, c.conname, la.attname, ft.relname, fa.attname \
    FROM pg_constraint c \
    JOIN pg_class t ON t.oid = c.conrelid \
    JOIN pg_namespace n ON n.oid = t.relnamespace \
    JOIN pg_class ft ON ft.oid = c.confrelid \
    JOIN LATERAL unnest(c.conkey, c.confkey) WITH ORDINALITY AS k(local_attnum, ref_attnum, ord) ON true \
    JOIN pg_attribute la ON la.attrelid = c.conrelid AND la.attnum = k.local_attnum \
    JOIN pg_attribute fa ON fa.attrelid = c.confrelid AND fa.attnum = k.ref_attnum \
    WHERE c.contype = 'f' \
      AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
    ORDER BY n.nspname, t.relname, c.conname, k.ord";

/// Unique constraints, one row per constrained column, ordered within each
/// constraint so a jointly-unique key is reassembled in the order it was
/// declared. The comment comes along: it is where the constraint's error message
/// lives (see [`PhysicalConstraint::comment`]).
const UNIQUE_SQL: &str = "\
    SELECT n.nspname, t.relname, c.conname, a.attname, obj_description(c.oid, 'pg_constraint') \
    FROM pg_constraint c \
    JOIN pg_class t ON t.oid = c.conrelid \
    JOIN pg_namespace n ON n.oid = t.relnamespace \
    JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON true \
    JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
    WHERE c.contype = 'u' \
      AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
    ORDER BY n.nspname, t.relname, c.conname, k.ord";

/// Indexes **no constraint owns**. A primary key and a unique constraint each
/// have an index behind them; reporting those here as well would show every
/// constraint twice and offer a `DROP INDEX` that Postgres refuses. `conindid`
/// is the join that excludes them.
///
/// Columns come from `indkey` (attribute numbers, zero for an expression) and
/// the expression from `pg_get_expr`, so an ordinary index reports its columns
/// and a full-text one reports the expression it is over.
const INDEX_SQL: &str = "\
    SELECT n.nspname, t.relname, i.relname, am.amname, \
           (SELECT array_agg(a.attname ORDER BY k.ord) \
              FROM unnest(ix.indkey::int[]) WITH ORDINALITY AS k(attnum, ord) \
              JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum), \
           pg_get_expr(ix.indexprs, ix.indrelid), \
           obj_description(i.oid, 'pg_class') \
    FROM pg_index ix \
    JOIN pg_class i ON i.oid = ix.indexrelid \
    JOIN pg_class t ON t.oid = ix.indrelid \
    JOIN pg_namespace n ON n.oid = t.relnamespace \
    JOIN pg_am am ON am.oid = i.relam \
    WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
      AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.oid) \
    ORDER BY n.nspname, t.relname, i.relname";

/// Row-level triggers, which is how a row constraint is enforced. `tgisinternal`
/// excludes the ones Postgres creates for its own foreign keys and deferred
/// constraints — those are the foreign keys, already reported as such.
const TRIGGER_SQL: &str = "\
    SELECT n.nspname, t.relname, g.tgname, obj_description(g.oid, 'pg_trigger') \
    FROM pg_trigger g \
    JOIN pg_class t ON t.oid = g.tgrelid \
    JOIN pg_namespace n ON n.oid = t.relnamespace \
    WHERE NOT g.tgisinternal \
      AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
    ORDER BY n.nspname, t.relname, g.tgname";

/// The `(schema, table)` identity used to collate rows from the separate
/// queries.
type TableKey = (String, String);

/// Introspect all user base tables reachable through `client`.
pub async fn introspect(client: &Client) -> Result<Vec<PhysicalTable>> {
    let mut tables: BTreeMap<TableKey, PhysicalTable> = BTreeMap::new();

    for row in run(client, TABLES_SQL).await? {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        tables.insert(
            (schema.clone(), name.clone()),
            PhysicalTable {
                name,
                schema: Some(schema),
                columns: Vec::new(),
                primary_key: Vec::new(),
                foreign_keys: Vec::new(),
                constraints: Vec::new(),
            },
        );
    }

    for row in run(client, COLUMNS_SQL).await? {
        let key: TableKey = (row.get(0), row.get(1));
        if let Some(table) = tables.get_mut(&key) {
            let is_nullable: String = row.get(4);
            let column_default: Option<String> = row.get(5);
            let is_identity: String = row.get(6);
            table.columns.push(Column {
                name: row.get(2),
                sql_type: row.get(3),
                nullable: is_nullable == "YES",
                generated: match is_identity == "YES" {
                    true => Some(ColumnGenerator::Identity),
                    false => column_default.map(ColumnGenerator::Default),
                },
            });
        }
    }

    for row in run(client, PK_SQL).await? {
        let key: TableKey = (row.get(0), row.get(1));
        if let Some(table) = tables.get_mut(&key) {
            table.primary_key.push(row.get(2));
        }
    }

    // Foreign keys span multiple rows (one per column pair); collate by
    // constraint name before turning each into a `ForeignKey`.
    type FkParts = (String, Vec<String>, Vec<String>); // (ref_table, local, referenced)
    let mut by_table: BTreeMap<TableKey, BTreeMap<String, FkParts>> = BTreeMap::new();
    for row in run(client, FK_SQL).await? {
        let key: TableKey = (row.get(0), row.get(1));
        let conname: String = row.get(2);
        let local_col: String = row.get(3);
        let ref_table: String = row.get(4);
        let ref_col: String = row.get(5);
        let parts = by_table
            .entry(key)
            .or_default()
            .entry(conname)
            .or_insert_with(|| (ref_table, Vec::new(), Vec::new()));
        parts.1.push(local_col);
        parts.2.push(ref_col);
    }
    for (key, constraints) in by_table {
        if let Some(table) = tables.get_mut(&key) {
            for (_conname, (ref_table, columns, referenced_columns)) in constraints {
                table.foreign_keys.push(ForeignKey {
                    columns,
                    referenced_table: ref_table,
                    referenced_columns,
                });
            }
        }
    }

    // Unique constraints span multiple rows, like foreign keys; collate by
    // constraint name, keeping the column order the key was declared in.
    type UniqueParts = (Vec<String>, Option<String>); // (columns, comment)
    let mut uniques: BTreeMap<TableKey, BTreeMap<String, UniqueParts>> = BTreeMap::new();
    for row in run(client, UNIQUE_SQL).await? {
        let key: TableKey = (row.get(0), row.get(1));
        let conname: String = row.get(2);
        let column: String = row.get(3);
        let comment: Option<String> = row.get(4);
        let entry = uniques
            .entry(key)
            .or_default()
            .entry(conname)
            .or_insert_with(|| (Vec::new(), comment));
        entry.0.push(column);
    }
    for (key, constraints) in uniques {
        if let Some(table) = tables.get_mut(&key) {
            for (name, (columns, comment)) in constraints {
                table.constraints.push(PhysicalConstraint {
                    name,
                    kind: PhysicalConstraintKind::Unique { columns },
                    comment,
                });
            }
        }
    }

    for row in run(client, INDEX_SQL).await? {
        let key: TableKey = (row.get(0), row.get(1));
        if let Some(table) = tables.get_mut(&key) {
            let columns: Option<Vec<String>> = row.get(4);
            table.constraints.push(PhysicalConstraint {
                name: row.get(2),
                kind: PhysicalConstraintKind::Index {
                    columns: columns.unwrap_or_default(),
                    expression: row.get(5),
                    method: row.get(3),
                },
                comment: row.get(6),
            });
        }
    }

    for row in run(client, TRIGGER_SQL).await? {
        let key: TableKey = (row.get(0), row.get(1));
        if let Some(table) = tables.get_mut(&key) {
            table.constraints.push(PhysicalConstraint {
                name: row.get(2),
                kind: PhysicalConstraintKind::RowTrigger,
                comment: row.get(3),
            });
        }
    }

    Ok(tables.into_values().collect())
}

/// Run a parameterless catalog query, mapping the driver error into ours.
async fn run(client: &Client, sql: &str) -> Result<Vec<Row>> {
    // Introspection is SQL this process issued too, and it is a large part of
    // what a startup does — an echo that hid it would leave an admin wondering
    // what the connection was busy with.
    sc_log::log_sql(sql, crate::exec::NO_BINDS);
    client.query(sql, &[]).await.map_err(|e| {
        Error::database(format!(
            "introspect query failed: {}\n  sql: {sql}",
            sc_error::format_chain(&e)
        ))
    })
}
