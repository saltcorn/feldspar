//! What a stage looks like: its columns, their types, and its **grain**
//! (analytics TODO A1.2) — and the schema a compile reads its tables from.
//!
//! The grain says what one row represents. It decides which relations a
//! formula may follow: `Ⱶ` from any foreign-key column always, because a key
//! stays a key through every operation; `Ↄ` only while each row corresponds to
//! a row of a table — the base table's rows until something changes the grain,
//! or the referenced rows after an Aggregate grouped by a single foreign key.

use std::collections::BTreeMap;

use sc_catalog::{Catalog, DataFieldKind, Table};
use sc_error::Result;
use sc_expr::{CalcFields, Formula, SchemaShape};
use sc_types::{BasicType, TypeRef};
use serde::{Deserialize, Serialize};

/// The type of a stage's column, as far as the compiler can tell before
/// anything is read.
///
/// `Unknown` is an honest answer, not an error: the type of a formula calling a
/// function is not derivable from the schema. A read fills it in from the
/// values that come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColType {
    /// A whole number.
    Int,
    /// A floating-point number.
    Float,
    /// An exact decimal number.
    Decimal,
    /// Text.
    Text,
    /// True or false.
    Bool,
    /// A calendar date.
    Date,
    /// A time of day.
    Time,
    /// An instant.
    Timestamp,
    /// A JSON document.
    Json,
    /// A UUID.
    Uuid,
    /// Raw bytes.
    Bytes,
    /// Not known until the data is read.
    Unknown,
}

impl ColType {
    /// The type a field of `type_` has.
    pub fn of_type(type_: &TypeRef) -> ColType {
        match type_ {
            TypeRef::Basic(basic) => ColType::of_basic(basic),
            _ => ColType::Unknown,
        }
    }

    /// The type a column of `basic` has.
    pub fn of_basic(basic: &BasicType) -> ColType {
        match basic {
            BasicType::Bool => ColType::Bool,
            BasicType::Int => ColType::Int,
            BasicType::Float => ColType::Float,
            BasicType::Decimal => ColType::Decimal,
            BasicType::Text => ColType::Text,
            BasicType::Bytes => ColType::Bytes,
            BasicType::Json => ColType::Json,
            BasicType::Uuid => ColType::Uuid,
            BasicType::Date => ColType::Date,
            BasicType::Time => ColType::Time,
            BasicType::Timestamp => ColType::Timestamp,
            BasicType::Other(_) => ColType::Text,
        }
    }

    /// The type of a value read back, for a column whose type was not known.
    pub fn of_value(value: &sc_query::Value) -> ColType {
        use sc_query::Value;
        match value {
            Value::Null => ColType::Unknown,
            Value::Bool(_) => ColType::Bool,
            Value::Int(_) => ColType::Int,
            Value::Float(_) => ColType::Float,
            Value::Decimal(_) => ColType::Decimal,
            Value::Text(_) => ColType::Text,
            Value::Bytes(_) => ColType::Bytes,
            Value::Json(_) => ColType::Json,
            Value::Uuid(_) => ColType::Uuid,
            Value::Date(_) => ColType::Date,
            Value::Time(_) => ColType::Time,
            Value::Timestamp(_) => ColType::Timestamp,
        }
    }

    /// Whether it is a number.
    pub fn is_numeric(self) -> bool {
        matches!(self, ColType::Int | ColType::Float | ColType::Decimal)
    }

    /// Whether it has an order a sort, a minimum or a range can use.
    pub fn is_ordered(self) -> bool {
        self.is_numeric()
            || matches!(
                self,
                ColType::Text | ColType::Date | ColType::Time | ColType::Timestamp
            )
    }

    /// The word the editor and a refusal use for it.
    pub fn name(self) -> &'static str {
        match self {
            ColType::Int => "integer",
            ColType::Float => "number",
            ColType::Decimal => "decimal",
            ColType::Text => "text",
            ColType::Bool => "boolean",
            ColType::Date => "date",
            ColType::Time => "time",
            ColType::Timestamp => "date and time",
            ColType::Json => "JSON",
            ColType::Uuid => "UUID",
            ColType::Bytes => "bytes",
            ColType::Unknown => "unknown",
        }
    }

    /// Whether a column of this type and one of `other` can be one column —
    /// stacked, unioned or joined on.
    pub fn compatible(self, other: ColType) -> bool {
        self == other
            || (self.is_numeric() && other.is_numeric())
            || self == ColType::Unknown
            || other == ColType::Unknown
    }

    /// The type one column of values of this type and of `other` has.
    pub fn unify(self, other: ColType) -> ColType {
        match (self, other) {
            (a, b) if a == b => a,
            (ColType::Unknown, b) => b,
            (a, ColType::Unknown) => a,
            (ColType::Int, ColType::Decimal) | (ColType::Decimal, ColType::Int) => ColType::Decimal,
            (a, b) if a.is_numeric() && b.is_numeric() => ColType::Float,
            (a, _) => a,
        }
    }

    /// The SQL type a literal or a `NULL` of this type is cast to, so both
    /// backends know what a bound parameter is. The same spelling serves both:
    /// Postgres reads each as the type it names, and SQLite takes its affinity
    /// from the words in it.
    pub(crate) fn sql_type(self) -> &'static str {
        match self {
            ColType::Int => "bigint",
            ColType::Float => "double precision",
            ColType::Decimal => "numeric",
            ColType::Text | ColType::Unknown => "text",
            ColType::Bool => "boolean",
            ColType::Date => "date",
            ColType::Time => "time",
            ColType::Timestamp => "timestamptz",
            ColType::Json => "jsonb",
            ColType::Uuid => "uuid",
            ColType::Bytes => "bytea",
        }
    }
}

/// Where a foreign-key column points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    /// The referenced table.
    pub table: String,
    /// The referenced field (usually its primary key).
    pub field: String,
}

/// One column of a stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageColumn {
    /// Its name — what later operations refer to it by.
    pub name: String,
    /// Its type.
    #[serde(rename = "type")]
    pub ty: ColType,
    /// Where it points, when it is a foreign key: it stays one through every
    /// operation, so `Ⱶ` can be followed from it anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<ForeignKey>,
}

/// What one row of a stage represents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grain {
    /// A row of `table`, identified by its primary key `key`. Held from the
    /// base until an operation changes the grain.
    Table {
        /// The table.
        table: String,
        /// Its primary-key field.
        key: String,
    },
    /// One row per combination of the `keys` columns — after an Aggregate or
    /// a Split.
    Group {
        /// The columns that identify a row.
        keys: Vec<String>,
    },
    /// Anything else: rows made by stacking, a union, a join that may repeat
    /// rows, a completion.
    Derived,
}

impl Grain {
    /// The sentence a refusal uses to say what a row is.
    pub fn describe(&self) -> String {
        match self {
            Grain::Table { table, .. } => format!("each row is a row of `{table}`"),
            Grain::Group { keys } if keys.is_empty() => "there is one row in all".to_owned(),
            Grain::Group { keys } => format!(
                "each row is one combination of {}",
                keys.iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Grain::Derived => "rows are derived and do not correspond to a table's".to_owned(),
        }
    }
}

/// A stage: its columns and its grain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageShape {
    /// The columns, in order.
    pub columns: Vec<StageColumn>,
    /// What one row represents.
    pub grain: Grain,
}

impl StageShape {
    /// The column called `name`.
    pub fn column(&self, name: &str) -> Option<&StageColumn> {
        self.columns.iter().find(|c| c.name == name)
    }
}

/// One table as a compile sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableInfo {
    /// Its name.
    pub name: String,
    /// The columns a dataset over it starts with: its stored fields, then its
    /// non-stored calculated fields (a calculated field that does not become
    /// SQL is left out — a dataset is one query).
    pub columns: Vec<StageColumn>,
    /// Which columns are calculated fields.
    pub calc: CalcFields,
    /// Its single primary-key field, when it has one.
    pub primary_key: Option<String>,
}

impl TableInfo {
    /// The column called `name`.
    pub fn column(&self, name: &str) -> Option<&StageColumn> {
        self.columns.iter().find(|c| c.name == name)
    }
}

/// Everything a compile needs to know about the database: the tables, their
/// columns and types, and the formula shape (§7.3) the formulas of operations
/// are checked against.
#[derive(Debug, Clone)]
pub struct Schema {
    /// The formula language's view of every table.
    pub shape: SchemaShape,
    /// The tables a dataset may be based on or combine with.
    pub tables: BTreeMap<String, TableInfo>,
    /// Whether the database has date, time and UUID types of its own
    /// (`DbCapabilities::native_temporal_types`); literals of those types are
    /// cast to text where it has not.
    pub native_temporal_types: bool,
}

impl Schema {
    /// The schema the catalog describes.
    ///
    /// System tables (`_fd_*`) are left out: they are the server's metadata, not
    /// data anybody analyses, and a dataset over one would be a way around the
    /// screens that guard them.
    pub fn of_catalog(catalog: &Catalog) -> Result<Schema> {
        let shape = catalog.schema_shape()?;
        let all = tables_by_name(catalog)?;
        // Tables in the primary database only: a dataset is one query, and a
        // query runs on one database. A provided table has no SQL at all.
        let primary = catalog.primary_db().clone();
        let tables = all
            .values()
            .filter(|table| {
                !table.is_system()
                    && table.database == primary
                    && matches!(table.source, sc_catalog::TableSource::Database)
            })
            .map(|table| (table.name.clone(), table_info(table, &all)))
            .collect();
        Ok(Schema {
            shape,
            tables,
            native_temporal_types: catalog.primary().capabilities().native_temporal_types,
        })
    }

    /// A schema from its parts (what the unit tests build).
    pub fn new(shape: SchemaShape, tables: impl IntoIterator<Item = TableInfo>) -> Schema {
        Schema {
            shape,
            tables: tables.into_iter().map(|t| (t.name.clone(), t)).collect(),
            native_temporal_types: true,
        }
    }

    /// The table called `name`, or the sentence saying there is none.
    pub fn table(&self, name: &str) -> std::result::Result<&TableInfo, String> {
        if name.starts_with("_fd_") {
            return Err(format!(
                "`{name}` is one of the server's own tables, and a dataset cannot be built on it"
            ));
        }
        self.tables
            .get(name)
            .ok_or_else(|| format!("there is no table called `{name}`"))
    }
}

/// Every table of the catalog by name — what a key field's target type is
/// looked up in.
fn tables_by_name(catalog: &Catalog) -> Result<BTreeMap<String, Table>> {
    Ok(catalog
        .tables()?
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect())
}

/// One catalog table as a [`TableInfo`].
fn table_info(table: &Table, all: &BTreeMap<String, Table>) -> TableInfo {
    let mut columns = Vec::new();
    let mut calc = CalcFields::new();
    for field in &table.fields {
        match &field.kind {
            DataFieldKind::Key {
                target_table,
                target_field,
                ..
            } => {
                // A key has its target's type: a key to an integer id holds
                // integers.
                let ty = all
                    .get(&target_table.0)
                    .and_then(|t| t.field(&target_field.0))
                    .map_or(ColType::of_type(&field.base.type_), |f| {
                        ColType::of_type(&f.base.type_)
                    });
                columns.push(StageColumn {
                    name: field.base.name.clone(),
                    ty,
                    key: Some(ForeignKey {
                        table: target_table.0.clone(),
                        field: target_field.0.clone(),
                    }),
                });
            }
            DataFieldKind::Calc { expression } => {
                if let Ok(formula) = Formula::parse(expression) {
                    calc.insert(field.base.name.clone(), formula);
                    columns.push(StageColumn {
                        name: field.base.name.clone(),
                        ty: ColType::of_type(&field.base.type_),
                        key: None,
                    });
                }
            }
            DataFieldKind::Plain | DataFieldKind::File { .. } => columns.push(StageColumn {
                name: field.base.name.clone(),
                ty: ColType::of_type(&field.base.type_),
                key: None,
            }),
        }
    }
    TableInfo {
        name: table.name.clone(),
        columns,
        calc,
        primary_key: match table.primary_key.as_slice() {
            [pk] => Some(pk.clone()),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_unify_and_other_types_must_match() {
        assert!(ColType::Int.compatible(ColType::Float));
        assert!(!ColType::Int.compatible(ColType::Text));
        assert_eq!(ColType::Int.unify(ColType::Float), ColType::Float);
        assert_eq!(ColType::Int.unify(ColType::Int), ColType::Int);
        assert_eq!(ColType::Unknown.unify(ColType::Date), ColType::Date);
    }

    #[test]
    fn a_grain_describes_itself_in_a_sentence() {
        assert_eq!(
            Grain::Group {
                keys: vec!["month".into()]
            }
            .describe(),
            "each row is one combination of `month`"
        );
    }
}
