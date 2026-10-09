//! Stat cards (analytics TODO A6.2): one number from a dataset, as a
//! dashboard shows it.
//!
//! A card is an aggregate of a column — the number of incidents, the mean
//! price — with an optional filter of its own, a number format, and two ways
//! of putting the number in context:
//!
//! - a **comparison**, either with the **previous period** (the card's time
//!   column, bucketed by a day, week, month, quarter or year: the value is the
//!   current period's and the comparison the one before), or with the value
//!   **unfiltered** (the same aggregate without the card's filter or the
//!   dashboard's selections, so a card can say "23% of all");
//! - a **sparkline**: the value in each of the last N periods.
//!
//! The current period is the one holding the latest date in the (filtered)
//! rows, or the one holding today: a dataset of last year's incidents still
//! has a "this month" to compare.
//!
//! **Execution.** Formulas are the dataset's own: the card's filter is a
//! Filter operation appended to the dataset's operations, compiled with them,
//! so it is checked and translated as any Filter is; a dashboard's selections
//! are appended the same way (A6.4, [`render_card_in`]). Periods are bucketed
//! without date functions — neither database has one the other shares — by
//! comparing the time column with the periods' bounds, worked out here: one
//! `CASE` gives each row its period's number, and one grouped query (or, for
//! a median, the plot renderer's percentiles) answers every period at once.

use std::sync::Arc;

use chrono::{Datelike, Duration, Months, NaiveDate, Utc};
use sc_catalog::Catalog;
use sc_dataset::{
    ColType, DatasetDef, DatasetId, Op, Operation, Options, Schema, StageShape, compile,
};
use sc_error::{Error, Result};
use sc_query::{BinOp, CaseArm, Expr, Projection, Select, Source, Value};
use serde::{Deserialize, Serialize};

use crate::crossfilter::{Scope, filter_failure};
use crate::plot::render::{DATA, POINTS, PlotRows, Renderer, agg, cast, f64_of, g, v};
use crate::plot::{DataRef, Layer, Mark, PlotSpec, Stat};

/// The most periods a sparkline may span.
pub const MAX_SPARK_PERIODS: u32 = 120;
/// The periods a sparkline spans when the card does not say.
pub const DEFAULT_SPARK_PERIODS: u32 = 12;
/// The longest a card's suffix may be.
pub const MAX_SUFFIX: usize = 20;
/// The most decimals a card's number may show.
pub const MAX_DECIMALS: u8 = 6;

/// A stat card: the content of a `stat_card` panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatCard {
    /// The dataset the number is taken from.
    pub dataset: DatasetId,
    /// What is computed.
    pub value: CardValue,
    /// A condition on the rows, in a formula over the dataset's columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// The date column periods are taken from: needed by a comparison with
    /// the previous period and by a sparkline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<CardTime>,
    /// What the value is compared with.
    #[serde(default)]
    pub comparison: Comparison,
    /// Whether a sparkline of the last periods is drawn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sparkline: bool,
    /// How many periods the sparkline spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub periods: Option<u32>,
    /// How the number is written. The browser writes it; the server checks
    /// it.
    #[serde(default)]
    pub format: CardFormat,
    /// Whether a rise is good news (drawn green) or bad (red, as for
    /// incidents).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub higher_is_better: bool,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// The aggregate a card shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardValue {
    /// The function.
    pub function: CardFunction,
    /// The column; `count` without one counts rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
}

/// The functions a card can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardFunction {
    /// The number of rows, or of values that are not missing.
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
}

impl CardFunction {
    /// Its name in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            CardFunction::Count => "count",
            CardFunction::CountDistinct => "number of distinct values",
            CardFunction::Sum => "total",
            CardFunction::Mean => "mean",
            CardFunction::Median => "median",
            CardFunction::Min => "minimum",
            CardFunction::Max => "maximum",
        }
    }

    /// Whether it needs a column of numbers.
    fn numeric(self) -> bool {
        !matches!(self, CardFunction::Count | CardFunction::CountDistinct)
    }

    /// Whether a period with no rows is 0 rather than missing.
    fn counts(self) -> bool {
        !self.numeric()
    }
}

/// Where a card's periods come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardTime {
    /// A date or timestamp column.
    pub column: String,
    /// How long a period is.
    pub period: Period,
    /// Which period is the current one.
    #[serde(default)]
    pub anchor: Anchor,
}

/// The length of a card's periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    /// A calendar day.
    Day,
    /// A week, Monday to Sunday.
    Week,
    /// A calendar month.
    Month,
    /// A calendar quarter.
    Quarter,
    /// A calendar year.
    Year,
}

impl Period {
    /// The first day of the period holding `day`.
    pub fn start_of(self, day: NaiveDate) -> NaiveDate {
        // The first of a month of `day`'s year is always a date.
        let first_of = |month: u32| NaiveDate::from_ymd_opt(day.year(), month, 1).unwrap_or(day);
        match self {
            Period::Day => day,
            Period::Week => day - Duration::days(i64::from(day.weekday().num_days_from_monday())),
            Period::Month => first_of(day.month()),
            Period::Quarter => first_of((day.month0() / 3) * 3 + 1),
            Period::Year => first_of(1),
        }
    }

    /// The period `n` periods after the one starting `start` (before, for a
    /// negative `n`), as its first day.
    pub fn shift(self, start: NaiveDate, n: i32) -> NaiveDate {
        let months = |m: i32| {
            let by = Months::new(m.unsigned_abs());
            if m >= 0 {
                start.checked_add_months(by)
            } else {
                start.checked_sub_months(by)
            }
            .unwrap_or(start)
        };
        match self {
            Period::Day => start + Duration::days(i64::from(n)),
            Period::Week => start + Duration::days(7 * i64::from(n)),
            Period::Month => months(n),
            Period::Quarter => months(3 * n),
            Period::Year => months(12 * n),
        }
    }

    /// Its name in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            Period::Day => "day",
            Period::Week => "week",
            Period::Month => "month",
            Period::Quarter => "quarter",
            Period::Year => "year",
        }
    }
}

/// Which period is a card's current one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    /// The period of the latest date in the rows.
    #[default]
    Latest,
    /// The period of today.
    Today,
}

/// What a card's value is compared with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// Nothing.
    #[default]
    None,
    /// The period before the current one.
    PreviousPeriod,
    /// The same aggregate without the card's filter (and the dashboard's).
    Unfiltered,
}

/// How a card writes its number.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardFormat {
    /// A number, a percentage (0.25 is 25%) or an amount of money.
    #[serde(default)]
    pub style: NumberStyle,
    /// Decimals shown; the browser's choice when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    /// The ISO 4217 code of a currency (`EUR`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// 1.2K rather than 1,234.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compact: bool,
    /// A unit written after the number (`m²`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

/// How a number is written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumberStyle {
    /// A plain number.
    #[default]
    Number,
    /// A fraction written as a percentage.
    Percent,
    /// An amount in a currency.
    Currency,
}

impl StatCard {
    /// What is wrong with it on its own, before its dataset is read.
    pub fn check(&self) -> Result<()> {
        if self.value.function.numeric() && self.value.column.is_none() {
            return Err(Error::invalid(format!(
                "a stat card's {} needs a column",
                self.value.function.describe()
            )));
        }
        if self.time.is_none() {
            if self.comparison == Comparison::PreviousPeriod {
                return Err(Error::invalid(
                    "a stat card compared with the previous period needs a date column to take periods from",
                ));
            }
            if self.sparkline {
                return Err(Error::invalid(
                    "a stat card's sparkline needs a date column to take periods from",
                ));
            }
        }
        if let Some(n) = self.periods
            && !(2..=MAX_SPARK_PERIODS).contains(&n)
        {
            return Err(Error::invalid(format!(
                "a sparkline spans 2 to {MAX_SPARK_PERIODS} periods, not {n}"
            )));
        }
        let format = &self.format;
        if format.decimals.is_some_and(|d| d > MAX_DECIMALS) {
            return Err(Error::invalid(format!(
                "a stat card shows at most {MAX_DECIMALS} decimals"
            )));
        }
        if format.style == NumberStyle::Currency {
            let code = format.currency.as_deref().unwrap_or_default();
            if code.len() != 3 || !code.chars().all(|c| c.is_ascii_uppercase()) {
                return Err(Error::invalid(format!(
                    "an amount of money needs a currency's three-letter code, such as EUR, not \"{code}\""
                )));
            }
        }
        if format
            .suffix
            .as_ref()
            .is_some_and(|s| s.chars().count() > MAX_SUFFIX)
        {
            return Err(Error::invalid(format!(
                "a stat card's unit is at most {MAX_SUFFIX} characters"
            )));
        }
        Ok(())
    }

    /// What it shows, in words: "count of rows", "mean of price".
    pub fn label(&self) -> String {
        match &self.value.column {
            None => "count of rows".to_owned(),
            Some(c) => format!("{} of {c}", self.value.function.describe()),
        }
    }

    /// What is wrong with it over the dataset's columns, as sentences.
    pub fn validate(&self, shape: &StageShape) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(name) = &self.value.column {
            match shape.column(name) {
                None => problems.push(format!("the dataset has no column `{name}`")),
                Some(c) if self.value.function.numeric() && !c.ty.is_numeric() => {
                    problems.push(format!(
                        "the {} needs numbers, and `{name}` is {}",
                        self.value.function.describe(),
                        article(c.ty.name())
                    ));
                }
                Some(_) => {}
            }
        }
        if let Some(time) = &self.time {
            match shape.column(&time.column) {
                None => problems.push(format!("the dataset has no column `{}`", time.column)),
                Some(c) if !matches!(c.ty, ColType::Date | ColType::Timestamp) => {
                    problems.push(format!(
                        "periods are taken from a date, and `{}` is {}",
                        time.column,
                        article(c.ty.name())
                    ));
                }
                Some(_) => {}
            }
        }
        problems
    }
}

fn article(noun: &str) -> String {
    match noun.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => format!("an {noun}"),
        _ => format!("a {noun}"),
    }
}

// --- what a card answers --------------------------------------------------------

/// What `render_card` answers: the card's numbers, or why there are none.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RenderedCard {
    /// The numbers.
    Card(CardData),
    /// The card cannot be made.
    Refused {
        /// The first reason, as a sentence.
        error: String,
        /// Every reason.
        problems: Vec<String>,
    },
}

impl RenderedCard {
    fn refuse(error: impl Into<String>) -> RenderedCard {
        let error = error.into();
        RenderedCard::Refused {
            problems: vec![error.clone()],
            error,
        }
    }
}

/// A card's numbers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CardData {
    /// The value: over every row, or over the current period. Missing when
    /// there is nothing to take it from.
    pub value: Option<f64>,
    /// What it is, in words.
    pub label: String,
    /// The current period, when the card has periods and there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period: Option<Span>,
    /// What it is compared with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<Compared>,
    /// The value in each of the last periods, oldest first, the current one
    /// last.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sparkline: Vec<SparkPoint>,
}

/// A period: its first day and the day after its last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    /// The first day.
    pub start: NaiveDate,
    /// The day after the last.
    pub end: NaiveDate,
    /// Its length.
    pub period: Period,
}

/// The value a card is compared with.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Compared {
    /// `previous_period` or `unfiltered`.
    pub kind: Comparison,
    /// The other value.
    pub value: Option<f64>,
    /// The previous period, for that comparison.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period: Option<Span>,
    /// The value less the other.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<f64>,
    /// The value over the other: 1.12 is 12% more, 0.23 is 23% of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
}

/// One period of a sparkline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SparkPoint {
    /// The period's first day.
    pub start: NaiveDate,
    /// Its value; missing for a mean of nothing.
    pub value: Option<f64>,
}

/// Work out `card` from its dataset as it is now. Reads as the admin, as
/// `render_plot` does.
pub async fn render_card(catalog: &Catalog, card: &StatCard) -> Result<RenderedCard> {
    render_card_in(catalog, card, &Scope::none()).await
}

/// [`render_card`] on a dashboard whose conditions apply (A6.4): they narrow
/// the rows as the card's own filter does, and a comparison with the value
/// **unfiltered** leaves both out — "34% of all" is a share of every row.
pub async fn render_card_in(
    catalog: &Catalog,
    card: &StatCard,
    scope: &Scope,
) -> Result<RenderedCard> {
    card.check()?;
    let Some(def) = sc_dataset::load_dataset(catalog, card.dataset).await? else {
        return Ok(RenderedCard::refuse(
            "the dataset this card reads is gone; pick another",
        ));
    };
    let mut filters: Vec<Operation> = card
        .filter
        .iter()
        .filter(|f| !f.trim().is_empty())
        .map(|f| Operation::new("_fd_card_filter", Op::filter(f.clone())))
        .collect();
    filters.extend(scope.operations(card.dataset));
    let all = match card_rows(catalog, &def, &[]).await? {
        Ok(rows) => rows,
        Err(sentence) => return Ok(RenderedCard::refuse(sentence)),
    };
    let problems = card.validate(&all.shape);
    if let Some(first) = problems.first() {
        return Ok(RenderedCard::Refused {
            error: first.clone(),
            problems,
        });
    }
    let filtered = if filters.is_empty() {
        None
    } else {
        match card_rows(catalog, &def, &filters).await? {
            Ok(rows) => Some(rows),
            Err(sentence) => {
                let mut def = def.clone();
                def.operations.extend(filters);
                let schema = Schema::of_catalog(catalog)?;
                let library = sc_dataset::load_library(catalog).await?;
                return Ok(RenderedCard::refuse(
                    filter_failure(&schema, &library, &def)
                        .unwrap_or_else(|| format!("the card's filter does not read: {sentence}")),
                ));
            }
        }
    };
    let rows = filtered.as_ref().unwrap_or(&all);
    let data = Card {
        card,
        rows,
        all: &all,
    };
    Ok(RenderedCard::Card(data.numbers().await?))
}

/// The rows of `def` with `extra` appended to its operations, or the sentence
/// saying why they do not read.
async fn card_rows(
    catalog: &Catalog,
    def: &DatasetDef,
    extra: &[Operation],
) -> Result<std::result::Result<PlotRows, String>> {
    let schema = Schema::of_catalog(catalog)?;
    let library = sc_dataset::load_library(catalog).await?;
    let mut def = def.clone();
    def.operations.extend(extra.iter().cloned());
    let compiled = compile(&schema, &library, &def, Options::default());
    let stage = match compiled.last() {
        Ok(stage) => stage,
        Err(e) => return Ok(Err(e)),
    };
    Ok(stage.unordered_query().map(|query| PlotRows {
        query,
        shape: stage.shape(),
        db: Arc::clone(catalog.primary()),
    }))
}

struct Card<'a> {
    card: &'a StatCard,
    /// The rows the value is taken from: filtered, when the card has a filter.
    rows: &'a PlotRows,
    /// Every row: what an unfiltered comparison reads.
    all: &'a PlotRows,
}

impl Card<'_> {
    async fn numbers(&self) -> Result<CardData> {
        let card = self.card;
        let mut out = CardData {
            value: None,
            label: card.label(),
            period: None,
            comparison: None,
            sparkline: Vec::new(),
        };
        let Some(time) = &card.time else {
            out.value = self.values(self.rows, None).await?[0];
            if card.comparison == Comparison::Unfiltered {
                let other = self.values(self.all, None).await?[0];
                out.comparison = Some(compared(Comparison::Unfiltered, out.value, other, None));
            }
            return Ok(out);
        };
        let Some(current) = self.current_period(time).await? else {
            // No dates to take a period from: nothing in any period.
            let empty = card.value.function.counts().then_some(0.0);
            out.value = empty;
            if card.comparison != Comparison::None {
                out.comparison = Some(compared(card.comparison, empty, empty, None));
            }
            return Ok(out);
        };
        let span = |start: NaiveDate| Span {
            start,
            end: time.period.shift(start, 1),
            period: time.period,
        };
        // The periods read: the sparkline's, or the current one and the one
        // before.
        let n = if card.sparkline {
            card.periods.unwrap_or(DEFAULT_SPARK_PERIODS) as i32
        } else if card.comparison == Comparison::PreviousPeriod {
            2
        } else {
            1
        };
        let bounds: Vec<NaiveDate> = (0..=n)
            .map(|i| time.period.shift(current, i - n + 1))
            .collect();
        let values = self.values(self.rows, Some((time, &bounds))).await?;
        out.value = values[values.len() - 1];
        out.period = Some(span(current));
        match card.comparison {
            Comparison::None => {}
            Comparison::PreviousPeriod => {
                let previous = time.period.shift(current, -1);
                let other = if values.len() >= 2 {
                    values[values.len() - 2]
                } else {
                    let two = [previous, current];
                    self.values(self.rows, Some((time, &two))).await?[0]
                };
                out.comparison = Some(compared(
                    Comparison::PreviousPeriod,
                    out.value,
                    other,
                    Some(span(previous)),
                ));
            }
            Comparison::Unfiltered => {
                let one = [current, time.period.shift(current, 1)];
                let other = self.values(self.all, Some((time, &one))).await?[0];
                out.comparison = Some(compared(Comparison::Unfiltered, out.value, other, None));
            }
        }
        if card.sparkline {
            out.sparkline = bounds
                .iter()
                .zip(&values)
                .map(|(start, value)| SparkPoint {
                    start: *start,
                    value: *value,
                })
                .collect();
        }
        Ok(out)
    }

    /// The first day of the current period: today's, or the latest date's in
    /// the rows. `None` when there are no dates.
    async fn current_period(&self, time: &CardTime) -> Result<Option<NaiveDate>> {
        let day =
            match time.anchor {
                Anchor::Today => Some(Utc::now().date_naive()),
                Anchor::Latest => {
                    let latest = Select::from(Source::subquery(self.rows.query.clone(), DATA))
                        .columns(vec![Projection::expr_as(
                            agg("max", vec![Expr::qcol(DATA, time.column.clone())]),
                            "_latest",
                        )]);
                    let rows = self.run(self.rows, latest).await?;
                    rows.into_iter()
                        .next()
                        .and_then(|r| r.into_values().into_iter().next())
                        .and_then(|v| day_of(&v))
                }
            };
        Ok(day.map(|d| time.period.start_of(d)))
    }

    /// The card's aggregate over `rows`: once over every row, or once per
    /// period between consecutive `bounds` (each period's first day, then the
    /// day after the last period).
    async fn values(
        &self,
        rows: &PlotRows,
        periods: Option<(&CardTime, &[NaiveDate])>,
    ) -> Result<Vec<Option<f64>>> {
        let card = self.card;
        let function = card.value.function;
        let column = card.value.column.as_deref();
        let field = |c: &str| Expr::qcol(DATA, c.to_owned());
        let input = match column {
            Some(c) if function.numeric() => cast(field(c), "double precision"),
            Some(c) => field(c),
            None => cast(Expr::lit(Value::Int(1)), "bigint"),
        };
        let mut columns = Vec::new();
        let mut filter = None;
        let groups = match periods {
            None => 0,
            Some((time, bounds)) => {
                let ty = rows
                    .shape
                    .column(&time.column)
                    .map_or(ColType::Date, |c| c.ty);
                let at = |d: NaiveDate| Expr::Lit(bound(d, ty));
                let t = field(&time.column);
                let arms = bounds[1..]
                    .iter()
                    .enumerate()
                    .map(|(i, end)| CaseArm {
                        when: Expr::binary(BinOp::Lt, t.clone(), at(*end)),
                        then: cast(Expr::lit(Value::Int(i as i64)), "bigint"),
                    })
                    .collect();
                columns.push(Projection::expr_as(
                    Expr::Case {
                        operand: None,
                        arms,
                        else_result: None,
                    },
                    g(0),
                ));
                filter = Some(
                    Expr::binary(BinOp::Ge, t.clone(), at(bounds[0])).and(Expr::binary(
                        BinOp::Lt,
                        t,
                        at(bounds[bounds.len() - 1]),
                    )),
                );
                1
            }
        };
        columns.push(Projection::expr_as(input, v(0)));
        let mut points = Select::from(Source::subquery(rows.query.clone(), DATA)).columns(columns);
        points.filter = filter;
        let buckets = periods.map_or(1, |(_, b)| b.len() - 1);
        let mut out: Vec<Option<f64>> = vec![function.counts().then_some(0.0); buckets];
        let bucket = |keys: &[Value]| -> Option<usize> {
            if groups == 0 {
                return Some(0);
            }
            keys.first().and_then(f64_of).map(|k| k as usize)
        };

        if function == CardFunction::Median {
            let carrier = carrier_spec(card.dataset);
            let renderer = Renderer::new(&carrier, rows, rows.shape.clone());
            let stats = renderer
                .percentiles(points, groups, &[0.5], buckets + 1)
                .await
                .map_err(halt)?;
            for s in stats {
                if let Some(i) = bucket(&s.keys).filter(|i| *i < buckets) {
                    out[i] = s.q.first().copied().filter(|q| q.is_finite());
                }
            }
            return Ok(out);
        }

        let value = Expr::qcol(POINTS, v(0));
        let aggregate = match function {
            CardFunction::Count => match column {
                None => agg("count", vec![]),
                Some(_) => agg("count", vec![value]),
            },
            CardFunction::CountDistinct => Expr::Agg {
                func: "count".into(),
                distinct: true,
                args: vec![value],
            },
            CardFunction::Sum => agg("sum", vec![value]),
            CardFunction::Mean => agg("avg", vec![value]),
            CardFunction::Min => agg("min", vec![value]),
            CardFunction::Max => agg("max", vec![value]),
            CardFunction::Median => unreachable!("taken above"),
        };
        let mut select_columns: Vec<Projection> = (0..groups)
            .map(|i| Projection::expr_as(Expr::qcol(POINTS, g(i)), g(i)))
            .collect();
        select_columns.push(Projection::expr_as(aggregate, "_value"));
        let mut select = Select::from(Source::subquery(points, POINTS)).columns(select_columns);
        select.group = (0..groups).map(|i| Expr::qcol(POINTS, g(i))).collect();
        for row in self.run(rows, select).await? {
            let values = row.into_values();
            let (keys, rest) = values.split_at(groups);
            if let Some(i) = bucket(keys).filter(|i| *i < buckets) {
                out[i] = rest.first().and_then(f64_of);
            }
        }
        Ok(out)
    }

    async fn run(&self, rows: &PlotRows, select: Select) -> Result<Vec<sc_db::Row>> {
        rows.db
            .query(&sc_query::Statement::from(select))
            .await?
            .try_collect()
            .await
    }
}

/// A plot spec with no layers over the card's dataset: what the plot
/// renderer's percentiles need beside the rows.
fn carrier_spec(dataset: DatasetId) -> PlotSpec {
    let mut spec = PlotSpec::single(
        DataRef::Dataset { dataset },
        Layer::new(Mark::Text, Stat::Count),
    );
    spec.layers.clear();
    spec
}

fn halt(h: crate::plot::render::Halt) -> Error {
    match h {
        crate::plot::render::Halt::Fail(e) => e,
        crate::plot::render::Halt::Refuse(s) => Error::invalid(s),
    }
}

/// A period's bound as the time column holds it: a date, or midnight UTC.
fn bound(day: NaiveDate, ty: ColType) -> Value {
    match ty {
        ColType::Timestamp => Value::Timestamp(day.and_time(chrono::NaiveTime::MIN).and_utc()),
        _ => Value::Date(day),
    }
}

/// The day a value read from a date or timestamp column is on. SQLite answers
/// an aggregate of one as its text.
fn day_of(value: &Value) -> Option<NaiveDate> {
    match value {
        Value::Date(d) => Some(*d),
        Value::Timestamp(t) => Some(t.date_naive()),
        Value::Text(s) => {
            let s = s.trim();
            NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").ok()
        }
        _ => None,
    }
}

fn compared(
    kind: Comparison,
    value: Option<f64>,
    other: Option<f64>,
    period: Option<Span>,
) -> Compared {
    let change = value.zip(other).map(|(a, b)| a - b);
    let ratio = value
        .zip(other)
        .filter(|(_, b)| *b != 0.0)
        .map(|(a, b)| a / b);
    Compared {
        kind,
        value: other,
        period,
        change,
        ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("a date")
    }

    #[test]
    fn periods_start_on_their_first_day_and_shift_by_whole_periods() {
        let d = day(2024, 5, 15); // a Wednesday
        assert_eq!(Period::Day.start_of(d), d);
        assert_eq!(Period::Week.start_of(d), day(2024, 5, 13));
        assert_eq!(Period::Month.start_of(d), day(2024, 5, 1));
        assert_eq!(Period::Quarter.start_of(d), day(2024, 4, 1));
        assert_eq!(Period::Year.start_of(d), day(2024, 1, 1));
        assert_eq!(
            Period::Quarter.start_of(day(2024, 12, 31)),
            day(2024, 10, 1)
        );

        assert_eq!(Period::Month.shift(day(2024, 1, 1), -1), day(2023, 12, 1));
        assert_eq!(Period::Month.shift(day(2024, 1, 1), 13), day(2025, 2, 1));
        assert_eq!(Period::Quarter.shift(day(2024, 10, 1), 1), day(2025, 1, 1));
        assert_eq!(Period::Week.shift(day(2024, 5, 13), -2), day(2024, 4, 29));
        assert_eq!(Period::Year.shift(day(2024, 1, 1), -3), day(2021, 1, 1));
        assert_eq!(Period::Day.shift(day(2024, 2, 28), 2), day(2024, 3, 1));
    }

    fn card(raw: serde_json::Value) -> StatCard {
        serde_json::from_value(raw).expect("a card")
    }

    #[test]
    fn a_card_round_trips_with_its_defaults_left_out() {
        let d = DatasetId::new();
        let minimal = card(json!({ "dataset": d, "value": { "function": "count" } }));
        assert_eq!(minimal.comparison, Comparison::None);
        assert!(minimal.higher_is_better);
        assert_eq!(
            serde_json::to_value(&minimal).expect("serialises"),
            json!({ "dataset": d, "value": { "function": "count" },
                    "comparison": "none", "format": { "style": "number" } })
        );
        let full = card(json!({
            "dataset": d,
            "value": { "function": "mean", "column": "price" },
            "filter": "price > 100000",
            "time": { "column": "sold_on", "period": "month", "anchor": "today" },
            "comparison": "previous_period",
            "sparkline": true, "periods": 24,
            "format": { "style": "currency", "currency": "EUR", "decimals": 0, "compact": true },
            "higher_is_better": false
        }));
        full.check().expect("a good card");
        assert_eq!(full.label(), "mean of price");
        let back: StatCard =
            serde_json::from_value(serde_json::to_value(&full).expect("serialises"))
                .expect("reads");
        assert_eq!(back, full);
    }

    #[test]
    fn a_card_that_cannot_be_made_is_refused_with_a_sentence() {
        let d = DatasetId::new();
        let refused = |raw: serde_json::Value, says: &str| {
            let err = card(raw).check().expect_err(says);
            assert!(err.to_string().contains(says), "{err} should say {says}");
        };
        refused(
            json!({ "dataset": d, "value": { "function": "sum" } }),
            "total needs a column",
        );
        refused(
            json!({ "dataset": d, "value": { "function": "count" }, "comparison": "previous_period" }),
            "needs a date column",
        );
        refused(
            json!({ "dataset": d, "value": { "function": "count" }, "sparkline": true }),
            "sparkline needs a date column",
        );
        refused(
            json!({ "dataset": d, "value": { "function": "count" },
                    "time": { "column": "at", "period": "day" }, "sparkline": true, "periods": 500 }),
            "2 to 120 periods",
        );
        refused(
            json!({ "dataset": d, "value": { "function": "count" },
                    "format": { "style": "currency", "currency": "euro" } }),
            "three-letter code",
        );
        refused(
            json!({ "dataset": d, "value": { "function": "count" }, "format": { "decimals": 9 } }),
            "at most 6 decimals",
        );

        // Over the dataset's columns.
        use sc_dataset::{Grain, StageColumn};
        let shape = StageShape {
            columns: vec![
                StageColumn {
                    name: "price".into(),
                    ty: ColType::Float,
                    key: None,
                },
                StageColumn {
                    name: "category".into(),
                    ty: ColType::Text,
                    key: None,
                },
                StageColumn {
                    name: "at".into(),
                    ty: ColType::Date,
                    key: None,
                },
            ],
            grain: Grain::Derived,
        };
        let problems = card(
            json!({ "dataset": d, "value": { "function": "mean", "column": "category" },
            "time": { "column": "price", "period": "month" } }),
        )
        .validate(&shape);
        assert_eq!(
            problems,
            vec![
                "the mean needs numbers, and `category` is a text",
                "periods are taken from a date, and `price` is a number",
            ]
        );
        assert!(
            card(json!({ "dataset": d, "value": { "function": "count_distinct", "column": "category" },
                "time": { "column": "at", "period": "week" } }))
            .validate(&shape)
            .is_empty()
        );
        assert_eq!(
            card(json!({ "dataset": d, "value": { "function": "max", "column": "nope" } }))
                .validate(&shape),
            vec!["the dataset has no column `nope`"]
        );
    }

    #[test]
    fn a_comparison_gives_the_change_and_the_ratio() {
        let c = compared(Comparison::PreviousPeriod, Some(12.0), Some(10.0), None);
        assert_eq!(c.change, Some(2.0));
        assert_eq!(c.ratio, Some(1.2));
        let c = compared(Comparison::Unfiltered, Some(3.0), Some(0.0), None);
        assert_eq!(c.ratio, None, "no ratio to nothing");
        let c = compared(Comparison::Unfiltered, None, Some(4.0), None);
        assert_eq!((c.change, c.ratio, c.value), (None, None, Some(4.0)));
        assert_eq!(
            day_of(&Value::Text("2024-03-05T10:00:00.000Z".into())),
            Some(day(2024, 3, 5))
        );
    }
}
