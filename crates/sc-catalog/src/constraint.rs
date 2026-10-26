//! Table constraints: jointly-unique keys, indexes, full-text indexes, vector
//! indexes and row constraints (design §5, §9; TODO Phase 2, TODO-agent-skills
//! AS2.2).
//!
//! ## They live in the database
//!
//! Everything else Saltcorn stores about a table is an *overlay* — a row in
//! `_fd_tables` adding what introspection cannot yield, under §9's rule that
//! nothing there may restate a fact the database already knows. A constraint is
//! such a fact. So there is no `_fd_constraints` table: a constraint is created
//! as the database object it *is*, read back by
//! [`introspect`](sc_db::DatabaseDriver::introspect) like the primary key and
//! the foreign keys, and presented here as a [`TableConstraint`]. Three
//! consequences, all of them the point:
//!
//! - a `UNIQUE` somebody added in `psql` shows up wherever Saltcorn's do,
//! - `pg_dump`/restore carries constraints without a metadata table to restore
//!   beside them,
//! - and there is no second copy to disagree with the first.
//!
//! ## What the database cannot hold, the comment holds
//!
//! Two things are Saltcorn's rather than Postgres's: the **error message** an
//! admin writes for a violated constraint, and the **formula** a row constraint
//! was generated from — a `plpgsql` body is not a formula, and re-deriving one
//! from the other is not a thing anybody should attempt. Both ride in the
//! object's comment as one JSON object under the [`META_KEY`] key
//! ([`ConstraintMeta`]). A comment is the natural home rather than a hiding
//! place: it is attached to the object, dropped with it, and carried by a dump.
//! A comment that is absent, is prose, or is JSON without our key leaves a
//! constraint that still works and is still listed — with no message and, for a
//! trigger, not recognised as a row constraint at all (see
//! [`TableConstraint::from_physical`]).
//!
//! ## A row constraint is a trigger
//!
//! GOALS requires constraint formulae to use join fields and aggregations, and a
//! `CHECK` constraint may not query another table — so a row constraint is one
//! `CONSTRAINT TRIGGER` over a `plpgsql` function that evaluates the *same*
//! `sc_query::Expr` the ownership translator produces, and raises the admin's
//! message. It is `DEFERRABLE INITIALLY IMMEDIATE` for the reason the foreign
//! keys already carry it (§13.1) — unchanged at the statement, deferrable to
//! commit by a caller holding the transaction, which is what lets rows that
//! point at each other arrive in a file's order — and it is `AFTER` because
//! that is what a deferrable trigger is.

use std::collections::BTreeSet;

use sc_db::{CommentTarget, IndexOn, PhysicalConstraint, PhysicalConstraintKind, SchemaChange};
use sc_error::{Error, Result};
use sc_expr::{Formula, Operation, SchemaShape, UserEnv};
use sc_query::{SqlDialect, render_policy_expr};
use sc_types::BasicType;
use serde_json::{Value as Json, json};

use crate::catalog::SchemaStep;
use crate::field::DataField;
use crate::projection::SchemaProjection;
use crate::table::Table;

/// The key a constraint's Saltcorn metadata sits under in the object's comment.
///
/// Nested under one key rather than spread across the comment so that "is this
/// comment ours?" has an answer: a comment somebody wrote in prose, or one
/// another tool left, parses as not-ours and is left alone.
pub const META_KEY: &str = "saltcorn_constraint";

/// The name prefix every constraint Saltcorn creates carries, by kind.
const UNIQUE_PREFIX: &str = "sc_uq_";
/// See [`UNIQUE_PREFIX`].
const INDEX_PREFIX: &str = "sc_ix_";
/// See [`UNIQUE_PREFIX`].
const FTS_PREFIX: &str = "sc_fts_";
/// See [`UNIQUE_PREFIX`].
const FORMULA_PREFIX: &str = "sc_ck_";
/// See [`UNIQUE_PREFIX`].
const VECTOR_PREFIX: &str = "sc_vx_";

/// The access method a vector index is built with: pgvector's HNSW graph, which
/// answers a nearest read without a training step (unlike `ivfflat`) and keeps
/// answering as rows are added.
pub const VECTOR_INDEX_METHOD: &str = "hnsw";

/// Postgres's identifier limit. A name longer than this is silently truncated by
/// the backend, which would make two long names one name.
const MAX_IDENT: usize = 63;

/// The dollar-quote tag the generated trigger function's body is wrapped in.
const BODY_TAG: &str = "$sc_constraint$";

/// One constraint on a table, as the catalog presents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableConstraint {
    /// The database object's name — the constraint's, the index's or the
    /// trigger's. Derived from what the constraint *is* (see
    /// [`TableConstraint::derived_name`]) for the ones Saltcorn creates, so
    /// adding the same one twice is refused by the name it already has.
    pub name: String,
    /// Which kind it is.
    pub kind: ConstraintKind,
    /// The message shown when it is violated, in the admin's own words. `None`
    /// leaves the database's own message, which names the constraint.
    pub error_message: Option<String>,
}

/// What a constraint constrains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintKind {
    /// These fields are **jointly** unique.
    Unique {
        /// The fields, in the order the key is declared.
        fields: Vec<String>,
    },
    /// An index, to make searching or joining on a field faster.
    ///
    /// Carries what the database reported rather than only what Saltcorn can
    /// create: the admin UI offers one field, but an index written by hand over
    /// two columns or over an expression is still an index, and hiding it would
    /// be hiding part of the schema.
    Index {
        /// The indexed fields, in index order; empty for an expression index.
        fields: Vec<String>,
        /// The indexed expression, for an index over one.
        expression: Option<String>,
        /// The access method (`btree`, `gin`, …).
        method: String,
    },
    /// A full-text search index over every text field of the table.
    FullTextSearch {
        /// The Postgres text-search configuration (`english`, `simple`, …).
        language: String,
    },
    /// A row constraint: a formula over the row that must be true of every row.
    Formula {
        /// The formula's source.
        formula: String,
    },
    /// An HNSW index over a vector field by cosine distance (TODO-agent-skills
    /// AS2.2), which is what a `nearest` read on Postgres uses instead of
    /// reading every row. pgvector only; SQLite has none.
    VectorIndex {
        /// The vector field.
        field: String,
    },
}

impl ConstraintKind {
    /// The word this kind is called by on the wire and on screen.
    pub fn type_name(&self) -> &'static str {
        match self {
            ConstraintKind::Unique { .. } => "unique",
            ConstraintKind::Index { .. } => "index",
            ConstraintKind::FullTextSearch { .. } => "full_text_search",
            ConstraintKind::Formula { .. } => "formula",
            ConstraintKind::VectorIndex { .. } => "vector_index",
        }
    }

    /// The fields this constraint names, which are the fields that may not be
    /// dropped while it exists.
    pub fn fields(&self) -> Vec<String> {
        match self {
            ConstraintKind::Unique { fields } => fields.clone(),
            ConstraintKind::Index { fields, .. } => fields.clone(),
            ConstraintKind::VectorIndex { field } => vec![field.clone()],
            // A full-text index is over *every* text field, and a formula's
            // fields are the formula's business (the schema editor asks it).
            ConstraintKind::FullTextSearch { .. } | ConstraintKind::Formula { .. } => Vec::new(),
        }
    }
}

/// The metadata a constraint carries in its comment: what the database has
/// nowhere to put.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConstraintMeta {
    /// The kind's [`ConstraintKind::type_name`], so a trigger can say it is a
    /// row constraint and not an application trigger somebody else wrote.
    kind: String,
    /// The formula source, for a row constraint.
    formula: Option<String>,
    /// The text-search configuration, for a full-text index.
    language: Option<String>,
    /// The admin's error message.
    error_message: Option<String>,
}

impl ConstraintMeta {
    /// The comment text this metadata is stored as.
    fn to_comment(&self) -> String {
        let mut body = json!({ "kind": self.kind });
        for (key, value) in [
            ("formula", &self.formula),
            ("language", &self.language),
            ("error_message", &self.error_message),
        ] {
            if let Some(value) = value
                && let Some(obj) = body.as_object_mut()
            {
                obj.insert(key.to_owned(), Json::String(value.clone()));
            }
        }
        json!({ META_KEY: body }).to_string()
    }

    /// The metadata in `comment`, or `None` when the comment is absent, is not
    /// JSON, or is JSON somebody else wrote.
    fn from_comment(comment: Option<&str>) -> Option<ConstraintMeta> {
        let parsed: Json = serde_json::from_str(comment?).ok()?;
        let body = parsed.get(META_KEY)?;
        let text = |key: &str| {
            body.get(key)
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Some(ConstraintMeta {
            kind: text("kind")?,
            formula: text("formula"),
            language: text("language"),
            error_message: text("error_message"),
        })
    }
}

impl TableConstraint {
    /// A constraint with no error message — the base to add one to.
    pub fn new(name: impl Into<String>, kind: ConstraintKind) -> TableConstraint {
        TableConstraint {
            name: name.into(),
            kind,
            error_message: None,
        }
    }

    /// Set the message shown when this constraint is violated.
    pub fn message(mut self, message: impl Into<String>) -> TableConstraint {
        let message = message.into();
        self.error_message = Some(message).filter(|m| !m.trim().is_empty());
        self
    }

    /// The database object's name for a constraint on `table`, derived from what
    /// it is (decision 9 of the milestone list).
    ///
    /// Derived rather than asked for, because it makes "the same constraint
    /// twice" a name collision the database refuses instead of two constraints
    /// enforcing one rule. A row constraint is the exception: its identity
    /// cannot be derived from its fields, so the admin gives it a short `name`
    /// and this prefixes it.
    pub fn derived_name(table: &str, kind: &ConstraintKind, given: &str) -> String {
        let raw = match kind {
            ConstraintKind::Unique { fields } => {
                format!("{UNIQUE_PREFIX}{table}_{}", fields.join("_"))
            }
            ConstraintKind::Index { fields, .. } => {
                format!("{INDEX_PREFIX}{table}_{}", fields.join("_"))
            }
            ConstraintKind::FullTextSearch { .. } => format!("{FTS_PREFIX}{table}"),
            ConstraintKind::Formula { .. } => format!("{FORMULA_PREFIX}{table}_{given}"),
            ConstraintKind::VectorIndex { field } => format!("{VECTOR_PREFIX}{table}_{field}"),
        };
        truncate_ident(&raw)
    }

    /// Whether this is a constraint Saltcorn created, which is what makes it
    /// safe to drop by the route that created it.
    pub fn is_saltcorn(&self) -> bool {
        [
            UNIQUE_PREFIX,
            INDEX_PREFIX,
            FTS_PREFIX,
            FORMULA_PREFIX,
            VECTOR_PREFIX,
        ]
        .iter()
        .any(|p| self.name.starts_with(p))
    }

    /// Read a constraint back from what introspection found, or `None` when the
    /// physical object is not one.
    ///
    /// The only thing that is *not* taken at face value is a trigger: an index
    /// is an index whoever made it, but a trigger is a row constraint **only if
    /// its comment says so**. A trigger is how the rest of the world implements
    /// auditing, denormalisation and half of everything else, and listing
    /// somebody's audit trigger as a constraint an admin may delete would be
    /// listing it as something it is not.
    pub fn from_physical(physical: &PhysicalConstraint) -> Option<TableConstraint> {
        let meta = ConstraintMeta::from_comment(physical.comment.as_deref());
        let error_message = meta.as_ref().and_then(|m| m.error_message.clone());
        let kind = match &physical.kind {
            PhysicalConstraintKind::Unique { columns } => ConstraintKind::Unique {
                fields: columns.clone(),
            },
            PhysicalConstraintKind::Index {
                columns,
                expression,
                method,
            } => match meta.as_ref().and_then(|m| m.language.clone()) {
                // A full-text index is one Saltcorn made and said so: the
                // expression alone would not say which language it used, and
                // guessing would be guessing at the one thing that decides
                // whether the index answers a search at all.
                Some(language) => ConstraintKind::FullTextSearch { language },
                // An HNSW index is over a vector by construction: it is the
                // only thing pgvector's method indexes.
                None if method == VECTOR_INDEX_METHOD && columns.len() == 1 => {
                    ConstraintKind::VectorIndex {
                        field: columns[0].clone(),
                    }
                }
                None => ConstraintKind::Index {
                    fields: columns.clone(),
                    expression: expression.clone(),
                    method: method.clone(),
                },
            },
            PhysicalConstraintKind::RowTrigger => ConstraintKind::Formula {
                formula: meta.as_ref()?.formula.clone()?,
            },
        };
        Some(TableConstraint {
            name: physical.name.clone(),
            kind,
            error_message,
        })
    }

    /// The metadata this constraint stores in its comment, or `None` when it has
    /// nothing to store — an ordinary index with no message needs no comment,
    /// and an empty comment is a comment somebody has to wonder about.
    fn meta(&self) -> Option<ConstraintMeta> {
        let (formula, language) = match &self.kind {
            ConstraintKind::Formula { formula } => (Some(formula.clone()), None),
            ConstraintKind::FullTextSearch { language } => (None, Some(language.clone())),
            _ => (None, None),
        };
        if formula.is_none() && language.is_none() && self.error_message.is_none() {
            return None;
        }
        Some(ConstraintMeta {
            kind: self.kind.type_name().to_owned(),
            formula,
            language,
            error_message: self.error_message.clone(),
        })
    }

    /// Where this constraint's comment goes: on the constraint, on the index or
    /// on the trigger.
    fn comment_target(&self, table: &str) -> CommentTarget {
        match &self.kind {
            ConstraintKind::Unique { .. } => CommentTarget::Constraint {
                table: table.to_owned(),
                name: self.name.clone(),
            },
            ConstraintKind::Index { .. }
            | ConstraintKind::FullTextSearch { .. }
            | ConstraintKind::VectorIndex { .. } => CommentTarget::Index {
                name: self.name.clone(),
            },
            ConstraintKind::Formula { .. } => CommentTarget::Trigger {
                table: table.to_owned(),
                name: self.name.clone(),
            },
        }
    }
}

/// The steps that create `constraint` on `table`, ready to join a schema batch's
/// transaction.
///
/// Everything structural is a [`SchemaChange`] the driver renders; the row
/// constraint's trigger is [`SchemaStep::Sql`], exactly as the RLS policies are
/// and for the identical reason — it carries an arbitrary boolean expression,
/// which is what `SchemaChange` deliberately does not model.
pub fn create_constraint_steps(
    dialect: &dyn SqlDialect,
    projection: &SchemaProjection,
    table: &Table,
    constraint: &TableConstraint,
) -> Result<Vec<SchemaStep>> {
    let mut steps = match &constraint.kind {
        ConstraintKind::Unique { fields } => {
            vec![SchemaStep::Change(SchemaChange::AddUniqueConstraint {
                table: table.name.clone(),
                name: constraint.name.clone(),
                columns: fields.clone(),
            })]
        }
        ConstraintKind::Index { fields, .. } => {
            vec![SchemaStep::Change(SchemaChange::CreateIndex {
                table: table.name.clone(),
                name: constraint.name.clone(),
                on: IndexOn::Columns(fields.clone()),
                method: None,
            })]
        }
        ConstraintKind::FullTextSearch { language } if dialect.is_sqlite() => {
            vec![SchemaStep::Sql(fts5_create_sql(
                dialect,
                table,
                &constraint.name,
                language,
            )?)]
        }
        ConstraintKind::FullTextSearch { language } => {
            vec![SchemaStep::Change(SchemaChange::CreateIndex {
                table: table.name.clone(),
                name: constraint.name.clone(),
                on: IndexOn::Expression(full_text_expression(dialect, table, language)?),
                method: Some("gin".to_owned()),
            })]
        }
        ConstraintKind::Formula { formula } => {
            vec![SchemaStep::Sql(row_constraint_sql(
                dialect, projection, table, constraint, formula,
            )?)]
        }
        ConstraintKind::VectorIndex { field } => {
            vec![SchemaStep::Sql(vector_index_sql(
                dialect,
                table,
                &constraint.name,
                field,
            )?)]
        }
    };
    if let Some(meta) = constraint.meta() {
        steps.push(SchemaStep::Change(SchemaChange::SetComment {
            target: constraint.comment_target(&table.name),
            comment: Some(meta.to_comment()),
        }));
    }
    Ok(steps)
}

/// The steps that drop `constraint` from `table`.
///
/// The comment needs no step of its own: Postgres drops it with the object it is
/// on, which is half of why a comment is the right place for the metadata.
pub fn drop_constraint_steps(
    dialect: &dyn SqlDialect,
    table_name: &str,
    constraint: &TableConstraint,
) -> Vec<SchemaStep> {
    match &constraint.kind {
        ConstraintKind::Unique { .. } => vec![SchemaStep::Change(SchemaChange::DropConstraint {
            table: table_name.to_owned(),
            name: constraint.name.clone(),
            if_exists: true,
        })],
        // An FTS5 table is not an index and keeps its comment in a table of the
        // driver's, which a dropped virtual table does not take with it.
        ConstraintKind::FullTextSearch { .. } if dialect.is_sqlite() => vec![
            SchemaStep::Sql(fts5_drop_sql(dialect, &constraint.name)),
            SchemaStep::Change(SchemaChange::SetComment {
                target: constraint.comment_target(table_name),
                comment: None,
            }),
        ],
        ConstraintKind::Index { .. }
        | ConstraintKind::FullTextSearch { .. }
        | ConstraintKind::VectorIndex { .. } => {
            vec![SchemaStep::Change(SchemaChange::DropIndex {
                name: constraint.name.clone(),
                if_exists: true,
            })]
        }
        // The function goes with the trigger, or a re-created constraint of the
        // same name would silently keep the *old* formula's body.
        ConstraintKind::Formula { .. } => vec![SchemaStep::Sql(format!(
            "DROP TRIGGER IF EXISTS {trigger} ON {table};\n\
             DROP FUNCTION IF EXISTS {function}();\n",
            trigger = dialect.quote_ident(&constraint.name),
            table = dialect.quote_ident(table_name),
            function = dialect.quote_ident(&function_name(&constraint.name)),
        ))],
    }
}

/// `CREATE INDEX … USING hnsw (field vector_cosine_ops)`: the index a nearest
/// read by cosine distance uses. Raw SQL rather than a
/// [`SchemaChange::CreateIndex`], because the operator class is part of the
/// column list and that change has nowhere to say one.
fn vector_index_sql(
    dialect: &dyn SqlDialect,
    table: &Table,
    name: &str,
    field: &str,
) -> Result<String> {
    if dialect.is_sqlite() {
        return Err(Error::invalid(format!(
            "a vector index needs PostgreSQL with pgvector; on SQLite a nearest read over \
             `{}` reads every row instead, which it does for tables of up to {} rows",
            table.name,
            crate::VECTOR_SCAN_LIMIT
        )));
    }
    let Some(found) = table.field(field) else {
        return Err(Error::invalid(format!(
            "`{}` has no field `{field}` to index",
            table.name
        )));
    };
    if !matches!(found.base.type_.as_basic(), Some(BasicType::Vector(n)) if *n > 0) {
        return Err(Error::invalid(format!(
            "a vector index is over a vector field, and `{}.{field}` is a {}",
            table.name,
            found.base.type_.name()
        )));
    }
    Ok(format!(
        "CREATE INDEX {} ON {} USING {VECTOR_INDEX_METHOD} ({} vector_cosine_ops);\n",
        dialect.quote_ident(name),
        dialect.quote_ident(&table.name),
        dialect.quote_ident(field),
    ))
}

/// The table's vector index on `field`, if it has one.
pub fn vector_index<'t>(table: &'t Table, field: &str) -> Option<&'t TableConstraint> {
    table
        .constraints
        .iter()
        .find(|c| matches!(&c.kind, ConstraintKind::VectorIndex { field: f } if f == field))
}

/// The columns a full-text index covers: every stored text field of the table,
/// in field order, except a secret — its column holds ciphertext, and a search
/// that matched it would be matching nothing anybody wrote.
///
/// One definition, read by the index's DDL and by every search that hopes to
/// use the index: the two must name the same columns in the same order.
pub fn full_text_columns(table: &Table) -> Vec<String> {
    table
        .fields
        .iter()
        .filter(|f| is_full_text_field(f))
        .map(|f| f.base.name.clone())
        .collect()
}

/// Whether full-text search reads `field`: a stored text column — `text`, or
/// a rich type stored as text, such as `string` — that is neither calculated
/// nor a secret.
pub fn is_full_text_field(field: &DataField) -> bool {
    !field.is_calc()
        && !field.is_secret()
        && BasicType::from_sql_type(field.base.type_.sql_type()) == BasicType::Text
}

/// The `to_tsvector` expression a full-text index is over — **one** definition
/// ([`sc_query::text_search_document`]), because an index and the search that
/// hopes to use it must be the same expression or the index is simply never
/// used.
pub fn full_text_expression(
    dialect: &dyn SqlDialect,
    table: &Table,
    language: &str,
) -> Result<String> {
    let columns = full_text_columns(table);
    if columns.is_empty() {
        return Err(no_text_fields(table));
    }
    check_language(language)?;
    sc_query::text_search_document(dialect, None, &columns, language)
}

fn no_text_fields(table: &Table) -> Error {
    Error::invalid(format!(
        "table `{}` has no text fields to index for full-text search",
        table.name
    ))
}

/// The table's full-text constraint, if it has one.
pub fn full_text_constraint(table: &Table) -> Option<(&TableConstraint, &str)> {
    table.constraints.iter().find_map(|c| match &c.kind {
        ConstraintKind::FullTextSearch { language } => Some((c, language.as_str())),
        _ => None,
    })
}

/// A search for `phrase` over `columns` of `table` that its full-text constraint
/// answers, or `None` when it has none (TODO-agent-skills AS1.1).
///
/// The value is the same on both backends: Postgres reads the document from the
/// columns (and uses the index when they are all of [`full_text_columns`]),
/// SQLite reads the FTS5 table the constraint keeps under its own name.
pub fn indexed_text_search(
    table: &Table,
    columns: Vec<String>,
    phrase: &str,
) -> Option<sc_query::TextSearch> {
    let (constraint, language) = full_text_constraint(table)?;
    let fts5 = match table.primary_key.as_slice() {
        [key] => Some(sc_query::Fts5Index {
            name: constraint.name.clone(),
            key: key.clone(),
            columns: full_text_columns(table),
        }),
        _ => None,
    };
    Some(sc_query::TextSearch {
        table: table.name.clone(),
        columns,
        language: language.to_owned(),
        phrase: phrase.to_owned(),
        fts5,
    })
}

/// The SQLite counterpart of the Postgres full-text index: an FTS5 table over
/// the table's text columns, kept in step by three triggers.
///
/// *External content* (`content=`): the FTS5 table holds the index and not a
/// second copy of the text, and its rowids are the table's integer key — which
/// is why one is required. A rowid that is not the key is renumbered by every
/// table rebuild SQLite's `ALTER TABLE` makes necessary, and an index keyed by
/// numbers that moved points at the wrong rows.
///
/// Everything is dropped first, so creating it again — what a change to the
/// table's text fields does — starts from nothing, and so does one whose table
/// was dropped from under it.
fn fts5_create_sql(
    dialect: &dyn SqlDialect,
    table: &Table,
    name: &str,
    language: &str,
) -> Result<String> {
    let columns = full_text_columns(table);
    if columns.is_empty() {
        return Err(no_text_fields(table));
    }
    check_language(language)?;
    let key = match table.primary_key.as_slice() {
        [key]
            if table
                .field(key)
                .is_some_and(|f| f.base.type_.as_basic() == Some(&BasicType::Int)) =>
        {
            key.clone()
        }
        _ => {
            return Err(Error::invalid(format!(
                "a full-text search index on SQLite needs a single integer primary key, which \
                 `{}` does not have",
                table.name
            )));
        }
    };
    let q = |ident: &str| dialect.quote_ident(ident);
    let fts = q(name);
    let quoted = |prefix: &str| -> String {
        columns
            .iter()
            .map(|c| format!("{prefix}{}", q(c)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (cols, new_vals, old_vals) = (quoted(""), quoted("new."), quoted("old."));
    let (key_q, tbl) = (q(&key), q(&table.name));
    let insert = format!("INSERT INTO {fts} (rowid, {cols}) VALUES (new.{key_q}, {new_vals});");
    let delete = format!(
        "INSERT INTO {fts} ({fts}, rowid, {cols}) VALUES ('delete', old.{key_q}, {old_vals});"
    );
    let mut sql = fts5_drop_sql(dialect, name);
    sql.push_str(&format!(
        "CREATE VIRTUAL TABLE {fts} USING fts5({cols}, content={content}, \
         content_rowid={rowid}, tokenize={tokenize});\n\
         CREATE TRIGGER {ai} AFTER INSERT ON {tbl} BEGIN {insert} END;\n\
         CREATE TRIGGER {ad} AFTER DELETE ON {tbl} BEGIN {delete} END;\n\
         CREATE TRIGGER {au} AFTER UPDATE ON {tbl} BEGIN {delete} {insert} END;\n\
         INSERT INTO {fts} ({fts}) VALUES ('rebuild');\n",
        content = dialect.quote_literal(&table.name),
        rowid = dialect.quote_literal(&key),
        tokenize = dialect.quote_literal(sc_query::search::fts5_tokenizer(language)),
        ai = q(&format!("{name}_ai")),
        ad = q(&format!("{name}_ad")),
        au = q(&format!("{name}_au")),
    ));
    Ok(sql)
}

/// Drop a SQLite full-text index: its triggers, then its FTS5 table.
fn fts5_drop_sql(dialect: &dyn SqlDialect, name: &str) -> String {
    let mut sql = String::new();
    for suffix in ["ai", "ad", "au"] {
        sql.push_str(&format!(
            "DROP TRIGGER IF EXISTS {};\n",
            dialect.quote_ident(&format!("{name}_{suffix}"))
        ));
    }
    sql.push_str(&format!(
        "DROP TABLE IF EXISTS {};\n",
        dialect.quote_ident(name)
    ));
    sql
}

/// A text-search configuration name is an identifier, not a string a caller
/// composes: it is inlined into DDL that cannot be parameterised, so anything
/// but a plain lowercase name is refused rather than escaped.
fn check_language(language: &str) -> Result<()> {
    if sc_query::search::is_language_name(language) {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "`{language}` is not a text-search configuration name"
        )))
    }
}

/// The `plpgsql` function and the constraint trigger that enforce a row
/// constraint.
///
/// The shape, and why each part of it:
///
/// ```sql
/// SELECT (<formula>) INTO ok FROM (SELECT (NEW).*) AS "<table>";
/// ```
///
/// The derived table gives the row being written the table's own name, so the
/// translated expression — whose column references are qualified with it, and
/// whose aggregations correlate to it — needs no rewriting at all: it is
/// **character for character** the expression the ownership translator would
/// produce for the same formula. That is what keeps a formula's meaning one
/// thing whether it is enforced by a policy, evaluated in the runtime, or
/// checked by this trigger.
fn row_constraint_sql(
    dialect: &dyn SqlDialect,
    projection: &SchemaProjection,
    table: &Table,
    constraint: &TableConstraint,
    formula: &str,
) -> Result<String> {
    let parsed = Formula::parse(formula)?;
    let shape = projection.shape();
    let calc = table.calc_formulas();
    let expr = sc_expr::translate(
        &parsed,
        // Any operation: a constraint formula that used the operation flags is
        // refused when it is saved (they are two questions — "is this row
        // allowed" and "who is writing it" — and a trigger can only answer the
        // first), so the fold cannot change what this expression says.
        Operation::Insert,
        &sc_expr::Env::new(&UserEnv::Inline(None)).with_calc(&calc),
        &shape,
        &table.name,
    )
    .map_err(Error::from)?;
    let predicate = render_policy_expr(dialect, &expr)?;

    let message = constraint.error_message.clone().unwrap_or_else(|| {
        format!(
            "row constraint `{}` on `{}` is not satisfied",
            constraint.name, table.name
        )
    });
    let function = dialect.quote_ident(&function_name(&constraint.name));
    let trigger = dialect.quote_ident(&constraint.name);
    let quoted_table = dialect.quote_ident(&table.name);

    let body = format!(
        "\nDECLARE sc_ok boolean;\n\
         BEGIN\n  \
           SELECT ({predicate}) INTO sc_ok FROM (SELECT (NEW).*) AS {quoted_table};\n  \
           IF sc_ok IS DISTINCT FROM true THEN\n    \
             RAISE EXCEPTION '%', {message} USING ERRCODE = 'check_violation', \
             CONSTRAINT = {constraint_name};\n  \
           END IF;\n  \
           RETURN NULL;\n\
         END\n",
        message = quote_literal(&message),
        // The constraint's own name in the error's `CONSTRAINT` field, which is
        // what lets the write path know *which* rule refused a row — the same
        // field Postgres fills in for a unique violation, so one lookup serves
        // both (§13.1).
        constraint_name = quote_literal(&constraint.name),
    );
    // The body is dollar-quoted, so a tag appearing inside it would end the
    // quote early and turn the rest of the function into a syntax error at best.
    // A formula would have to contain the tag as a *string literal* to do it,
    // which is exotic enough to refuse rather than to work around.
    if body.contains(BODY_TAG) {
        return Err(Error::invalid(format!(
            "the formula of `{}` contains `{BODY_TAG}`, which the generated trigger \
             cannot quote",
            constraint.name
        )));
    }

    Ok(format!(
        "CREATE OR REPLACE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS \
         {BODY_TAG}{body}{BODY_TAG};\n\
         DROP TRIGGER IF EXISTS {trigger} ON {quoted_table};\n\
         CREATE CONSTRAINT TRIGGER {trigger} AFTER INSERT OR UPDATE ON {quoted_table} \
         DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION {function}();\n"
    ))
}

/// The name of the function behind a row constraint's trigger — the trigger's
/// name with a suffix, so the pair is findable from either half and a table's
/// functions are as readable as its triggers.
fn function_name(constraint: &str) -> String {
    truncate_ident(&format!("{constraint}_fn"))
}

/// A SQL string literal holding `value`.
fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Fit `name` into Postgres's identifier limit **without** letting two long
/// names become one.
///
/// Postgres truncates a long identifier silently, so two unique constraints over
/// fields whose names share a 63-byte prefix would be one constraint — and the
/// second would be refused as a duplicate of a constraint that is not the same
/// rule. A hash of the full name in the last eight bytes makes that impossible;
/// FNV-1a rather than the standard hasher because the value has to be the same
/// in every build and every process.
fn truncate_ident(name: &str) -> String {
    if name.len() <= MAX_IDENT {
        return name.to_owned();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let suffix = format!("_{:07x}", hash & 0xfff_ffff);
    let keep = MAX_IDENT - suffix.len();
    // Cut on a character boundary: an identifier may be non-ASCII, and half a
    // character is not a name.
    let mut cut = keep;
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}", &name[..cut])
}

/// The constraint whose violation Postgres is reporting in `message`, out of
/// `constraints` — the lookup that turns a database error into the admin's own
/// words (§13.1).
///
/// Matched on the name in the message rather than on a SQLSTATE, for the reason
/// [`rls`](crate::rls)'s policy-violation mapping is: by the time an error
/// reaches here it is a formatted chain, and the name is the part of it that
/// identifies *which* rule was broken. The name is bounded by quotes in
/// Postgres's message (`… violates unique constraint "sc_uq_x"`), so a
/// constraint whose name is a prefix of another's cannot be mistaken for it.
pub fn violated_constraint<'a>(
    constraints: &'a [TableConstraint],
    message: &str,
) -> Option<&'a TableConstraint> {
    constraints
        .iter()
        .find(|c| message.contains(&format!("\"{}\"", c.name)))
}

/// The fields of its own table that a row constraint's formula reads.
///
/// Parsed rather than searched for as text: a field called `id` appears inside
/// `valid`, and refusing to drop a column because another column's name contains
/// its name is a refusal nobody can act on. A formula that no longer parses
/// reads nothing — its trigger is already broken, and a drop is as likely to be
/// part of the repair as part of the damage.
pub fn formula_fields(
    projection: &SchemaProjection,
    table_name: &str,
    formula: &str,
) -> BTreeSet<String> {
    let Ok(parsed) = Formula::parse(formula) else {
        return BTreeSet::new();
    };
    match parsed.validate(&projection.shape(), table_name) {
        Ok(analysis) => analysis.fields,
        Err(_) => BTreeSet::new(),
    }
}

/// Every field the constraints of a table name, so a field drop can be refused
/// by the constraint that needs it.
pub fn constrained_fields(constraints: &[TableConstraint]) -> BTreeSet<String> {
    constraints
        .iter()
        .flat_map(|c| c.kind.fields())
        .collect::<BTreeSet<_>>()
}

/// The shape a row constraint's formula is validated against — exposed so the
/// schema editor validates with the same rules the trigger is generated under.
pub fn validate_formula(
    projection: &SchemaProjection,
    table_name: &str,
    formula: &str,
) -> Result<()> {
    let parsed = Formula::parse(formula)?;
    let shape: SchemaShape = projection.shape();
    let analysis = parsed.validate(&shape, table_name)?;
    // Refused **by name**, both of them, because both are questions a trigger
    // cannot answer: it has no request, so `user` is null in the database and
    // whatever the session says in the admin UI, and it fires on insert and
    // update alike, so `_insert` would have to be two different constants at
    // once.
    if analysis.uses_caller() {
        return Err(Error::invalid(
            "a row constraint cannot use `user` or `session`: it is checked by the database, \
             which has no session. Put the rule in the table's ownership formula instead.",
        ));
    }
    if !analysis.flags.is_empty() {
        return Err(Error::invalid(
            "a row constraint cannot use the operation flags (`_insert`, `_update`, …): it \
             is checked the same way on every write.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::{DataField, DbId};
    use sc_query::SqlDialect;
    use sc_types::{BasicType, TypeRef};

    struct Pg;

    impl SqlDialect for Pg {
        fn quote_ident(&self, ident: &str) -> String {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
        fn placeholder(&self, position: usize) -> String {
            format!("${position}")
        }
    }

    fn employees() -> Table {
        Table::projected(
            DbId::primary(),
            "employees",
            vec![
                DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key(),
                DataField::plain("name", TypeRef::Basic(BasicType::Text)),
                DataField::plain("note", TypeRef::Basic(BasicType::Text)),
                DataField::plain("salary", TypeRef::Basic(BasicType::Int)),
            ],
            vec!["id".into()],
        )
    }

    fn projection_of(tables: Vec<Table>) -> SchemaProjection {
        SchemaProjection::new(tables)
    }

    #[test]
    fn a_name_says_what_the_constraint_is() {
        assert_eq!(
            TableConstraint::derived_name(
                "member",
                &ConstraintKind::Unique {
                    fields: vec!["org".into(), "email".into()],
                },
                "",
            ),
            "sc_uq_member_org_email"
        );
        assert_eq!(
            TableConstraint::derived_name(
                "member",
                &ConstraintKind::Index {
                    fields: vec!["email".into()],
                    expression: None,
                    method: "btree".into(),
                },
                "",
            ),
            "sc_ix_member_email"
        );
        assert_eq!(
            TableConstraint::derived_name(
                "member",
                &ConstraintKind::FullTextSearch {
                    language: "english".into(),
                },
                "",
            ),
            "sc_fts_member"
        );
        assert_eq!(
            TableConstraint::derived_name(
                "member",
                &ConstraintKind::Formula {
                    formula: "salary > 0".into(),
                },
                "paid",
            ),
            "sc_ck_member_paid"
        );
    }

    #[test]
    fn two_long_names_do_not_become_one_name() {
        // Postgres truncates a long identifier silently, which would make two
        // different rules one constraint — and the second one "already exists".
        let long = "l".repeat(40);
        let a = TableConstraint::derived_name(
            &long,
            &ConstraintKind::Unique {
                fields: vec![long.clone(), "a".into()],
            },
            "",
        );
        let b = TableConstraint::derived_name(
            &long,
            &ConstraintKind::Unique {
                fields: vec![long.clone(), "b".into()],
            },
            "",
        );
        assert_eq!(a.len(), MAX_IDENT);
        assert_eq!(b.len(), MAX_IDENT);
        assert_ne!(a, b);
        // …and the same input still gives the same name, or "does this already
        // exist" would be unanswerable.
        assert_eq!(
            a,
            TableConstraint::derived_name(
                &long,
                &ConstraintKind::Unique {
                    fields: vec![long.clone(), "a".into()],
                },
                "",
            )
        );
    }

    #[test]
    fn metadata_round_trips_through_a_comment() {
        let constraint = TableConstraint::new(
            "sc_ck_employees_paid",
            ConstraintKind::Formula {
                formula: "salary > 0".into(),
            },
        )
        .message("we don't work for free");
        let comment = constraint.meta().expect("has metadata").to_comment();
        let back = TableConstraint::from_physical(&PhysicalConstraint {
            name: "sc_ck_employees_paid".into(),
            kind: PhysicalConstraintKind::RowTrigger,
            comment: Some(comment),
        })
        .expect("recognised");
        assert_eq!(back, constraint);
    }

    #[test]
    fn a_comment_that_is_not_ours_leaves_the_constraint_working_and_unnamed() {
        // A `UNIQUE` created in `psql`, with somebody's note on it: still a
        // constraint, still listed, simply with no message of ours.
        for comment in [None, Some("legacy, do not remove"), Some("{\"other\":1}")] {
            let read = TableConstraint::from_physical(&PhysicalConstraint {
                name: "member_org_email_key".into(),
                kind: PhysicalConstraintKind::Unique {
                    columns: vec!["org".into()],
                },
                comment: comment.map(str::to_owned),
            })
            .expect("a unique constraint is one whoever made it");
            assert_eq!(read.error_message, None);
            assert!(!read.is_saltcorn());
        }

        // A trigger, though, is a row constraint **only if it says so**:
        // somebody's audit trigger is not a rule an admin may delete here.
        assert_eq!(
            TableConstraint::from_physical(&PhysicalConstraint {
                name: "audit_employees".into(),
                kind: PhysicalConstraintKind::RowTrigger,
                comment: Some("keeps the audit log".into()),
            }),
            None
        );
    }

    #[test]
    fn a_row_constraint_is_a_deferrable_constraint_trigger_over_the_new_row() {
        let table = employees();
        let projection = projection_of(vec![table.clone()]);
        let constraint = TableConstraint::new(
            "sc_ck_employees_paid",
            ConstraintKind::Formula {
                formula: "salary > 0".into(),
            },
        )
        .message("we don't work for free");
        let steps = create_constraint_steps(&Pg, &projection, &table, &constraint).expect("steps");
        let sql = match &steps[0] {
            SchemaStep::Sql(sql) => sql.clone(),
            other => panic!("expected generated SQL, got {other:?}"),
        };

        // The row being written is given the table's own name, so the
        // translated expression needs no rewriting: `"employees"."salary"` is
        // what the ownership translator produces for the same formula.
        assert!(
            sql.contains(
                "SELECT ((\"employees\".\"salary\" > 0)) INTO sc_ok \
                 FROM (SELECT (NEW).*) AS \"employees\""
            ),
            "{sql}"
        );
        // Deferrable, so a CSV import may put the check off to commit — and
        // therefore AFTER, which is what a constraint trigger is.
        assert!(
            sql.contains(
                "CREATE CONSTRAINT TRIGGER \"sc_ck_employees_paid\" AFTER INSERT OR UPDATE \
                 ON \"employees\" DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW"
            ),
            "{sql}"
        );
        // The admin's message, with its apostrophe doubled rather than ending
        // the literal, and passed as a `%` argument so a `%` in it is text.
        assert!(
            sql.contains(
                "RAISE EXCEPTION '%', 'we don''t work for free' USING ERRCODE = \
                 'check_violation', CONSTRAINT = 'sc_ck_employees_paid'"
            ),
            "{sql}"
        );
        // …and the comment that carries the formula back.
        match &steps[1] {
            SchemaStep::Change(SchemaChange::SetComment { comment, .. }) => {
                assert!(
                    comment
                        .as_deref()
                        .unwrap_or_default()
                        .contains("salary > 0")
                );
            }
            other => panic!("expected the metadata comment, got {other:?}"),
        }
    }

    #[test]
    fn a_row_constraint_with_no_message_names_itself() {
        let table = employees();
        let projection = projection_of(vec![table.clone()]);
        let constraint = TableConstraint::new(
            "sc_ck_employees_paid",
            ConstraintKind::Formula {
                formula: "salary > 0".into(),
            },
        );
        let steps = create_constraint_steps(&Pg, &projection, &table, &constraint).expect("steps");
        let SchemaStep::Sql(sql) = &steps[0] else {
            panic!("expected generated SQL")
        };
        assert!(
            sql.contains("row constraint `sc_ck_employees_paid` on `employees` is not satisfied"),
            "{sql}"
        );
    }

    #[test]
    fn the_full_text_expression_is_every_text_field_coalesced() {
        let table = employees();
        let sql = full_text_expression(&Pg, &table, "english").expect("expression");
        assert_eq!(
            sql,
            "to_tsvector('english'::regconfig, coalesce(\"name\", '') || ' ' || \
             coalesce(\"note\", ''))"
        );
        // A configuration name is an identifier inlined into DDL, so anything
        // that is not one is refused rather than escaped.
        assert!(full_text_expression(&Pg, &table, "english'; drop table x --").is_err());
        // …and a table with nothing to index says so instead of creating an
        // index over the empty string.
        let no_text = Table::projected(
            DbId::primary(),
            "readings",
            vec![DataField::plain("value", TypeRef::Basic(BasicType::Int))],
            Vec::new(),
        );
        assert!(full_text_expression(&Pg, &no_text, "english").is_err());
    }

    #[test]
    fn dropping_a_row_constraint_takes_its_function_with_it() {
        // Or a constraint of the same name created later would run the old
        // formula's body — the one failure of this design that would be silent.
        let constraint = TableConstraint::new(
            "sc_ck_employees_paid",
            ConstraintKind::Formula {
                formula: "salary > 0".into(),
            },
        );
        let steps = drop_constraint_steps(&Pg, "employees", &constraint);
        let SchemaStep::Sql(sql) = &steps[0] else {
            panic!("expected generated SQL")
        };
        assert!(sql.contains("DROP TRIGGER IF EXISTS \"sc_ck_employees_paid\" ON \"employees\""));
        assert!(sql.contains("DROP FUNCTION IF EXISTS \"sc_ck_employees_paid_fn\"()"));
    }

    #[test]
    fn a_formula_that_asks_a_question_a_trigger_cannot_answer_is_refused_by_name() {
        let projection = projection_of(vec![employees()]);
        assert!(validate_formula(&projection, "employees", "salary > 0").is_ok());

        let err = validate_formula(&projection, "employees", "user.id === id").unwrap_err();
        assert!(err.to_string().contains("user"), "{err}");
        let err = validate_formula(&projection, "employees", "_insert || salary > 0").unwrap_err();
        assert!(err.to_string().contains("_insert"), "{err}");
    }

    #[test]
    fn the_violated_constraint_is_the_one_the_message_names() {
        let constraints = vec![
            TableConstraint::new(
                "sc_uq_member_org",
                ConstraintKind::Unique {
                    fields: vec!["org".into()],
                },
            )
            .message("one per org"),
            // A name that is a *prefix* of the first: the quotes in Postgres's
            // message are what keep these apart.
            TableConstraint::new(
                "sc_uq_member_org_email",
                ConstraintKind::Unique {
                    fields: vec!["org".into(), "email".into()],
                },
            )
            .message("that pair is taken"),
        ];
        let message = "duplicate key value violates unique constraint \
                       \"sc_uq_member_org_email\"";
        assert_eq!(
            violated_constraint(&constraints, message).map(|c| c.name.as_str()),
            Some("sc_uq_member_org_email")
        );
        // A constraint with no message of its own still matches: what it
        // decides is whether the failure is the *caller's* fault (a 400 naming
        // the rule) or the server's (a 500), and that is true either way.
        let plain = vec![TableConstraint::new(
            "sc_uq_member_org",
            ConstraintKind::Unique {
                fields: vec!["org".into()],
            },
        )];
        assert!(
            violated_constraint(&plain, "violates unique constraint \"sc_uq_member_org\"")
                .is_some()
        );
        // …and an error naming no constraint of this table is not a match at
        // all: inventing a friendly message for an unanticipated fault is how a
        // real one gets hidden.
        assert!(violated_constraint(&plain, "connection closed").is_none());
    }
}
