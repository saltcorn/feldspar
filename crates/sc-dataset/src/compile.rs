//! From a definition to one query (analytics TODO A1.2–A1.6).
//!
//! A dataset is compiled operation by operation into a **stage**: the query so
//! far, as a `FROM` with named column expressions over it, and a `WHERE`, a
//! `GROUP BY` or a `LIMIT` when the operations so far put one there. Each
//! operation reads the stage before it and makes the next.
//!
//! **Merging, and nesting.** Every formula is translated as though it read its
//! row from an alias, `_fd_row`, over a table-shaped view of the stage (its
//! columns, their foreign keys, and — while rows are rows of a table — whose
//! rows they are). Then either:
//!
//! - it is **merged** into the stage: each `"_fd_row"."price"` is replaced with
//!   the expression the stage computes `price` by, and the result becomes one
//!   more projection, or is anded into the `WHERE`. A Filter after a Calculated
//!   column after the base is still `SELECT … FROM houses WHERE …`; or
//! - the stage is **sealed** first: wrapped as a subquery, `FROM (…) AS
//!   "_fd_s3"`, whose columns are then plain references. That happens when
//!   merging would change the meaning — a condition on a window or on an
//!   aggregate cannot go in a `WHERE`, and nothing can be added below a `GROUP
//!   BY` or a `LIMIT`.
//!
//! So a dataset is nested subqueries, one per operation that needs one — the
//! goals document's "compiled into a single SQL query where possible".
//!
//! **Hidden columns.** A stage carries columns nobody sees: `_fd_key`, the base
//! table's primary key, while rows are rows of it (what a prediction and a
//! split identify a row by, and what `Ↄ` correlates on); and one `_fd_o…`
//! column per sort key, because a subquery does not keep its order — the order
//! is the stage's, applied when it is read. Reads therefore page stably: after
//! the sort keys, a stage is always ordered by its row key, its group keys, or
//! every column, in that order of preference.
//!
//! **Errors are sentences, and they stop.** An operation that does not compile
//! — a formula naming a column an earlier operation removed — is reported by id
//! with the sentence saying why, the stages before it still read, and the ones
//! after it are not reached. The definition is never refused for it: marking an
//! operation is the editor's job, and the admin repairs it there.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, Duration, Months, NaiveDate};
use sc_expr::{
    Ambient, Env, Formula, Operation as FormulaOperation, TableShape, TranslateError, UserEnv,
    translate_rooted, translate_value_rooted,
};
use sc_query::{
    BinOp, CaseArm, Expr, Join, JoinKind as SqlJoinKind, Nulls, OrderBy, OrderDir, Projection,
    Select, Source, UnOp, Value,
};
use serde::Serialize;
use serde_json::Value as Json;

use crate::def::{
    AggregateOp, Base, CalculatedOp, CompleteOp, CompleteValues, DatasetDef, DatasetId, FilterOp,
    JoinKind, JoinOp, LimitMode, LimitOp, Op, OrderKey, Other, SelectOp, SortOp, SplitOp,
    SplitSummary, StackOp, SummaryFunction, UnionOp, WindowFunction, WindowOp,
};
use crate::infer::{Known, infer};
use crate::shape::{ColType, ForeignKey, Grain, Schema, StageColumn, StageShape};
use crate::walk;

/// The hidden column holding the base table's primary key while rows are rows
/// of it.
pub const ROW_KEY: &str = "_fd_key";
/// The column a model reads a related dataset's row labels in.
pub const LABEL_COLUMN: &str = "_fd_label";
/// The alias a formula is translated at before it is merged into a stage.
const ROW: &str = "_fd_row";
/// The name a stage has as a "table" of the formula language's schema.
const STAGE_NAME: &str = "_fd_stage";
/// The alias of the base table in the innermost query.
const BASE_ALIAS: &str = "_fd_b";
/// How many values a Complete range may generate.
pub const MAX_RANGE_VALUES: usize = 10_000;

/// Every dataset a compile may reach: the one compiled, the one it is based
/// on, the ones it joins. Loaded once and handed in, so the compiler is a pure
/// function of its inputs.
#[derive(Debug, Clone, Default)]
pub struct Library {
    defs: BTreeMap<DatasetId, DatasetDef>,
}

impl Library {
    /// A library of `defs`.
    pub fn new(defs: impl IntoIterator<Item = DatasetDef>) -> Library {
        Library {
            defs: defs.into_iter().map(|d| (d.id, d)).collect(),
        }
    }

    /// The dataset with this id.
    pub fn get(&self, id: DatasetId) -> Option<&DatasetDef> {
        self.defs.get(&id)
    }

    /// Add or replace a dataset — how a definition being edited is compiled
    /// against the others before it is saved.
    pub fn insert(&mut self, def: DatasetDef) {
        self.defs.insert(def.id, def);
    }

    /// Every dataset, by id.
    pub fn defs(&self) -> impl Iterator<Item = &DatasetDef> {
        self.defs.values()
    }

    /// `root` and every dataset it reaches through its base, joins and unions,
    /// root first and the rest by id — what a fit snapshots. Missing datasets
    /// are left out; compiling the snapshot then says which.
    pub fn closure(&self, root: DatasetId) -> Vec<DatasetDef> {
        let mut seen = BTreeSet::new();
        let mut queue = vec![root];
        while let Some(id) = queue.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(def) = self.defs.get(&id) {
                queue.extend(def.dependencies());
            }
        }
        let mut out: Vec<DatasetDef> = Vec::with_capacity(seen.len());
        if let Some(def) = self.defs.get(&root) {
            out.push(def.clone());
        }
        out.extend(
            seen.iter()
                .filter(|id| **id != root)
                .filter_map(|id| self.defs.get(id).cloned()),
        );
        out
    }
}

/// How a compile treats the operations that choose rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Skip every Filter and Limit, here and in the datasets this one is based
    /// on: a prediction asks about rows the filter says the model was not
    /// fitted *from*, and must still compute their columns the same way
    /// (§14.2's "a prediction reads past the dataset's filter").
    pub skip_filters: bool,
}

/// What became of one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    /// It compiled; the stage after it reads.
    Ok,
    /// It is switched off, so the stage after it is the stage before it.
    Disabled,
    /// It does not compile, for the reason given.
    Invalid,
    /// An earlier operation does not compile, so this one was not tried.
    NotReached,
}

/// The base of a compiled dataset: its columns, or why there are none.
#[derive(Debug, Clone, Serialize)]
pub struct BaseReport {
    /// The columns and grain the operations start from.
    pub shape: Option<StageShape>,
    /// Why the base does not read: a table that is gone, a dataset it is
    /// based on with an error, a cycle.
    pub error: Option<String>,
}

/// What became of one operation, for the editor to show on it.
#[derive(Debug, Clone, Serialize)]
pub struct OperationReport {
    /// The operation's id.
    pub id: String,
    /// Its kind.
    pub kind: String,
    /// What became of it.
    pub status: OpStatus,
    /// The columns and grain after it, when it was reached and compiled (or is
    /// disabled).
    pub shape: Option<StageShape>,
    /// Why it does not compile.
    pub error: Option<String>,
}

/// A compiled dataset: a report for the base and every operation, and the
/// stages that read.
#[derive(Debug, Clone)]
pub struct Compilation {
    /// The base.
    pub base: BaseReport,
    /// One report per operation of the dataset itself, in order.
    pub operations: Vec<OperationReport>,
    /// `stages[0]` is the base; `stages[i + 1]` is after operation `i`.
    stages: Vec<Option<Stage>>,
}

impl Compilation {
    /// The stage after the first `upto` operations (0 is the base), or the
    /// sentence saying why it does not read.
    pub fn stage(&self, upto: usize) -> Result<&Stage, String> {
        if upto > self.operations.len() {
            return Err(format!(
                "the dataset has {} operations, so there is no stage after operation {upto}",
                self.operations.len()
            ));
        }
        if let Some(Some(stage)) = self.stages.get(upto) {
            return Ok(stage);
        }
        if let Some(error) = &self.base.error {
            return Err(format!("its base does not read: {error}"));
        }
        match self.first_error() {
            Some((i, report)) => Err(format!(
                "operation {} ({}) has an error: {}",
                i + 1,
                report.kind,
                report.error.as_deref().unwrap_or("it does not compile")
            )),
            None => Err("this stage does not read".to_owned()),
        }
    }

    /// The stage after the last operation.
    pub fn last(&self) -> Result<&Stage, String> {
        self.stage(self.operations.len())
    }

    /// The first operation that does not compile, with its position.
    pub fn first_error(&self) -> Option<(usize, &OperationReport)> {
        self.operations
            .iter()
            .enumerate()
            .find(|(_, r)| r.status == OpStatus::Invalid)
    }

    /// Whether the base and every enabled operation compile.
    pub fn is_valid(&self) -> bool {
        self.base.error.is_none() && self.first_error().is_none()
    }
}

/// Compile `def` against `schema`, reading the datasets it reaches from
/// `library`.
pub fn compile(
    schema: &Schema,
    library: &Library,
    def: &DatasetDef,
    options: Options,
) -> Compilation {
    let compiler = Compiler {
        schema,
        library,
        options,
        next: Cell::new(0),
    };
    compiler.compile_def(def, &[def.id])
}

/// One column of a stage, with the expression that computes it.
#[derive(Debug, Clone)]
struct Col {
    name: String,
    expr: Expr,
    ty: ColType,
    key: Option<ForeignKey>,
    /// Not shown: the row key and the sort keys.
    hidden: bool,
    /// Holds the base table's primary key (the visible `id`, and `_fd_key`) —
    /// what makes a join on it keep the grain.
    row_key: bool,
}

/// The query so far. See the module docs.
#[derive(Debug, Clone)]
pub struct Stage {
    from: Source,
    joins: Vec<Join>,
    filter: Option<Expr>,
    group: Vec<Expr>,
    limit: Option<u64>,
    limit_order: Vec<OrderBy>,
    cols: Vec<Col>,
    /// The order, as hidden columns and directions.
    order: Vec<(String, bool)>,
    grain: Grain,
    /// A `GROUP BY` or a `LIMIT` is in place: the next operation reads this
    /// stage as a subquery.
    closed: bool,
    /// The database has no date, time or UUID types, so casts to them are
    /// casts to text (see `Schema::native_temporal_types`).
    text_casts: bool,
}

/// What a stage's rows are restricted to when read: the rows of the base table
/// a condition over that table selects. What a prediction reads (the rows a
/// caller named, by key or by a filter).
#[derive(Debug, Clone)]
pub struct Restriction {
    /// The base table.
    pub table: String,
    /// Its primary key.
    pub key: String,
    /// A condition over it, its columns written as `"table"."column"`.
    pub filter: Expr,
}

impl Stage {
    /// The stage's columns and grain.
    pub fn shape(&self) -> StageShape {
        StageShape {
            columns: self
                .cols
                .iter()
                .filter(|c| !c.hidden)
                .map(|c| StageColumn {
                    name: c.name.clone(),
                    ty: c.ty,
                    key: c.key.clone(),
                })
                .collect(),
            grain: self.grain.clone(),
        }
    }

    /// Whether rows are rows of the base table and carry its primary key, as
    /// [`ROW_KEY`].
    pub fn has_row_key(&self) -> bool {
        matches!(self.grain, Grain::Table { .. }) && self.col(ROW_KEY).is_some()
    }

    /// The query reading this stage's visible columns — and [`ROW_KEY`] too
    /// when `with_key` — in its order, restricted, one page of it.
    pub fn rows_query(
        &self,
        restrict: Option<&Restriction>,
        page: Option<(u64, u64)>,
        with_key: bool,
    ) -> Result<Select, String> {
        let (sealed, alias) = self.sealed_for_read();
        let mut select = Select::from(sealed.from.clone());
        let mut columns: Vec<Projection> = self
            .cols
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| Projection::expr_as(Expr::qcol(alias.clone(), c.name.clone()), c.name.clone()))
            .collect();
        if with_key && self.has_row_key() {
            columns.push(Projection::expr_as(
                Expr::qcol(alias.clone(), ROW_KEY),
                ROW_KEY,
            ));
        }
        if columns.is_empty() {
            return Err("the dataset has no columns at this stage".to_owned());
        }
        select.columns = columns;
        select.filter = self.restriction(&alias, restrict)?;
        let mut order = sealed.order_exprs();
        order.extend(sealed.tie_break());
        select.order = order;
        if let Some((offset, limit)) = page {
            select.limit = Some(limit);
            if offset > 0 {
                select.offset = Some(offset);
            }
        }
        Ok(self.finish(select))
    }

    /// The query counting this stage's rows, restricted.
    pub fn count_query(&self, restrict: Option<&Restriction>) -> Result<Select, String> {
        let (sealed, alias) = self.sealed_for_read();
        let mut select = Select::from(sealed.from.clone())
            .columns(vec![Projection::expr_as(agg("count", Vec::new()), "n")]);
        select.filter = self.restriction(&alias, restrict)?;
        Ok(self.finish(select))
    }

    /// The distinct values of `column` at this stage, most frequent first, at
    /// most `limit` of them — what a Split's columns are pre-filled from.
    pub fn values_query(&self, column: &str, limit: u64) -> Result<Select, String> {
        if self.col(column).is_none_or(|c| c.hidden) {
            return Err(format!("there is no column `{column}` at this stage"));
        }
        let (sealed, alias) = self.sealed_for_read();
        let value = Expr::qcol(alias, column);
        let mut select = Select::from(sealed.from.clone())
            .columns(vec![
                Projection::expr_as(value.clone(), "value"),
                Projection::expr_as(agg("count", Vec::new()), "n"),
            ])
            .filter(Expr::unary(UnOp::IsNotNull, value.clone()));
        select.group = vec![value.clone()];
        select.order = vec![OrderBy::desc(agg("count", Vec::new())), OrderBy::asc(value)];
        select.limit = Some(limit);
        Ok(self.finish(select))
    }

    /// A query of this stage as the database will run it: on a backend with
    /// no date, time or UUID types, the casts to them become casts to text.
    fn finish(&self, mut select: Select) -> Select {
        if self.text_casts {
            walk::rewrite_select(&mut select, &mut text_cast);
        }
        select
    }

    /// This stage sealed under the alias a read uses: its `from` is the whole
    /// stage as a subquery, and its columns read from it.
    fn sealed_for_read(&self) -> (Stage, String) {
        let alias = "_fd_out".to_owned();
        let mut sealed = self.clone();
        sealed.seal(alias.clone());
        (sealed, alias)
    }

    /// The `WHERE` a restriction becomes on a read of this stage.
    fn restriction(
        &self,
        alias: &str,
        restrict: Option<&Restriction>,
    ) -> Result<Option<Expr>, String> {
        let Some(r) = restrict else {
            return Ok(None);
        };
        if !self.has_row_key() {
            return Err(format!(
                "rows can be asked for by `{}` only while they are rows of it, and {}",
                r.table,
                self.grain.describe()
            ));
        }
        let keys = Select::from(Source::table(r.table.clone()))
            .columns(vec![Projection::expr(Expr::qcol(
                r.table.clone(),
                r.key.clone(),
            ))])
            .filter(r.filter.clone());
        Ok(Some(Expr::In {
            e: Box::new(Expr::qcol(alias, ROW_KEY)),
            set: sc_query::InSet::Subquery(Box::new(keys)),
        }))
    }

    fn col(&self, name: &str) -> Option<&Col> {
        self.cols.iter().find(|c| c.name == name)
    }

    fn visible(&self, name: &str) -> Option<&Col> {
        self.col(name).filter(|c| !c.hidden)
    }

    /// The column `name`, or the sentence saying it is not there.
    fn require(&self, name: &str, what: &str) -> Result<&Col, String> {
        self.visible(name).ok_or_else(|| {
            format!(
                "{what} `{name}` is not a column at this point (the columns are {})",
                self.column_list()
            )
        })
    }

    fn column_list(&self) -> String {
        let names: Vec<String> = self
            .cols
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| format!("`{}`", c.name))
            .collect();
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    }

    /// The select this stage is.
    fn to_select(&self) -> Select {
        let mut select = Select::from(self.from.clone()).columns(
            self.cols
                .iter()
                .map(|c| Projection::expr_as(c.expr.clone(), c.name.clone()))
                .collect(),
        );
        select.joins = self.joins.clone();
        select.filter = self.filter.clone();
        select.group = self.group.clone();
        if self.limit.is_some() {
            select.order = self.limit_order.clone();
            select.limit = self.limit;
        }
        select
    }

    /// Wrap the stage as a subquery aliased `alias`; its columns become plain
    /// references to the subquery's.
    fn seal(&mut self, alias: String) {
        let select = self.to_select();
        self.from = Source::subquery(select, alias.clone());
        self.joins.clear();
        self.filter = None;
        self.group.clear();
        self.limit = None;
        self.limit_order.clear();
        self.closed = false;
        for c in &mut self.cols {
            c.expr = Expr::qcol(alias.clone(), c.name.clone());
        }
    }

    /// Whether an expression reading `refs` can be merged into this stage.
    fn mergeable(&self, refs: &BTreeSet<String>, allow_window: bool) -> bool {
        !self.closed
            && refs.iter().all(|r| {
                self.col(r)
                    .is_none_or(|c| allow_window || !walk::has_window_or_aggregate(&c.expr))
            })
    }

    /// The order, as `ORDER BY` keys over this stage's `FROM`.
    fn order_exprs(&self) -> Vec<OrderBy> {
        self.order
            .iter()
            .filter_map(|(name, desc)| self.col(name).map(|c| sorted(c.expr.clone(), *desc)))
            .collect()
    }

    /// What breaks ties after the order, so every read pages the same way: the
    /// row key, else the group keys, else every column.
    fn tie_break(&self) -> Vec<OrderBy> {
        if self.has_row_key()
            && let Some(key) = self.col(ROW_KEY)
        {
            return vec![sorted(key.expr.clone(), false)];
        }
        if let Grain::Group { keys } = &self.grain {
            let keys: Vec<OrderBy> = keys
                .iter()
                .filter_map(|k| self.col(k))
                .map(|c| sorted(c.expr.clone(), false))
                .collect();
            if !keys.is_empty() {
                return keys;
            }
        }
        self.cols
            .iter()
            .filter(|c| !c.hidden && !matches!(c.ty, ColType::Json | ColType::Bytes))
            .map(|c| sorted(c.expr.clone(), false))
            .collect()
    }

    /// The schema the formula language checks a formula over this stage
    /// against: the database's tables, and the stage as one more.
    fn formula_shape(&self, base: &sc_expr::SchemaShape) -> sc_expr::SchemaShape {
        let mut table = TableShape::new();
        for c in self.cols.iter().filter(|c| !c.hidden) {
            table = match &c.key {
                Some(k) => table.key_field(&c.name, &k.table, &k.field),
                None => table.field(&c.name),
            };
        }
        // Whose rows these are, which is what `Ↄ` needs (A1.2): a table's,
        // identified by the hidden row key, or — after an Aggregate grouped by
        // one foreign key — the referenced table's, identified by that key.
        match &self.grain {
            Grain::Table { table: t, key } if self.has_row_key() => {
                table = table.rows_of(t, key, ROW_KEY);
            }
            Grain::Group { keys } if keys.len() == 1 => {
                if let Some(ForeignKey { table: t, field }) =
                    self.visible(&keys[0]).and_then(|c| c.key.clone())
                {
                    table = table.rows_of(t, field, &keys[0]);
                }
            }
            _ => {}
        }
        base.clone().table(STAGE_NAME, table)
    }

    fn known(&self) -> Vec<Known<'_>> {
        self.cols
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| Known {
                name: &c.name,
                ty: c.ty,
                key: c.key.as_ref(),
            })
            .collect()
    }

    /// Set a visible column, replacing one of the same name in place.
    fn put(&mut self, col: Col) {
        match self.cols.iter_mut().find(|c| c.name == col.name) {
            Some(existing) => *existing = col,
            None => self.cols.push(col),
        }
    }
}

/// A translated formula.
struct Translated {
    expr: Expr,
    ty: ColType,
    key: Option<ForeignKey>,
    row_key: bool,
}

struct Compiler<'a> {
    schema: &'a Schema,
    library: &'a Library,
    options: Options,
    next: Cell<usize>,
}

impl Compiler<'_> {
    /// A fresh name starting with `prefix` — an alias or a hidden column.
    fn fresh(&self, prefix: &str) -> String {
        let n = self.next.get() + 1;
        self.next.set(n);
        format!("{prefix}{n}")
    }

    fn compile_def(&self, def: &DatasetDef, stack: &[DatasetId]) -> Compilation {
        let mut stages = Vec::with_capacity(def.operations.len() + 1);
        let mut operations = Vec::with_capacity(def.operations.len());
        let base = self.base_stage(&def.base, stack);
        let base_report = match &base {
            Ok(stage) => BaseReport {
                shape: Some(stage.shape()),
                error: None,
            },
            Err(e) => BaseReport {
                shape: None,
                error: Some(e.clone()),
            },
        };
        let mut current = base.ok();
        stages.push(current.clone());
        for op in &def.operations {
            let kind = op.op.kind().to_owned();
            let report = match current.take() {
                None => {
                    stages.push(None);
                    OperationReport {
                        id: op.id.clone(),
                        kind,
                        status: OpStatus::NotReached,
                        shape: None,
                        error: None,
                    }
                }
                Some(stage) if !op.enabled => {
                    let shape = stage.shape();
                    stages.push(Some(stage.clone()));
                    current = Some(stage);
                    OperationReport {
                        id: op.id.clone(),
                        kind,
                        status: OpStatus::Disabled,
                        shape: Some(shape),
                        error: None,
                    }
                }
                Some(stage) => match self.apply(stage, &op.op, stack) {
                    Ok(next) => {
                        let shape = next.shape();
                        stages.push(Some(next.clone()));
                        current = Some(next);
                        OperationReport {
                            id: op.id.clone(),
                            kind,
                            status: OpStatus::Ok,
                            shape: Some(shape),
                            error: None,
                        }
                    }
                    Err(e) => {
                        stages.push(None);
                        OperationReport {
                            id: op.id.clone(),
                            kind,
                            status: OpStatus::Invalid,
                            shape: None,
                            error: Some(e),
                        }
                    }
                },
            };
            operations.push(report);
        }
        Compilation {
            base: base_report,
            operations,
            stages,
        }
    }

    /// The stage a base provides.
    fn base_stage(&self, base: &Base, stack: &[DatasetId]) -> Result<Stage, String> {
        match base {
            Base::Table { table } => self.table_stage(table),
            Base::Dataset { dataset } => self.dataset_stage(*dataset, stack, "it is based on"),
        }
    }

    /// The last stage of another dataset, or the sentence saying why it does
    /// not read. `how` says how this one reads it ("it is based on").
    fn dataset_stage(
        &self,
        id: DatasetId,
        stack: &[DatasetId],
        how: &str,
    ) -> Result<Stage, String> {
        if stack.contains(&id) {
            let name = self
                .library
                .get(id)
                .map_or_else(|| id.to_string(), |d| d.name.clone());
            return Err(format!(
                "the dataset `{name}` {how} leads back to itself, and a dataset cannot be \
                 built from its own rows"
            ));
        }
        let def = self
            .library
            .get(id)
            .ok_or_else(|| format!("the dataset {how} no longer exists"))?;
        let mut inner = stack.to_vec();
        inner.push(id);
        let compiled = self.compile_def(def, &inner);
        compiled
            .last()
            .cloned()
            .map_err(|e| format!("the dataset `{}` {how} does not read: {e}", def.name))
    }

    /// The rows of a table: its columns under the base alias, and its primary
    /// key as the hidden row key.
    fn table_stage(&self, name: &str) -> Result<Stage, String> {
        let info = self.schema.table(name)?;
        let user = UserEnv::Inline(None);
        let env = Env::new(&user).with_calc(&info.calc);
        let mut cols = Vec::with_capacity(info.columns.len() + 1);
        for column in &info.columns {
            let expr = match info.calc.get(&column.name) {
                // A non-stored calculated field is its formula, inlined; one
                // that does not become SQL is not a column of the dataset,
                // which is one query.
                Some(formula) => {
                    match translate_value_rooted(
                        formula,
                        &env,
                        &self.schema.shape,
                        name,
                        BASE_ALIAS,
                    ) {
                        Ok(expr) => expr,
                        Err(_) => continue,
                    }
                }
                None => Expr::qcol(BASE_ALIAS, column.name.clone()),
            };
            cols.push(Col {
                name: column.name.clone(),
                expr,
                ty: column.ty,
                key: column.key.clone(),
                hidden: false,
                row_key: info.primary_key.as_deref() == Some(column.name.as_str()),
            });
        }
        let grain = match &info.primary_key {
            Some(pk) => {
                cols.push(Col {
                    name: ROW_KEY.to_owned(),
                    expr: Expr::qcol(BASE_ALIAS, pk.clone()),
                    ty: info.column(pk).map_or(ColType::Unknown, |c| c.ty),
                    key: None,
                    hidden: true,
                    row_key: true,
                });
                Grain::Table {
                    table: name.to_owned(),
                    key: pk.clone(),
                }
            }
            None => Grain::Derived,
        };
        Ok(Stage {
            from: Source::table_as(name, BASE_ALIAS),
            joins: Vec::new(),
            filter: None,
            group: Vec::new(),
            limit: None,
            limit_order: Vec::new(),
            cols,
            order: Vec::new(),
            grain,
            closed: false,
            text_casts: !self.schema.native_temporal_types,
        })
    }

    fn apply(&self, stage: Stage, op: &Op, stack: &[DatasetId]) -> Result<Stage, String> {
        match op {
            Op::Calculated(c) => self.calculated(stage, c),
            Op::Filter(_) | Op::Limit(_) if self.options.skip_filters => Ok(stage),
            Op::Filter(f) => self.filter(stage, f),
            Op::Select(s) => select(stage, s),
            Op::Sort(s) => self.sort(stage, s),
            Op::Window(w) => self.window(stage, w),
            Op::Aggregate(a) => self.aggregate(stage, a),
            Op::Limit(l) => self.limit(stage, l),
            Op::Stack(s) => self.stack(stage, s),
            Op::Split(s) => self.split(stage, s),
            Op::Complete(c) => self.complete(stage, c),
            Op::Join(j) => self.join(stage, j, stack),
            Op::Union(u) => self.union(stage, u, stack),
        }
    }

    /// Seal `stage` under a fresh alias.
    fn seal(&self, stage: &mut Stage) {
        stage.seal(self.fresh("_fd_s"));
    }

    /// Translate `source` over `stage`, merging it or sealing the stage first
    /// (see the module docs). `predicate` translates it as a condition.
    fn translate(
        &self,
        stage: &mut Stage,
        source: &str,
        predicate: bool,
        allow_window: bool,
    ) -> Result<Translated, String> {
        if source.trim().is_empty() {
            return Err("the formula is empty".to_owned());
        }
        let formula = Formula::parse(source)
            .map_err(|e| format!("`{source}` is not a formula: {}", plain(&e.to_string())))?;
        let shape = stage.formula_shape(&self.schema.shape);
        let analysis = formula.validate(&shape, STAGE_NAME).map_err(|e| {
            let message = e.to_string();
            // An aggregation over a child table needs rows that are rows of
            // the table the child points at; say what a row is instead.
            if source.contains(sc_expr::INVERSE) && message.contains("points at") {
                format!(
                    "{}; an aggregation over a child table needs each row to be a row of the                      table it points at, and here {}",
                    plain(&message),
                    stage.grain.describe()
                )
            } else {
                plain(&message)
            }
        })?;
        if analysis.uses(Ambient::User) || !analysis.flags.is_empty() {
            return Err(
                "a dataset cannot use `user` or the operation flags: it has no caller, and its \
                 rows must not depend on who reads them"
                    .to_owned(),
            );
        }
        if !analysis.model_calls.is_empty() {
            return Err(
                "a dataset formula cannot call `predict`: a model's predictions are added by \
                 a Model predictions operation (milestone A7)"
                    .to_owned(),
            );
        }
        let user = UserEnv::Inline(None);
        let env = Env::new(&user);
        let translated = if predicate {
            translate_rooted(
                &formula,
                FormulaOperation::Read,
                &env,
                &shape,
                STAGE_NAME,
                ROW,
            )
        } else {
            translate_value_rooted(&formula, &env, &shape, STAGE_NAME, ROW)
        };
        let mut expr = translated.map_err(|e| match e {
            TranslateError::Untranslatable(what) => {
                format!("`{source}` cannot be computed by the database ({what})")
            }
            TranslateError::Error(e) => plain(&e.to_string()),
        })?;
        let refs = walk::columns_of(&expr, ROW);
        if !stage.mergeable(&refs, allow_window) {
            self.seal(stage);
        }
        walk::substitute(&mut expr, ROW, &|c| stage.col(c).map(|c| c.expr.clone()));
        let (ty, key) = infer(formula.ast(), &stage.known(), self.schema);
        let row_key = matches!(
            formula.ast(),
            sc_expr::Ast::Ident(n) if stage.visible(n).is_some_and(|c| c.row_key)
        );
        Ok(Translated {
            expr,
            ty: if predicate { ColType::Bool } else { ty },
            key,
            row_key,
        })
    }

    // --- the operations that keep the grain (A1.3) -------------------------

    fn calculated(&self, mut stage: Stage, c: &CalculatedOp) -> Result<Stage, String> {
        let name = column_name(&c.name, "the new column")?;
        let t = self.translate(&mut stage, &c.formula, false, true)?;
        stage.put(Col {
            name,
            expr: t.expr,
            ty: t.ty,
            key: t.key,
            hidden: false,
            row_key: t.row_key,
        });
        Ok(stage)
    }

    fn filter(&self, mut stage: Stage, f: &FilterOp) -> Result<Stage, String> {
        let t = self.translate(&mut stage, &f.formula, true, false)?;
        stage.filter = Some(match stage.filter.take() {
            Some(existing) => existing.and(t.expr),
            None => t.expr,
        });
        Ok(stage)
    }

    fn sort(&self, mut stage: Stage, s: &SortOp) -> Result<Stage, String> {
        if s.keys.is_empty() {
            return Err("a sort needs at least one key".to_owned());
        }
        let mut keys = Vec::with_capacity(s.keys.len());
        for key in &s.keys {
            let t = self.translate(&mut stage, &key.formula, false, true)?;
            if matches!(t.ty, ColType::Json | ColType::Bytes) {
                return Err(format!(
                    "`{}` is {}, which has no order to sort by",
                    key.formula,
                    t.ty.name()
                ));
            }
            keys.push((t, key.descending));
        }
        // Stable: rows that tie on every new key keep the order they had.
        let mut order = Vec::with_capacity(keys.len() + stage.order.len());
        for (t, descending) in keys {
            let name = self.fresh("_fd_o");
            stage.cols.push(hidden(&name, t.expr, t.ty));
            order.push((name, descending));
        }
        order.append(&mut stage.order);
        stage.order = order;
        Ok(stage)
    }

    fn window(&self, mut stage: Stage, w: &WindowOp) -> Result<Stage, String> {
        let name = column_name(&w.name, "the new column")?;
        let column = match (&w.column, w.function.needs_column()) {
            (Some(c), true) => Some(stage.require(c, "the column")?.clone()),
            (None, true) => return Err("this window function needs a column".to_owned()),
            (_, false) => None,
        };
        let numeric = matches!(
            w.function,
            WindowFunction::Difference
                | WindowFunction::CumulativeSum
                | WindowFunction::CumulativeMean
                | WindowFunction::GroupSum
                | WindowFunction::GroupMean
                | WindowFunction::Share
        );
        if let Some(c) = &column
            && numeric
            && !c.ty.is_numeric()
            && c.ty != ColType::Unknown
        {
            return Err(format!(
                "`{}` is {}, and this window function needs a number",
                c.name,
                c.ty.name()
            ));
        }
        let mut refs: BTreeSet<String> = w.partition.iter().cloned().collect();
        for key in &w.order {
            stage.require(&key.column, "the order column")?;
            refs.insert(key.column.clone());
        }
        for p in &w.partition {
            stage.require(p, "the group column")?;
        }
        refs.extend(column.iter().map(|c| c.name.clone()));
        refs.extend(stage.order.iter().map(|(n, _)| n.clone()));
        refs.insert(ROW_KEY.to_owned());
        if !stage.mergeable(&refs, false) {
            self.seal(&mut stage);
        }
        if w.function == WindowFunction::Fill {
            // The last value that was not missing: number the runs that start
            // at each value (`count` of the values so far), then take each
            // run's first value.
            let run = self.fresh("_fd_w");
            let (partition, order) = window_frame(&stage, w, true);
            let value = stage
                .require(&w.column.clone().unwrap_or_default(), "the column")?
                .expr
                .clone();
            let counted = win("count", vec![value], partition, order);
            stage.cols.push(hidden(&run, counted, ColType::Int));
            self.seal(&mut stage);
            let (mut partition, order) = window_frame(&stage, w, true);
            partition.push(
                stage
                    .col(&run)
                    .map(|c| c.expr.clone())
                    .unwrap_or(Expr::lit(0_i64)),
            );
            let c = column.as_ref().map(|c| c.name.clone()).unwrap_or_default();
            let value = stage.require(&c, "the column")?.clone();
            stage.put(Col {
                name,
                expr: win("first_value", vec![value.expr.clone()], partition, order),
                ty: value.ty,
                key: value.key.clone(),
                hidden: false,
                row_key: false,
            });
            return Ok(stage);
        }
        let ordered = !matches!(
            w.function,
            WindowFunction::GroupSum
                | WindowFunction::GroupMean
                | WindowFunction::GroupCount
                | WindowFunction::GroupMin
                | WindowFunction::GroupMax
                | WindowFunction::Share
        );
        let (partition, order) = window_frame(&stage, w, w.function != WindowFunction::Rank);
        let order = if ordered { order } else { Vec::new() };
        let value = column
            .as_ref()
            .and_then(|c| stage.col(&c.name))
            .map(|c| c.expr.clone());
        let v = || value.clone().unwrap_or(Expr::lit(Value::Null));
        let offset = i64::from(w.offset.unwrap_or(1).max(1));
        let (expr, ty, key) = match w.function {
            WindowFunction::Lag | WindowFunction::Lead => {
                let func = if w.function == WindowFunction::Lag {
                    "lag"
                } else {
                    "lead"
                };
                let c = column.as_ref();
                (
                    win(func, vec![v(), offset_arg(offset)], partition, order),
                    c.map_or(ColType::Unknown, |c| c.ty),
                    c.and_then(|c| c.key.clone()),
                )
            }
            WindowFunction::Difference => (
                Expr::binary(
                    BinOp::Sub,
                    v(),
                    win("lag", vec![v(), offset_arg(1)], partition, order),
                ),
                column.as_ref().map_or(ColType::Float, |c| c.ty),
                None,
            ),
            WindowFunction::CumulativeSum | WindowFunction::GroupSum => (
                win("sum", vec![v()], partition, order),
                column.as_ref().map_or(ColType::Float, |c| c.ty),
                None,
            ),
            WindowFunction::CumulativeMean | WindowFunction::GroupMean => (
                win("avg", vec![v()], partition, order),
                ColType::Float,
                None,
            ),
            WindowFunction::Rank => (
                win("rank", Vec::new(), partition, order),
                ColType::Int,
                None,
            ),
            WindowFunction::RowNumber => (
                win("row_number", Vec::new(), partition, order),
                ColType::Int,
                None,
            ),
            WindowFunction::GroupCount => (
                win("count", vec![v()], partition, order),
                ColType::Int,
                None,
            ),
            WindowFunction::GroupMin | WindowFunction::GroupMax => {
                let func = if w.function == WindowFunction::GroupMin {
                    "min"
                } else {
                    "max"
                };
                let c = column.as_ref();
                (
                    win(func, vec![v()], partition, order),
                    c.map_or(ColType::Unknown, |c| c.ty),
                    c.and_then(|c| c.key.clone()),
                )
            }
            WindowFunction::Share => (
                Expr::binary(
                    BinOp::Div,
                    Expr::binary(BinOp::Mul, v(), typed(Value::Float(1.0), ColType::Float)),
                    Expr::Func {
                        name: "nullif".into(),
                        args: vec![
                            win("sum", vec![v()], partition, order),
                            typed(Value::Int(0), ColType::Int),
                        ],
                    },
                ),
                ColType::Float,
                None,
            ),
            WindowFunction::Fill => unreachable!("handled above"),
        };
        stage.put(Col {
            name,
            expr,
            ty,
            key,
            hidden: false,
            row_key: false,
        });
        Ok(stage)
    }

    // --- the operations that change the grain (A1.4) ----------------------

    fn aggregate(&self, mut stage: Stage, a: &AggregateOp) -> Result<Stage, String> {
        if a.group_by.is_empty() && a.summaries.is_empty() {
            return Err("an aggregate needs a group key or a summary".to_owned());
        }
        let mut names = BTreeSet::new();
        for name in a
            .group_by
            .iter()
            .map(|g| &g.name)
            .chain(a.summaries.iter().map(|s| &s.name))
        {
            let name = column_name(name, "a result column")?;
            if !names.insert(name.clone()) {
                return Err(format!("two result columns are called `{name}`"));
            }
        }
        // Check the summaries against the columns before anything is built,
        // so the sentence names the summary rather than a SQL error.
        let mut helpers_needed = false;
        for s in &a.summaries {
            let column = match &s.column {
                Some(c) => Some(stage.visible(c).ok_or_else(|| {
                    format!(
                        "the summary `{}` reads `{c}`, and that is not a column at this point \
                         (the columns are {})",
                        s.name,
                        stage.column_list()
                    )
                })?),
                None if s.function == SummaryFunction::Count => None,
                None => {
                    return Err(format!("the summary `{}` needs a column", s.name));
                }
            };
            if let Some(c) = column {
                check_summary(s.function, c)?;
            }
            if let Some(order) = &s.order {
                stage.require(&order.column, "the order column")?;
            }
            helpers_needed |= matches!(
                s.function,
                SummaryFunction::Median | SummaryFunction::First | SummaryFunction::Last
            );
        }
        // Group keys are formulas; a key that reads a window must not be
        // grouped on in the same select, so anything not mergeable seals.
        if stage.closed
            || stage
                .cols
                .iter()
                .any(|c| walk::has_window_or_aggregate(&c.expr))
        {
            self.seal(&mut stage);
        }
        let mut keys = Vec::with_capacity(a.group_by.len());
        for g in &a.group_by {
            let t = self.translate(&mut stage, &g.formula, false, false)?;
            if matches!(t.ty, ColType::Json) {
                return Err(format!(
                    "`{}` is JSON, which cannot be grouped by",
                    g.formula
                ));
            }
            keys.push((g.name.trim().to_owned(), t));
        }
        // The median, first and last are taken with window functions over
        // each group, computed a level down and aggregated here (so the same
        // SQL works on both databases, neither of which agrees with the other
        // on a median aggregate).
        let mut helper_cols: BTreeMap<usize, (String, String)> = BTreeMap::new();
        if helpers_needed {
            let partition: Vec<Expr> = keys.iter().map(|(_, t)| t.expr.clone()).collect();
            for (i, s) in a.summaries.iter().enumerate() {
                let Some(c) = &s.column else { continue };
                let value = stage
                    .col(c)
                    .map(|c| c.expr.clone())
                    .unwrap_or(Expr::lit(Value::Null));
                match s.function {
                    SummaryFunction::Median => {
                        let rank = self.fresh("_fd_w");
                        let count = self.fresh("_fd_w");
                        stage.cols.push(hidden(
                            &rank,
                            Expr::row_number(partition.clone(), vec![sorted(value.clone(), false)]),
                            ColType::Int,
                        ));
                        stage.cols.push(hidden(
                            &count,
                            win("count", vec![value], partition.clone(), Vec::new()),
                            ColType::Int,
                        ));
                        helper_cols.insert(i, (rank, count));
                    }
                    SummaryFunction::First | SummaryFunction::Last => {
                        let rank = self.fresh("_fd_w");
                        let mut order: Vec<OrderBy> = match &s.order {
                            Some(OrderKey { column, descending }) => {
                                let e = stage.col(column).map(|c| c.expr.clone());
                                e.map(|e| vec![sorted(e, *descending)]).unwrap_or_default()
                            }
                            None => stage.order_exprs(),
                        };
                        order.extend(stage.tie_break());
                        if s.function == SummaryFunction::Last {
                            for o in &mut order {
                                o.dir = match o.dir {
                                    OrderDir::Asc => OrderDir::Desc,
                                    OrderDir::Desc => OrderDir::Asc,
                                };
                                o.nulls = Some(match o.nulls {
                                    Some(Nulls::Last) | None => Nulls::First,
                                    Some(Nulls::First) => Nulls::Last,
                                });
                            }
                        }
                        stage.cols.push(hidden(
                            &rank,
                            Expr::row_number(partition.clone(), order),
                            ColType::Int,
                        ));
                        helper_cols.insert(i, (rank, String::new()));
                    }
                    _ => {}
                }
            }
            // Key expressions are re-read from the sealed stage below.
            let key_names: Vec<String> = keys.iter().map(|(n, _)| n.clone()).collect();
            for (name, t) in &keys {
                stage
                    .cols
                    .push(hidden(&format!("_fd_g_{name}"), t.expr.clone(), t.ty));
            }
            self.seal(&mut stage);
            for (name, t) in keys.iter_mut() {
                if let Some(c) = stage.col(&format!("_fd_g_{name}")) {
                    t.expr = c.expr.clone();
                }
            }
            let _ = key_names;
        }
        let mut cols = Vec::with_capacity(keys.len() * 2 + a.summaries.len());
        let mut order = Vec::with_capacity(keys.len());
        for (name, t) in &keys {
            cols.push(Col {
                name: name.clone(),
                expr: t.expr.clone(),
                ty: t.ty,
                key: t.key.clone(),
                hidden: false,
                row_key: false,
            });
        }
        for (i, s) in a.summaries.iter().enumerate() {
            let column = s.column.as_ref().and_then(|c| stage.col(c)).cloned();
            let value = column.as_ref().map(|c| c.expr.clone());
            let helper = |n: &str| {
                stage
                    .col(n)
                    .map(|c| c.expr.clone())
                    .unwrap_or(Expr::lit(Value::Null))
            };
            let (expr, ty, key) = match s.function {
                SummaryFunction::Count => (
                    agg("count", value.into_iter().collect()),
                    ColType::Int,
                    None,
                ),
                SummaryFunction::CountDistinct => (
                    Expr::Agg {
                        func: "count".into(),
                        distinct: true,
                        args: value.into_iter().collect(),
                    },
                    ColType::Int,
                    None,
                ),
                SummaryFunction::Sum => (
                    agg("sum", value.into_iter().collect()),
                    column.as_ref().map_or(ColType::Float, |c| c.ty),
                    None,
                ),
                SummaryFunction::Mean => (
                    agg("avg", value.into_iter().collect()),
                    ColType::Float,
                    None,
                ),
                SummaryFunction::Sd => (
                    agg("stddev_samp", value.into_iter().collect()),
                    ColType::Float,
                    None,
                ),
                SummaryFunction::Min | SummaryFunction::Max => (
                    agg(
                        if s.function == SummaryFunction::Min {
                            "min"
                        } else {
                            "max"
                        },
                        value.into_iter().collect(),
                    ),
                    column.as_ref().map_or(ColType::Unknown, |c| c.ty),
                    column.as_ref().and_then(|c| c.key.clone()),
                ),
                SummaryFunction::Median => {
                    let (rank, count) = helper_cols.get(&i).cloned().unwrap_or_default();
                    let (rank, count) = (helper(&rank), helper(&count));
                    let two = || typed(Value::Int(2), ColType::Int);
                    let one = || typed(Value::Int(1), ColType::Int);
                    let lower = Expr::binary(
                        BinOp::Div,
                        Expr::binary(BinOp::Add, count.clone(), one()),
                        two(),
                    );
                    let upper =
                        Expr::binary(BinOp::Div, Expr::binary(BinOp::Add, count, two()), two());
                    let middle = Expr::In {
                        e: Box::new(rank),
                        set: sc_query::InSet::List(vec![lower, upper]),
                    };
                    (
                        agg(
                            "avg",
                            vec![Expr::binary(
                                BinOp::Mul,
                                case_when(middle, value.unwrap_or(Expr::lit(Value::Null))),
                                typed(Value::Float(1.0), ColType::Float),
                            )],
                        ),
                        ColType::Float,
                        None,
                    )
                }
                SummaryFunction::First | SummaryFunction::Last => {
                    let (rank, _) = helper_cols.get(&i).cloned().unwrap_or_default();
                    let first =
                        Expr::binary(BinOp::Eq, helper(&rank), typed(Value::Int(1), ColType::Int));
                    let c = column.as_ref();
                    let ty = c.map_or(ColType::Unknown, |c| c.ty);
                    // `max` over the one row numbered 1; a boolean has no
                    // `max` in Postgres, so it goes through an integer.
                    let expr = if ty == ColType::Bool {
                        Expr::binary(
                            BinOp::Eq,
                            agg(
                                "max",
                                vec![case_when(
                                    first,
                                    Expr::Case {
                                        operand: None,
                                        arms: vec![CaseArm {
                                            when: value.clone().unwrap_or(Expr::lit(Value::Null)),
                                            then: typed(Value::Int(1), ColType::Int),
                                        }],
                                        else_result: Some(Box::new(typed(
                                            Value::Int(0),
                                            ColType::Int,
                                        ))),
                                    },
                                )],
                            ),
                            typed(Value::Int(1), ColType::Int),
                        )
                    } else {
                        agg(
                            "max",
                            vec![case_when(first, value.unwrap_or(Expr::lit(Value::Null)))],
                        )
                    };
                    (expr, ty, c.and_then(|c| c.key.clone()))
                }
            };
            cols.push(Col {
                name: s.name.trim().to_owned(),
                expr,
                ty,
                key,
                hidden: false,
                row_key: false,
            });
        }
        for (name, t) in &keys {
            let hidden_name = self.fresh("_fd_o");
            cols.push(hidden(&hidden_name, t.expr.clone(), t.ty));
            order.push((hidden_name, false));
            let _ = name;
        }
        stage.group = keys.iter().map(|(_, t)| t.expr.clone()).collect();
        stage.cols = cols;
        stage.order = order;
        stage.grain = Grain::Group {
            keys: keys.iter().map(|(n, _)| n.clone()).collect(),
        };
        stage.closed = true;
        Ok(stage)
    }

    fn limit(&self, mut stage: Stage, l: &LimitOp) -> Result<Stage, String> {
        if l.n == 0 {
            return Err("a limit of 0 rows keeps nothing".to_owned());
        }
        match l.mode {
            LimitMode::First => {
                if stage.closed {
                    self.seal(&mut stage);
                }
                let mut order = stage.order_exprs();
                order.extend(stage.tie_break());
                stage.limit_order = order;
                stage.limit = Some(l.n);
                stage.closed = true;
            }
            LimitMode::Sample => {
                // A random sample reproducible from its seed on both
                // databases, neither of which can seed `random()` per query:
                // each row's position in the order is scrambled by
                // multiply-and-square steps modulo the prime 2³¹−1 (every
                // product fits in 63 bits), and the smallest N are kept.
                if stage.closed {
                    self.seal(&mut stage);
                }
                let mut order = stage.order_exprs();
                order.extend(stage.tie_break());
                let position = self.fresh("_fd_w");
                stage.cols.push(hidden(
                    &position,
                    Expr::row_number(Vec::new(), order),
                    ColType::Int,
                ));
                self.seal(&mut stage);
                let p = stage
                    .col(&position)
                    .map(|c| c.expr.clone())
                    .unwrap_or(Expr::lit(0_i64));
                const M: i64 = 2_147_483_647;
                let int = |v: i64| typed(Value::Int(v), ColType::Int);
                let modm = |e: Expr| Expr::binary(BinOp::Mod, e, int(M));
                let seed = l.seed.rem_euclid(M);
                let x1 = modm(Expr::binary(
                    BinOp::Add,
                    Expr::binary(BinOp::Mul, modm(p), int(48_271)),
                    int(seed),
                ));
                let x2 = modm(Expr::binary(
                    BinOp::Add,
                    Expr::binary(BinOp::Mul, x1.clone(), x1),
                    int(seed.wrapping_mul(7_919).rem_euclid(M)),
                ));
                let x3 = modm(Expr::binary(BinOp::Mul, x2, int(16_807)));
                let mut order = vec![sorted(x3, false)];
                order.extend(stage.tie_break());
                stage.limit_order = order;
                stage.limit = Some(l.n);
                stage.closed = true;
            }
            LimitMode::Top => {
                for g in &l.group_by {
                    stage.require(g, "the group column")?;
                }
                for k in &l.order {
                    stage.require(&k.column, "the order column")?;
                }
                if stage.closed
                    || stage
                        .cols
                        .iter()
                        .any(|c| walk::has_window_or_aggregate(&c.expr))
                {
                    self.seal(&mut stage);
                }
                let partition = l
                    .group_by
                    .iter()
                    .filter_map(|g| stage.col(g).map(|c| c.expr.clone()))
                    .collect();
                let mut order: Vec<OrderBy> = if l.order.is_empty() {
                    stage.order_exprs()
                } else {
                    l.order
                        .iter()
                        .filter_map(|k| {
                            stage
                                .col(&k.column)
                                .map(|c| sorted(c.expr.clone(), k.descending))
                        })
                        .collect()
                };
                order.extend(stage.tie_break());
                let position = self.fresh("_fd_w");
                stage.cols.push(hidden(
                    &position,
                    Expr::row_number(partition, order),
                    ColType::Int,
                ));
                self.seal(&mut stage);
                let p = stage
                    .col(&position)
                    .map(|c| c.expr.clone())
                    .unwrap_or(Expr::lit(0_i64));
                let n = i64::try_from(l.n).unwrap_or(i64::MAX);
                stage.filter = Some(Expr::binary(
                    BinOp::Le,
                    p,
                    typed(Value::Int(n), ColType::Int),
                ));
            }
        }
        Ok(stage)
    }

    fn stack(&self, mut stage: Stage, s: &StackOp) -> Result<Stage, String> {
        if s.columns.is_empty() {
            return Err("a stack needs at least one column".to_owned());
        }
        let names_to = column_name(&s.names_to, "the name column")?;
        let values_to = column_name(&s.values_to, "the value column")?;
        if names_to == values_to {
            return Err("the name column and the value column need different names".to_owned());
        }
        let mut ty = ColType::Unknown;
        let mut first: Option<&str> = None;
        for c in &s.columns {
            let col = stage.require(c, "the stacked column")?;
            if !ty.compatible(col.ty) {
                return Err(format!(
                    "stacked columns become one column, so they must be of one kind: `{}` is {} \
                     and `{c}` is {}",
                    first.unwrap_or(c),
                    ty.name(),
                    col.ty.name()
                ));
            }
            ty = ty.unify(col.ty);
            first.get_or_insert(c);
        }
        let stacked: BTreeSet<&str> = s.columns.iter().map(String::as_str).collect();
        let kept: Vec<Col> = stage
            .cols
            .iter()
            .filter(|c| !stacked.contains(c.name.as_str()))
            .cloned()
            .collect();
        for c in &kept {
            if !c.hidden && (c.name == names_to || c.name == values_to) {
                return Err(format!(
                    "`{}` is already a column; name the stacked columns' name or value column \
                     something else",
                    c.name
                ));
            }
        }
        self.seal(&mut stage);
        let position = self.fresh("_fd_o");
        let parts: Vec<Select> = s
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut columns: Vec<Projection> = kept
                    .iter()
                    .filter_map(|k| stage.col(&k.name))
                    .map(|k| Projection::expr_as(k.expr.clone(), k.name.clone()))
                    .collect();
                columns.push(Projection::expr_as(
                    typed(Value::Text(c.clone()), ColType::Text),
                    names_to.clone(),
                ));
                let value = stage
                    .col(c)
                    .map(|c| c.expr.clone())
                    .unwrap_or(Expr::lit(Value::Null));
                columns.push(Projection::expr_as(cast(value, ty), values_to.clone()));
                columns.push(Projection::expr_as(
                    typed(Value::Int(i as i64), ColType::Int),
                    position.clone(),
                ));
                let mut part = Select::from(stage.from.clone()).columns(columns);
                part.joins = stage.joins.clone();
                part.filter = stage.filter.clone();
                part
            })
            .collect();
        let alias = self.fresh("_fd_s");
        let mut cols: Vec<Col> = kept
            .into_iter()
            .map(|c| Col {
                expr: Expr::qcol(alias.clone(), c.name.clone()),
                ..c
            })
            .collect();
        let at = cols.iter().position(|c| c.hidden).unwrap_or(cols.len());
        cols.insert(
            at,
            Col {
                name: names_to.clone(),
                expr: Expr::qcol(alias.clone(), names_to.clone()),
                ty: ColType::Text,
                key: None,
                hidden: false,
                row_key: false,
            },
        );
        cols.insert(
            at + 1,
            Col {
                name: values_to.clone(),
                expr: Expr::qcol(alias.clone(), values_to.clone()),
                ty,
                key: None,
                hidden: false,
                row_key: false,
            },
        );
        cols.push(Col {
            expr: Expr::qcol(alias.clone(), position.clone()),
            ..hidden(&position, Expr::lit(0_i64), ColType::Int)
        });
        // Each original row's stacked values together, in the stacked
        // columns' order: the order the rows had, their row key, then the
        // column's position.
        let mut order = stage.order.clone();
        if stage.col(ROW_KEY).is_some() {
            order.push((ROW_KEY.to_owned(), false));
        }
        order.push((position, false));
        Ok(Stage {
            from: Source::union_all(parts, alias),
            joins: Vec::new(),
            filter: None,
            group: Vec::new(),
            limit: None,
            limit_order: Vec::new(),
            cols,
            order,
            grain: Grain::Derived,
            closed: false,
            text_casts: stage.text_casts,
        })
    }

    fn split(&self, mut stage: Stage, s: &SplitOp) -> Result<Stage, String> {
        let names = stage.require(&s.names_from, "the name column")?.clone();
        let values = stage.require(&s.values_from, "the value column")?.clone();
        if s.id_columns.is_empty() {
            return Err("a split needs at least one column identifying a row".to_owned());
        }
        for c in &s.id_columns {
            stage.require(c, "the identifying column")?;
            if *c == s.names_from || *c == s.values_from {
                return Err(format!(
                    "`{c}` cannot identify a row and be split at the same time"
                ));
            }
        }
        if s.values.is_empty() {
            return Err(
                "a split has no new columns yet: read the values of the name column from the \
                 data first"
                    .to_owned(),
            );
        }
        let mut seen: BTreeSet<&str> = s.id_columns.iter().map(String::as_str).collect();
        for v in &s.values {
            column_name(v, "a new column")?;
            if !seen.insert(v.as_str()) {
                return Err(format!(
                    "the new column `{v}` would be there twice (it is a value of `{}` and \
                     already a column, or listed twice)",
                    s.names_from
                ));
            }
        }
        if matches!(s.summary, SplitSummary::Sum | SplitSummary::Mean) && !values.ty.is_numeric() {
            return Err(format!(
                "`{}` is {}, which cannot be added up",
                values.name,
                values.ty.name()
            ));
        }
        self.seal(&mut stage);
        let name_expr = stage
            .col(&names.name)
            .map(|c| c.expr.clone())
            .unwrap_or(Expr::lit(Value::Null));
        let value_expr = stage
            .col(&values.name)
            .map(|c| c.expr.clone())
            .unwrap_or(Expr::lit(Value::Null));
        // The name column compared as text, since the new columns' names are
        // text; a text column is compared as it is.
        let name_text = if names.ty == ColType::Text {
            name_expr
        } else {
            cast(name_expr, ColType::Text)
        };
        let mut cols = Vec::new();
        let mut order = Vec::new();
        let mut group = Vec::new();
        for c in &s.id_columns {
            let col = stage
                .col(c)
                .cloned()
                .ok_or_else(|| format!("`{c}` is not a column"))?;
            group.push(col.expr.clone());
            let o = self.fresh("_fd_o");
            order.push((o.clone(), false));
            cols.push(Col {
                hidden: false,
                row_key: false,
                ..col.clone()
            });
            cols.push(hidden(&o, col.expr, col.ty));
        }
        let (func, ty) = match s.summary {
            SplitSummary::First | SplitSummary::Max => ("max", values.ty),
            SplitSummary::Min => ("min", values.ty),
            SplitSummary::Sum => ("sum", values.ty),
            SplitSummary::Mean => ("avg", ColType::Float),
            SplitSummary::Count => ("count", ColType::Int),
        };
        if matches!(func, "max" | "min") && values.ty == ColType::Bool {
            return Err(format!(
                "`{}` is true or false, which has no largest value; count it instead",
                values.name
            ));
        }
        let at = cols.iter().position(|c| c.hidden).unwrap_or(cols.len());
        let mut new_cols = Vec::with_capacity(s.values.len());
        for v in &s.values {
            let matches = Expr::binary(
                BinOp::Eq,
                name_text.clone(),
                typed(Value::Text(v.clone()), ColType::Text),
            );
            new_cols.push(Col {
                name: v.clone(),
                expr: agg(func, vec![case_when(matches, value_expr.clone())]),
                ty,
                key: if func == "count" {
                    None
                } else {
                    values.key.clone()
                },
                hidden: false,
                row_key: false,
            });
        }
        cols.splice(at..at, new_cols);
        stage.group = group;
        stage.cols = cols;
        stage.order = order;
        stage.grain = Grain::Group {
            keys: s.id_columns.clone(),
        };
        stage.closed = true;
        Ok(stage)
    }

    fn complete(&self, mut stage: Stage, c: &CompleteOp) -> Result<Stage, String> {
        if c.columns.is_empty() {
            return Err("a complete needs at least one column".to_owned());
        }
        let mut completed = BTreeSet::new();
        for column in &c.columns {
            stage.require(&column.column, "the completed column")?;
            if !completed.insert(column.column.as_str()) {
                return Err(format!("`{}` is completed twice", column.column));
            }
        }
        for f in &c.fill {
            stage.require(&f.column, "the filled column")?;
            if completed.contains(f.column.as_str()) {
                return Err(format!(
                    "`{}` is completed, so it is never missing and has nothing to fill",
                    f.column
                ));
            }
        }
        self.seal(&mut stage);
        let data = stage.to_select();
        // One derived table of values per completed column, crossed.
        let mut sources: Vec<Source> = Vec::with_capacity(c.columns.len());
        for column in &c.columns {
            let col = stage
                .col(&column.column)
                .cloned()
                .ok_or("the column vanished")?;
            let alias = self.fresh("_fd_v");
            let values: Select = match &column.values {
                CompleteValues::Data => {
                    let inner = self.fresh("_fd_d");
                    let mut s =
                        Select::from(Source::subquery(data.clone(), inner.clone())).columns(vec![
                            Projection::expr_as(Expr::qcol(inner.clone(), col.name.clone()), "v"),
                        ]);
                    s.group = vec![Expr::qcol(inner, col.name.clone())];
                    s
                }
                CompleteValues::Table => {
                    let key = col.key.clone().ok_or_else(|| {
                        format!(
                            "`{}` is not a foreign key, so there is no table to take its values \
                             from",
                            col.name
                        )
                    })?;
                    Select::from(Source::table(key.table.clone())).columns(vec![
                        Projection::expr_as(Expr::qcol(key.table, key.field), "v"),
                    ])
                }
                CompleteValues::Range { from, to, step } => {
                    let values = range_values(col.ty, from, to, step.as_ref())
                        .map_err(|e| format!("the range of `{}`: {e}", col.name))?;
                    let parts: Vec<Select> = values
                        .into_iter()
                        .map(|v| {
                            Select::from(Source::Nothing)
                                .columns(vec![Projection::expr_as(typed(v, col.ty), "v")])
                        })
                        .collect();
                    let union = self.fresh("_fd_u");
                    Select::from(Source::union_all(parts, union.clone()))
                        .columns(vec![Projection::expr_as(Expr::qcol(union, "v"), "v")])
                }
            };
            sources.push(Source::subquery(values, alias));
        }
        // Every combination of the values, with a marker column that says a
        // row of the data matched one.
        let mut combos = Select::from(sources[0].clone()).columns(
            c.columns
                .iter()
                .zip(&sources)
                .map(|(column, src)| {
                    let Source::Subquery { alias, .. } = src else {
                        unreachable!()
                    };
                    Projection::expr_as(Expr::qcol(alias.clone(), "v"), column.column.clone())
                })
                .chain(std::iter::once(Projection::expr_as(
                    typed(Value::Int(1), ColType::Int),
                    "_fd_m",
                )))
                .collect(),
        );
        for src in &sources[1..] {
            combos.joins.push(Join {
                kind: SqlJoinKind::Cross,
                source: src.clone(),
                on: None,
            });
        }
        let fill: BTreeMap<&str, &Json> = c
            .fill
            .iter()
            .map(|f| (f.column.as_str(), &f.value))
            .collect();
        let mut fills: BTreeMap<String, Expr> = BTreeMap::new();
        for col in &stage.cols {
            if let Some(value) = fill.get(col.name.as_str()) {
                let value = json_value(value, col.ty)
                    .map_err(|e| format!("the fill value of `{}`: {e}", col.name))?;
                fills.insert(col.name.clone(), typed(value, col.ty));
            }
        }
        // Not a FULL JOIN: Postgres will not full-join on `IS NOT DISTINCT
        // FROM`, and a missing value is a value to complete. So: every
        // combination with the data that matches it, then the rows of the data
        // no combination matches (values outside a range), one after the other.
        let matched = |data: &str, combos: &str| {
            c.columns
                .iter()
                .map(|column| {
                    Expr::binary(
                        BinOp::IsNotDistinct,
                        Expr::qcol(data, column.column.clone()),
                        Expr::qcol(combos, column.column.clone()),
                    )
                })
                .reduce(Expr::and)
        };
        let project = |data: &str, from_combos: Option<&str>| -> Vec<Projection> {
            let mut out: Vec<Projection> = stage
                .cols
                .iter()
                .map(|col| {
                    let from_data = Expr::qcol(data, col.name.clone());
                    let expr = match (from_combos, completed.contains(col.name.as_str())) {
                        (Some(combos), true) => Expr::qcol(combos, col.name.clone()),
                        _ => match fills.get(&col.name) {
                            Some(fill) => Expr::Func {
                                name: "coalesce".into(),
                                args: vec![from_data, fill.clone()],
                            },
                            None => from_data,
                        },
                    };
                    Projection::expr_as(expr, col.name.clone())
                })
                .collect();
            // The completed columns again, as the new order's hidden keys.
            for (i, column) in c.columns.iter().enumerate() {
                let expr = match from_combos {
                    Some(combos) => Expr::qcol(combos, column.column.clone()),
                    None => Expr::qcol(data, column.column.clone()),
                };
                out.push(Projection::expr_as(expr, format!("_fd_oc{i}")));
            }
            out
        };
        let (a_data, a_combos) = (self.fresh("_fd_s"), self.fresh("_fd_c"));
        let mut added = Select::from(Source::subquery(combos.clone(), a_combos.clone()))
            .columns(project(&a_data, Some(&a_combos)));
        added.joins.push(Join {
            kind: SqlJoinKind::Left,
            source: Source::subquery(data.clone(), a_data.clone()),
            on: matched(&a_data, &a_combos),
        });
        let (b_data, b_combos) = (self.fresh("_fd_s"), self.fresh("_fd_c"));
        let mut outside =
            Select::from(Source::subquery(data, b_data.clone())).columns(project(&b_data, None));
        outside.joins.push(Join {
            kind: SqlJoinKind::Left,
            source: Source::subquery(combos, b_combos.clone()),
            on: matched(&b_data, &b_combos),
        });
        outside.filter = Some(Expr::unary(UnOp::IsNull, Expr::qcol(b_combos, "_fd_m")));

        let alias = self.fresh("_fd_s");
        let mut cols: Vec<Col> = stage
            .cols
            .iter()
            .map(|col| Col {
                expr: Expr::qcol(alias.clone(), col.name.clone()),
                ..col.clone()
            })
            .collect();
        // Completed rows sort by the completed columns, then as they were.
        let mut order = Vec::new();
        for (i, _) in c.columns.iter().enumerate() {
            let name = format!("_fd_oc{i}");
            cols.push(Col {
                expr: Expr::qcol(alias.clone(), name.clone()),
                ..hidden(&name, Expr::lit(Value::Null), ColType::Unknown)
            });
            order.push((name, false));
        }
        order.extend(stage.order.iter().cloned());
        let keys: BTreeSet<&str> = c.columns.iter().map(|c| c.column.as_str()).collect();
        let grain = match &stage.grain {
            Grain::Group { keys: g }
                if g.iter().map(String::as_str).collect::<BTreeSet<_>>() == keys =>
            {
                stage.grain.clone()
            }
            _ => Grain::Derived,
        };
        Ok(Stage {
            from: Source::union_all(vec![added, outside], alias),
            joins: Vec::new(),
            filter: None,
            group: Vec::new(),
            limit: None,
            limit_order: Vec::new(),
            cols,
            order,
            grain,
            closed: false,
            text_casts: stage.text_casts,
        })
    }

    // --- the operations that combine (A1.5) ---------------------------------

    /// The last stage of what a Join or Union reads.
    fn other_stage(&self, other: &Other, stack: &[DatasetId]) -> Result<Stage, String> {
        match other {
            Other::Table { table } => self.table_stage(table),
            Other::Dataset { dataset } => self.dataset_stage(*dataset, stack, "it combines with"),
        }
    }

    fn join(&self, mut stage: Stage, j: &JoinOp, stack: &[DatasetId]) -> Result<Stage, String> {
        let mut right = self.other_stage(&j.with, stack)?;
        if j.on.is_empty() && j.asof.is_none() {
            return Err("a join needs at least one pair of key columns".to_owned());
        }
        for key in j.on.iter().chain(j.asof.iter()) {
            let l = stage.require(&key.left, "the key column")?;
            let r = right
                .visible(&key.right)
                .ok_or_else(|| format!("`{}` is not a column of what is joined", key.right))?;
            if !l.ty.compatible(r.ty) {
                return Err(format!(
                    "`{}` is {} and `{}` is {}, so they cannot be matched",
                    l.name,
                    l.ty.name(),
                    r.name,
                    r.ty.name()
                ));
            }
        }
        if let Some(asof) = &j.asof {
            if j.kind == JoinKind::Full {
                return Err("a nearest-earlier match keeps every row of these, so it is a left or an inner join, not a full one".to_owned());
            }
            let l = stage.require(&asof.left, "the date column")?;
            if !l.ty.is_ordered() {
                return Err(format!(
                    "`{}` is {}, which has no earlier and later",
                    l.name,
                    l.ty.name()
                ));
            }
        }
        let wanted: Option<BTreeSet<&str>> = j
            .columns
            .as_ref()
            .map(|cs| cs.iter().map(String::as_str).collect());
        if let Some(wanted) = &wanted {
            for w in wanted {
                if right.visible(w).is_none() {
                    return Err(format!("`{w}` is not a column of what is joined"));
                }
            }
        }
        // Does each of these rows match at most one of the other's? Then the
        // grain holds (an as-of join picks one; a key that is the other's
        // primary key or its group key matches one).
        let unique = j.asof.is_some()
            || {
                let right_keys: BTreeSet<&str> = j.on.iter().map(|k| k.right.as_str()).collect();
                let by_row_key =
                    j.on.iter()
                        .any(|k| right.visible(&k.right).is_some_and(|c| c.row_key));
                let by_group = matches!(&right.grain, Grain::Group { keys } if !keys.is_empty() && keys.iter().all(|k| right_keys.contains(k.as_str())));
                by_row_key || by_group
            };
        self.seal(&mut stage);
        let left_alias = match &stage.from {
            Source::Subquery { alias, .. } => alias.clone(),
            _ => unreachable!("a sealed stage reads a subquery"),
        };
        self.seal(&mut right);
        let right_select = right.to_select();
        let right_alias = self.fresh("_fd_r");
        let lcol = |name: &str| Expr::qcol(left_alias.clone(), name);
        let rcol = |name: &str| Expr::qcol(right_alias.clone(), name);
        let mut on =
            j.on.iter()
                .map(|k| lcol(&k.left).eq(rcol(&k.right)))
                .reduce(Expr::and);
        if let Some(asof) = &j.asof {
            // The other's latest row at or before this one's date, among the
            // rows matching the equality keys.
            let inner = self.fresh("_fd_r");
            let mut latest = Select::from(Source::subquery(right_select.clone(), inner.clone()))
                .columns(vec![Projection::expr(agg(
                    "max",
                    vec![Expr::qcol(inner.clone(), asof.right.clone())],
                ))]);
            let mut cond = Expr::binary(
                BinOp::Le,
                Expr::qcol(inner.clone(), asof.right.clone()),
                lcol(&asof.left),
            );
            for k in &j.on {
                cond = cond.and(Expr::qcol(inner.clone(), k.right.clone()).eq(lcol(&k.left)));
            }
            latest.filter = Some(cond);
            let matched = rcol(&asof.right).eq(Expr::Subquery(Box::new(latest)));
            on = Some(match on {
                Some(on) => on.and(matched),
                None => matched,
            });
        }
        // The columns: these rows', then the other's, less its key columns
        // (equal to these rows' where they matched) and those not asked for.
        let right_keys: BTreeSet<&str> = j.on.iter().map(|k| k.right.as_str()).collect();
        let mut cols: Vec<Col> = Vec::new();
        for c in &stage.cols {
            let mut col = c.clone();
            col.expr = lcol(&c.name);
            if j.kind == JoinKind::Full
                && let Some(k) = j.on.iter().find(|k| k.left == c.name)
            {
                col.expr = Expr::Func {
                    name: "coalesce".into(),
                    args: vec![lcol(&c.name), rcol(&k.right)],
                };
            }
            if j.kind == JoinKind::Full || !unique {
                col.row_key = false;
            }
            cols.push(col);
        }
        let taken: BTreeSet<String> = cols.iter().map(|c| c.name.clone()).collect();
        let at = cols.iter().position(|c| c.hidden).unwrap_or(cols.len());
        let mut added = Vec::new();
        for c in right.cols.iter().filter(|c| !c.hidden) {
            if right_keys.contains(c.name.as_str()) {
                continue;
            }
            if let Some(wanted) = &wanted
                && !wanted.contains(c.name.as_str())
            {
                continue;
            }
            let mut name = c.name.clone();
            if taken.contains(&name) || added.iter().any(|a: &Col| a.name == name) {
                name = format!("{}{}", c.name, j.suffix);
                if taken.contains(&name) || name.starts_with("_fd_") {
                    return Err(format!(
                        "the joined column `{}` is already a column here, and so is `{name}`; \
                         choose another suffix",
                        c.name
                    ));
                }
            }
            added.push(Col {
                name,
                expr: rcol(&c.name),
                ty: c.ty,
                key: c.key.clone(),
                hidden: false,
                row_key: false,
            });
        }
        cols.splice(at..at, added);
        let grain = if unique && j.kind != JoinKind::Full {
            stage.grain.clone()
        } else {
            Grain::Derived
        };
        if matches!(grain, Grain::Derived) {
            cols.retain(|c| c.name != ROW_KEY);
        }
        Ok(Stage {
            from: stage.from,
            joins: vec![Join {
                kind: match j.kind {
                    JoinKind::Inner => SqlJoinKind::Inner,
                    JoinKind::Left => SqlJoinKind::Left,
                    JoinKind::Full => SqlJoinKind::Full,
                },
                source: Source::subquery(right_select, right_alias),
                on,
            }],
            filter: None,
            group: Vec::new(),
            limit: None,
            limit_order: Vec::new(),
            cols,
            text_casts: stage.text_casts,
            order: stage.order,
            grain,
            closed: false,
        })
    }

    fn union(&self, mut stage: Stage, u: &UnionOp, stack: &[DatasetId]) -> Result<Stage, String> {
        let mut other = self.other_stage(&u.with, stack)?;
        let source = match &u.source_column {
            Some(name) => Some(column_name(name, "the source column")?),
            None => None,
        };
        // Matched by name: these rows' columns in order, then the other's that
        // are not among them.
        let mine: Vec<Col> = stage.cols.iter().filter(|c| !c.hidden).cloned().collect();
        let theirs: Vec<Col> = other.cols.iter().filter(|c| !c.hidden).cloned().collect();
        let mut columns: Vec<(String, ColType, Option<ForeignKey>)> = Vec::new();
        for c in &mine {
            let ty = match theirs.iter().find(|t| t.name == c.name) {
                Some(t) if !c.ty.compatible(t.ty) => {
                    return Err(format!(
                        "`{}` is {} here and {} in what is appended, so they cannot be one column",
                        c.name,
                        c.ty.name(),
                        t.ty.name()
                    ));
                }
                Some(t) => c.ty.unify(t.ty),
                None => c.ty,
            };
            let key = match theirs.iter().find(|t| t.name == c.name) {
                Some(t) if t.key == c.key => c.key.clone(),
                Some(_) => None,
                None => c.key.clone(),
            };
            columns.push((c.name.clone(), ty, key));
        }
        for t in &theirs {
            if !mine.iter().any(|c| c.name == t.name) {
                columns.push((t.name.clone(), t.ty, t.key.clone()));
            }
        }
        if let Some(s) = &source
            && columns.iter().any(|(n, _, _)| n == s)
        {
            return Err(format!(
                "`{s}` is already a column; name the source column something else"
            ));
        }
        let labels: [String; 2] = match u.source_labels.as_slice() {
            [a, b] => [a.clone(), b.clone()],
            _ => [
                "this".to_owned(),
                match &u.with {
                    Other::Table { table } => table.clone(),
                    Other::Dataset { dataset } => self
                        .library
                        .get(*dataset)
                        .map_or_else(|| dataset.to_string(), |d| d.name.clone()),
                },
            ],
        };
        self.seal(&mut stage);
        self.seal(&mut other);
        let branch = self.fresh("_fd_o");
        let part = |side: &Stage, label: &str, index: i64| -> Select {
            let mut projections: Vec<Projection> = columns
                .iter()
                .map(|(name, ty, _)| {
                    let expr = match side.visible(name) {
                        Some(c) => cast(c.expr.clone(), *ty),
                        None => typed(Value::Null, *ty),
                    };
                    Projection::expr_as(expr, name.clone())
                })
                .collect();
            if let Some(s) = &source {
                projections.push(Projection::expr_as(
                    typed(Value::Text(label.to_owned()), ColType::Text),
                    s.clone(),
                ));
            }
            projections.push(Projection::expr_as(
                typed(Value::Int(index), ColType::Int),
                branch.clone(),
            ));
            let mut select = Select::from(side.from.clone()).columns(projections);
            select.joins = side.joins.clone();
            select.filter = side.filter.clone();
            select
        };
        let parts = vec![part(&stage, &labels[0], 0), part(&other, &labels[1], 1)];
        let alias = self.fresh("_fd_s");
        let mut cols: Vec<Col> = columns
            .into_iter()
            .map(|(name, ty, key)| Col {
                expr: Expr::qcol(alias.clone(), name.clone()),
                name,
                ty,
                key,
                hidden: false,
                row_key: false,
            })
            .collect();
        if let Some(s) = &source {
            cols.push(Col {
                name: s.clone(),
                expr: Expr::qcol(alias.clone(), s.clone()),
                ty: ColType::Text,
                key: None,
                hidden: false,
                row_key: false,
            });
        }
        cols.push(Col {
            expr: Expr::qcol(alias.clone(), branch.clone()),
            ..hidden(&branch, Expr::lit(0_i64), ColType::Int)
        });
        Ok(Stage {
            from: Source::union_all(parts, alias),
            joins: Vec::new(),
            filter: None,
            group: Vec::new(),
            limit: None,
            limit_order: Vec::new(),
            cols,
            order: vec![(branch, false)],
            grain: Grain::Derived,
            closed: false,
            text_casts: stage.text_casts,
        })
    }
}

/// Keep, drop, rename and reorder: the one operation with no formula in it.
fn select(mut stage: Stage, s: &SelectOp) -> Result<Stage, String> {
    if s.columns.is_empty() {
        return Err("a selection must keep at least one column".to_owned());
    }
    let mut cols = Vec::with_capacity(s.columns.len());
    let mut seen = BTreeSet::new();
    let mut renames: BTreeMap<String, String> = BTreeMap::new();
    for c in &s.columns {
        let col = stage.require(&c.column, "the column")?.clone();
        let name = column_name(c.output(), "a column")?;
        if !seen.insert(name.clone()) {
            return Err(format!("two columns would be called `{name}`"));
        }
        renames.insert(c.column.clone(), name.clone());
        cols.push(Col { name, ..col });
    }
    // Hidden columns come along: the row key and the order are the stage's,
    // not anybody's to select.
    cols.extend(stage.cols.iter().filter(|c| c.hidden).cloned());
    if let Grain::Group { keys } = &stage.grain {
        stage.grain = if keys.iter().all(|k| renames.contains_key(k)) {
            Grain::Group {
                keys: keys.iter().map(|k| renames[k].clone()).collect(),
            }
        } else {
            Grain::Derived
        };
    }
    stage.cols = cols;
    Ok(stage)
}

/// The groups and the order of a window, over `stage`'s `FROM`.
fn window_frame(stage: &Stage, w: &WindowOp, tie_break: bool) -> (Vec<Expr>, Vec<OrderBy>) {
    let partition = w
        .partition
        .iter()
        .filter_map(|p| stage.col(p).map(|c| c.expr.clone()))
        .collect();
    let mut order: Vec<OrderBy> = if w.order.is_empty() {
        stage.order_exprs()
    } else {
        w.order
            .iter()
            .filter_map(|k| {
                stage
                    .col(&k.column)
                    .map(|c| sorted(c.expr.clone(), k.descending))
            })
            .collect()
    };
    if tie_break {
        order.extend(stage.tie_break());
    }
    (partition, order)
}

/// Whether `function` can summarise `column`.
fn check_summary(function: SummaryFunction, column: &Col) -> Result<(), String> {
    let ty = column.ty;
    let refuse = |why: &str| Err(format!("`{}` is {}, {why}", column.name, ty.name()));
    if ty == ColType::Unknown {
        return Ok(());
    }
    match function {
        SummaryFunction::Sum
        | SummaryFunction::Mean
        | SummaryFunction::Median
        | SummaryFunction::Sd
            if !ty.is_numeric() =>
        {
            refuse("and this summary needs a number")
        }
        SummaryFunction::Min | SummaryFunction::Max if !ty.is_ordered() => {
            refuse("which has no smallest or largest value")
        }
        SummaryFunction::CountDistinct | SummaryFunction::First | SummaryFunction::Last
            if ty == ColType::Json =>
        {
            refuse("which cannot be compared")
        }
        _ => Ok(()),
    }
}

/// A column name an operation creates: not empty, and not one of the reserved
/// hidden names.
fn column_name(name: &str, what: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("{what} has no name"));
    }
    // The one reserved name an operation may make: the label a model reads
    // beside a related dataset's rows (`sc-model`'s `LABEL_COLUMN`), added by
    // the model, never by a person.
    if name.starts_with("_fd_") && name != LABEL_COLUMN {
        return Err(format!(
            "`{name}` starts with `_fd_`, which is reserved for the server's own names"
        ));
    }
    Ok(name.to_owned())
}

/// A hidden column.
fn hidden(name: &str, expr: Expr, ty: ColType) -> Col {
    Col {
        name: name.to_owned(),
        expr,
        ty,
        key: None,
        hidden: true,
        row_key: name == ROW_KEY,
    }
}

/// An aggregate call.
fn agg(func: &str, args: Vec<Expr>) -> Expr {
    Expr::Agg {
        func: func.to_owned(),
        distinct: false,
        args,
    }
}

/// A window call.
fn win(func: &str, args: Vec<Expr>, partition: Vec<Expr>, order: Vec<OrderBy>) -> Expr {
    Expr::Window {
        func: func.to_owned(),
        args,
        partition,
        order,
    }
}

/// `CASE WHEN cond THEN value END`.
fn case_when(cond: Expr, value: Expr) -> Expr {
    Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: cond,
            then: value,
        }],
        else_result: None,
    }
}

/// `expr` cast to `ty`, unless its type is not known.
fn cast(expr: Expr, ty: ColType) -> Expr {
    if ty == ColType::Unknown {
        return expr;
    }
    Expr::Cast {
        expr: Box::new(expr),
        type_name: ty.sql_type().to_owned(),
    }
}

/// A literal, cast to its type so both databases know what the placeholder is
/// (Postgres cannot type `SELECT $1` on its own).
fn typed(value: Value, ty: ColType) -> Expr {
    Expr::Cast {
        expr: Box::new(Expr::Lit(value)),
        type_name: ty.sql_type().to_owned(),
    }
}

/// A cast to a type SQLite stores as text, made a cast to text (see
/// `Stage::finish`); `None` for anything else.
fn text_cast(e: &Expr) -> Option<Expr> {
    match e {
        Expr::Cast { expr, type_name }
            if matches!(
                type_name.as_str(),
                "date" | "time" | "timestamptz" | "uuid" | "jsonb"
            ) =>
        {
            let mut inner = (**expr).clone();
            walk::rewrite(&mut inner, &mut text_cast);
            Some(Expr::Cast {
                expr: Box::new(inner),
                type_name: "text".to_owned(),
            })
        }
        _ => None,
    }
}

/// A `lag`/`lead` offset: Postgres declares it `integer`, not `bigint`.
fn offset_arg(n: i64) -> Expr {
    Expr::Cast {
        expr: Box::new(Expr::Lit(Value::Int(n))),
        type_name: "integer".to_owned(),
    }
}

/// An `ORDER BY` key with nulls last, stated: the databases disagree about
/// where an ascending sort puts them, and an order that changed with the
/// database would not be one.
fn sorted(expr: Expr, descending: bool) -> OrderBy {
    OrderBy {
        expr,
        dir: if descending {
            OrderDir::Desc
        } else {
            OrderDir::Asc
        },
        nulls: Some(Nulls::Last),
    }
}

/// A JSON value as a value of a column of type `ty`.
fn json_value(value: &Json, ty: ColType) -> Result<Value, String> {
    let bad = || format!("{value} is not a value of a {} column", ty.name());
    Ok(match (ty, value) {
        (_, Json::Null) => Value::Null,
        (ColType::Int, Json::Number(n)) => Value::Int(n.as_i64().ok_or_else(bad)?),
        (ColType::Float | ColType::Decimal | ColType::Unknown, Json::Number(n)) => {
            Value::Float(n.as_f64().ok_or_else(bad)?)
        }
        (ColType::Bool, Json::Bool(b)) => Value::Bool(*b),
        (ColType::Date, Json::String(s)) => {
            Value::Date(NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| bad())?)
        }
        (ColType::Text | ColType::Unknown, Json::String(s)) => Value::Text(s.clone()),
        (ColType::Uuid, Json::String(s)) => {
            Value::Uuid(uuid::Uuid::parse_str(s).map_err(|_| bad())?)
        }
        _ => return Err(bad()),
    })
}

/// The values of a Complete range.
fn range_values(
    ty: ColType,
    from: &Json,
    to: &Json,
    step: Option<&Json>,
) -> Result<Vec<Value>, String> {
    let too_many = || {
        format!("it has more than {MAX_RANGE_VALUES} values; use a larger step or a shorter range")
    };
    match ty {
        ColType::Int | ColType::Float | ColType::Decimal => {
            let (Some(a), Some(b)) = (from.as_f64(), to.as_f64()) else {
                return Err("a number range goes from a number to a number".to_owned());
            };
            let step = match step {
                None | Some(Json::Null) => 1.0,
                Some(s) => s.as_f64().ok_or("the step of a number range is a number")?,
            };
            if step <= 0.0 {
                return Err("the step must be more than 0".to_owned());
            }
            if a > b {
                return Err("it ends before it starts".to_owned());
            }
            if (b - a) / step > MAX_RANGE_VALUES as f64 {
                return Err(too_many());
            }
            let mut out = Vec::new();
            let mut i = 0.0;
            loop {
                let v = a + i * step;
                if v > b + step * 1e-9 {
                    break;
                }
                out.push(if ty == ColType::Int {
                    Value::Int(v.round() as i64)
                } else {
                    Value::Float(v)
                });
                i += 1.0;
            }
            Ok(out)
        }
        ColType::Date => {
            let parse = |j: &Json| {
                j.as_str()
                    .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                    .ok_or("a date range goes from a date to a date, written YYYY-MM-DD")
            };
            let (a, b) = (parse(from)?, parse(to)?);
            if a > b {
                return Err("it ends before it starts".to_owned());
            }
            let unit = match step {
                None | Some(Json::Null) => "day",
                Some(s) => s
                    .as_str()
                    .ok_or("the step of a date range is day, week, month or year")?,
            };
            let mut out = Vec::new();
            let mut d = a;
            while d <= b {
                if out.len() >= MAX_RANGE_VALUES {
                    return Err(too_many());
                }
                out.push(Value::Date(d));
                d = match unit {
                    "day" => d + Duration::days(1),
                    "week" => d + Duration::weeks(1),
                    "month" => d
                        .checked_add_months(Months::new(1))
                        .ok_or("the range runs past the end of the calendar")?,
                    "year" => d
                        .with_year(d.year() + 1)
                        .or_else(|| d.checked_add_months(Months::new(12)))
                        .ok_or("the range runs past the end of the calendar")?,
                    other => {
                        return Err(format!(
                            "`{other}` is not a step of a date range; it is day, week, month or year"
                        ));
                    }
                };
            }
            Ok(out)
        }
        other => Err(format!(
            "a range of values is for numbers and dates, and this column is {}",
            other.name()
        )),
    }
}

/// A formula error with the stage's internal name taken out of it.
fn plain(message: &str) -> String {
    message
        .replace("`_fd_stage`", "the dataset")
        .replace("_fd_stage", "the dataset")
        .replace("formula on the dataset: ", "")
}
