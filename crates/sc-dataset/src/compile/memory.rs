//! After a Model predictions operation (analytics TODO A7.3): the rows are
//! computed in memory.
//!
//! A prediction is not SQL — it is a call to a model's provider — so a stage
//! after one cannot be a query. The SQL stage before the first Model
//! predictions operation is the **source**; it is read (bounded by
//! [`MAX_IN_MEMORY_ROWS`]) and the operations from there on are evaluated over
//! the rows, in order, as a [`Tail`]:
//!
//! - **Model predictions**: one batched call per operation through the
//!   catalog's [`ModelHost`](sc_catalog::ModelHost), every row's columns as
//!   the model's inputs, matched by name;
//! - **Calculated column** and **Filter**, by the reified evaluator — the
//!   formula over the row, as a calculated field computed after a read is
//!   ([`JsEvaluator`](sc_expr::JsEvaluator)). A formula here cannot follow a
//!   key or aggregate a child table: there is no database below it;
//! - **Select columns**, **Sort**, **Aggregate** and **Limit**, in Rust.
//!
//! The other operations need the database (a join, a window over ordered
//! rows, a reshape) and are refused after a prediction with a sentence saying
//! so. A stage's shape is still compiled, so the editor shows the columns
//! after each operation and the errors on them, without reading anything.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use sc_error::{Error, Result};
use sc_expr::{AmbientValues, Formula, FormulaCall, Operation as FormulaOperation};
use sc_query::{Expr, Value};
use sc_types::{value_from_json, value_to_json};
use serde_json::Value as Json;

use super::{Col, Compiler, STAGE_NAME, Stage, check_summary, column_name, plain, select};
use crate::access::Reader;
use crate::compile::Restriction;
use crate::def::{
    AggregateOp, CalculatedOp, FilterOp, LimitMode, LimitOp, Op, PredictOp, SelectOp, SortOp,
    SummaryFunction,
};
use crate::infer::infer;
use crate::read::Rows;
use crate::shape::{ColType, Grain};

/// The most rows a dataset with Model predictions reads into memory.
pub const MAX_IN_MEMORY_ROWS: u64 = 100_000;

/// The interval a Model predictions operation asks for when it names none:
/// the 5 % and 95 % quantiles.
pub const DEFAULT_LEVEL: f64 = 0.9;

/// What a compile knows about a model a Model predictions operation names —
/// read from the catalog's model host when the library is loaded, since
/// models are above this crate.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// The type of its prediction, `Unknown` when it depends on the fit (a
    /// random forest is a regressor or a classifier by its label's type).
    pub prediction: ColType,
    /// What its provider can say about how sure a prediction is: `none`,
    /// `interval`, `probabilities` or `distribution`.
    pub uncertainty: String,
}

impl ModelInfo {
    /// Whether a prediction comes with an interval.
    fn interval(&self) -> bool {
        matches!(self.uncertainty.as_str(), "interval" | "distribution")
            && matches!(self.prediction, ColType::Float | ColType::Unknown)
    }

    /// Whether a prediction comes with the class's probability.
    fn probability(&self) -> bool {
        self.uncertainty == "probabilities"
            && matches!(self.prediction, ColType::Text | ColType::Unknown)
    }
}

/// The operations after (and including) the first Model predictions, over
/// the SQL stage before it.
#[derive(Debug, Clone)]
pub struct Tail {
    /// The stage read from the database.
    pub(crate) source: Stage,
    /// The operations evaluated over its rows, in order.
    steps: Vec<Step>,
}

#[derive(Debug, Clone)]
enum Step {
    Predict {
        op: PredictOp,
        lower: Option<String>,
        upper: Option<String>,
        probability: Option<String>,
    },
    Calculated {
        name: String,
        formula: Formula,
    },
    Filter {
        formula: Formula,
    },
    Select(SelectOp),
    Sort(Vec<(Key, bool)>),
    Aggregate {
        keys: Vec<(String, Key)>,
        op: AggregateOp,
    },
    Limit(LimitOp),
}

/// A value a Sort or an Aggregate's group key reads: a column, or a formula
/// over the row.
#[derive(Debug, Clone)]
enum Key {
    Column(String),
    Formula(Formula),
}

/// The sentence a SQL read of a stage computed in memory fails with.
pub(crate) const NOT_SQL: &str = "the rows after a Model predictions operation are computed in \
     memory, not by the database, so they cannot be read as a query here";

impl Compiler<'_> {
    /// `op` over `stage`, which is computed in memory (or becomes so with
    /// this Model predictions operation).
    pub(super) fn in_memory(&self, mut stage: Stage, op: &Op) -> Result<Stage> {
        let mut tail = match stage.tail.take() {
            Some(tail) => (*tail).clone(),
            None => Tail {
                source: stage.clone(),
                steps: Vec::new(),
            },
        };
        let step = match op {
            Op::Predict(p) => self.predict(&mut stage, p)?,
            Op::Calculated(c) => self.calculated_in_memory(&mut stage, c)?,
            Op::Filter(FilterOp { formula }) => {
                let (formula, _) = self.row_formula(&stage, formula)?;
                Step::Filter { formula }
            }
            Op::Select(s) => {
                stage = select(stage, s)?;
                Step::Select(s.clone())
            }
            Op::Sort(s) => self.sort_in_memory(&stage, s)?,
            Op::Aggregate(a) => self.aggregate_in_memory(&mut stage, a)?,
            Op::Limit(l) => {
                limit_columns(&stage, l)?;
                Step::Limit(l.clone())
            }
            other => {
                return Err(Error::invalid(format!(
                    "a {} cannot follow a Model predictions operation: the rows after one are \
                     computed in memory, where a Calculated column, a Filter, Select columns, \
                     a Sort, an Aggregate, a Limit and another Model predictions can be; put \
                     it before the predictions instead",
                    kind_words(other)
                )));
            }
        };
        tail.steps.push(step);
        stage.tail = Some(Arc::new(tail));
        Ok(stage)
    }

    fn predict(&self, stage: &mut Stage, p: &PredictOp) -> Result<Step> {
        if p.model.trim().is_empty() {
            return Err(Error::invalid(
                "choose the model whose predictions are added",
            ));
        }
        let info = self.library.model(p.model.trim())?;
        let name = column_name(&p.name, "the prediction column")?;
        if let Some(level) = p.level {
            if !(level > 0.0 && level < 1.0) {
                return Err(Error::invalid(format!(
                    "the interval's level is a probability between 0 and 1, not {level}"
                )));
            }
        }
        let column = |name: &str, ty: ColType| Col {
            name: name.to_owned(),
            expr: Expr::lit(Value::Null),
            ty,
            key: None,
            hidden: false,
            row_key: false,
        };
        stage.put(column(&name, info.prediction));
        let (mut lower, mut upper, mut probability) = (None, None, None);
        if p.interval && info.interval() {
            for (slot, suffix) in [(&mut lower, "_lower"), (&mut upper, "_upper")] {
                let n = format!("{name}{suffix}");
                stage.put(column(&n, ColType::Float));
                *slot = Some(n);
            }
        }
        if p.interval && info.probability() {
            let n = format!("{name}_probability");
            stage.put(column(&n, ColType::Float));
            probability = Some(n);
        }
        Ok(Step::Predict {
            op: PredictOp { name, ..p.clone() },
            lower,
            upper,
            probability,
        })
    }

    /// `source` parsed and checked as a formula over one row of `stage`, with
    /// the type it computes: no key followed, no child table, no caller — the
    /// row is all there is.
    fn row_formula(&self, stage: &Stage, source: &str) -> Result<(Formula, ColType)> {
        if source.trim().is_empty() {
            return Err(Error::invalid("the formula is empty"));
        }
        let formula = Formula::parse(source).map_err(|e| {
            Error::invalid(format!(
                "`{source}` is not a formula: {}",
                plain(&e.to_string())
            ))
        })?;
        let shape = stage.formula_shape(&self.schema.shape);
        let analysis = formula
            .validate(&shape, STAGE_NAME)
            .map_err(|e| Error::invalid(plain(&e.to_string())))?;
        let after = "after a Model predictions operation a formula is computed in memory over \
                     the row";
        if !analysis.join_paths.is_empty() || !analysis.agg_uses.is_empty() {
            return Err(Error::invalid(format!(
                "`{source}` follows a key or reads a child table, and {after}: add the columns \
                 it needs before the predictions"
            )));
        }
        if analysis.uses_caller() || !analysis.flags.is_empty() {
            return Err(Error::invalid(
                "a dataset cannot use `user`, `session` or the operation flags: it has no \
                 caller, and its rows must not depend on who reads them",
            ));
        }
        if !analysis.model_calls.is_empty()
            || !analysis.module_calls.is_empty()
            || !analysis.geo_calls.is_empty()
        {
            return Err(Error::invalid(format!(
                "`{source}` calls a model, a module or a geometry function, and {after}: add a \
                 Model predictions operation for a model"
            )));
        }
        let (ty, _) = infer(formula.ast(), &stage.known(), self.schema);
        Ok((formula, ty))
    }

    fn calculated_in_memory(&self, stage: &mut Stage, c: &CalculatedOp) -> Result<Step> {
        let name = column_name(&c.name, "the new column")?;
        let (formula, ty) = self.row_formula(stage, &c.formula)?;
        stage.put(Col {
            name: name.clone(),
            expr: Expr::lit(Value::Null),
            ty,
            key: None,
            hidden: false,
            row_key: false,
        });
        Ok(Step::Calculated { name, formula })
    }

    /// A Sort or group key: the column it names, or its formula.
    fn key(&self, stage: &Stage, source: &str) -> Result<(Key, ColType)> {
        let trimmed = source.trim();
        if let Some(col) = stage.visible(trimmed) {
            return Ok((Key::Column(trimmed.to_owned()), col.ty));
        }
        let (formula, ty) = self.row_formula(stage, source)?;
        Ok((Key::Formula(formula), ty))
    }

    fn sort_in_memory(&self, stage: &Stage, s: &SortOp) -> Result<Step> {
        if s.keys.is_empty() {
            return Err(Error::invalid("a sort needs at least one key"));
        }
        let mut keys = Vec::with_capacity(s.keys.len());
        for k in &s.keys {
            let (key, ty) = self.key(stage, &k.formula)?;
            if matches!(ty, ColType::Json | ColType::Bytes | ColType::Geometry) {
                return Err(Error::invalid(format!(
                    "`{}` is {}, which has no order to sort by",
                    k.formula,
                    ty.name()
                )));
            }
            keys.push((key, k.descending));
        }
        Ok(Step::Sort(keys))
    }

    fn aggregate_in_memory(&self, stage: &mut Stage, a: &AggregateOp) -> Result<Step> {
        if a.group_by.is_empty() && a.summaries.is_empty() {
            return Err(Error::invalid(
                "an aggregate needs a group key or a summary",
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        let mut cols = Vec::new();
        let mut keys = Vec::new();
        for g in &a.group_by {
            let name = column_name(&g.name, "a result column")?;
            if !names.insert(name.clone()) {
                return Err(Error::invalid(format!(
                    "two result columns are called `{name}`"
                )));
            }
            let (key, ty) = self.key(stage, &g.formula)?;
            let fk = match &key {
                Key::Column(c) => stage.visible(c).and_then(|c| c.key.clone()),
                Key::Formula(_) => None,
            };
            cols.push(Col {
                name: name.clone(),
                expr: Expr::lit(Value::Null),
                ty,
                key: fk,
                hidden: false,
                row_key: false,
            });
            keys.push((name, key));
        }
        for s in &a.summaries {
            let name = column_name(&s.name, "a result column")?;
            if !names.insert(name.clone()) {
                return Err(Error::invalid(format!(
                    "two result columns are called `{name}`"
                )));
            }
            let column = match &s.column {
                Some(c) => Some(stage.visible(c).cloned().ok_or_else(|| {
                    Error::invalid(format!(
                        "the summary `{}` reads `{c}`, and that is not a column at this point \
                         (the columns are {})",
                        s.name,
                        stage.column_list()
                    ))
                })?),
                None if s.function == SummaryFunction::Count => None,
                None => {
                    return Err(Error::invalid(format!(
                        "the summary `{}` needs a column",
                        s.name
                    )));
                }
            };
            if let Some(c) = &column {
                check_summary(s.function, c)?;
            }
            if let Some(order) = &s.order {
                stage.require(&order.column, "the order column")?;
            }
            let ty = match s.function {
                SummaryFunction::Count | SummaryFunction::CountDistinct => ColType::Int,
                SummaryFunction::Sum => column.as_ref().map_or(ColType::Float, |c| c.ty),
                SummaryFunction::Mean | SummaryFunction::Median | SummaryFunction::Sd => {
                    ColType::Float
                }
                SummaryFunction::Min
                | SummaryFunction::Max
                | SummaryFunction::First
                | SummaryFunction::Last => column.as_ref().map_or(ColType::Unknown, |c| c.ty),
                SummaryFunction::Union => {
                    return Err(Error::invalid(
                        "the union of geometries is computed by the database, and the rows \
                         after a Model predictions operation are computed in memory",
                    ));
                }
            };
            cols.push(Col {
                name,
                expr: Expr::lit(Value::Null),
                ty,
                key: column.and_then(|c| c.key).filter(|_| {
                    matches!(
                        s.function,
                        SummaryFunction::Min
                            | SummaryFunction::Max
                            | SummaryFunction::First
                            | SummaryFunction::Last
                    )
                }),
                hidden: false,
                row_key: false,
            });
        }
        stage.cols = cols;
        stage.order.clear();
        stage.grain = if keys.is_empty() {
            Grain::Derived
        } else {
            Grain::Group {
                keys: keys.iter().map(|(n, _)| n.clone()).collect(),
            }
        };
        Ok(Step::Aggregate {
            keys,
            op: a.clone(),
        })
    }
}

/// A Limit's columns exist.
fn limit_columns(stage: &Stage, l: &LimitOp) -> Result<()> {
    if l.mode == LimitMode::Top && l.order.is_empty() && l.group_by.is_empty() {
        return Err(Error::invalid(
            "the top rows of each group need the groups or an order",
        ));
    }
    for c in &l.group_by {
        stage.require(c, "the group column")?;
    }
    for k in &l.order {
        stage.require(&k.column, "the order column")?;
    }
    Ok(())
}

/// An operation's kind, as words for a sentence.
fn kind_words(op: &Op) -> &'static str {
    match op {
        Op::Window(_) => "Window column",
        Op::Stack(_) => "Stack",
        Op::Split(_) => "Split",
        Op::Complete(_) => "Complete",
        Op::Join(_) => "Join",
        Op::Union(_) => "Union",
        Op::SpatialJoin(_) => "Spatial join",
        _ => "operation",
    }
}

// --- evaluation ---------------------------------------------------------

/// The rows of `stage` — the source read from the database, restricted, and
/// every operation of its tail over them. The row key is kept while rows are
/// rows of the base table.
pub(crate) async fn evaluate(
    reader: &Reader,
    stage: &Stage,
    tail: &Tail,
    restrict: Option<&Restriction>,
) -> Result<Rows> {
    let source =
        crate::read::read_sql_rows(reader, &tail.source, restrict, Some(MAX_IN_MEMORY_ROWS + 1))
            .await?;
    if source.rows.len() as u64 > MAX_IN_MEMORY_ROWS {
        return Err(Error::invalid(format!(
            "a dataset with Model predictions reads its rows into memory, at most \
             {MAX_IN_MEMORY_ROWS} of them, and this one has more: filter or sample it before \
             the predictions"
        )));
    }
    let mut work = Work {
        names: source.columns.iter().map(|c| c.name.clone()).collect(),
        rows: source.rows,
        keys: source.keys,
    };
    for step in &tail.steps {
        work.apply(reader, step).await?;
    }
    // In the stage's column order, typed as the shape says.
    let shape = stage.shape();
    let at: Vec<Option<usize>> = shape
        .columns
        .iter()
        .map(|c| work.names.iter().position(|n| *n == c.name))
        .collect();
    let rows: Vec<Vec<Value>> = work
        .rows
        .into_iter()
        .map(|row| {
            at.iter()
                .map(|i| i.and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    let keys = if stage.has_row_key() {
        work.keys
    } else {
        Vec::new()
    };
    let mut rows = rows;
    let columns = crate::read::typed(shape.columns, &mut rows);
    Ok(Rows {
        columns,
        grain: shape.grain,
        rows,
        keys,
    })
}

/// The rows while the tail is evaluated: named columns, and each row's key
/// while there is one.
struct Work {
    names: Vec<String>,
    rows: Vec<Vec<Value>>,
    keys: Vec<Value>,
}

impl Work {
    fn index(&self, name: &str) -> Result<usize> {
        self.names
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| Error::invalid(format!("there is no column `{name}` at this point")))
    }

    /// Set column `name` to `values`, replacing one of the same name.
    fn put(&mut self, name: &str, values: Vec<Value>) {
        match self.names.iter().position(|n| n == name) {
            Some(i) => {
                for (row, v) in self.rows.iter_mut().zip(values) {
                    row[i] = v;
                }
            }
            None => {
                self.names.push(name.to_owned());
                for (row, v) in self.rows.iter_mut().zip(values) {
                    row.push(v);
                }
            }
        }
    }

    /// Row `i` as a formula's scope.
    fn scope(&self, i: usize) -> BTreeMap<String, Value> {
        self.names
            .iter()
            .cloned()
            .zip(self.rows[i].iter().cloned())
            .collect()
    }

    /// Keep the rows at `positions`, in that order.
    fn keep(&mut self, positions: &[usize]) {
        let rows = std::mem::take(&mut self.rows);
        let mut rows: Vec<Option<Vec<Value>>> = rows.into_iter().map(Some).collect();
        self.rows = positions.iter().filter_map(|&i| rows[i].take()).collect();
        if !self.keys.is_empty() {
            self.keys = positions.iter().map(|&i| self.keys[i].clone()).collect();
        }
    }

    async fn values(&self, reader: &Reader, key: &Key) -> Result<Vec<Value>> {
        match key {
            Key::Column(c) => {
                let i = self.index(c)?;
                Ok(self.rows.iter().map(|r| r[i].clone()).collect())
            }
            Key::Formula(f) => {
                let mut out = Vec::with_capacity(self.rows.len());
                for i in 0..self.rows.len() {
                    out.push(eval_value(reader, f, self.scope(i)).await?);
                }
                Ok(out)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn apply(&mut self, reader: &Reader, step: &Step) -> Result<()> {
        match step {
            Step::Predict {
                op,
                lower,
                upper,
                probability,
            } => {
                let host = reader.models().ok_or_else(|| {
                    Error::invalid(format!(
                        "this server has no model support, so the predictions of `{}` cannot \
                         be added",
                        op.model
                    ))
                })?;
                let inputs: Vec<Json> = (0..self.rows.len())
                    .map(|i| {
                        Json::Object(
                            self.names
                                .iter()
                                .zip(&self.rows[i])
                                .map(|(n, v)| (n.clone(), value_to_json(v)))
                                .collect(),
                        )
                    })
                    .collect();
                let answers = host
                    .predict_inputs(
                        op.model.trim(),
                        op.fit.as_deref(),
                        &inputs,
                        op.level.unwrap_or(DEFAULT_LEVEL),
                    )
                    .await?;
                if answers.len() != self.rows.len() {
                    return Err(Error::msg(format!(
                        "`{}` answered {} predictions for {} rows",
                        op.model,
                        answers.len(),
                        self.rows.len()
                    )));
                }
                let field = |key: &str| -> Vec<Value> {
                    answers
                        .iter()
                        .map(|a| a.get(key).map_or(Value::Null, value_from_json))
                        .collect()
                };
                self.put(&op.name, field("value"));
                for (column, key) in [
                    (lower, "lower"),
                    (upper, "upper"),
                    (probability, "probability"),
                ] {
                    if let Some(column) = column {
                        self.put(column, field(key));
                    }
                }
            }
            Step::Calculated { name, formula } => {
                let values = self.values(reader, &Key::Formula(formula.clone())).await?;
                self.put(name, values);
            }
            Step::Filter { formula } => {
                let evaluator = evaluator(reader)?;
                let mut kept = Vec::new();
                for i in 0..self.rows.len() {
                    let call = FormulaCall {
                        formula: formula.clone(),
                        op: FormulaOperation::Read,
                        row: self.scope(i),
                        user: None,
                        ambient: AmbientValues::new(),
                        now: chrono::Utc::now(),
                    };
                    if evaluator.eval(call).await? {
                        kept.push(i);
                    }
                }
                self.keep(&kept);
            }
            Step::Select(s) => {
                let mut names = Vec::with_capacity(s.columns.len());
                let mut at = Vec::with_capacity(s.columns.len());
                for c in &s.columns {
                    at.push(self.index(&c.column)?);
                    names.push(c.output().to_owned());
                }
                for row in &mut self.rows {
                    *row = at.iter().map(|&i| row[i].clone()).collect();
                }
                self.names = names;
            }
            Step::Sort(keys) => {
                let mut columns = Vec::with_capacity(keys.len());
                for (key, descending) in keys {
                    columns.push((self.values(reader, key).await?, *descending));
                }
                let mut order: Vec<usize> = (0..self.rows.len()).collect();
                order.sort_by(|&a, &b| {
                    for (values, descending) in &columns {
                        let o = ordered(&values[a], &values[b], *descending);
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    Ordering::Equal
                });
                self.keep(&order);
            }
            Step::Aggregate { keys, op } => self.aggregate(reader, keys, op).await?,
            Step::Limit(l) => self.limit(l)?,
        }
        Ok(())
    }

    async fn aggregate(
        &mut self,
        reader: &Reader,
        keys: &[(String, Key)],
        op: &AggregateOp,
    ) -> Result<()> {
        let mut key_values = Vec::with_capacity(keys.len());
        for (_, key) in keys {
            key_values.push(self.values(reader, key).await?);
        }
        // The groups, in the order of their keys (as the SQL's tie-break).
        let mut groups: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
        let mut by_key: BTreeMap<String, usize> = BTreeMap::new();
        for i in 0..self.rows.len() {
            let key: Vec<Value> = key_values.iter().map(|v| v[i].clone()).collect();
            let text = format!("{key:?}");
            match by_key.get(&text) {
                Some(&g) => groups[g].1.push(i),
                None => {
                    by_key.insert(text, groups.len());
                    groups.push((key, vec![i]));
                }
            }
        }
        if keys.is_empty() && groups.is_empty() {
            groups.push((Vec::new(), Vec::new()));
        }
        groups.sort_by(|a, b| {
            for (x, y) in a.0.iter().zip(&b.0) {
                let o = ordered(x, y, false);
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
        let mut rows = Vec::with_capacity(groups.len());
        for (key, members) in &groups {
            let mut row = key.clone();
            for s in &op.summaries {
                let values: Vec<&Value> = match &s.column {
                    Some(c) => {
                        let i = self.index(c)?;
                        members.iter().map(|&m| &self.rows[m][i]).collect()
                    }
                    None => Vec::new(),
                };
                let order = match &s.order {
                    Some(k) => Some((self.index(&k.column)?, k.descending)),
                    None => None,
                };
                row.push(
                    if s.function == SummaryFunction::Count && s.column.is_none() {
                        #[allow(clippy::cast_possible_wrap, reason = "a row count")]
                        Value::Int(members.len() as i64)
                    } else {
                        summary(s.function, &values, members, order, &self.rows)
                    },
                );
            }
            rows.push(row);
        }
        self.names = keys
            .iter()
            .map(|(n, _)| n.clone())
            .chain(op.summaries.iter().map(|s| s.name.clone()))
            .collect();
        self.rows = rows;
        self.keys.clear();
        Ok(())
    }

    #[allow(clippy::cast_possible_truncation, reason = "a row count within memory")]
    fn limit(&mut self, l: &LimitOp) -> Result<()> {
        let n = l.n as usize;
        let kept: Vec<usize> = match l.mode {
            LimitMode::First => (0..self.rows.len().min(n)).collect(),
            LimitMode::Sample => {
                // Reproducibly by the seed: the rows with the n smallest
                // scrambles of their position, in their order.
                let mut scored: Vec<(u64, usize)> = (0..self.rows.len())
                    .map(|i| (scramble(i as u64, l.seed), i))
                    .collect();
                scored.sort_unstable();
                let mut kept: Vec<usize> = scored.into_iter().take(n).map(|(_, i)| i).collect();
                kept.sort_unstable();
                kept
            }
            LimitMode::Top => {
                let groups: Vec<usize> = l
                    .group_by
                    .iter()
                    .map(|c| self.index(c))
                    .collect::<Result<_>>()?;
                let order: Vec<(usize, bool)> = l
                    .order
                    .iter()
                    .map(|k| Ok((self.index(&k.column)?, k.descending)))
                    .collect::<Result<_>>()?;
                let mut positions: Vec<usize> = (0..self.rows.len()).collect();
                positions.sort_by(|&a, &b| {
                    for &(c, descending) in &order {
                        let o = ordered(&self.rows[a][c], &self.rows[b][c], descending);
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    a.cmp(&b)
                });
                let mut taken: BTreeMap<String, usize> = BTreeMap::new();
                let mut kept = Vec::new();
                for i in positions {
                    let group: Vec<&Value> = groups.iter().map(|&c| &self.rows[i][c]).collect();
                    let count = taken.entry(format!("{group:?}")).or_default();
                    if *count < n {
                        *count += 1;
                        kept.push(i);
                    }
                }
                // In the order the rows had.
                kept.sort_unstable();
                kept
            }
        };
        self.keep(&kept);
        Ok(())
    }
}

/// The formula evaluator, or the sentence saying there is none.
fn evaluator(reader: &Reader) -> Result<Arc<dyn sc_expr::JsEvaluator>> {
    reader.evaluator().ok_or_else(|| {
        Error::invalid(
            "the formulas after a Model predictions operation are computed by the formula \
             evaluator, and this server has none running",
        )
    })
}

/// A formula's value over one row.
async fn eval_value(
    reader: &Reader,
    formula: &Formula,
    row: BTreeMap<String, Value>,
) -> Result<Value> {
    let call = FormulaCall {
        formula: formula.clone(),
        op: FormulaOperation::Read,
        row,
        user: None,
        ambient: AmbientValues::new(),
        now: chrono::Utc::now(),
    };
    Ok(value_from_json(&evaluator(reader)?.eval_value(call).await?))
}

/// A position's place in a seeded shuffle (SplitMix64).
fn scramble(position: u64, seed: i64) -> u64 {
    #[allow(clippy::cast_sign_loss, reason = "the seed's bits")]
    let mut z = position
        .wrapping_add(seed as u64)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Two values in a sort's order, nulls last whichever way, as the SQL orders
/// them.
fn ordered(a: &Value, b: &Value, descending: bool) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let o = compare(a, b);
            if descending { o.reverse() } else { o }
        }
    }
}

/// Two values that are not null, compared: numbers as numbers, the rest by
/// their kind's own order.
fn compare(a: &Value, b: &Value) -> Ordering {
    match (number(a), number(b)) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        _ => match (a, b) {
            (Value::Text(x), Value::Text(y)) => x.cmp(y),
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            (Value::Date(x), Value::Date(y)) => x.cmp(y),
            (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
            _ => format!("{a:?}").cmp(&format!("{b:?}")),
        },
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a number as the float it is summarised as"
)]
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(x) => Some(*x),
        Value::Decimal(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

/// One summary of a group.
#[allow(clippy::cast_precision_loss, reason = "counts and sums as floats")]
fn summary(
    function: SummaryFunction,
    values: &[&Value],
    members: &[usize],
    order: Option<(usize, bool)>,
    rows: &[Vec<Value>],
) -> Value {
    let present: Vec<&Value> = values.iter().copied().filter(|v| !v.is_null()).collect();
    let numbers: Vec<f64> = present.iter().filter_map(|v| number(v)).collect();
    let n = numbers.len() as f64;
    let mean = || numbers.iter().sum::<f64>() / n;
    match function {
        SummaryFunction::Count =>
        {
            #[allow(clippy::cast_possible_wrap, reason = "a row count")]
            Value::Int(present.len() as i64)
        }
        SummaryFunction::CountDistinct => {
            let distinct: std::collections::BTreeSet<String> =
                present.iter().map(|v| format!("{v:?}")).collect();
            #[allow(clippy::cast_possible_wrap, reason = "a row count")]
            Value::Int(distinct.len() as i64)
        }
        SummaryFunction::Sum if present.is_empty() => Value::Null,
        SummaryFunction::Sum => {
            if present.iter().all(|v| matches!(v, Value::Int(_))) {
                Value::Int(
                    present
                        .iter()
                        .map(|v| match v {
                            Value::Int(n) => *n,
                            _ => 0,
                        })
                        .sum(),
                )
            } else {
                Value::Float(numbers.iter().sum())
            }
        }
        SummaryFunction::Mean if numbers.is_empty() => Value::Null,
        SummaryFunction::Mean => Value::Float(mean()),
        SummaryFunction::Median if numbers.is_empty() => Value::Null,
        SummaryFunction::Median => {
            let mut sorted = numbers.clone();
            sorted.sort_by(f64::total_cmp);
            let k = sorted.len();
            Value::Float(if k % 2 == 1 {
                sorted[k / 2]
            } else {
                (sorted[k / 2 - 1] + sorted[k / 2]) / 2.0
            })
        }
        SummaryFunction::Sd if numbers.len() < 2 => Value::Null,
        SummaryFunction::Sd => {
            let m = mean();
            Value::Float((numbers.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1.0)).sqrt())
        }
        SummaryFunction::Min | SummaryFunction::Max => {
            let want = if function == SummaryFunction::Min {
                Ordering::Less
            } else {
                Ordering::Greater
            };
            present
                .iter()
                .copied()
                .reduce(|best, v| if compare(v, best) == want { v } else { best })
                .cloned()
                .unwrap_or(Value::Null)
        }
        SummaryFunction::First | SummaryFunction::Last => {
            let mut at: Vec<usize> = (0..members.len()).collect();
            if let Some((c, descending)) = order {
                at.sort_by(|&a, &b| {
                    ordered(&rows[members[a]][c], &rows[members[b]][c], descending)
                });
            }
            let pick = if function == SummaryFunction::First {
                at.first()
            } else {
                at.last()
            };
            pick.and_then(|&i| values.get(i).map(|v| (*v).clone()))
                .unwrap_or(Value::Null)
        }
        SummaryFunction::Union => Value::Null,
    }
}
