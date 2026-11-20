//! The Simulation workspace's server half (analytics TODO A7.4–A7.5): the
//! **profiler** and the **scenario comparison** over a fitted model.
//!
//! Both start from the model's **inputs**: the columns a prediction reads
//! from a row ([`sc_model::prediction_inputs`]), each described from the
//! training data — its kind (number, integer, date, category or yes/no), its
//! typical value (the mean, or the most common level) and its range or
//! levels. A request names values for some of them; the others are typical.
//!
//! - [`profile`] predicts at the given values and, for each input, along a
//!   grid of its range (or every level) with the others held fixed — the
//!   profile curves. Every row goes to the provider in **one batched
//!   prediction**, so a Stan posterior runs standalone generated quantities
//!   once per request rather than once per point.
//! - [`compare`] predicts each [`Scenario`] and says how the prediction is
//!   distributed: the draws (thinned) and their density where the model
//!   predicts from draws, the t (or normal) density of a regression's
//!   prediction interval, or a classifier's class probabilities. Densities
//!   share one grid, so scenarios are drawn on one axis.
//!
//! Scenarios are kept in a Simulation workspace's state
//! ([`SimulationState`]), which [`check_state`](crate::panel::check_state)
//! checks.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use sc_error::{Error, Result};
use sc_model::{
    Column, ColumnType, DatasetSource, Frame, Model, ModelInstance, ModelRegistry, Outcome,
    Prediction, PredictionInput, Read,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};
use statrs::distribution::{Continuous, ContinuousCDF, Normal, StudentsT};

/// The points of a numeric or date profile curve when none is asked for.
pub const DEFAULT_POINTS: usize = 21;
/// The most points a profile curve may have.
pub const MAX_POINTS: usize = 101;
/// The most levels a category input offers, the most common first; a curve
/// over a category has a point per level.
pub const MAX_LEVELS: usize = 50;
/// The most scenarios one comparison predicts.
pub const MAX_SCENARIOS: usize = 20;
/// The most draws a scenario's distribution carries back.
pub const MAX_DRAWS: usize = 1_000;
/// The points of the shared density grid.
pub const DENSITY_POINTS: usize = 128;
/// The most training rows read to describe the inputs.
pub const TRAINING_ROWS: u64 = 100_000;
/// The longest a scenario's name may be, in bytes.
pub const MAX_SCENARIO_NAME: usize = 200;

/// What kind of control an input is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// A real number: a slider over its range.
    Number,
    /// A whole number: a slider in steps of one.
    Integer,
    /// A date or timestamp: a date picker over its range.
    Date,
    /// A category: a drop-down of its levels.
    Category,
    /// Yes or no.
    Boolean,
}

/// One input of the model, described from the training data.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Input {
    /// The column.
    pub name: String,
    /// The control it is.
    pub kind: InputKind,
    /// Where it starts: the mean (rounded for an integer, the median for a
    /// date) or the most common level.
    pub typical: Json,
    /// The smallest value in the training data, for a number or a date.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<Json>,
    /// The largest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<Json>,
    /// A category's levels (the most common first when there are more than
    /// [`MAX_LEVELS`]; otherwise in order), or `[false, true]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<Json>,
    /// Whether the data has levels beyond [`levels`](Input::levels).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub more_levels: bool,
    /// The type the column is read as in a row being predicted.
    #[serde(skip)]
    column_type: ColumnType,
    /// For a date: whether every training value fell on midnight, so the
    /// grid steps in days and values are written as dates.
    #[serde(skip)]
    whole_days: bool,
}

/// A named set of input values (A7.5). Inputs it does not name are typical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    /// Its name, unique among the workspace's scenarios.
    pub name: String,
    /// The values it sets, by input.
    #[serde(default)]
    pub values: Map<String, Json>,
}

/// A Simulation workspace's state: the model, the current inputs and the
/// saved scenarios. The workspace owns it; the server checks its shape.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SimulationState {
    /// The model simulated, by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<uuid::Uuid>,
    /// A fixed fit; the model's active one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<uuid::Uuid>,
    /// The interval's level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<f64>,
    /// The current input values.
    #[serde(default)]
    pub values: Map<String, Json>,
    /// The saved scenarios, in the order they were saved.
    #[serde(default)]
    pub scenarios: Vec<Scenario>,
}

impl SimulationState {
    /// The state `json` holds, refused with a sentence when it is not one.
    pub fn from_json(json: &Json) -> Result<SimulationState> {
        let state: SimulationState = serde_json::from_value(json.clone())
            .map_err(|e| Error::invalid(format!("a simulation's state: {e}")))?;
        if let Some(level) = state.level {
            check_level(level)?;
        }
        check_scenarios(&state.scenarios)?;
        Ok(state)
    }
}

/// Refuse scenarios that are not a list of up to [`MAX_SCENARIOS`] with
/// distinct, non-empty names.
pub fn check_scenarios(scenarios: &[Scenario]) -> Result<()> {
    if scenarios.len() > MAX_SCENARIOS {
        return Err(Error::invalid(format!(
            "a simulation keeps at most {MAX_SCENARIOS} scenarios, and this has {}",
            scenarios.len()
        )));
    }
    let mut seen = BTreeSet::new();
    for (i, s) in scenarios.iter().enumerate() {
        let name = s.name.trim();
        if name.is_empty() {
            return Err(Error::invalid(format!("scenario {} has no name", i + 1)));
        }
        if name.len() > MAX_SCENARIO_NAME {
            return Err(Error::invalid(format!(
                "scenario {} has a name longer than {MAX_SCENARIO_NAME} bytes",
                i + 1
            )));
        }
        if !seen.insert(name) {
            return Err(Error::invalid(format!(
                "two scenarios are called “{name}”: give each its own name"
            )));
        }
    }
    Ok(())
}

fn check_level(level: f64) -> Result<()> {
    if level > 0.0 && level < 1.0 {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "an interval's level is a probability between 0 and 1, not {level}"
        )))
    }
}

/// What a profile is asked for.
#[derive(Debug, Clone, Default)]
pub struct ProfileRequest {
    /// The values of some inputs; the others are typical.
    pub values: Map<String, Json>,
    /// The interval's level; [`sc_model::DEFAULT_LEVEL`] when absent.
    pub level: Option<f64>,
    /// The points of each numeric or date curve; [`DEFAULT_POINTS`] when
    /// absent.
    pub points: Option<usize>,
}

/// One point of a profile curve: the input's value and the prediction there.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurvePoint {
    /// The input's value.
    pub x: Json,
    /// The prediction at it, as [`prediction_json`] writes it.
    pub prediction: Json,
}

/// How the prediction changes along one input, the others held fixed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Curve {
    /// The input.
    pub input: String,
    /// Its points, in order.
    pub points: Vec<CurvePoint>,
}

/// A profile: the inputs, the values used, the prediction at them and a
/// curve per input.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Profile {
    /// The model's outcome (`regression`, `classification`, `posterior`).
    pub outcome: String,
    /// What the provider says about how sure a prediction is.
    pub uncertainty: String,
    /// The interval's level.
    pub level: f64,
    /// The inputs.
    pub inputs: Vec<Input>,
    /// Every input's value: those asked for, and the typical ones.
    pub values: Map<String, Json>,
    /// The prediction at `values`.
    pub prediction: Json,
    /// A curve per input, in the inputs' order.
    pub curves: Vec<Curve>,
}

/// How a scenario's prediction is distributed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Spread {
    /// Draws: a posterior's, or a formula model's expected response.
    Draws {
        /// The draws, thinned to at most [`MAX_DRAWS`].
        draws: Vec<f64>,
        /// How many there were.
        total: usize,
        /// Their kernel density on the comparison's grid.
        density: Vec<f64>,
    },
    /// A regression's prediction interval as a Student t (or, with many
    /// degrees of freedom, normal) distribution.
    Interval {
        /// The t distribution's degrees of freedom, `None` for normal.
        df: Option<f64>,
        /// Its density on the comparison's grid.
        density: Vec<f64>,
    },
    /// A classifier's probability of each class.
    Classes {
        /// By class.
        probabilities: BTreeMap<String, f64>,
    },
    /// Only the prediction: the provider gives no uncertainty.
    Point,
}

/// One scenario's prediction.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScenarioResult {
    /// The scenario's name.
    pub name: String,
    /// Every input's value: the scenario's, and the typical ones.
    pub values: Map<String, Json>,
    /// The prediction, as [`prediction_json`] writes it.
    pub prediction: Json,
    /// How it is distributed.
    pub spread: Spread,
}

/// The scenarios compared.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    /// The model's outcome.
    pub outcome: String,
    /// What the provider says about how sure a prediction is.
    pub uncertainty: String,
    /// The interval's level.
    pub level: f64,
    /// The inputs, as [`Profile::inputs`].
    pub inputs: Vec<Input>,
    /// The grid every density is on; empty when none is.
    pub grid: Vec<f64>,
    /// Each scenario's prediction, in order.
    pub scenarios: Vec<ScenarioResult>,
}

/// What the profiler and the comparison need: the registry and the dataset
/// source predictions go through, and the row cap.
pub struct Predictor<'a> {
    /// The model providers.
    pub registry: &'a ModelRegistry,
    /// How a dataset is read.
    pub source: &'a dyn DatasetSource,
    /// The most rows a read may return.
    pub cap: u64,
    /// The model.
    pub model: &'a Model,
    /// The fit predicted with.
    pub instance: &'a ModelInstance,
}

impl Predictor<'_> {
    /// Refuse an outcome a simulation cannot draw; answer its wire name.
    fn outcome(&self) -> Result<String> {
        let outcome = self.instance.outcome()?;
        match outcome {
            Outcome::Regression { .. }
            | Outcome::Classification { .. }
            | Outcome::Posterior { .. } => Ok(outcome.name().to_owned()),
            other => Err(Error::invalid(format!(
                "`{}` predicts {}, and a simulation shows a number or a class as its inputs \
                 change",
                self.model.name,
                match other {
                    Outcome::Cluster => "a cluster",
                    Outcome::Embedding { .. } => "a vector",
                    _ => "nothing per row (it is a hypothesis test)",
                }
            ))),
        }
    }

    fn uncertainty(&self) -> String {
        self.registry
            .get(self.model.provider.trim())
            .map_or("none", |p| p.uncertainty().name())
            .to_owned()
    }

    /// The model's inputs, described from the training data.
    pub async fn inputs(&self) -> Result<Vec<Input>> {
        let dataset =
            sc_model::fitted_dataset(self.instance).unwrap_or_else(|| self.model.dataset.clone());
        let read = Read::all(self.cap).first(self.cap.min(TRAINING_ROWS));
        let training = self.source.read(&dataset, &read).await?;
        let names = match sc_model::prediction_inputs(
            self.registry,
            &self.model.provider,
            self.instance,
        )? {
            Some(names) => names,
            // Nothing names them: every column but the label.
            None => {
                let label = self.instance.outcome()?.label().map(str::to_owned);
                training
                    .columns
                    .iter()
                    .filter(|(n, _)| Some(n) != label.as_ref())
                    .map(|(n, _)| PredictionInput {
                        column: n.clone(),
                        categorical: false,
                    })
                    .collect()
            }
        };
        let encoding = if self.instance.encoding.is_null() {
            None
        } else {
            Some(self.instance.encoding()?)
        };
        names
            .iter()
            .map(|input| {
                let categories = encoding.as_ref().and_then(|e| {
                    e.columns.iter().find_map(|c| match c {
                        sc_model::ColumnEncoding::OneHot { column, categories }
                            if *column == input.column =>
                        {
                            Some(categories.as_slice())
                        }
                        _ => None,
                    })
                });
                describe_input(&self.model.name, &training, input, categories)
            })
            .collect()
    }

    /// Predict `rows` (one object per row, over `inputs`) at `level`.
    async fn predict(
        &self,
        inputs: &[Input],
        rows: &[Json],
        level: f64,
    ) -> Result<Vec<Prediction>> {
        let frame = frame_of(inputs, rows)?;
        sc_model::predict_rows_at(
            self.registry,
            &self.model.provider,
            self.instance,
            &frame,
            Some(level),
        )
        .await
        .map_err(|e| Error::invalid(format!("`{}`: {e}", self.model.name)))
    }
}

/// The profile `request` asks for (A7.4).
pub async fn profile(p: &Predictor<'_>, request: &ProfileRequest) -> Result<Profile> {
    let outcome = p.outcome()?;
    let level = request.level.unwrap_or(sc_model::DEFAULT_LEVEL);
    check_level(level)?;
    let points = request.points.unwrap_or(DEFAULT_POINTS);
    if !(2..=MAX_POINTS).contains(&points) {
        return Err(Error::invalid(format!(
            "a profile curve has between 2 and {MAX_POINTS} points, not {points}"
        )));
    }
    let inputs = p.inputs().await?;
    let values = resolve_values(&p.model.name, &inputs, &request.values)?;
    let (rows, spans) = profile_rows(&inputs, &values, points);
    let predictions = p.predict(&inputs, &rows, level).await?;
    let mut answers = predictions.iter().map(prediction_json);
    let prediction = answers
        .next()
        .transpose()?
        .ok_or_else(|| Error::msg("no prediction at the profile's values".to_owned()))?;
    let mut curves = Vec::with_capacity(inputs.len());
    for (input, xs) in inputs.iter().zip(spans) {
        let mut points = Vec::with_capacity(xs.len());
        for x in xs {
            let prediction = answers
                .next()
                .transpose()?
                .ok_or_else(|| Error::msg("fewer predictions than curve points".to_owned()))?;
            points.push(CurvePoint { x, prediction });
        }
        curves.push(Curve {
            input: input.name.clone(),
            points,
        });
    }
    Ok(Profile {
        outcome,
        uncertainty: p.uncertainty(),
        level,
        inputs,
        values,
        prediction,
        curves,
    })
}

/// Each of `scenarios` predicted, with its distribution (A7.5).
pub async fn compare(
    p: &Predictor<'_>,
    scenarios: &[Scenario],
    level: Option<f64>,
) -> Result<Comparison> {
    let outcome = p.outcome()?;
    let level = level.unwrap_or(sc_model::DEFAULT_LEVEL);
    check_level(level)?;
    if scenarios.is_empty() {
        return Err(Error::invalid("name at least one scenario to compare"));
    }
    check_scenarios(scenarios)?;
    let inputs = p.inputs().await?;
    let mut rows = Vec::with_capacity(scenarios.len());
    for s in scenarios {
        let values = resolve_values(&p.model.name, &inputs, &s.values)
            .map_err(|e| Error::invalid(format!("scenario “{}”: {e}", s.name.trim())))?;
        rows.push(Json::Object(values));
    }
    let frame = frame_of(&inputs, &rows)?;
    // Draws where the model predicts from them: one call answers both the
    // summary and the distribution.
    let draws =
        sc_model::predict_rows_draws(p.registry, &p.model.provider, p.instance, &frame).await?;
    let predictions: Vec<Prediction> = match &draws {
        Some(draws) => draws
            .iter()
            .zip(scenarios)
            .map(|(d, s)| {
                sc_model::distribution_of(d, level).ok_or_else(|| {
                    Error::invalid(format!(
                        "no draw of the prediction for scenario “{}” is a finite number",
                        s.name.trim()
                    ))
                })
            })
            .collect::<Result<_>>()?,
        None => p.predict(&inputs, &rows, level).await?,
    };
    let shapes: Vec<Shape> = predictions
        .iter()
        .enumerate()
        .map(|(i, prediction)| Shape::of(prediction, draws.as_ref().map(|d| &d[i][..])))
        .collect();
    let grid = shared_grid(&shapes);
    let mut out = Vec::with_capacity(scenarios.len());
    for (((s, row), prediction), shape) in scenarios.iter().zip(rows).zip(&predictions).zip(shapes)
    {
        let Json::Object(values) = row else {
            continue;
        };
        out.push(ScenarioResult {
            name: s.name.trim().to_owned(),
            values,
            prediction: prediction_json(prediction)?,
            spread: shape.spread(prediction, &grid),
        });
    }
    Ok(Comparison {
        outcome,
        uncertainty: p.uncertainty(),
        level,
        inputs,
        grid,
        scenarios: out,
    })
}

/// A prediction as the profiler writes it: `value`, and as the provider
/// gives them `sd`, `lower`, `upper` and `level`, or `probability` and
/// `probabilities`.
pub fn prediction_json(prediction: &Prediction) -> Result<Json> {
    let mut out = Map::new();
    out.insert("value".to_owned(), prediction.to_json()?);
    match prediction {
        Prediction::Distribution {
            sd,
            lower,
            upper,
            level,
            ..
        } => {
            out.insert("sd".to_owned(), json!(sd));
            out.insert("lower".to_owned(), json!(lower));
            out.insert("upper".to_owned(), json!(upper));
            out.insert("level".to_owned(), json!(level));
        }
        Prediction::Class {
            probability,
            probabilities,
            ..
        } => {
            if let Some(p) = probability {
                out.insert("probability".to_owned(), json!(p));
            }
            if !probabilities.is_empty() {
                out.insert("probabilities".to_owned(), json!(probabilities));
            }
        }
        _ => {}
    }
    Ok(Json::Object(out))
}

// --- inputs -------------------------------------------------------------------

/// `input` described from its column in `training`. `categories` are the
/// levels a one-hot encoding was fitted with.
fn describe_input(
    model: &str,
    training: &Frame,
    input: &PredictionInput,
    categories: Option<&[String]>,
) -> Result<Input> {
    let name = &input.column;
    let column = training.column(name).ok_or_else(|| {
        Error::invalid(format!(
            "`{model}` reads `{name}`, which its dataset no longer has: fit the model again"
        ))
    })?;
    let column_type = column.kind();
    let blank = |kind: InputKind, typical: Json| Input {
        name: name.clone(),
        kind,
        typical,
        min: None,
        max: None,
        levels: Vec::new(),
        more_levels: false,
        column_type,
        whole_days: false,
    };
    let no_values = || {
        Error::invalid(format!(
            "`{name}` has no values in `{model}`'s data, so it has no typical value to start from"
        ))
    };
    let categorical = input.categorical || categories.is_some() || column_type == ColumnType::Str;
    if categorical {
        let mut counts = level_counts(column);
        if let Some(fitted) = categories {
            // The fit's levels, in its order, read as the column is.
            let levels: Vec<Json> = fitted.iter().map(|c| typed_level(c, column_type)).collect();
            counts.retain(|(l, _)| levels.contains(l));
            let typical = counts
                .first()
                .map(|(l, _)| l.clone())
                .or_else(|| levels.first().cloned())
                .ok_or_else(no_values)?;
            let more = levels.len() > MAX_LEVELS;
            let mut input = blank(InputKind::Category, typical);
            input.levels = levels.into_iter().take(MAX_LEVELS).collect();
            input.more_levels = more;
            return Ok(input);
        }
        let typical = counts
            .first()
            .map(|(l, _)| l.clone())
            .ok_or_else(no_values)?;
        let more = counts.len() > MAX_LEVELS;
        let mut levels: Vec<Json> = counts.into_iter().map(|(l, _)| l).collect();
        if more {
            levels.truncate(MAX_LEVELS);
        } else {
            levels.sort_by(compare_json);
        }
        let mut input = blank(InputKind::Category, typical);
        input.levels = levels;
        input.more_levels = more;
        return Ok(input);
    }
    match column {
        Column::Bool(values) => {
            let yes = values.iter().flatten().filter(|b| **b).count();
            let no = values.iter().flatten().count() - yes;
            if yes + no == 0 {
                return Err(no_values());
            }
            let mut input = blank(InputKind::Boolean, Json::Bool(yes > no));
            input.levels = vec![Json::Bool(false), Json::Bool(true)];
            Ok(input)
        }
        Column::Date(values) => {
            let mut seconds: Vec<i64> = values.iter().flatten().copied().collect();
            if seconds.is_empty() {
                return Err(no_values());
            }
            seconds.sort_unstable();
            let whole_days = seconds.iter().all(|s| s.rem_euclid(86_400) == 0);
            let median = seconds[(seconds.len() - 1) / 2];
            let at = |s: i64| date_json(s, whole_days);
            let mut input = blank(InputKind::Date, at(median));
            input.min = Some(at(seconds[0]));
            input.max = Some(at(seconds[seconds.len() - 1]));
            input.whole_days = whole_days;
            Ok(input)
        }
        Column::Int(values) => {
            let xs: Vec<i64> = values.iter().flatten().copied().collect();
            let (min, max) = match (xs.iter().min(), xs.iter().max()) {
                (Some(a), Some(b)) => (*a, *b),
                _ => return Err(no_values()),
            };
            #[allow(clippy::cast_precision_loss, reason = "a mean of integers")]
            let mean = xs.iter().map(|x| *x as f64).sum::<f64>() / xs.len() as f64;
            let mut input = blank(InputKind::Integer, json!(round_i64(mean)));
            input.min = Some(json!(min));
            input.max = Some(json!(max));
            Ok(input)
        }
        Column::Float(values) => {
            let xs: Vec<f64> = values
                .iter()
                .flatten()
                .copied()
                .filter(|x| x.is_finite())
                .collect();
            if xs.is_empty() {
                return Err(no_values());
            }
            let min = xs.iter().copied().fold(f64::INFINITY, f64::min);
            let max = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            #[allow(clippy::cast_precision_loss, reason = "a row count")]
            let mean = xs.iter().sum::<f64>() / xs.len() as f64;
            let mut input = blank(InputKind::Number, json!(mean));
            input.min = Some(json!(min));
            input.max = Some(json!(max));
            Ok(input)
        }
        Column::Str(_) | Column::Null(_) => Err(no_values()),
    }
}

/// Each distinct value of `column` with how often it occurs, the most
/// common first (ties in the values' order).
fn level_counts(column: &Column) -> Vec<(Json, usize)> {
    let values = column_json(column);
    let mut counts: BTreeMap<String, (Json, usize)> = BTreeMap::new();
    for v in values.into_iter().filter(|v| !v.is_null()) {
        counts
            .entry(v.to_string())
            .and_modify(|(_, n)| *n += 1)
            .or_insert((v, 1));
    }
    let mut out: Vec<(Json, usize)> = counts.into_values().collect();
    out.sort_by(|(a, n), (b, m)| m.cmp(n).then_with(|| compare_json(a, b)));
    out
}

fn column_json(column: &Column) -> Vec<Json> {
    match column {
        Column::Float(v) => v
            .iter()
            .map(|x| x.map_or(Json::Null, |x| json!(x)))
            .collect(),
        Column::Int(v) | Column::Date(v) => v
            .iter()
            .map(|x| x.map_or(Json::Null, |x| json!(x)))
            .collect(),
        Column::Bool(v) => v.iter().map(|x| x.map_or(Json::Null, Json::Bool)).collect(),
        Column::Str(v) => v
            .iter()
            .map(|x| x.clone().map_or(Json::Null, Json::String))
            .collect(),
        Column::Null(n) => vec![Json::Null; *n],
    }
}

/// A one-hot level, as text, read as a value of a column of `ty`.
fn typed_level(level: &str, ty: ColumnType) -> Json {
    match ty {
        ColumnType::Int => level
            .parse::<i64>()
            .map_or_else(|_| json!(level), |n| json!(n)),
        ColumnType::Float => level
            .parse::<f64>()
            .map_or_else(|_| json!(level), |n| json!(n)),
        ColumnType::Bool => level
            .parse::<bool>()
            .map_or_else(|_| json!(level), Json::Bool),
        _ => json!(level),
    }
}

fn compare_json(a: &Json, b: &Json) -> std::cmp::Ordering {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        _ => a.to_string().cmp(&b.to_string()),
    }
}

fn round_i64(x: f64) -> i64 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a value inside an integer column's range"
    )]
    let n = x.round() as i64;
    n
}

fn date_json(seconds: i64, whole_days: bool) -> Json {
    let Some(t) = DateTime::<Utc>::from_timestamp(seconds, 0) else {
        return json!(seconds);
    };
    if whole_days {
        json!(t.format("%Y-%m-%d").to_string())
    } else {
        json!(t.to_rfc3339())
    }
}

/// Every input's value: `given`'s, checked against the input's kind, and
/// the typical value for the rest. An input `given` names that the model
/// does not read is refused by name.
fn resolve_values(
    model: &str,
    inputs: &[Input],
    given: &Map<String, Json>,
) -> Result<Map<String, Json>> {
    for name in given.keys() {
        if !inputs.iter().any(|i| i.name == *name) {
            return Err(Error::invalid(format!(
                "`{name}` is not an input of `{model}`: its inputs are {}",
                inputs
                    .iter()
                    .map(|i| format!("`{}`", i.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut out = Map::new();
    for input in inputs {
        let value = match given.get(&input.name) {
            None | Some(Json::Null) => input.typical.clone(),
            Some(v) => check_value(input, v)?,
        };
        out.insert(input.name.clone(), value);
    }
    Ok(out)
}

fn check_value(input: &Input, value: &Json) -> Result<Json> {
    let refuse = |what: &str| {
        Error::invalid(format!(
            "`{}` is {what}, and {value} is not one",
            input.name
        ))
    };
    match input.kind {
        InputKind::Number => value
            .as_f64()
            .filter(|x| x.is_finite())
            .map(|x| json!(x))
            .ok_or_else(|| refuse("a number")),
        InputKind::Integer => match value.as_f64() {
            Some(x) if x.is_finite() && x.fract() == 0.0 => Ok(json!(round_i64(x))),
            _ => Err(refuse("a whole number")),
        },
        InputKind::Boolean => value
            .as_bool()
            .map(Json::Bool)
            .ok_or_else(|| refuse("yes or no")),
        InputKind::Date => match value {
            Json::String(_) | Json::Number(_) => Ok(value.clone()),
            _ => Err(refuse("a date")),
        },
        InputKind::Category => match value {
            Json::String(_) | Json::Number(_) | Json::Bool(_) => Ok(value.clone()),
            _ => Err(refuse("a category")),
        },
    }
}

/// The rows a profile predicts — the values first, then each input's curve
/// with the others at their values — and each curve's x values.
fn profile_rows(
    inputs: &[Input],
    values: &Map<String, Json>,
    points: usize,
) -> (Vec<Json>, Vec<Vec<Json>>) {
    let mut rows = vec![Json::Object(values.clone())];
    let mut spans = Vec::with_capacity(inputs.len());
    for input in inputs {
        let xs = span(input, points);
        for x in &xs {
            let mut row = values.clone();
            row.insert(input.name.clone(), x.clone());
            rows.push(Json::Object(row));
        }
        spans.push(xs);
    }
    (rows, spans)
}

/// The values along `input`'s curve: `points` across its range, or each
/// level.
fn span(input: &Input, points: usize) -> Vec<Json> {
    match input.kind {
        InputKind::Category | InputKind::Boolean => input.levels.clone(),
        InputKind::Number => {
            let (Some(lo), Some(hi)) = (num(&input.min), num(&input.max)) else {
                return vec![input.typical.clone()];
            };
            linspace(lo, hi, points)
                .into_iter()
                .map(|x| json!(x))
                .collect()
        }
        InputKind::Integer => {
            let (Some(lo), Some(hi)) = (num(&input.min), num(&input.max)) else {
                return vec![input.typical.clone()];
            };
            let mut xs: Vec<i64> = linspace(lo, hi, points)
                .into_iter()
                .map(round_i64)
                .collect();
            xs.dedup();
            xs.into_iter().map(|x| json!(x)).collect()
        }
        InputKind::Date => {
            let seconds = |v: &Option<Json>| {
                v.as_ref()
                    .and_then(Json::as_str)
                    .and_then(|s| {
                        s.parse::<DateTime<Utc>>().ok().or_else(|| {
                            s.parse::<chrono::NaiveDate>()
                                .ok()
                                .and_then(|d| d.and_hms_opt(0, 0, 0))
                                .map(|t| t.and_utc())
                        })
                    })
                    .map(|t| t.timestamp())
            };
            let (Some(lo), Some(hi)) = (seconds(&input.min), seconds(&input.max)) else {
                return vec![input.typical.clone()];
            };
            #[allow(clippy::cast_precision_loss, reason = "seconds since 1970")]
            let grid = linspace(lo as f64, hi as f64, points);
            let mut xs: Vec<i64> = grid
                .into_iter()
                .map(|s| {
                    let s = round_i64(s);
                    if input.whole_days {
                        s.div_euclid(86_400) * 86_400
                    } else {
                        s
                    }
                })
                .collect();
            xs.dedup();
            xs.into_iter()
                .map(|s| date_json(s, input.whole_days))
                .collect()
        }
    }
}

fn num(v: &Option<Json>) -> Option<f64> {
    v.as_ref().and_then(Json::as_f64)
}

fn linspace(lo: f64, hi: f64, n: usize) -> Vec<f64> {
    if n < 2 || hi <= lo {
        return vec![lo];
    }
    #[allow(clippy::cast_precision_loss, reason = "a handful of points")]
    let step = (hi - lo) / (n - 1) as f64;
    #[allow(clippy::cast_precision_loss, reason = "a handful of points")]
    (0..n).map(|i| lo + i as f64 * step).collect()
}

/// A frame of `rows` read with each input's training type.
fn frame_of(inputs: &[Input], rows: &[Json]) -> Result<Frame> {
    let types: Vec<(String, ColumnType)> = inputs
        .iter()
        .map(|i| (i.name.clone(), i.column_type))
        .collect();
    Frame::from_rows(rows, &types)
}

// --- distributions --------------------------------------------------------------

/// What a scenario's prediction is distributed as, before the grid is known.
enum Shape {
    Draws(Vec<f64>),
    Interval {
        mean: f64,
        scale: f64,
        df: Option<f64>,
    },
    Classes,
    Point,
}

impl Shape {
    fn of(prediction: &Prediction, draws: Option<&[f64]>) -> Shape {
        if let Some(d) = draws {
            return Shape::Draws(d.iter().copied().filter(|x| x.is_finite()).collect());
        }
        match prediction {
            Prediction::Distribution {
                mean,
                sd,
                upper,
                level,
                ..
            } if *sd > 0.0 => Shape::Interval {
                mean: *mean,
                scale: *sd,
                df: t_degrees_of_freedom((upper - mean) / sd, *level),
            },
            Prediction::Class { probabilities, .. } if !probabilities.is_empty() => Shape::Classes,
            _ => Shape::Point,
        }
    }

    /// The range its density is worth drawing over.
    fn support(&self) -> Option<(f64, f64)> {
        match self {
            Shape::Draws(d) if !d.is_empty() => {
                let lo = d.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = d.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let bw = draws_bandwidth(d);
                Some((lo - 3.0 * bw, hi + 3.0 * bw))
            }
            Shape::Interval { mean, scale, df } => {
                let reach = if df.is_some_and(|df| df < 10.0) {
                    6.0
                } else {
                    4.0
                };
                Some((mean - reach * scale, mean + reach * scale))
            }
            _ => None,
        }
    }

    fn spread(self, prediction: &Prediction, grid: &[f64]) -> Spread {
        match self {
            Shape::Draws(d) => {
                let bw = draws_bandwidth(&d);
                let weighted: Vec<(f64, f64)> = d.iter().map(|x| (*x, 1.0)).collect();
                let density = crate::plot::kde(&weighted, bw, grid);
                let total = d.len();
                let stride = total.div_ceil(MAX_DRAWS).max(1);
                Spread::Draws {
                    draws: d.into_iter().step_by(stride).collect(),
                    total,
                    density,
                }
            }
            Shape::Interval { mean, scale, df } => {
                let density = grid
                    .iter()
                    .map(|x| {
                        let z = (x - mean) / scale;
                        let pdf = match df.and_then(|df| StudentsT::new(0.0, 1.0, df).ok()) {
                            Some(t) => t.pdf(z),
                            None => Normal::standard().pdf(z),
                        };
                        pdf / scale
                    })
                    .collect();
                Spread::Interval { df, density }
            }
            Shape::Classes => match prediction {
                Prediction::Class { probabilities, .. } => Spread::Classes {
                    probabilities: probabilities.clone(),
                },
                _ => Spread::Point,
            },
            Shape::Point => Spread::Point,
        }
    }
}

fn draws_bandwidth(d: &[f64]) -> f64 {
    if d.is_empty() {
        return 1.0;
    }
    #[allow(clippy::cast_precision_loss, reason = "a draw count")]
    let n = d.len() as f64;
    let mean = d.iter().sum::<f64>() / n;
    let sd = if d.len() > 1 {
        (d.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
    } else {
        0.0
    };
    let mut sorted = d.to_vec();
    sorted.sort_by(f64::total_cmp);
    let iqr = sc_model::quantile(&sorted, 0.75) - sc_model::quantile(&sorted, 0.25);
    crate::plot::bandwidth_nrd0(d.len() as u64, sd, iqr, mean)
}

/// One grid across every scenario's support, so their densities share an
/// axis; empty when no scenario has a density.
fn shared_grid(shapes: &[Shape]) -> Vec<f64> {
    let supports: Vec<(f64, f64)> = shapes.iter().filter_map(Shape::support).collect();
    if supports.is_empty() {
        return Vec::new();
    }
    let lo = supports.iter().map(|s| s.0).fold(f64::INFINITY, f64::min);
    let hi = supports
        .iter()
        .map(|s| s.1)
        .fold(f64::NEG_INFINITY, f64::max);
    if !(lo.is_finite() && hi.is_finite()) || hi <= lo {
        return Vec::new();
    }
    linspace(lo, hi, DENSITY_POINTS)
}

/// The degrees of freedom of the Student t whose central `level` interval
/// reaches `t` standard units: what a regression's prediction interval was
/// computed with. `None` when the interval is no wider than a normal one
/// (many degrees of freedom), so the normal is the distribution.
pub fn t_degrees_of_freedom(t: f64, level: f64) -> Option<f64> {
    if !(t.is_finite() && t > 0.0 && level > 0.0 && level < 1.0) {
        return None;
    }
    let q = 0.5 + level / 2.0;
    let z = Normal::standard().inverse_cdf(q);
    if t <= z * (1.0 + 1e-9) {
        return None;
    }
    let quantile = |df: f64| {
        StudentsT::new(0.0, 1.0, df)
            .map(|d| d.inverse_cdf(q))
            .unwrap_or(f64::NAN)
    };
    // The quantile falls as the degrees of freedom grow: bisect on their log.
    let (mut lo, mut hi) = (-1.0_f64, 7.0_f64);
    if quantile(10f64.powf(lo)) < t {
        return Some(10f64.powf(lo));
    }
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        if quantile(10f64.powf(mid)) > t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let df = 10f64.powf((lo + hi) / 2.0);
    (df < 1e6).then_some(df)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(columns: Vec<(&str, Column)>) -> Frame {
        Frame::new(
            columns
                .into_iter()
                .map(|(n, c)| (n.to_owned(), c))
                .collect(),
            Vec::new(),
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    fn input(name: &str, categorical: bool) -> PredictionInput {
        PredictionInput {
            column: name.to_owned(),
            categorical,
        }
    }

    fn describe(training: &Frame, name: &str, categorical: bool) -> Input {
        describe_input("m", training, &input(name, categorical), None)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Each kind of input starts at its typical value: the mean, the rounded
    /// mean, the median date, the most common level.
    #[test]
    fn inputs_start_at_typical_values() {
        let training = frame(vec![
            (
                "area",
                Column::Float(vec![Some(50.0), Some(70.0), None, Some(120.0)]),
            ),
            (
                "rooms",
                Column::Int(vec![Some(2), Some(3), Some(3), Some(5)]),
            ),
            (
                "area_name",
                Column::Str(vec![
                    Some("north".into()),
                    Some("south".into()),
                    Some("north".into()),
                    None,
                ]),
            ),
            (
                "garden",
                Column::Bool(vec![Some(true), Some(false), Some(true), Some(true)]),
            ),
            (
                "listed",
                Column::Date(vec![
                    Some(0),
                    Some(86_400),
                    Some(2 * 86_400),
                    Some(10 * 86_400),
                ]),
            ),
            (
                "county",
                Column::Int(vec![Some(7), Some(9), Some(9), Some(9)]),
            ),
        ]);
        let area = describe(&training, "area", false);
        assert_eq!(area.kind, InputKind::Number);
        assert_eq!(area.typical, json!(80.0));
        assert_eq!(
            (area.min, area.max),
            (Some(json!(50.0)), Some(json!(120.0)))
        );
        let rooms = describe(&training, "rooms", false);
        assert_eq!((rooms.kind, rooms.typical), (InputKind::Integer, json!(3)));
        let name = describe(&training, "area_name", false);
        assert_eq!(name.kind, InputKind::Category);
        assert_eq!(name.typical, json!("north"));
        assert_eq!(name.levels, vec![json!("north"), json!("south")]);
        let garden = describe(&training, "garden", false);
        assert_eq!(
            (garden.kind, garden.typical),
            (InputKind::Boolean, json!(true))
        );
        let listed = describe(&training, "listed", false);
        assert_eq!(listed.kind, InputKind::Date);
        assert_eq!(listed.typical, json!("1970-01-02"));
        assert_eq!(listed.max, Some(json!("1970-01-11")));
        // An integer an index binding reads is a category of its values.
        let county = describe(&training, "county", true);
        assert_eq!(county.kind, InputKind::Category);
        assert_eq!(county.typical, json!(9));
        assert_eq!(county.levels, vec![json!(7), json!(9)]);
        // A one-hot encoding's levels are the fit's, read as the column is.
        let fitted = ["7".to_owned(), "8".to_owned(), "9".to_owned()];
        let county = describe_input("m", &training, &input("county", true), Some(&fitted))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(county.levels, vec![json!(7), json!(8), json!(9)]);
        // A column the dataset lost is refused by name.
        let err =
            describe_input("m", &training, &input("gone", false), None).expect_err("no column");
        assert!(err.to_string().contains("`gone`"), "{err}");
    }

    /// A profile's rows: the values, then each input's curve with the others
    /// fixed — integers stepped by whole numbers, categories by level.
    #[test]
    fn a_profile_varies_one_input_at_a_time() {
        let training = frame(vec![
            ("x", Column::Float(vec![Some(0.0), Some(10.0)])),
            ("n", Column::Int(vec![Some(1), Some(3)])),
            ("g", Column::Str(vec![Some("a".into()), Some("b".into())])),
        ]);
        let inputs: Vec<Input> = ["x", "n", "g"]
            .iter()
            .map(|n| describe(&training, n, false))
            .collect();
        let given: Map<String, Json> = [("g".to_owned(), json!("b"))].into_iter().collect();
        let values = resolve_values("m", &inputs, &given).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            Json::Object(values.clone()),
            json!({ "x": 5.0, "n": 2, "g": "b" })
        );
        let (rows, spans) = profile_rows(&inputs, &values, 5);
        assert_eq!(
            spans[0],
            vec![json!(0.0), json!(2.5), json!(5.0), json!(7.5), json!(10.0)]
        );
        assert_eq!(spans[1], vec![json!(1), json!(2), json!(3)]);
        assert_eq!(spans[2], vec![json!("a"), json!("b")]);
        assert_eq!(rows.len(), 1 + 5 + 3 + 2);
        assert_eq!(rows[2], json!({ "x": 2.5, "n": 2, "g": "b" }));
        assert_eq!(rows[6], json!({ "x": 5.0, "n": 1, "g": "b" }));
        // A frame of them reads each input as its training column.
        let f = frame_of(&inputs, &rows).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(f.column("n").map(Column::kind), Some(ColumnType::Int));
        // An unknown input, and a value of the wrong kind, are refused.
        let bad: Map<String, Json> = [("y".to_owned(), json!(1))].into_iter().collect();
        let err = resolve_values("m", &inputs, &bad).expect_err("unknown");
        assert!(
            err.to_string().contains("`y` is not an input of `m`"),
            "{err}"
        );
        let bad: Map<String, Json> = [("n".to_owned(), json!(2.5))].into_iter().collect();
        let err = resolve_values("m", &inputs, &bad).expect_err("not whole");
        assert!(err.to_string().contains("whole number"), "{err}");
    }

    /// The degrees of freedom are recovered from an interval computed with
    /// them, and a normal interval has none.
    #[test]
    fn an_intervals_degrees_of_freedom_are_recovered() {
        for df in [3.0, 7.0, 25.0] {
            let t = StudentsT::new(0.0, 1.0, df)
                .unwrap_or_else(|e| panic!("{e}"))
                .inverse_cdf(0.95);
            let got = t_degrees_of_freedom(t, 0.9).unwrap_or_else(|| panic!("df {df}"));
            assert!((got - df).abs() / df < 1e-6, "{got} for {df}");
        }
        assert_eq!(t_degrees_of_freedom(1.6448536269514722, 0.9), None);
    }

    /// Scenario names are required and distinct, and the state is read back.
    #[test]
    fn scenarios_are_named_once() {
        let state = SimulationState::from_json(&json!({
            "level": 0.8,
            "values": { "area": 90 },
            "scenarios": [
                { "name": "baseline", "values": {} },
                { "name": "north side", "values": { "neighbourhood": "north" } },
            ],
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(state.scenarios.len(), 2);
        let err = SimulationState::from_json(&json!({
            "scenarios": [{ "name": "a" }, { "name": " a " }],
        }))
        .expect_err("twice");
        assert!(
            err.to_string().contains("two scenarios are called “a”"),
            "{err}"
        );
        let err = SimulationState::from_json(&json!({ "scenarios": [{ "name": "" }] }))
            .expect_err("unnamed");
        assert!(err.to_string().contains("scenario 1 has no name"), "{err}");
        let err = SimulationState::from_json(&json!({ "level": 2 })).expect_err("level");
        assert!(err.to_string().contains("between 0 and 1"), "{err}");
    }

    /// Densities share one grid, and each integrates to about one; a
    /// classifier's spread is its probabilities.
    #[test]
    fn spreads_share_a_grid() {
        let t = StudentsT::new(0.0, 1.0, 5.0)
            .unwrap_or_else(|e| panic!("{e}"))
            .inverse_cdf(0.95);
        let interval = Prediction::distribution(10.0, 2.0, 10.0 - 2.0 * t, 10.0 + 2.0 * t, 0.9);
        let draws: Vec<f64> = (0..400).map(|i| 20.0 + f64::from(i % 20) / 10.0).collect();
        let drawn = sc_model::distribution_of(&draws, 0.9).unwrap_or_else(|| panic!("draws"));
        let shapes = vec![Shape::of(&interval, None), Shape::of(&drawn, Some(&draws))];
        let grid = shared_grid(&shapes);
        assert_eq!(grid.len(), DENSITY_POINTS);
        assert!(grid[0] < 0.0 && grid[DENSITY_POINTS - 1] > 22.0, "{grid:?}");
        let step = grid[1] - grid[0];
        let mut it = shapes.into_iter();
        let Some(first) = it.next() else {
            panic!("shapes")
        };
        let Spread::Interval { df, density } = first.spread(&interval, &grid) else {
            panic!("an interval");
        };
        assert!(df.is_some_and(|df| (df - 5.0).abs() < 1e-6), "{df:?}");
        let area: f64 = density.iter().sum::<f64>() * step;
        assert!((area - 1.0).abs() < 0.02, "{area}");
        let Some(second) = it.next() else {
            panic!("shapes")
        };
        let Spread::Draws {
            draws: kept,
            total,
            density,
        } = second.spread(&drawn, &grid)
        else {
            panic!("draws");
        };
        assert_eq!((kept.len(), total), (400, 400));
        let area: f64 = density.iter().sum::<f64>() * step;
        assert!((area - 1.0).abs() < 0.02, "{area}");
        let class = Prediction::Class {
            class: "yes".into(),
            probability: Some(0.7),
            probabilities: [("no".to_owned(), 0.3), ("yes".to_owned(), 0.7)]
                .into_iter()
                .collect(),
        };
        let Spread::Classes { probabilities } = Shape::of(&class, None).spread(&class, &[]) else {
            panic!("classes");
        };
        assert_eq!(probabilities["yes"], 0.7);
    }
}
