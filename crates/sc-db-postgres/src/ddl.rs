//! Rendering a [`SchemaChange`] to Postgres DDL.
//!
//! One [`SchemaChange`] renders to one DDL statement (technical design §5) —
//! or, where Postgres has no single statement for what was asked (`SET PRIMARY
//! KEY`, `SET COLUMN GENERATOR`), one `;`-joined batch that leaves the schema in
//! the state the change describes. Identifiers (table and column names) are
//! quoted through the dialect; the `sql_type` of a [`ColumnDef`] and the SQL of a
//! [`ColumnGenerator::Default`] are **structural SQL fragments** supplied by
//! trusted schema definitions (e.g. `int8`, `varchar(255)`, `now()`), not user
//! data, so they pass through verbatim — DDL cannot be parameterised in any
//! case.
//!
//! The cardinal rule from the goals is honoured by omission: creating a table
//! emits **exactly** the columns given and **never** invents an `id` column. A
//! primary key is declared only when one is asked for, and may be composite.

use sc_db::{ColumnDef, ColumnGenerator, CommentTarget, IndexOn, SchemaChange};
use sc_error::{Error, Result};
use sc_query::SqlDialect;

use crate::dialect::PgDialect;

/// Render `change` to a single Postgres DDL statement.
pub fn render(dialect: &PgDialect, change: &SchemaChange) -> Result<String> {
    let sql = match change {
        SchemaChange::CreateTable {
            name,
            columns,
            primary_key,
            unlogged,
        } => {
            // Every primary-key column must be one of the declared columns —
            // catch a malformed change before Postgres does, with a clearer
            // message.
            for pk in primary_key {
                if !columns.iter().any(|c| &c.name == pk) {
                    return Err(Error::invalid(format!(
                        "primary key column `{pk}` is not a column of table `{name}`"
                    )));
                }
            }
            let mut items: Vec<String> = columns.iter().map(|c| column_def(dialect, c)).collect();
            if !primary_key.is_empty() {
                let cols = primary_key
                    .iter()
                    .map(|c| dialect.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                items.push(format!("PRIMARY KEY ({cols})"));
            }
            // `UNLOGGED` is honoured rather than checked: the driver advertises
            // the capability, so a change that asks for it here has already been
            // routed to a backend that has it.
            format!(
                "CREATE {}TABLE {} ({})",
                if *unlogged { "UNLOGGED " } else { "" },
                dialect.quote_ident(name),
                items.join(", ")
            )
        }
        SchemaChange::DropTable { name, if_exists } => {
            format!(
                "DROP TABLE {}{}",
                if_exists_clause(*if_exists),
                dialect.quote_ident(name)
            )
        }
        SchemaChange::AddColumn { table, column } => {
            format!(
                "ALTER TABLE {} ADD COLUMN {}",
                dialect.quote_ident(table),
                column_def(dialect, column)
            )
        }
        SchemaChange::DropColumn {
            table,
            column,
            if_exists,
        } => {
            format!(
                "ALTER TABLE {} DROP COLUMN {}{}",
                dialect.quote_ident(table),
                if_exists_clause(*if_exists),
                dialect.quote_ident(column)
            )
        }
        SchemaChange::RenameColumn { table, from, to } => {
            format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                dialect.quote_ident(table),
                dialect.quote_ident(from),
                dialect.quote_ident(to)
            )
        }
        SchemaChange::SetPrimaryKey { table, columns } => {
            // Two statements, because a table may already have a key and
            // Postgres has no `ALTER PRIMARY KEY`. The old constraint is dropped
            // by the name Postgres gives every primary key it creates —
            // `<table>_pkey` — which is what `ADD PRIMARY KEY` below will name
            // the new one, so the pair is stable under repetition. `IF EXISTS`
            // covers the ordinary case of a table that has no key yet.
            //
            // The columns are also made `NOT NULL`: Postgres does this itself
            // when adding a primary key, but only in the sense of rejecting the
            // change if a null is present. Saying it explicitly means the
            // introspected column matches the field the admin declared.
            //
            // **No columns means no key**: the drop alone. That is the state a
            // table is created in and the state it returns to when the last key
            // field stops being one, so it has to be expressible.
            let quoted_table = dialect.quote_ident(table);
            let mut sql = format!(
                "ALTER TABLE {quoted_table} DROP CONSTRAINT IF EXISTS {}",
                dialect.quote_ident(&format!("{table}_pkey"))
            );
            if !columns.is_empty() {
                for column in columns {
                    sql.push_str(&format!(
                        "; ALTER TABLE {quoted_table} ALTER COLUMN {} SET NOT NULL",
                        dialect.quote_ident(column)
                    ));
                }
                let cols = columns
                    .iter()
                    .map(|c| dialect.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                sql.push_str(&format!(
                    "; ALTER TABLE {quoted_table} ADD PRIMARY KEY ({cols})"
                ));
            }
            sql
        }
        SchemaChange::SetColumnGenerator {
            table,
            column,
            generator,
        } => {
            let quoted_table = dialect.quote_ident(table);
            let quoted_column = dialect.quote_ident(column);
            // A column has one generator, never both kinds, so every change
            // starts by taking away whichever it has. **Identity first**:
            // Postgres refuses `DROP DEFAULT` on an identity column outright
            // (with a hint to drop the identity instead), so the other order
            // fails on exactly the column this exists to change. Both drops are
            // no-ops on a column that has neither.
            let mut sql = format!(
                "ALTER TABLE {quoted_table} ALTER COLUMN {quoted_column} DROP IDENTITY IF EXISTS; \
                 ALTER TABLE {quoted_table} ALTER COLUMN {quoted_column} DROP DEFAULT"
            );
            match generator {
                Some(ColumnGenerator::Identity) => {
                    // The column must already be `NOT NULL` — Postgres will not
                    // make a nullable column an identity — which is why the
                    // caller emits `SET PRIMARY KEY` (which sets it) first.
                    sql.push_str(&format!(
                        "; ALTER TABLE {quoted_table} ALTER COLUMN {quoted_column} \
                         ADD GENERATED BY DEFAULT AS IDENTITY"
                    ));
                    // The rows already in the table own numbers the brand-new
                    // sequence would hand out all over again — a table gets its
                    // key late exactly when it already has rows, so the very
                    // first insert afterwards would collide. Postgres does not
                    // look at the data when adding an identity, so the sequence
                    // is wound past the largest key there is. `is_called =
                    // false` means "hand out this value next", so an empty table
                    // starts at 1.
                    sql.push_str(&format!(
                        "; SELECT setval(pg_get_serial_sequence({}, {}), \
                         coalesce(max({quoted_column}), 0) + 1, false) FROM {quoted_table}",
                        quote_string(&quoted_table),
                        quote_string(column)
                    ));
                }
                Some(ColumnGenerator::Default(default)) => {
                    sql.push_str(&format!(
                        "; ALTER TABLE {quoted_table} ALTER COLUMN {quoted_column} \
                         SET DEFAULT {default}"
                    ));
                }
                None => {}
            }
            sql
        }
        SchemaChange::SetColumnNullable {
            table,
            column,
            nullable,
        } => {
            format!(
                "ALTER TABLE {} ALTER COLUMN {} {} NOT NULL",
                dialect.quote_ident(table),
                dialect.quote_ident(column),
                if *nullable { "DROP" } else { "SET" }
            )
        }
        SchemaChange::SetColumnReference {
            table,
            column,
            references,
        } => {
            // The key being replaced is found by what it constrains rather than
            // by name: one declared by `ADD COLUMN … REFERENCES` is named by
            // Postgres, and one on a table built outside Saltcorn is named by
            // whoever built it. So a block walks the catalog for every
            // single-column foreign key on this column and drops each — none at
            // all being the ordinary case of a plain column becoming a key.
            //
            // Names reach the block as string literals, not identifiers:
            // `to_regclass` resolves the quoted table name through the
            // connection's `search_path`, exactly as the `ALTER TABLE` beside it
            // does.
            let quoted_table = dialect.quote_ident(table);
            let mut sql = format!(
                "DO $sc$ DECLARE fk record; BEGIN \
                 FOR fk IN SELECT c.conname FROM pg_constraint c \
                 JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1] \
                 WHERE c.conrelid = to_regclass({table_lit}) AND c.contype = 'f' \
                 AND array_length(c.conkey, 1) = 1 AND a.attname = {column_lit} \
                 LOOP EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', {table_lit}, fk.conname); \
                 END LOOP; END $sc$",
                table_lit = quote_string(&quoted_table),
                column_lit = quote_string(column),
            );
            if let Some(target) = references {
                // Declared as `column_def` declares one — deferrable, checked
                // immediately — so a key repointed here is the same key a field
                // created pointing there would have had. The name is left to
                // Postgres, which is what `ADD COLUMN … REFERENCES` does too.
                sql.push_str(&format!(
                    "; ALTER TABLE {quoted_table} ADD FOREIGN KEY ({}) REFERENCES {} ({}) \
                     DEFERRABLE INITIALLY IMMEDIATE",
                    dialect.quote_ident(column),
                    dialect.quote_ident(&target.table),
                    dialect.quote_ident(&target.column),
                ));
            }
            sql
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
            format!(
                "ALTER TABLE {} ADD CONSTRAINT {} UNIQUE ({})",
                dialect.quote_ident(table),
                dialect.quote_ident(name),
                quoted_list(dialect, columns)
            )
        }
        SchemaChange::DropConstraint {
            table,
            name,
            if_exists,
        } => {
            format!(
                "ALTER TABLE {} DROP CONSTRAINT {}{}",
                dialect.quote_ident(table),
                if_exists_clause(*if_exists),
                dialect.quote_ident(name)
            )
        }
        SchemaChange::CreateIndex {
            table,
            name,
            on,
            method,
        } => {
            // The `USING` clause and, for an expression index, the expression
            // itself are structural SQL fragments from trusted code — the same
            // rule `sql_type` and a column default are passed under, and the
            // same one DDL leaves no alternative to.
            let using = match method {
                Some(m) => format!(" USING {m}"),
                None => String::new(),
            };
            let target = match on {
                IndexOn::Columns(columns) => {
                    if columns.is_empty() {
                        return Err(Error::invalid(format!(
                            "index `{name}` on `{table}` names no columns"
                        )));
                    }
                    quoted_list(dialect, columns)
                }
                // Parenthesised: Postgres requires an index expression to be,
                // unless it is a bare function call — and "unless" is not a rule
                // worth carrying when the extra parentheses are free.
                IndexOn::Expression(expr) => format!("({expr})"),
            };
            format!(
                "CREATE INDEX {} ON {}{using} ({target})",
                dialect.quote_ident(name),
                dialect.quote_ident(table),
            )
        }
        SchemaChange::DropIndex { name, if_exists } => {
            format!(
                "DROP INDEX {}{}",
                if_exists_clause(*if_exists),
                dialect.quote_ident(name)
            )
        }
        SchemaChange::SetComment { target, comment } => {
            // `NULL` — the keyword, not a quoted string — is how a comment is
            // removed; `IS ''` would leave an empty comment behind, which reads
            // back as "somebody commented this and said nothing".
            let text = match comment {
                Some(text) => quote_string(text),
                None => "NULL".to_owned(),
            };
            let what = match target {
                CommentTarget::Constraint { table, name } => format!(
                    "CONSTRAINT {} ON {}",
                    dialect.quote_ident(name),
                    dialect.quote_ident(table)
                ),
                CommentTarget::Index { name } => format!("INDEX {}", dialect.quote_ident(name)),
                CommentTarget::Trigger { table, name } => format!(
                    "TRIGGER {} ON {}",
                    dialect.quote_ident(name),
                    dialect.quote_ident(table)
                ),
            };
            format!("COMMENT ON {what} IS {text}")
        }
    };
    Ok(sql)
}

/// A comma-separated list of quoted identifiers.
fn quoted_list(dialect: &PgDialect, items: &[String]) -> String {
    items
        .iter()
        .map(|c| dialect.quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A SQL string literal holding `value` — for the places a name is passed as
/// *text* rather than as an identifier (`pg_get_serial_sequence`, the catalog
/// lookup that finds a column's foreign key).
fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Render one column definition:
/// `"name" type [NOT NULL] [DEFAULT … | GENERATED …] [UNIQUE] [REFERENCES "t" ("c")]`.
fn column_def(dialect: &PgDialect, col: &ColumnDef) -> String {
    let mut s = dialect.quote_ident(&col.name);
    s.push(' ');
    s.push_str(&col.sql_type);
    if !col.nullable {
        s.push_str(" NOT NULL");
    }
    match &col.generated {
        Some(ColumnGenerator::Identity) => s.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
        Some(ColumnGenerator::Default(sql)) => {
            s.push_str(" DEFAULT ");
            s.push_str(sql);
        }
        None => {}
    }
    if col.unique {
        s.push_str(" UNIQUE");
    }
    if let Some(target) = &col.references {
        // A column-level REFERENCES clause, so the same rendering serves both
        // CREATE TABLE and ADD COLUMN. The referenced column is named
        // explicitly rather than left implicit: the goals require keys onto
        // non-primary-key columns, and the implicit form always means the PK.
        s.push_str(" REFERENCES ");
        s.push_str(&dialect.quote_ident(&target.table));
        s.push_str(" (");
        s.push_str(&dialect.quote_ident(&target.column));
        s.push(')');
        // **Deferrable, checked immediately by default.** Every write behaves
        // exactly as it did — a bad key is refused at the statement, with the
        // statement's error — but a caller holding a transaction may say `SET
        // CONSTRAINTS ALL DEFERRED` and have the check happen at commit
        // instead. That is what makes a CSV of a self-referencing table
        // importable (§13.1): the rows arrive in the file's order, and a parent
        // that appears three lines further down is there by the time the
        // transaction commits. A constraint declared without this cannot be
        // deferred later — it is fixed at creation — so it is the default here
        // rather than something asked for per table.
        s.push_str(" DEFERRABLE INITIALLY IMMEDIATE");
    }
    s
}

/// `"IF EXISTS "` or `""`.
fn if_exists_clause(if_exists: bool) -> &'static str {
    if if_exists { "IF EXISTS " } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_ok(change: &SchemaChange) -> String {
        render(&PgDialect::new(), change).expect("render")
    }

    #[test]
    fn create_table_with_composite_pk_and_no_invented_id() {
        let change = SchemaChange::CreateTable {
            name: "member".into(),
            columns: vec![
                ColumnDef::new("org", "int8").not_null(),
                ColumnDef::new("user_id", "int8").not_null(),
                ColumnDef::new("email", "text").unique(),
            ],
            primary_key: vec!["org".into(), "user_id".into()],
            unlogged: false,
        };
        let sql = render_ok(&change);
        assert_eq!(
            sql,
            "CREATE TABLE \"member\" (\"org\" int8 NOT NULL, \"user_id\" int8 NOT NULL, \
             \"email\" text UNIQUE, PRIMARY KEY (\"org\", \"user_id\"))"
        );
        // No auto `id` column is ever added.
        assert!(!sql.contains("\"id\""));
    }

    #[test]
    fn create_table_renders_a_foreign_key_naming_the_target_column() {
        // The referenced column is named explicitly, not left implicit: the
        // goals require keys onto non-primary-key columns, and an implicit
        // REFERENCES always means the target's primary key.
        let change = SchemaChange::CreateTable {
            name: "users".into(),
            columns: vec![
                ColumnDef::new("id", "uuid").not_null(),
                ColumnDef::new("role", "int8")
                    .not_null()
                    .references("_fd_roles", "role"),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        };
        assert_eq!(
            render_ok(&change),
            "CREATE TABLE \"users\" (\"id\" uuid NOT NULL, \
             \"role\" int8 NOT NULL REFERENCES \"_fd_roles\" (\"role\") \
             DEFERRABLE INITIALLY IMMEDIATE, PRIMARY KEY (\"id\"))"
        );
    }

    #[test]
    fn a_column_can_be_created_already_filling_itself_in() {
        let change = SchemaChange::CreateTable {
            name: "invoice".into(),
            columns: vec![
                ColumnDef::new("id", "int8").not_null().identity(),
                ColumnDef::new("ref", "uuid")
                    .not_null()
                    .default("gen_random_uuid()"),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        };
        assert_eq!(
            render_ok(&change),
            "CREATE TABLE \"invoice\" (\"id\" int8 NOT NULL GENERATED BY DEFAULT AS IDENTITY, \
             \"ref\" uuid NOT NULL DEFAULT gen_random_uuid(), PRIMARY KEY (\"id\"))"
        );
    }

    #[test]
    fn a_column_that_already_exists_can_be_made_to_fill_itself_in() {
        // The identity has to be *added* to a column that is already there,
        // because a table gets its key late — and the sequence has to be wound
        // past the rows that are already in it, or the very first insert
        // afterwards would collide with a key somebody already has.
        let change = SchemaChange::SetColumnGenerator {
            table: "reading".into(),
            column: "code".into(),
            generator: Some(ColumnGenerator::Identity),
        };
        assert_eq!(
            render_ok(&change),
            "ALTER TABLE \"reading\" ALTER COLUMN \"code\" DROP IDENTITY IF EXISTS; \
             ALTER TABLE \"reading\" ALTER COLUMN \"code\" DROP DEFAULT; \
             ALTER TABLE \"reading\" ALTER COLUMN \"code\" ADD GENERATED BY DEFAULT AS IDENTITY; \
             SELECT setval(pg_get_serial_sequence('\"reading\"', 'code'), \
             coalesce(max(\"code\"), 0) + 1, false) FROM \"reading\""
        );

        // A default is the other half, and needs no winding: it is evaluated per
        // row rather than counted.
        let change = SchemaChange::SetColumnGenerator {
            table: "reading".into(),
            column: "code".into(),
            generator: Some(ColumnGenerator::Default("gen_random_uuid()".into())),
        };
        assert!(render_ok(&change).ends_with(
            "ALTER TABLE \"reading\" ALTER COLUMN \"code\" SET DEFAULT gen_random_uuid()"
        ));

        // And taking it away is the drops alone. The identity goes first in
        // every case: Postgres refuses `DROP DEFAULT` on an identity column
        // outright, so the other order fails on the very column this changes.
        let change = SchemaChange::SetColumnGenerator {
            table: "reading".into(),
            column: "code".into(),
            generator: None,
        };
        assert_eq!(
            render_ok(&change),
            "ALTER TABLE \"reading\" ALTER COLUMN \"code\" DROP IDENTITY IF EXISTS; \
             ALTER TABLE \"reading\" ALTER COLUMN \"code\" DROP DEFAULT"
        );
    }

    #[test]
    fn add_column_can_be_a_foreign_key() {
        let change = SchemaChange::AddColumn {
            table: "book".into(),
            column: ColumnDef::new("author", "int8").references("person", "id"),
        };
        // `DEFERRABLE INITIALLY IMMEDIATE`: the check happens at the statement
        // as it always did, but a transaction may now defer it to commit — which
        // is what lets a CSV of a self-referencing table load in file order. A
        // constraint that was not declared deferrable can never be deferred.
        assert_eq!(
            render_ok(&change),
            "ALTER TABLE \"book\" ADD COLUMN \"author\" int8 \
             REFERENCES \"person\" (\"id\") DEFERRABLE INITIALLY IMMEDIATE"
        );
    }

    #[test]
    fn a_column_can_be_made_to_reject_or_accept_nulls() {
        let change = SchemaChange::SetColumnNullable {
            table: "book".into(),
            column: "title".into(),
            nullable: false,
        };
        assert_eq!(
            render_ok(&change),
            "ALTER TABLE \"book\" ALTER COLUMN \"title\" SET NOT NULL"
        );
        let change = SchemaChange::SetColumnNullable {
            table: "book".into(),
            column: "title".into(),
            nullable: true,
        };
        assert_eq!(
            render_ok(&change),
            "ALTER TABLE \"book\" ALTER COLUMN \"title\" DROP NOT NULL"
        );
    }

    #[test]
    fn a_primary_key_can_be_set_on_a_table_that_has_none() {
        // Three statements in one: a table created without a key (which is every
        // table, per the goals) has no constraint to drop, and the columns are
        // made NOT NULL so the introspected column matches the declared field.
        let change = SchemaChange::SetPrimaryKey {
            table: "invoice".into(),
            columns: vec!["id".into()],
        };
        assert_eq!(
            render_ok(&change),
            "ALTER TABLE \"invoice\" DROP CONSTRAINT IF EXISTS \"invoice_pkey\"; \
             ALTER TABLE \"invoice\" ALTER COLUMN \"id\" SET NOT NULL; \
             ALTER TABLE \"invoice\" ADD PRIMARY KEY (\"id\")"
        );

        // A composite key is grown by replacing the key, not by adding to it.
        let change = SchemaChange::SetPrimaryKey {
            table: "member".into(),
            columns: vec!["org".into(), "user_id".into()],
        };
        assert!(render_ok(&change).ends_with("ADD PRIMARY KEY (\"org\", \"user_id\")"));

        // No columns is "this table has no key", which is the state every table
        // is created in and the one it returns to when the last key field stops
        // being one.
        let change = SchemaChange::SetPrimaryKey {
            table: "member".into(),
            columns: Vec::new(),
        };
        assert_eq!(
            render_ok(&change),
            "ALTER TABLE \"member\" DROP CONSTRAINT IF EXISTS \"member_pkey\""
        );
    }

    #[test]
    fn create_table_renders_column_default() {
        let change = SchemaChange::CreateTable {
            name: "t".into(),
            columns: vec![ColumnDef::new("active", "bool").not_null().default("true")],
            primary_key: vec![],
            unlogged: false,
        };
        assert_eq!(
            render_ok(&change),
            "CREATE TABLE \"t\" (\"active\" bool NOT NULL DEFAULT true)"
        );
    }

    #[test]
    fn create_table_rejects_pk_column_that_is_not_declared() {
        let change = SchemaChange::CreateTable {
            name: "t".into(),
            columns: vec![ColumnDef::new("a", "int8")],
            primary_key: vec!["b".into()],
            unlogged: false,
        };
        assert!(render(&PgDialect::new(), &change).is_err());
    }

    #[test]
    fn a_column_reference_drops_whatever_key_it_had_and_adds_the_new_one() {
        let sql = render_ok(&SchemaChange::SetColumnReference {
            table: "book".into(),
            column: "author".into(),
            references: Some(sc_db::ColumnRef {
                table: "writer".into(),
                column: "id".into(),
            }),
        });
        // The old key is found by the column it constrains, not by a name.
        assert!(sql.starts_with("DO $sc$"), "{sql}");
        assert!(sql.contains("to_regclass('\"book\"')"), "{sql}");
        assert!(sql.contains("a.attname = 'author'"), "{sql}");
        assert!(
            sql.ends_with(
                "; ALTER TABLE \"book\" ADD FOREIGN KEY (\"author\") REFERENCES \"writer\" \
                 (\"id\") DEFERRABLE INITIALLY IMMEDIATE"
            ),
            "{sql}"
        );

        // No target: the drop alone.
        let sql = render_ok(&SchemaChange::SetColumnReference {
            table: "book".into(),
            column: "author".into(),
            references: None,
        });
        assert!(sql.ends_with("END $sc$"), "{sql}");
        assert!(!sql.contains("ADD FOREIGN KEY"), "{sql}");
    }

    #[test]
    fn a_jointly_unique_constraint_is_a_table_level_constraint() {
        // The column-level `UNIQUE` a `ColumnDef` carries cannot say "these two
        // together", which is the whole of what this variant exists for.
        assert_eq!(
            render_ok(&SchemaChange::AddUniqueConstraint {
                table: "member".into(),
                name: "sc_uq_member_org_email".into(),
                columns: vec!["org".into(), "email".into()],
            }),
            "ALTER TABLE \"member\" ADD CONSTRAINT \"sc_uq_member_org_email\" \
             UNIQUE (\"org\", \"email\")"
        );
        assert_eq!(
            render_ok(&SchemaChange::DropConstraint {
                table: "member".into(),
                name: "sc_uq_member_org_email".into(),
                if_exists: true,
            }),
            "ALTER TABLE \"member\" DROP CONSTRAINT IF EXISTS \"sc_uq_member_org_email\""
        );
        // A constraint over no columns is a mistake by the caller building it,
        // caught here rather than as a syntax error from Postgres.
        assert!(
            render(
                &PgDialect::new(),
                &SchemaChange::AddUniqueConstraint {
                    table: "member".into(),
                    name: "sc_uq_member".into(),
                    columns: Vec::new(),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn an_index_is_over_columns_or_over_an_expression() {
        assert_eq!(
            render_ok(&SchemaChange::CreateIndex {
                table: "book".into(),
                name: "sc_ix_book_author".into(),
                on: IndexOn::Columns(vec!["author".into()]),
                method: None,
            }),
            "CREATE INDEX \"sc_ix_book_author\" ON \"book\" (\"author\")"
        );
        // The full-text form: an expression, and the access method that can
        // index one. Parenthesised whatever the expression is.
        assert_eq!(
            render_ok(&SchemaChange::CreateIndex {
                table: "book".into(),
                name: "sc_fts_book".into(),
                on: IndexOn::Expression("to_tsvector('english', coalesce(\"title\", ''))".into()),
                method: Some("gin".into()),
            }),
            "CREATE INDEX \"sc_fts_book\" ON \"book\" USING gin \
             ((to_tsvector('english', coalesce(\"title\", ''))))"
        );
        assert_eq!(
            render_ok(&SchemaChange::DropIndex {
                name: "sc_fts_book".into(),
                if_exists: true,
            }),
            "DROP INDEX IF EXISTS \"sc_fts_book\""
        );
    }

    #[test]
    fn a_comment_carries_the_metadata_and_null_takes_it_away() {
        // Where a constraint's error message and a row constraint's formula
        // live. The text is a string literal, so an apostrophe in an admin's
        // message doubles rather than ending it.
        assert_eq!(
            render_ok(&SchemaChange::SetComment {
                target: CommentTarget::Constraint {
                    table: "member".into(),
                    name: "sc_uq_member_org_email".into(),
                },
                comment: Some("that's taken".into()),
            }),
            "COMMENT ON CONSTRAINT \"sc_uq_member_org_email\" ON \"member\" IS 'that''s taken'"
        );
        assert_eq!(
            render_ok(&SchemaChange::SetComment {
                target: CommentTarget::Index {
                    name: "sc_fts_book".into()
                },
                comment: None,
            }),
            "COMMENT ON INDEX \"sc_fts_book\" IS NULL"
        );
        assert_eq!(
            render_ok(&SchemaChange::SetComment {
                target: CommentTarget::Trigger {
                    table: "emp".into(),
                    name: "sc_ck_emp_paid".into(),
                },
                comment: Some("{}".into()),
            }),
            "COMMENT ON TRIGGER \"sc_ck_emp_paid\" ON \"emp\" IS '{}'"
        );
    }

    #[test]
    fn drop_table_with_and_without_if_exists() {
        assert_eq!(
            render_ok(&SchemaChange::DropTable {
                name: "member".into(),
                if_exists: true,
            }),
            "DROP TABLE IF EXISTS \"member\""
        );
        assert_eq!(
            render_ok(&SchemaChange::DropTable {
                name: "member".into(),
                if_exists: false,
            }),
            "DROP TABLE \"member\""
        );
    }

    #[test]
    fn add_and_drop_column() {
        assert_eq!(
            render_ok(&SchemaChange::AddColumn {
                table: "member".into(),
                column: ColumnDef::new("note", "text"),
            }),
            "ALTER TABLE \"member\" ADD COLUMN \"note\" text"
        );
        assert_eq!(
            render_ok(&SchemaChange::DropColumn {
                table: "member".into(),
                column: "note".into(),
                if_exists: true,
            }),
            "ALTER TABLE \"member\" DROP COLUMN IF EXISTS \"note\""
        );
    }

    #[test]
    fn rename_column() {
        assert_eq!(
            render_ok(&SchemaChange::RenameColumn {
                table: "member".into(),
                from: "note".into(),
                to: "remark".into(),
            }),
            "ALTER TABLE \"member\" RENAME COLUMN \"note\" TO \"remark\""
        );
    }
}
