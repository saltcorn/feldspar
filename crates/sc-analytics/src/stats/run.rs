//! Running the tests over a dataset (analytics TODO A2.12–A2.14): the
//! sufficient statistics in SQL over the dataset's compiled query, by the
//! plot renderer's machinery, and the values the rank tests need read from a
//! sample.
//!
//! Each design asks the database one grouped question — the moments of Y by
//! each group of X, the counts of each combination of categories, the centred
//! sums of two numbers — with Wrap's value as one more group key, so Wrap
//! repeats the analysis without repeating the queries. Then, for the tests
//! that read values (the rank tests, Shapiro–Wilk, logistic regression, the
//! assumption checks), one more query reads the values, at most
//! [`TEST_SAMPLE`] of each Wrap group: a random sample, the same each time,
//! taken as the plots take theirs. A result read from a sample says so.

use std::collections::HashMap;

use sc_catalog::Catalog;
use sc_dataset::{ColType, StageShape, scramble, value_json};
use sc_error::Result;
use sc_query::{BinOp, Expr, Nulls, OrderBy, OrderDir, Projection, Select, Source, UnOp, Value};
use serde::Serialize;
use serde_json::Value as Json;

use super::choose::{
    ASSUMPTION_LEVEL, Check, Choice, Design, LARGE_GROUP, MIN_EVENTS, MIN_EXPECTED, SMALL_GROUP,
    TestSpec, choose,
};
use super::hypothesis::{
    Moments, TestKind, TestResult, anova, binomial, chisq_fit, chisq_independence, expected_counts,
    fisher_exact, kruskal_wallis, levene, linear_regression, logistic_regression, mann_whitney,
    one_sample_t, paired_signed_rank, paired_t, pearson, shapiro, signed_rank, spearman, tukey_hsd,
    welch_t,
};
use crate::plot::render::{
    Halt, INNER, Key, POINTS, Renderer, SEED, Step, agg, cast, f64_of, group_exprs, group_order,
    group_projections, key_values, last_stage, v,
};
use crate::plot::validate::Dim;
use crate::plot::{Channel, FieldDef, Layer, LinearSums, Mark, PlotSpec, Stat};

/// The most values of one Wrap group the tests that read values read; more
/// are sampled. Shapiro–Wilk reads at most this many in any case.
pub const TEST_SAMPLE: u64 = 5_000;
/// The most Wrap groups the analysis is repeated for.
pub const MAX_SECTIONS: usize = 48;
/// The most groups (or categories) a test compares.
pub const MAX_GROUPS: usize = 50;
/// The most groups Tukey's pairwise comparisons are made for.
pub const MAX_PAIRWISE_GROUPS: usize = 20;

const SAMPLED: &str = "_fd_r";

/// What `run_tests` answers: the analysis, or why there is none.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum TestsAnswer {
    /// The tests' results.
    Analysis(Box<Analysis>),
    /// No test applies.
    Refused {
        /// Why, as a sentence.
        error: String,
        /// Every reason.
        problems: Vec<String>,
    },
}

impl TestsAnswer {
    fn refuse(error: impl Into<String>) -> TestsAnswer {
        let error = error.into();
        TestsAnswer::Refused {
            problems: vec![error.clone()],
            error,
        }
    }
}

/// The tests for one set of roles, repeated for each value of Wrap.
#[derive(Debug, Clone, Serialize)]
pub struct Analysis {
    /// Which analysis the roles make.
    pub design: Design,
    /// The columns on Y.
    pub y: Vec<String>,
    /// The column on X.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    /// The column on Wrap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The value a single number is tested against.
    pub mu: f64,
    /// The confidence level of the intervals.
    pub level: f64,
    /// One per value of Wrap, in order (one in all without Wrap).
    pub sections: Vec<Section>,
}

/// A group compared, or a category counted.
#[derive(Debug, Clone, Serialize)]
pub struct Level {
    /// Its value (a bin's lower edge).
    pub value: Json,
    /// A bin's upper edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<Json>,
    /// How many rows it has.
    pub n: u64,
    /// Its mean, for a group of numbers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    /// Its standard deviation, for a group of numbers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sd: Option<f64>,
}

/// One test's place in the analysis, and what it answered.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Which test.
    pub test: TestKind,
    /// `main`, or `alternative`: the test shown alongside when the main
    /// one's assumptions are doubtful.
    pub role: &'static str,
    /// What it answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<TestResult>,
    /// Why it could not be computed here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One of Tukey's pairwise comparisons, between two of the section's levels.
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    /// The first level's index.
    pub a: usize,
    /// The second level's index.
    pub b: usize,
    /// The second level's mean less the first's.
    pub difference: f64,
    /// The simultaneous interval's lower end.
    pub lower: f64,
    /// Its upper end.
    pub upper: f64,
    /// The adjusted p-value.
    pub p_value: f64,
}

/// The analysis of one Wrap group (or of every row, without Wrap).
#[derive(Debug, Clone, Serialize)]
pub struct Section {
    /// The Wrap value (a bin's lower edge); absent without Wrap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<Json>,
    /// A Wrap bin's upper edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_end: Option<Json>,
    /// How many rows have every role's value.
    pub n: u64,
    /// The groups compared — X's values — or the categories counted —
    /// Y's values; for a paired analysis, the two columns.
    pub levels: Vec<Level>,
    /// For two categories, Y's values (the table's columns).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<Level>,
    /// Which of the levels is the outcome counted (a binomial's success, a
    /// logistic regression's event): the second of two.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<usize>,
    /// The tests, the main ones first.
    pub tests: Vec<Entry>,
    /// Tukey's pairwise comparisons, after an analysis of variance.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comparisons: Vec<Comparison>,
    /// What the assumption checks found.
    pub checks: Vec<Check>,
    /// The test the plain-language sentence reports: the main one, or the
    /// alternative when a check failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred: Option<TestKind>,
    /// How many values the tests that read values read, when that is a
    /// sample of `n`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampled: Option<u64>,
    /// Why there is no test for this group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Section {
    fn new(by: &[Json]) -> Section {
        Section {
            by: by.first().cloned(),
            by_end: by.get(1).cloned(),
            n: 0,
            levels: Vec::new(),
            categories: Vec::new(),
            event: None,
            tests: Vec::new(),
            comparisons: Vec::new(),
            checks: Vec::new(),
            preferred: None,
            sampled: None,
            error: None,
        }
    }

    fn push(
        &mut self,
        design: Design,
        test: TestKind,
        outcome: std::result::Result<TestResult, String>,
    ) {
        let role = if design.alternatives().contains(&test) {
            "alternative"
        } else {
            "main"
        };
        let (result, error) = match outcome {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e)),
        };
        self.tests.push(Entry {
            test,
            role,
            result,
            error,
        });
    }

    fn result(&self, test: TestKind) -> Option<&TestResult> {
        self.tests
            .iter()
            .find(|e| e.test == test)
            .and_then(|e| e.result.as_ref())
    }

    /// The main test, unless a check failed and the alternative answered.
    fn prefer(&mut self, main: TestKind, alternative: Option<TestKind>) {
        let doubtful = self.checks.iter().any(|c| !c.ok);
        self.preferred = match alternative {
            Some(alt) if doubtful && self.result(alt).is_some() => Some(alt),
            _ if self.result(main).is_some() => Some(main),
            _ => alternative.filter(|a| self.result(*a).is_some()),
        };
    }
}

/// Run the tests the roles of `spec` make, over its dataset: every Wrap
/// group's results, or the sentence saying why there is no test. Reads as
/// the admin, as `render_plot` does.
pub async fn run_tests(catalog: &Catalog, spec: &TestSpec) -> Result<TestsAnswer> {
    let stage = match last_stage(catalog, &spec.data).await? {
        Ok(stage) => stage,
        Err(sentence) => return Ok(TestsAnswer::refuse(sentence)),
    };
    let shape = stage.shape();
    let choice = match choose(spec, &shape) {
        Ok(c) => c,
        Err(sentence) => return Ok(TestsAnswer::refuse(sentence)),
    };
    let mut carrier = PlotSpec::single(spec.data.clone(), Layer::new(Mark::Point, Stat::Identity));
    carrier.layers.clear();
    let mut tester = Tester {
        r: Renderer::new(catalog, &carrier, &stage, shape.clone()),
        spec,
        choice: &choice,
        shape: &shape,
    };
    match tester.analyse().await {
        Ok(sections) => Ok(TestsAnswer::Analysis(Box::new(Analysis {
            design: choice.design,
            y: choice.y.iter().map(|(f, _)| f.field.clone()).collect(),
            x: choice.x.as_ref().map(|(f, _)| f.field.clone()),
            by: choice.by.as_ref().map(|(f, _)| f.field.clone()),
            mu: spec.mu,
            level: spec.level,
            sections,
        }))),
        Err(Halt::Refuse(sentence)) => Ok(TestsAnswer::refuse(sentence)),
        Err(Halt::Fail(e)) => Err(e),
    }
}

/// A group key's values made into one string, so rows from different
/// queries find their group whatever type a database returned a number as.
fn canon(values: &[Value]) -> String {
    let mut out = String::new();
    for value in values {
        match f64_of(value) {
            Some(x) => out.push_str(&format!("n{x:e}|")),
            None => out.push_str(&format!("{}|", value_json(value))),
        }
    }
    out
}

/// Rows grouped by their keys, in the order the groups first appear.
struct Grouped<T> {
    order: Vec<(Vec<Value>, T)>,
    index: HashMap<String, usize>,
}

impl<T: Default> Grouped<T> {
    fn new() -> Grouped<T> {
        Grouped {
            order: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn entry(&mut self, keys: &[Value]) -> &mut T {
        let k = canon(keys);
        let i = match self.index.get(&k) {
            Some(i) => *i,
            None => {
                self.order.push((keys.to_vec(), T::default()));
                self.index.insert(k, self.order.len() - 1);
                self.order.len() - 1
            }
        };
        &mut self.order[i].1
    }

    fn get(&self, keys: &[Value]) -> Option<&T> {
        self.index.get(&canon(keys)).map(|i| &self.order[*i].1)
    }
}

struct Tester<'a> {
    r: Renderer<'a>,
    spec: &'a TestSpec,
    choice: &'a Choice,
    shape: &'a StageShape,
}

impl Tester<'_> {
    async fn analyse(&mut self) -> Step<Vec<Section>> {
        match self.choice.design {
            Design::OneNumber => self.one_number().await,
            Design::OneCategory => self.one_category().await,
            Design::NumberByGroups => self.number_by_groups().await,
            Design::TwoCategories => self.two_categories().await,
            Design::TwoNumbers => self.two_numbers().await,
            Design::CategoryByNumber => self.category_by_number().await,
            Design::Paired => self.paired().await,
        }
    }

    // --- keys, inputs and queries ----------------------------------------------

    fn dim(&self, channel: Channel, f: &FieldDef) -> Dim {
        Dim {
            channel,
            field: f.field.clone(),
            ty: self
                .shape
                .column(&f.field)
                .map_or(ColType::Unknown, |c| c.ty),
            bin: f.bin,
        }
    }

    /// The Wrap key, if there is one.
    async fn by_keys(&mut self) -> Step<Vec<Key>> {
        match &self.choice.by {
            None => Ok(Vec::new()),
            Some((f, _)) => {
                let dim = self.dim(Channel::Wrap, f);
                Ok(vec![self.r.key(&dim).await?])
            }
        }
    }

    async fn key_of(&mut self, channel: Channel, f: &FieldDef) -> Step<Key> {
        let dim = self.dim(channel, f);
        self.r.key(&dim).await
    }

    fn number(&self, f: &FieldDef) -> Expr {
        cast(self.r.field(&f.field), "double precision")
    }

    fn present(&self, f: &FieldDef) -> Expr {
        Expr::unary(UnOp::IsNotNull, self.r.field(&f.field))
    }

    fn y(&self, i: usize) -> &FieldDef {
        &self.choice.y[i].0
    }

    fn x(&self) -> &FieldDef {
        // Every design that calls this has an X.
        &self.choice.x.as_ref().map_or(&self.choice.y[0], |x| x).0
    }

    /// `SELECT keys, count(_v0), avg(_v0), stddev_samp(_v0)` and, for each
    /// further input, its average: one row per group, in key order.
    async fn moments(
        &mut self,
        keys: &[Key],
        inputs: Vec<Expr>,
        filters: &[Expr],
    ) -> Step<Vec<(Vec<Value>, Moments, Vec<f64>)>> {
        let extra = inputs.len().saturating_sub(1);
        let points = self.r.points(keys, inputs, filters)?;
        let value = Expr::qcol(POINTS, v(0));
        let mut columns = group_projections(keys.len());
        columns.push(Projection::expr_as(agg("count", vec![value.clone()]), "_n"));
        columns.push(Projection::expr_as(agg("avg", vec![value.clone()]), "_m"));
        columns.push(Projection::expr_as(agg("stddev_samp", vec![value]), "_s"));
        for i in 1..=extra {
            columns.push(Projection::expr_as(
                agg("avg", vec![Expr::qcol(POINTS, v(i))]),
                format!("_a{i}"),
            ));
        }
        let rows = self.grouped(points, columns, keys.len()).await?;
        Ok(rows
            .into_iter()
            .map(|(k, s)| {
                let n = s.first().and_then(f64_of).unwrap_or(0.0) as u64;
                let m = Moments {
                    n,
                    mean: s.get(1).and_then(f64_of).unwrap_or(f64::NAN),
                    sd: s.get(2).and_then(f64_of).unwrap_or(f64::NAN),
                };
                let more = s[3..]
                    .iter()
                    .map(|x| f64_of(x).unwrap_or(f64::NAN))
                    .collect();
                (k, m, more)
            })
            .collect())
    }

    /// `SELECT keys, count(*)` for each combination of the keys.
    async fn counts(&mut self, keys: &[Key], filters: &[Expr]) -> Step<Vec<(Vec<Value>, u64)>> {
        let points = self.r.points(keys, Vec::new(), filters)?;
        let mut columns = group_projections(keys.len());
        columns.push(Projection::expr_as(agg("count", vec![]), "_n"));
        let rows = self.grouped(points, columns, keys.len()).await?;
        Ok(rows
            .into_iter()
            .map(|(k, s)| (k, s.first().and_then(f64_of).unwrap_or(0.0) as u64))
            .collect())
    }

    /// A grouped select over `points`, its rows split into the keys and the
    /// rest. Refused when there are more groups than any analysis compares.
    async fn grouped(
        &mut self,
        points: Select,
        columns: Vec<Projection>,
        keys: usize,
    ) -> Step<Vec<(Vec<Value>, Vec<Value>)>> {
        let limit = (MAX_SECTIONS + 1) * (MAX_GROUPS + 1) * (MAX_GROUPS + 1);
        let mut select = Select::from(Source::subquery(points, POINTS)).columns(columns);
        select.group = group_exprs(POINTS, keys);
        select.order = group_order(POINTS, keys);
        select.limit = Some(limit as u64 + 1);
        let rows = self.r.run(select).await?;
        if rows.len() > limit {
            return Err(Halt::Refuse(
                "the columns have too many combinations of values to test; bin them, or filter the dataset"
                    .to_owned(),
            ));
        }
        Ok(rows
            .into_iter()
            .map(|row| {
                let mut values = row.into_values();
                let rest = values.split_off(keys);
                (values, rest)
            })
            .collect())
    }

    /// The values of `inputs` for each group of `keys` (the first `by` of
    /// them Wrap's), at most [`TEST_SAMPLE`] of each Wrap group when
    /// `sample`.
    async fn values(
        &mut self,
        keys: &[Key],
        by: usize,
        inputs: Vec<Expr>,
        filters: &[Expr],
        sample: bool,
    ) -> Step<Grouped<Vec<Vec<f64>>>> {
        let width = inputs.len();
        let points = self.r.points(keys, inputs, filters)?;
        let select = if sample {
            sample_by(points, keys.len() + width, by, TEST_SAMPLE)
        } else {
            points
        };
        let mut out: Grouped<Vec<Vec<f64>>> = Grouped::new();
        for row in self.r.run(select).await? {
            let values = row.into_values();
            let (k, rest) = values.split_at(keys.len());
            let numbers: Option<Vec<f64>> = rest.iter().map(f64_of).collect();
            if let Some(numbers) = numbers {
                out.entry(k).push(numbers);
            }
        }
        Ok(out)
    }

    /// The Wrap groups of `rows` (whose keys start with Wrap's), in order, as
    /// sections; refused when there are more than [`MAX_SECTIONS`].
    fn sections<'r>(
        &self,
        by_keys: &[Key],
        rows: impl IntoIterator<Item = &'r Vec<Value>>,
    ) -> Step<Vec<(Vec<Value>, Section)>> {
        let mut out: Vec<(Vec<Value>, Section)> = Vec::new();
        for k in rows {
            let b = &k[..by_keys.len()];
            if !out.iter().any(|(seen, _)| canon(seen) == canon(b)) {
                out.push((b.to_vec(), Section::new(&key_values(by_keys, b))));
            }
        }
        if out.len() > MAX_SECTIONS {
            let (f, _) = self
                .choice
                .by
                .as_ref()
                .map_or((&self.choice.y[0].0, ColType::Unknown), |(f, t)| (f, *t));
            return Err(Halt::Refuse(format!(
                "Wrap by `{}` makes {} groups, and the tests are repeated for at most {MAX_SECTIONS}; {}",
                f.field,
                out.len(),
                if f.bin.is_some() {
                    "make its bins wider"
                } else {
                    "bin it, or filter the dataset"
                }
            )));
        }
        if out.is_empty() {
            // Nothing to test: one section saying so.
            let mut empty = Section::new(&[]);
            empty.error = Some("no row has a value in every column the tests read".to_owned());
            out.push((vec![Value::Null; by_keys.len()], empty));
        }
        Ok(out)
    }

    fn level(&self, key: &Key, value: &Value, n: u64) -> Level {
        let mut json = key_values(std::slice::from_ref(key), std::slice::from_ref(value));
        let end = (json.len() > 1).then(|| json.remove(1));
        Level {
            value: json.into_iter().next().unwrap_or(Json::Null),
            end,
            n,
            mean: None,
            sd: None,
        }
    }

    fn group_size_check(of: Option<Json>, n: u64) -> Check {
        let mut c = Check::new("group_size", n >= SMALL_GROUP);
        c.of = of;
        c.n = Some(n);
        c
    }

    /// Shapiro–Wilk on `values` as an assumption check (none for fewer than
    /// three values, or when every value is the same).
    fn normality_check(of: Json, values: &[f64]) -> Option<Check> {
        let t = shapiro(&values[..values.len().min(TEST_SAMPLE as usize)]).ok()?;
        let n = values.len() as u64;
        let mut c = Check::new(
            "normality",
            t.p_value >= ASSUMPTION_LEVEL || n >= LARGE_GROUP,
        );
        c.of = Some(of);
        c.n = Some(n);
        c.p_value = Some(t.p_value);
        Some(c)
    }

    // --- the designs --------------------------------------------------------------

    async fn one_number(&mut self) -> Step<Vec<Section>> {
        let level = self.spec.level;
        let mu = self.spec.mu;
        let y = self.y(0).clone();
        let by = self.by_keys().await?;
        let filters = vec![self.present(&y)];
        let groups = self.moments(&by, vec![self.number(&y)], &filters).await?;
        let mut sections = self.sections(&by, groups.iter().map(|g| &g.0))?;
        let sample = groups.iter().any(|(_, m, _)| m.n > TEST_SAMPLE);
        let values = self
            .values(&by, by.len(), vec![self.number(&y)], &filters, sample)
            .await?;
        for (b, section) in &mut sections {
            let Some((_, m, _)) = groups.iter().find(|(k, _, _)| canon(k) == canon(b)) else {
                continue;
            };
            let xs: Vec<f64> = values
                .get(b)
                .map(|v| v.iter().map(|r| r[0]).collect())
                .unwrap_or_default();
            let sampled = m.n > xs.len() as u64;
            section.n = m.n;
            if sampled {
                section.sampled = Some(xs.len() as u64);
            }
            section.push(
                Design::OneNumber,
                TestKind::OneSampleT,
                one_sample_t(m, mu, level),
            );
            section.push(
                Design::OneNumber,
                TestKind::ShapiroWilk,
                mark(shapiro(&xs), sampled),
            );
            section.push(
                Design::OneNumber,
                TestKind::SignedRank,
                mark(signed_rank(&xs, mu, level), sampled),
            );
            section.checks.push(Self::group_size_check(None, m.n));
            if let Some(c) = Self::normality_check(Json::from("values"), &xs) {
                section.checks.push(c);
            }
            section.prefer(TestKind::OneSampleT, Some(TestKind::SignedRank));
        }
        Ok(sections.into_iter().map(|(_, s)| s).collect())
    }

    async fn one_category(&mut self) -> Step<Vec<Section>> {
        let level = self.spec.level;
        let y = self.y(0).clone();
        let mut keys = self.by_keys().await?;
        let nb = keys.len();
        keys.push(self.key_of(Channel::Y, &y).await?);
        let rows = self.counts(&keys, &[self.present(&y)]).await?;
        let mut sections = self.sections(&keys[..nb], rows.iter().map(|g| &g.0))?;
        for (b, section) in &mut sections {
            let mine: Vec<&(Vec<Value>, u64)> = rows
                .iter()
                .filter(|(k, _)| canon(&k[..nb]) == canon(b))
                .collect();
            section.levels = mine
                .iter()
                .map(|(k, n)| self.level(&keys[nb], &k[nb], *n))
                .collect();
            section.n = mine.iter().map(|(_, n)| n).sum();
            if mine.len() > MAX_GROUPS {
                section.error = Some(too_many(&y.field, mine.len()));
                continue;
            }
            let counts: Vec<u64> = mine.iter().map(|(_, n)| *n).collect();
            section.push(
                Design::OneCategory,
                TestKind::ChiSquareFit,
                chisq_fit(&counts),
            );
            if counts.len() == 2 {
                section.event = Some(1);
                section.push(
                    Design::OneCategory,
                    TestKind::Binomial,
                    binomial(counts[1], counts[0] + counts[1], 0.5, level),
                );
            }
            if !counts.is_empty() {
                let expected = section.n as f64 / counts.len() as f64;
                let mut c = Check::new("expected_counts", expected >= MIN_EXPECTED);
                c.value = Some(expected);
                section.checks.push(c);
            }
            // Two values: the binomial test is exact.
            let main = if counts.len() == 2 {
                TestKind::Binomial
            } else {
                TestKind::ChiSquareFit
            };
            section.prefer(main, None);
        }
        Ok(sections.into_iter().map(|(_, s)| s).collect())
    }

    async fn number_by_groups(&mut self) -> Step<Vec<Section>> {
        let level = self.spec.level;
        let (y, x) = (self.y(0).clone(), self.x().clone());
        let mut keys = self.by_keys().await?;
        let nb = keys.len();
        keys.push(self.key_of(Channel::X, &x).await?);
        let filters = vec![self.present(&y), self.present(&x)];
        let groups = self.moments(&keys, vec![self.number(&y)], &filters).await?;
        let mut sections = self.sections(&keys[..nb], groups.iter().map(|g| &g.0))?;
        // Totals of each Wrap group, to know whether to sample.
        let sample = sections.iter().any(|(b, _)| {
            groups
                .iter()
                .filter(|(k, _, _)| canon(&k[..nb]) == canon(b))
                .map(|(_, m, _)| m.n)
                .sum::<u64>()
                > TEST_SAMPLE
        });
        let values = self
            .values(&keys, nb, vec![self.number(&y)], &filters, sample)
            .await?;
        for (b, section) in &mut sections {
            let mine: Vec<&(Vec<Value>, Moments, Vec<f64>)> = groups
                .iter()
                .filter(|(k, _, _)| canon(&k[..nb]) == canon(b))
                .collect();
            section.levels = mine
                .iter()
                .map(|(k, m, _)| {
                    let mut l = self.level(&keys[nb], &k[nb], m.n);
                    l.mean = Some(m.mean);
                    l.sd = m.sd.is_finite().then_some(m.sd);
                    l
                })
                .collect();
            section.n = mine.iter().map(|(_, m, _)| m.n).sum();
            if mine.len() < 2 {
                let rows = if section.by.is_some() {
                    "the rows of this group"
                } else {
                    "the rows"
                };
                section.error = Some(if mine.is_empty() {
                    format!(
                        "none of {rows} has both a `{}` and a `{}`",
                        y.field, x.field
                    )
                } else {
                    format!(
                        "the tests compare groups of `{x}`, and {rows} that have a `{y}` all have the same `{x}`",
                        x = x.field,
                        y = y.field
                    )
                });
                continue;
            }
            if mine.len() > MAX_GROUPS {
                section.error = Some(too_many(&x.field, mine.len()));
                continue;
            }
            let moments: Vec<Moments> = mine.iter().map(|(_, m, _)| *m).collect();
            let samples: Vec<Vec<f64>> = mine
                .iter()
                .map(|(k, _, _)| {
                    values
                        .get(k)
                        .map(|v| v.iter().map(|r| r[0]).collect())
                        .unwrap_or_default()
                })
                .collect();
            let read: u64 = samples.iter().map(|s| s.len() as u64).sum();
            let sampled = read < section.n;
            if sampled {
                section.sampled = Some(read);
            }
            let design = Design::NumberByGroups;
            if moments.len() == 2 {
                section.push(
                    design,
                    TestKind::WelchT,
                    welch_t(&moments[0], &moments[1], level),
                );
                section.push(
                    design,
                    TestKind::MannWhitney,
                    mark(mann_whitney(&samples[0], &samples[1], level), sampled),
                );
            } else {
                section.push(design, TestKind::Anova, anova(&moments));
                section.push(
                    design,
                    TestKind::KruskalWallis,
                    mark(kruskal_wallis(&samples), sampled),
                );
                if moments.len() <= MAX_PAIRWISE_GROUPS
                    && let Ok(pairs) = tukey_hsd(&moments, level)
                {
                    section.comparisons = pairs
                        .into_iter()
                        .map(|p| Comparison {
                            a: p.a,
                            b: p.b,
                            difference: p.difference,
                            lower: p.lower,
                            upper: p.upper,
                            p_value: p.p_value,
                        })
                        .collect();
                }
            }
            for (l, s) in section.levels.clone().iter().zip(&samples) {
                section
                    .checks
                    .push(Self::group_size_check(Some(l.value.clone()), l.n));
                if let Some(c) = Self::normality_check(l.value.clone(), s) {
                    section.checks.push(c);
                }
            }
            if moments.len() > 2
                && let Ok(t) = levene(&samples)
            {
                let mut c = Check::new("equal_variances", t.p_value >= ASSUMPTION_LEVEL);
                c.p_value = Some(t.p_value);
                c.n = Some(read);
                section.checks.push(c);
            }
            if moments.len() == 2 {
                section.prefer(TestKind::WelchT, Some(TestKind::MannWhitney));
            } else {
                section.prefer(TestKind::Anova, Some(TestKind::KruskalWallis));
            }
        }
        Ok(sections.into_iter().map(|(_, s)| s).collect())
    }

    async fn two_categories(&mut self) -> Step<Vec<Section>> {
        let level = self.spec.level;
        let (y, x) = (self.y(0).clone(), self.x().clone());
        let mut keys = self.by_keys().await?;
        let nb = keys.len();
        keys.push(self.key_of(Channel::X, &x).await?);
        keys.push(self.key_of(Channel::Y, &y).await?);
        let rows = self
            .counts(&keys, &[self.present(&y), self.present(&x)])
            .await?;
        let mut sections = self.sections(&keys[..nb], rows.iter().map(|g| &g.0))?;
        for (b, section) in &mut sections {
            let mine: Vec<&(Vec<Value>, u64)> = rows
                .iter()
                .filter(|(k, _)| canon(&k[..nb]) == canon(b))
                .collect();
            // The table's rows (X) and columns (Y), each in order.
            let mut xs: Grouped<u64> = Grouped::new();
            let mut ys: Grouped<u64> = Grouped::new();
            for (k, n) in &mine {
                *xs.entry(&k[nb..=nb]) += n;
                *ys.entry(&k[nb + 1..]) += n;
            }
            let mut x_levels = xs.order;
            let mut y_levels = ys.order;
            let order = |a: &(Vec<Value>, u64), b: &(Vec<Value>, u64)| {
                crate::plot::render::compare_json(&value_json(&a.0[0]), &value_json(&b.0[0]))
            };
            x_levels.sort_by(order);
            y_levels.sort_by(order);
            section.levels = x_levels
                .iter()
                .map(|(k, n)| self.level(&keys[nb], &k[0], *n))
                .collect();
            section.categories = y_levels
                .iter()
                .map(|(k, n)| self.level(&keys[nb + 1], &k[0], *n))
                .collect();
            section.n = mine.iter().map(|(_, n)| n).sum();
            if x_levels.len() > MAX_GROUPS || y_levels.len() > MAX_GROUPS {
                let (f, k) = if x_levels.len() > MAX_GROUPS {
                    (&x.field, x_levels.len())
                } else {
                    (&y.field, y_levels.len())
                };
                section.error = Some(too_many(f, k));
                continue;
            }
            let mut table = vec![vec![0_u64; y_levels.len()]; x_levels.len()];
            for (k, n) in &mine {
                let i = x_levels
                    .iter()
                    .position(|(v, _)| canon(v) == canon(&k[nb..=nb]));
                let j = y_levels
                    .iter()
                    .position(|(v, _)| canon(v) == canon(&k[nb + 1..]));
                if let (Some(i), Some(j)) = (i, j) {
                    table[i][j] += n;
                }
            }
            section.push(
                Design::TwoCategories,
                TestKind::ChiSquareIndependence,
                chisq_independence(&table),
            );
            section.push(
                Design::TwoCategories,
                TestKind::FisherExact,
                fisher_exact(&table, level),
            );
            if x_levels.len() >= 2 && y_levels.len() >= 2 {
                let least = expected_counts(&table)
                    .into_iter()
                    .flatten()
                    .fold(f64::INFINITY, f64::min);
                let mut c = Check::new("expected_counts", least >= MIN_EXPECTED);
                c.value = Some(least);
                section.checks.push(c);
            }
            section.prefer(TestKind::ChiSquareIndependence, Some(TestKind::FisherExact));
        }
        Ok(sections.into_iter().map(|(_, s)| s).collect())
    }

    async fn two_numbers(&mut self) -> Step<Vec<Section>> {
        let level = self.spec.level;
        let (y, x) = (self.y(0).clone(), self.x().clone());
        let by = self.by_keys().await?;
        let filters = vec![self.present(&y), self.present(&x)];
        let points = self
            .r
            .points(&by, vec![self.number(&x), self.number(&y)], &filters)?;
        let groups: Vec<(Vec<Value>, LinearSums)> = self.r.linear_sums(points, by.len()).await?;
        let mut sections = self.sections(&by, groups.iter().map(|g| &g.0))?;
        let sample = groups.iter().any(|(_, s)| s.n > TEST_SAMPLE);
        let values = self
            .values(
                &by,
                by.len(),
                vec![self.number(&x), self.number(&y)],
                &filters,
                sample,
            )
            .await?;
        for (b, section) in &mut sections {
            let Some((_, sums)) = groups.iter().find(|(k, _)| canon(k) == canon(b)) else {
                continue;
            };
            let pairs: &[Vec<f64>] = values.get(b).map_or(&[], Vec::as_slice);
            let (xs, ys): (Vec<f64>, Vec<f64>) = pairs.iter().map(|r| (r[0], r[1])).unzip();
            let sampled = sums.n > xs.len() as u64;
            section.n = sums.n;
            if sampled {
                section.sampled = Some(xs.len() as u64);
            }
            let design = Design::TwoNumbers;
            section.push(design, TestKind::Pearson, pearson(sums, level));
            let fit = linear_regression(sums, level);
            let line = fit.as_ref().ok().and_then(|t| {
                let slope = t.estimate.as_ref()?.value;
                let intercept = t.details.iter().find(|d| d.name == "intercept")?.value;
                Some((intercept, slope))
            });
            section.push(design, TestKind::LinearRegression, fit);
            section.push(
                design,
                TestKind::Spearman,
                mark(spearman(&xs, &ys), sampled),
            );
            section.checks.push(Self::group_size_check(None, sums.n));
            if let Some((a, b)) = line {
                let residuals: Vec<f64> =
                    xs.iter().zip(&ys).map(|(x, y)| y - (a + b * x)).collect();
                if let Some(c) = Self::normality_check(Json::from("residuals"), &residuals) {
                    section.checks.push(c);
                }
            }
            section.prefer(TestKind::Pearson, Some(TestKind::Spearman));
        }
        Ok(sections.into_iter().map(|(_, s)| s).collect())
    }

    async fn category_by_number(&mut self) -> Step<Vec<Section>> {
        let level = self.spec.level;
        let (y, x) = (self.y(0).clone(), self.x().clone());
        let mut keys = self.by_keys().await?;
        let nb = keys.len();
        keys.push(self.key_of(Channel::Y, &y).await?);
        let filters = vec![self.present(&y), self.present(&x)];
        let rows = self.counts(&keys, &filters).await?;
        let mut sections = self.sections(&keys[..nb], rows.iter().map(|g| &g.0))?;
        let sample = sections.iter().any(|(b, _)| {
            rows.iter()
                .filter(|(k, _)| canon(&k[..nb]) == canon(b))
                .map(|(_, n)| n)
                .sum::<u64>()
                > TEST_SAMPLE
        });
        let values = self
            .values(&keys, nb, vec![self.number(&x)], &filters, sample)
            .await?;
        for (b, section) in &mut sections {
            let mine: Vec<&(Vec<Value>, u64)> = rows
                .iter()
                .filter(|(k, _)| canon(&k[..nb]) == canon(b))
                .collect();
            section.levels = mine
                .iter()
                .map(|(k, n)| self.level(&keys[nb], &k[nb], *n))
                .collect();
            section.n = mine.iter().map(|(_, n)| n).sum();
            if mine.len() != 2 {
                section.error = Some(if mine.len() < 2 {
                    format!(
                        "every row has the same `{}`, so there is nothing to explain",
                        y.field
                    )
                } else {
                    format!(
                        "a logistic regression explains a column with two values, and `{}` has {}; make a column of the outcome that matters in the dataset",
                        y.field,
                        mine.len()
                    )
                });
                continue;
            }
            section.event = Some(1);
            let mut xs = Vec::new();
            let mut outcome = Vec::new();
            for (i, (k, _)) in mine.iter().enumerate() {
                for r in values.get(k).map_or(&[][..], Vec::as_slice) {
                    xs.push(r[0]);
                    outcome.push(i == 1);
                }
            }
            let sampled = (xs.len() as u64) < section.n;
            if sampled {
                section.sampled = Some(xs.len() as u64);
            }
            section.push(
                Design::CategoryByNumber,
                TestKind::LogisticRegression,
                mark(logistic_regression(&xs, &outcome, level), sampled),
            );
            let rarer = mine.iter().map(|(_, n)| *n).min().unwrap_or(0);
            let mut c = Check::new("events", rarer >= MIN_EVENTS);
            c.value = Some(rarer as f64);
            section.checks.push(c);
            section.prefer(TestKind::LogisticRegression, None);
        }
        Ok(sections.into_iter().map(|(_, s)| s).collect())
    }

    async fn paired(&mut self) -> Step<Vec<Section>> {
        let level = self.spec.level;
        let (a, b) = (self.y(0).clone(), self.y(1).clone());
        let by = self.by_keys().await?;
        let filters = vec![self.present(&a), self.present(&b)];
        let difference = Expr::binary(BinOp::Sub, self.number(&a), self.number(&b));
        let groups = self
            .moments(
                &by,
                vec![difference.clone(), self.number(&a), self.number(&b)],
                &filters,
            )
            .await?;
        let mut sections = self.sections(&by, groups.iter().map(|g| &g.0))?;
        let sample = groups.iter().any(|(_, m, _)| m.n > TEST_SAMPLE);
        let values = self
            .values(&by, by.len(), vec![difference], &filters, sample)
            .await?;
        for (key, section) in &mut sections {
            let Some((_, m, means)) = groups.iter().find(|(k, _, _)| canon(k) == canon(key)) else {
                continue;
            };
            let ds: Vec<f64> = values
                .get(key)
                .map(|v| v.iter().map(|r| r[0]).collect())
                .unwrap_or_default();
            let sampled = m.n > ds.len() as u64;
            section.n = m.n;
            if sampled {
                section.sampled = Some(ds.len() as u64);
            }
            section.levels = [&a, &b]
                .iter()
                .zip(means)
                .map(|(f, mean)| Level {
                    value: Json::from(f.field.clone()),
                    end: None,
                    n: m.n,
                    mean: Some(*mean),
                    sd: None,
                })
                .collect();
            section.push(Design::Paired, TestKind::PairedT, paired_t(m, level));
            section.push(
                Design::Paired,
                TestKind::PairedSignedRank,
                mark(paired_signed_rank(&ds, level), sampled),
            );
            section.checks.push(Self::group_size_check(None, m.n));
            if let Some(c) = Self::normality_check(Json::from("differences"), &ds) {
                section.checks.push(c);
            }
            section.prefer(TestKind::PairedT, Some(TestKind::PairedSignedRank));
        }
        Ok(sections.into_iter().map(|(_, s)| s).collect())
    }
}

/// Mark a result as read from a sample.
fn mark(
    result: std::result::Result<TestResult, String>,
    sampled: bool,
) -> std::result::Result<TestResult, String> {
    result.map(|mut r| {
        r.sampled = sampled;
        r
    })
}

fn too_many(field: &str, k: usize) -> String {
    format!(
        "`{field}` has {k} values, and the tests compare at most {MAX_GROUPS}; bin it, or filter the dataset"
    )
}

/// At most `limit` of `points`' rows (its first `width` columns) for each
/// group of its first `by` columns: a random sample, the same each time —
/// the rows numbered in order of every column, the numbers scrambled from a
/// fixed seed, as the plots sample, and the first `limit` of each group kept.
fn sample_by(points: Select, width: usize, by: usize, limit: u64) -> Select {
    let names: Vec<String> = points
        .columns
        .iter()
        .take(width)
        .enumerate()
        .map(|(i, p)| match p {
            Projection::Expr { alias: Some(a), .. } => a.clone(),
            _ => format!("_c{i}"),
        })
        .collect();
    let every: Vec<OrderBy> = names
        .iter()
        .map(|n| OrderBy {
            expr: Expr::qcol(POINTS, n.clone()),
            dir: OrderDir::Asc,
            nulls: Some(Nulls::Last),
        })
        .collect();
    let mut numbered: Vec<Projection> = names
        .iter()
        .map(|n| Projection::expr_as(Expr::qcol(POINTS, n.clone()), n.clone()))
        .collect();
    numbered.push(Projection::expr_as(
        Expr::row_number(Vec::new(), every),
        "_rn",
    ));
    let numbered = Select::from(Source::subquery(points, POINTS)).columns(numbered);
    let mut ranked: Vec<Projection> = names
        .iter()
        .map(|n| Projection::expr_as(Expr::qcol(INNER, n.clone()), n.clone()))
        .collect();
    ranked.push(Projection::expr_as(
        Expr::row_number(
            group_exprs(INNER, by),
            vec![OrderBy::asc(scramble(Expr::qcol(INNER, "_rn"), SEED))],
        ),
        "_rk",
    ));
    let ranked = Select::from(Source::subquery(numbered, INNER)).columns(ranked);
    Select::from(Source::subquery(ranked, SAMPLED))
        .columns(
            names
                .iter()
                .map(|n| Projection::expr_as(Expr::qcol(SAMPLED, n.clone()), n.clone()))
                .collect(),
        )
        .filter(Expr::binary(
            BinOp::Le,
            Expr::qcol(SAMPLED, "_rk"),
            cast(Expr::lit(Value::Int(limit as i64)), "bigint"),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_whatever_type_a_number_came_back_as() {
        assert_eq!(canon(&[Value::Int(3)]), canon(&[Value::Float(3.0)]));
        assert_ne!(canon(&[Value::Int(3)]), canon(&[Value::Text("3".into())]));
        assert_ne!(canon(&[Value::Null]), canon(&[Value::Text("null".into())]));
    }

    #[test]
    fn a_section_prefers_the_alternative_when_a_check_fails() {
        let ok = |test| TestResult {
            test,
            statistic: None,
            df: Vec::new(),
            p_value: 0.5,
            estimate: None,
            effect: None,
            details: Vec::new(),
            n: 10,
            method: None,
            sampled: false,
        };
        let mut s = Section::new(&[]);
        s.push(
            Design::NumberByGroups,
            TestKind::WelchT,
            Ok(ok(TestKind::WelchT)),
        );
        s.push(
            Design::NumberByGroups,
            TestKind::MannWhitney,
            Ok(ok(TestKind::MannWhitney)),
        );
        assert_eq!(s.tests[0].role, "main");
        assert_eq!(s.tests[1].role, "alternative");
        s.checks.push(Check::new("group_size", true));
        s.prefer(TestKind::WelchT, Some(TestKind::MannWhitney));
        assert_eq!(s.preferred, Some(TestKind::WelchT));
        s.checks.push(Check::new("normality", false));
        s.prefer(TestKind::WelchT, Some(TestKind::MannWhitney));
        assert_eq!(s.preferred, Some(TestKind::MannWhitney));
        // The main test when the alternative could not be computed.
        s.tests[1].result = None;
        s.prefer(TestKind::WelchT, Some(TestKind::MannWhitney));
        assert_eq!(s.preferred, Some(TestKind::WelchT));
    }
}
