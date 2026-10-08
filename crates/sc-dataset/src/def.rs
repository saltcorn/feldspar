//! What a dataset **is**: a base and an ordered list of operations (analytics
//! TODO A1.1; the goals document's "Dataset operations").
//!
//! Pure data, serialised as the `_fd_datasets` row's JSON columns and as the
//! API's wire shape, so every type here is the JSON the Analytics UI edits.
//! What the operations *mean* — the columns they produce and the SQL they
//! become — is [`compile`](crate::compile)'s business, not this module's: a
//! definition that no longer compiles (a column an earlier operation removed) is
//! still a definition, stored and editable, with the operation marked.
//!
//! An operation is `{ id, enabled, kind, params }`:
//!
//! ```json
//! { "id": "a1f3", "enabled": true, "kind": "calculated",
//!   "params": { "name": "price_per_m2", "formula": "price / area" } }
//! ```
//!
//! The `id` is what the editor, the error report and a panel's reference name
//! an operation by, because its position changes whenever the list is
//! reordered.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

/// Identifies a dataset: the UUID primary key of its `_fd_datasets` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DatasetId(pub Uuid);

impl DatasetId {
    /// Mint an id for a new dataset.
    pub fn new() -> DatasetId {
        DatasetId(Uuid::new_v4())
    }
}

impl Default for DatasetId {
    fn default() -> Self {
        DatasetId::new()
    }
}

impl std::fmt::Display for DatasetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for DatasetId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(DatasetId)
    }
}

/// Where a dataset's rows start: a table, or another dataset whose operations
/// then come first.
///
/// Picked when the dataset is created and **never changed** (the goals
/// document): every operation after it is written against the columns it
/// provides, so changing it would be starting over, which is what a new
/// dataset is for. The store refuses a changed base by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Base {
    /// The rows of a table: its stored fields, and those of its non-stored
    /// calculated fields that become SQL.
    Table {
        /// The table's name.
        table: String,
    },
    /// The rows another dataset produces after its last operation.
    Dataset {
        /// The dataset's id.
        dataset: DatasetId,
    },
}

impl Base {
    /// A table base.
    pub fn table(table: impl Into<String>) -> Base {
        Base::Table {
            table: table.into(),
        }
    }

    /// A dataset base.
    pub fn dataset(dataset: DatasetId) -> Base {
        Base::Dataset { dataset }
    }
}

/// A persistent, named dataset: a base followed by an ordered list of
/// operations. The rows are never materialised; [`compile`](crate::compile)
/// turns the definition into one query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetDef {
    /// Stable identity.
    pub id: DatasetId,
    /// Unique and human-facing: what the model form and the explorer list.
    pub name: String,
    /// Free text; empty means none given.
    #[serde(default)]
    pub description: String,
    /// Where the rows start.
    pub base: Base,
    /// What happens to them, in order.
    #[serde(default)]
    pub operations: Vec<Operation>,
}

impl DatasetDef {
    /// A new dataset over `base` with no operations.
    pub fn new(name: impl Into<String>, base: Base) -> DatasetDef {
        DatasetDef {
            id: DatasetId::new(),
            name: name.into(),
            description: String::new(),
            base,
            operations: Vec::new(),
        }
    }

    /// A new dataset over the table `table` with no operations.
    pub fn over_table(name: impl Into<String>, table: impl Into<String>) -> DatasetDef {
        DatasetDef::new(name, Base::table(table))
    }

    /// This dataset with `op` appended, enabled, under the next free id
    /// (builder sugar, mostly for tests).
    pub fn then(mut self, op: Op) -> DatasetDef {
        let id = format!("op{}", self.operations.len() + 1);
        self.operations.push(Operation::new(id, op));
        self
    }

    /// The operation with this id.
    pub fn operation(&self, id: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.id == id)
    }

    /// The dataset Model code used to be: the named columns, each computed by a
    /// formula, one optional filter and an order — and nothing else of the
    /// table (analytics goals, "Additional changes to core").
    ///
    /// A Calculated column per column, a Filter, a Sort, and a Select columns
    /// that keeps exactly the named ones. What the models tutorial and the
    /// tests of `sc-model` and `sc-stan` build, and what `TABLES_RENAME.sql`
    /// writes for a model stored before datasets had names.
    pub fn from_columns(
        name: impl Into<String>,
        table: impl Into<String>,
        columns: &[(&str, &str)],
        filter: Option<&str>,
        order: &[(&str, bool)],
    ) -> DatasetDef {
        let mut def = DatasetDef::over_table(name, table);
        for (column, formula) in columns {
            def = def.then(Op::calculated(*column, *formula));
        }
        if let Some(filter) = filter {
            def = def.then(Op::filter(filter));
        }
        if !order.is_empty() {
            def = def.then(Op::Sort(SortOp {
                keys: order
                    .iter()
                    .map(|(formula, descending)| SortKey {
                        formula: (*formula).to_owned(),
                        descending: *descending,
                    })
                    .collect(),
            }));
        }
        def.then(Op::Select(SelectOp {
            columns: columns
                .iter()
                .map(|(column, _)| SelectColumn::keep(*column))
                .collect(),
        }))
    }

    /// The datasets this one reads besides itself: its base, if that is a
    /// dataset, and every dataset a Join or a Union names — enabled or not,
    /// because enabling one must not be what introduces a cycle.
    pub fn dependencies(&self) -> Vec<DatasetId> {
        let mut out = Vec::new();
        if let Base::Dataset { dataset } = &self.base {
            out.push(*dataset);
        }
        for op in &self.operations {
            if let Some(Other::Dataset { dataset }) = op.op.other() {
                out.push(*dataset);
            }
        }
        out
    }
}

/// One step of a dataset, with the identity and the switch every step has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    /// Unique within the dataset; what the editor and an error report name the
    /// operation by, since its position changes when the list is reordered.
    pub id: String,
    /// A disabled operation is skipped, as though it were not there, and kept
    /// so it can be switched back on.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// What it does.
    #[serde(flatten)]
    pub op: Op,
}

fn enabled_by_default() -> bool {
    true
}

impl Operation {
    /// An enabled operation.
    pub fn new(id: impl Into<String>, op: Op) -> Operation {
        Operation {
            id: id.into(),
            enabled: true,
            op,
        }
    }

    /// The same operation, switched off.
    pub fn disabled(mut self) -> Operation {
        self.enabled = false;
        self
    }
}

/// The operations of the goals document: the ones that keep the grain, the
/// ones that change it, and the ones that combine with another table or
/// dataset — A1's, and the Spatial join of A5. (Neighbourhood column and Model
/// predictions arrive with the milestones that bring their machinery.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum Op {
    /// Add or replace a column computed by a formula over the current row
    /// (`mutate`).
    Calculated(CalculatedOp),
    /// Keep the rows a condition holds for (`filter`, `semi_join`, …).
    Filter(FilterOp),
    /// Keep, drop, rename and reorder columns (`select`, `rename`,
    /// `relocate`).
    Select(SelectOp),
    /// Order the rows (`arrange`).
    Sort(SortOp),
    /// A column computed over the ordered rows of a group (`group_by` +
    /// `mutate`, `fill`).
    Window(WindowOp),
    /// One row per combination of the group keys, with summary columns
    /// (`summarise`, `count`, `distinct`).
    Aggregate(AggregateOp),
    /// The first rows, a sample, or the top rows of each group (`slice_*`).
    Limit(LimitOp),
    /// Columns into rows of name/value pairs (`pivot_longer`).
    Stack(StackOp),
    /// The values of a name column into columns of their own (`pivot_wider`).
    Split(SplitOp),
    /// Rows for the combinations of values that are missing (`complete`).
    Complete(CompleteOp),
    /// Join another table or dataset on key columns (`*_join`, `closest()`).
    Join(JoinOp),
    /// Append the rows of another table or dataset (`bind_rows`).
    Union(UnionOp),
    /// Join another table or dataset where the geometries meet, or to the
    /// nearest (`st_join`; analytics TODO A5.4).
    SpatialJoin(SpatialJoinOp),
}

impl Op {
    /// A Calculated column.
    pub fn calculated(name: impl Into<String>, formula: impl Into<String>) -> Op {
        Op::Calculated(CalculatedOp {
            name: name.into(),
            formula: formula.into(),
        })
    }

    /// A Filter.
    pub fn filter(formula: impl Into<String>) -> Op {
        Op::Filter(FilterOp {
            formula: formula.into(),
        })
    }

    /// The operation's kind as its JSON tag spells it.
    pub fn kind(&self) -> &'static str {
        match self {
            Op::Calculated(_) => "calculated",
            Op::Filter(_) => "filter",
            Op::Select(_) => "select",
            Op::Sort(_) => "sort",
            Op::Window(_) => "window",
            Op::Aggregate(_) => "aggregate",
            Op::Limit(_) => "limit",
            Op::Stack(_) => "stack",
            Op::Split(_) => "split",
            Op::Complete(_) => "complete",
            Op::Join(_) => "join",
            Op::Union(_) => "union",
            Op::SpatialJoin(_) => "spatial_join",
        }
    }

    /// The table or dataset a combining operation reads, if this is one.
    pub fn other(&self) -> Option<&Other> {
        match self {
            Op::Join(j) => Some(&j.with),
            Op::Union(u) => Some(&u.with),
            Op::SpatialJoin(j) => Some(&j.with),
            _ => None,
        }
    }
}

/// Add or replace a column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalculatedOp {
    /// The column's name; replacing an existing column keeps its position.
    pub name: String,
    /// An `sc-expr` formula over the current row. It can follow foreign keys
    /// (`neighbourhoodⱵname`) and, while rows are rows of a table, aggregate
    /// over child tables (`viewingsↃhouse.length`).
    pub formula: String,
}

/// Keep the rows a condition holds for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterOp {
    /// A boolean formula over the current row.
    pub formula: String,
}

/// Keep, drop, rename and reorder columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOp {
    /// The columns kept, in their new order.
    pub columns: Vec<SelectColumn>,
}

/// One column a Select columns keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectColumn {
    /// Its name before this operation.
    pub column: String,
    /// Its name after, when that is different.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<String>,
}

impl SelectColumn {
    /// Keep `column` under its own name.
    pub fn keep(column: impl Into<String>) -> SelectColumn {
        SelectColumn {
            column: column.into(),
            rename: None,
        }
    }

    /// Keep `column`, renamed to `to`.
    pub fn renamed(column: impl Into<String>, to: impl Into<String>) -> SelectColumn {
        SelectColumn {
            column: column.into(),
            rename: Some(to.into()),
        }
    }

    /// The name it has after the operation.
    pub fn output(&self) -> &str {
        self.rename
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or(&self.column)
    }
}

/// Order the rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortOp {
    /// The keys, most significant first. Rows that tie on every key keep the
    /// order they had.
    pub keys: Vec<SortKey>,
}

/// One key of a Sort: a formula, and which way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    /// A formula — usually a column's name.
    pub formula: String,
    /// Largest first.
    #[serde(default)]
    pub descending: bool,
}

/// One key of an order inside an operation (a Window's, a Limit's, a
/// summary's): a column, and which way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderKey {
    /// A column of the current rows.
    pub column: String,
    /// Largest first.
    #[serde(default)]
    pub descending: bool,
}

impl OrderKey {
    /// Ascending by `column`.
    pub fn asc(column: impl Into<String>) -> OrderKey {
        OrderKey {
            column: column.into(),
            descending: false,
        }
    }

    /// Descending by `column`.
    pub fn desc(column: impl Into<String>) -> OrderKey {
        OrderKey {
            column: column.into(),
            descending: true,
        }
    }
}

/// A column computed over the ordered rows of a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowOp {
    /// The new column's name.
    pub name: String,
    /// What it computes.
    pub function: WindowFunction,
    /// The column it is computed from; not needed by `rank` and `row_number`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// How many rows back (`lag`) or forward (`lead`); 1 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// The groups, as columns; empty for the rows as one group.
    #[serde(default)]
    pub partition: Vec<String>,
    /// The order within a group; empty for the order the rows already have.
    #[serde(default)]
    pub order: Vec<OrderKey>,
}

/// What a Window column computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowFunction {
    /// The value `offset` rows earlier.
    Lag,
    /// The value `offset` rows later.
    Lead,
    /// The difference from the previous row's value.
    Difference,
    /// The running total.
    CumulativeSum,
    /// The running mean.
    CumulativeMean,
    /// The rank (ties share one, and the next rank is skipped).
    Rank,
    /// The row's number, from 1.
    RowNumber,
    /// The group's total.
    GroupSum,
    /// The group's mean.
    GroupMean,
    /// The number of rows in the group with a value.
    GroupCount,
    /// The group's smallest value.
    GroupMin,
    /// The group's largest value.
    GroupMax,
    /// The row's share of its group's total.
    Share,
    /// The last value that was not missing (`fill`).
    Fill,
}

impl WindowFunction {
    /// Whether it reads a column.
    pub fn needs_column(self) -> bool {
        !matches!(self, WindowFunction::Rank | WindowFunction::RowNumber)
    }
}

/// One row per combination of the group keys, with summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateOp {
    /// The group keys. Each is a formula, so a key can bin its value.
    #[serde(default)]
    pub group_by: Vec<GroupKey>,
    /// The summary columns. With none, the operation is `distinct`.
    #[serde(default)]
    pub summaries: Vec<Summary>,
}

/// One group key of an Aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKey {
    /// The key column's name in the result.
    pub name: String,
    /// A formula over the current row — usually a column's name.
    pub formula: String,
}

impl GroupKey {
    /// Group by the column `column`, keeping its name.
    pub fn column(column: impl Into<String>) -> GroupKey {
        let column = column.into();
        GroupKey {
            name: column.clone(),
            formula: column,
        }
    }
}

/// One summary column of an Aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// Its name in the result.
    pub name: String,
    /// What it computes.
    pub function: SummaryFunction,
    /// The column summarised; `count` without one counts rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// The order `first` and `last` are taken by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<OrderKey>,
}

impl Summary {
    /// A summary of `function` over `column`.
    pub fn of(
        name: impl Into<String>,
        function: SummaryFunction,
        column: impl Into<String>,
    ) -> Summary {
        Summary {
            name: name.into(),
            function,
            column: Some(column.into()),
            order: None,
        }
    }

    /// The number of rows.
    pub fn count(name: impl Into<String>) -> Summary {
        Summary {
            name: name.into(),
            function: SummaryFunction::Count,
            column: None,
            order: None,
        }
    }
}

/// What a summary computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryFunction {
    /// The number of rows, or of values of the column that are not missing.
    Count,
    /// The number of distinct values.
    CountDistinct,
    /// The total.
    Sum,
    /// The mean.
    Mean,
    /// The median.
    Median,
    /// The smallest value.
    Min,
    /// The largest value.
    Max,
    /// The sample standard deviation.
    Sd,
    /// The value in the first row by an order.
    First,
    /// The value in the last row by an order.
    Last,
    /// The union of the geometries: regions dissolved into one (analytics
    /// TODO A5.4). Computed by PostGIS.
    Union,
}

/// Keep some of the rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitOp {
    /// Which rows.
    pub mode: LimitMode,
    /// How many (per group, for `top`).
    pub n: u64,
    /// The sample's seed: the same seed over the same rows is the same sample.
    #[serde(default)]
    pub seed: i64,
    /// The groups `top` takes its rows from.
    #[serde(default)]
    pub group_by: Vec<String>,
    /// The order `top` ranks by; empty for the order the rows already have.
    #[serde(default)]
    pub order: Vec<OrderKey>,
}

/// Which rows a Limit keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitMode {
    /// The first N in the current order.
    First,
    /// N rows at random, reproducibly by the seed.
    Sample,
    /// The first N of each group by an order.
    Top,
}

/// Columns into rows of name/value pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackOp {
    /// The columns stacked.
    pub columns: Vec<String>,
    /// The new column holding each stacked column's name.
    pub names_to: String,
    /// The new column holding its value.
    pub values_to: String,
}

/// The values of a name column into columns of their own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitOp {
    /// The column whose values become the new columns' names.
    pub names_from: String,
    /// The column whose values fill them.
    pub values_from: String,
    /// The columns identifying a result row.
    pub id_columns: Vec<String>,
    /// The new columns, **fixed when the operation is defined** (pre-filled
    /// from the data) so a new value in the data does not change the columns
    /// later operations, panels and models are written against.
    pub values: Vec<String>,
    /// How several values for one cell are combined.
    #[serde(default)]
    pub summary: SplitSummary,
}

/// How a Split combines several values for one cell.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitSummary {
    /// One of them (the largest): the ordinary case, where there is only one.
    #[default]
    First,
    /// Their total.
    Sum,
    /// Their mean.
    Mean,
    /// How many there are.
    Count,
    /// The smallest.
    Min,
    /// The largest.
    Max,
}

/// Rows for missing combinations of values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteOp {
    /// The columns whose combinations are completed, each with where its
    /// values come from.
    pub columns: Vec<CompleteColumn>,
    /// The value the other columns take in an added row; missing when not
    /// listed.
    #[serde(default)]
    pub fill: Vec<FillValue>,
}

/// One column a Complete completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteColumn {
    /// The column.
    pub column: String,
    /// Where its values come from.
    pub values: CompleteValues,
}

/// Where a Complete column's values come from.
// One per Complete column, parsed once per compile: boxing the range buys
// nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum CompleteValues {
    /// The values in the data.
    Data,
    /// A range: numbers from `from` to `to` by `step` (1 by default), or dates
    /// from `from` to `to` by `step` of `day`, `week`, `month` or `year`.
    Range {
        /// The first value.
        from: Json,
        /// The last value, included when the steps reach it.
        to: Json,
        /// The step; a number, or a date unit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<Json>,
    },
    /// Every row of the table a foreign-key column refers to, so that regions
    /// with no data still appear.
    Table,
}

/// The value a column takes in the rows a Complete adds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillValue {
    /// The column.
    pub column: String,
    /// Its value.
    pub value: Json,
}

/// The table or dataset a Join or a Union reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Other {
    /// A table's rows.
    Table {
        /// Its name.
        table: String,
    },
    /// A dataset's rows after its last operation.
    Dataset {
        /// Its id.
        dataset: DatasetId,
    },
}

/// Join another table or dataset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinOp {
    /// What is joined.
    pub with: Other,
    /// Which rows survive.
    pub kind: JoinKind,
    /// The equality keys: a column of these rows, a column of the other.
    #[serde(default)]
    pub on: Vec<JoinKey>,
    /// The "nearest earlier" key: each row takes the other's row with the
    /// latest `right` at or before its own `left` (an as-of join).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asof: Option<JoinKey>,
    /// The other's columns brought across; all of them when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// Appended to one of the other's column names that is already taken.
    #[serde(default = "default_suffix")]
    pub suffix: String,
}

fn default_suffix() -> String {
    "_right".to_owned()
}

/// One pair of join columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinKey {
    /// A column of the rows so far.
    pub left: String,
    /// A column of the other table or dataset.
    pub right: String,
}

impl JoinKey {
    /// `left` = `right`.
    pub fn new(left: impl Into<String>, right: impl Into<String>) -> JoinKey {
        JoinKey {
            left: left.into(),
            right: right.into(),
        }
    }
}

/// Which rows a Join keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinKind {
    /// Rows with a match on both sides.
    Inner,
    /// Every row of these, matched or not.
    Left,
    /// Every row of both.
    Full,
}

/// Append the rows of another table or dataset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnionOp {
    /// What is appended.
    pub with: Other,
    /// A new column saying which source a row came from, when named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_column: Option<String>,
    /// That column's value for these rows and for the other's; the two names
    /// when absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_labels: Vec<String>,
}

/// Join another table or dataset by where the geometries are (analytics TODO
/// A5.4): the goals document's Spatial join.
///
/// Aggregating points to regions is this followed by an Aggregate by the
/// region, and "distance to the nearest" is a `nearest` join with a
/// `distance_column`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialJoinOp {
    /// What is joined.
    pub with: Other,
    /// Which rows survive: `inner` or `left` (a spatial join keeps the rows of
    /// these, so it is never `full`).
    pub kind: JoinKind,
    /// How the geometries must be placed for two rows to match.
    pub relation: SpatialRelation,
    /// The geometry column of the rows so far.
    pub left: String,
    /// The geometry column of the other table or dataset.
    pub right: String,
    /// In metres: how near `within_distance` means, and, for `nearest`, how
    /// far to look at most (no limit when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
    /// A new column holding the distance in metres between the two matched
    /// geometries, when named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance_column: Option<String>,
    /// The other's columns brought across; all of them when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// Appended to one of the other's column names that is already taken.
    #[serde(default = "default_suffix")]
    pub suffix: String,
}

/// How two geometries must be placed for a Spatial join to match them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpatialRelation {
    /// They share at least one point.
    Intersects,
    /// This row's geometry contains the other's (a region and its points).
    Contains,
    /// This row's geometry is inside the other's (a point and its region).
    Within,
    /// They are no further apart than `distance` metres.
    WithinDistance,
    /// The other's one row whose geometry is nearest to this row's — so each
    /// row matches at most one, and the grain is kept.
    Nearest,
}

impl SpatialRelation {
    /// The word the editor and a refusal use for it.
    pub fn describe(self) -> &'static str {
        match self {
            SpatialRelation::Intersects => "intersects",
            SpatialRelation::Contains => "contains",
            SpatialRelation::Within => "is within",
            SpatialRelation::WithinDistance => "is within a distance of",
            SpatialRelation::Nearest => "is nearest to",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_operation_is_id_enabled_kind_and_params() {
        let op = Operation::new("a1", Op::calculated("ppm", "price / area"));
        let json = serde_json::to_value(&op).expect("json");
        assert_eq!(
            json,
            json!({
                "id": "a1",
                "enabled": true,
                "kind": "calculated",
                "params": { "name": "ppm", "formula": "price / area" }
            })
        );
        let back: Operation = serde_json::from_value(json).expect("back");
        assert_eq!(back, op);
        // `enabled` defaults to on.
        let bare: Operation = serde_json::from_value(
            json!({ "id": "b", "kind": "filter", "params": { "formula": "x > 1" } }),
        )
        .expect("bare");
        assert!(bare.enabled);
    }

    #[test]
    fn the_legacy_shape_is_calculated_columns_a_filter_a_sort_and_a_select() {
        let def = DatasetDef::from_columns(
            "d",
            "houses",
            &[("price", "price"), ("area", "area")],
            Some("sold === true"),
            &[("price", true)],
        );
        let kinds: Vec<&str> = def.operations.iter().map(|o| o.op.kind()).collect();
        assert_eq!(
            kinds,
            ["calculated", "calculated", "filter", "sort", "select"]
        );
        assert_eq!(def.base, Base::table("houses"));
    }

    #[test]
    fn a_spatial_join_is_its_relation_two_geometry_columns_and_a_distance() {
        let op = Op::SpatialJoin(SpatialJoinOp {
            with: Other::Table {
                table: "districts".into(),
            },
            kind: JoinKind::Left,
            relation: SpatialRelation::WithinDistance,
            left: "location".into(),
            right: "outline".into(),
            distance: Some(250.0),
            distance_column: None,
            columns: None,
            suffix: default_suffix(),
        });
        let json = serde_json::to_value(Operation::new("s", op.clone())).expect("json");
        assert_eq!(json["kind"], "spatial_join");
        assert_eq!(json["params"]["relation"], "within_distance");
        assert_eq!(json["params"]["distance"], 250.0);
        let back: Operation = serde_json::from_value(json!({
            "id": "s", "kind": "spatial_join",
            "params": {
                "with": { "kind": "table", "table": "districts" }, "kind": "left",
                "relation": "within_distance", "left": "location", "right": "outline",
                "distance": 250.0
            }
        }))
        .expect("back");
        assert_eq!(back.op, op);
        // A dataset it joins is a dependency, as a Join's is.
        let other = DatasetId::new();
        let def = DatasetDef::over_table("d", "t").then(Op::SpatialJoin(SpatialJoinOp {
            with: Other::Dataset { dataset: other },
            ..match op {
                Op::SpatialJoin(j) => j,
                _ => unreachable!(),
            }
        }));
        assert_eq!(def.dependencies(), vec![other]);
    }

    #[test]
    fn dependencies_are_the_base_and_every_combined_dataset() {
        let a = DatasetId::new();
        let b = DatasetId::new();
        let def = DatasetDef::new("d", Base::dataset(a)).then(Op::Union(UnionOp {
            with: Other::Dataset { dataset: b },
            source_column: None,
            source_labels: Vec::new(),
        }));
        assert_eq!(def.dependencies(), vec![a, b]);
    }
}
