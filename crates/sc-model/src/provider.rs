//! What a model provider *is*, and what a fit of one produces (TODO §7, §10).
//!
//! A provider is code that can fit something: `linear_regression`, `kmeans`, a
//! module's `sklearn`. It is an extension point of exactly the shape an
//! `Action` and an `AgentTrait` already are — a trait object, registered by
//! name, declaring its settings as [`FormField`]s so the admin UI renders a
//! provider it has never heard of — and the three things worth reading this
//! module for are the three places it deliberately differs.
//!
//! ## Metrics are the host's; parameters are the provider's
//!
//! A provider returns a [`FitResult`] — its serialised state and its
//! parameters — and **no metrics**. `sc-model` computes those itself, by running
//! the fitted state back over each split and scoring the predictions (Phase 3).
//! Two reasons: it makes providers *comparable*, so the smartcore regression and
//! the scikit-learn one are scored by the same code on the same rows and the
//! number on the screen means one thing; and a provider written in another
//! language does not have to reimplement R² to be a citizen here.
//!
//! What a provider does own is its **parameters**, which is where providers
//! genuinely differ — and they are structured for display ([`ParameterBlock`])
//! rather than free JSON, so a coefficient table renders as a table without the
//! admin UI knowing what a coefficient is.
//!
//! ## The outcome is a function of the configuration
//!
//! Not a constant: a random forest is a regressor or a classifier depending on
//! the type of the column its configuration names as the label. So
//! [`ModelProvider::outcome`] takes the dataset's shape and the configuration
//! and answers an [`Outcome`], which is what the UI renders against, what the
//! metric set is chosen by, and what `predict()` checks before it writes a
//! number into a text column. The alternative is four providers where there is
//! one algorithm.
//!
//! ## Prediction takes a frame, not a row
//!
//! A single row is a frame of one. Batching is what makes a Python provider
//! usable at all — the call is the cost, not the arithmetic — and it is what
//! lets the metric pass score 50 000 rows in one call rather than in 50 000.

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::dataset::DatasetShape;
use crate::frame::{ColumnType, Frame};
use crate::interface::Interface;
use crate::posterior::{DrawPlan, FitContext, PosteriorInput, PosteriorResult};

/// The [`OptionsSource::ServerQuery`] name meaning "every column of the
/// dataset".
///
/// A provider naming a label needs to offer *these* columns as its options
/// (§10), and it cannot know them when it is written — so it declares the field
/// with this query and [`resolve_column_options`] fills the list in once a
/// dataset exists. The existing options vocabulary rather than a new one:
/// `OptionsSource::ServerQuery` is documented as "a named server-side source,
/// resolved to `Static` before the spec is used", which is exactly this.
pub const COLUMNS_QUERY: &str = "dataset_columns";
/// The query name meaning "the dataset's numeric columns" — what a regression's
/// label picker offers.
pub const NUMERIC_COLUMNS_QUERY: &str = "dataset_numeric_columns";
/// The query name meaning "the dataset's categorical columns" (text and
/// boolean) — what a classification's label picker offers.
pub const CATEGORICAL_COLUMNS_QUERY: &str = "dataset_categorical_columns";

/// Resolve every column-options query in `spec` against `shape`, leaving other
/// fields alone.
///
/// This is the default body of [`ModelProvider::config_spec`], and it is what
/// lets a provider declared in JavaScript or Python — which cannot run Rust code
/// to build a form — still offer a label picker over the dataset's own columns.
pub fn resolve_column_options(spec: Vec<FormField>, shape: &DatasetShape) -> Vec<FormField> {
    spec.into_iter()
        .map(|field| match field.query() {
            Some(COLUMNS_QUERY) => {
                let names: Vec<&str> = shape.columns.iter().map(|c| c.name.as_str()).collect();
                field.with_resolved_options(names)
            }
            Some(NUMERIC_COLUMNS_QUERY) => {
                let names = shape.numeric_columns();
                field.with_resolved_options(names)
            }
            Some(CATEGORICAL_COLUMNS_QUERY) => {
                let names: Vec<&str> = shape
                    .columns
                    .iter()
                    .filter(|c| matches!(c.ty, ColumnType::Str | ColumnType::Bool))
                    .map(|c| c.name.as_str())
                    .collect();
                field.with_resolved_options(names)
            }
            _ => field,
        })
        .collect()
}

/// What a fit of a given configuration produces (§10).
///
/// The five are not a taxonomy of algorithms — they are a taxonomy of *answers*,
/// which is what everything downstream needs: [`Test`](Outcome::Test) has no
/// per-row output at all, so nothing asks a t-test to predict, and
/// [`Cluster`](Outcome::Cluster) answers an integer, so `predict()` refuses to
/// write it into a text field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// A number per row: the value of `label`.
    Regression {
        /// The dataset column being predicted.
        label: String,
    },
    /// A class per row: the value of `label`.
    Classification {
        /// The dataset column being predicted.
        label: String,
        /// The classes, once a fit has seen them. `None` before one has — the
        /// classes are the *data's*, and no amount of reading the configuration
        /// discovers them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        classes: Option<Vec<String>>,
    },
    /// A cluster number per row.
    Cluster,
    /// A vector per row.
    Embedding {
        /// How many components the vector has.
        dimensions: usize,
    },
    /// **No per-row output**: the parameters are the result. A hypothesis test.
    Test,
    /// A posterior: draws of every declared parameter, which *are* the result
    /// (Stan TODO §1). It answers per row only when the program was written to
    /// and the configuration names the generated quantity that does (§19).
    Posterior {
        /// The generated-quantities variable a prediction reads, or `None` for
        /// a program that is inspected rather than applied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prediction: Option<String>,
    },
}

impl Outcome {
    /// The stable wire name of the variant — what the UI switches on.
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Regression { .. } => "regression",
            Outcome::Classification { .. } => "classification",
            Outcome::Cluster => "cluster",
            Outcome::Embedding { .. } => "embedding",
            Outcome::Test => "test",
            Outcome::Posterior { .. } => "posterior",
        }
    }

    /// The dataset column this outcome predicts, for the supervised two.
    pub fn label(&self) -> Option<&str> {
        match self {
            Outcome::Regression { label } | Outcome::Classification { label, .. } => Some(label),
            _ => None,
        }
    }

    /// Whether a fit of this outcome answers anything **per row** — whether
    /// prediction means anything at all.
    ///
    /// False for [`Test`](Outcome::Test), which is the whole reason the variant
    /// exists: an ANOVA has an answer, and the answer is not a column. False
    /// for a [`Posterior`](Outcome::Posterior) too, unless it names the
    /// generated quantity a prediction reads.
    pub fn predicts(&self) -> bool {
        match self {
            Outcome::Test => false,
            Outcome::Posterior { prediction } => prediction.is_some(),
            _ => true,
        }
    }

    /// Whether this is a posterior — which is fitted by sampling rather than by
    /// splitting, encoding and scoring, and so goes down its own path.
    pub fn is_posterior(&self) -> bool {
        matches!(self, Outcome::Posterior { .. })
    }

    /// The type of field a prediction of this outcome can be written into —
    /// what `predict()` checks its target against (§12), and `None` for an
    /// outcome that produces nothing per row.
    ///
    /// An embedding is [`Json`](BasicType::Json) because a vector is not a
    /// scalar and rendering it as text would make it unreadable by anything that
    /// wanted to use it. A posterior's prediction is a distribution whose value
    /// in a row is its mean (Stan TODO §19).
    pub fn prediction_type(&self) -> Option<BasicType> {
        match self {
            Outcome::Regression { .. } => Some(BasicType::Float),
            Outcome::Classification { .. } => Some(BasicType::Text),
            Outcome::Cluster => Some(BasicType::Int),
            Outcome::Embedding { .. } => Some(BasicType::Json),
            Outcome::Test => None,
            Outcome::Posterior { prediction } => prediction.as_ref().map(|_| BasicType::Float),
        }
    }
}

/// What a provider *declares* its outcome to be, before a configuration exists
/// to resolve it against.
///
/// [`Outcome`] is a function of the configuration, and a provider written in
/// Rust computes it in code. One supplied by a module cannot — the seam carries
/// data, not closures — so it declares which configuration key holds the label
/// and what happens to it, and [`resolve`](OutcomeSpec::resolve) does the rest.
/// The built-ins use the same declaration, because a second mechanism for the
/// same question would be two things to keep in step.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutcomeSpec {
    /// Predicts the column the configuration key names: a **regression** when
    /// that column is numeric and a **classification** when it is not.
    ///
    /// The `random_forest` case, and the reason [`ModelProvider::outcome`] takes
    /// the shape rather than being a constant.
    Supervised {
        /// The configuration key holding the label column's name.
        label: String,
    },
    /// Always a regression over the column the key names.
    Regression {
        /// The configuration key holding the label column's name.
        label: String,
    },
    /// Always a classification over the column the key names.
    Classification {
        /// The configuration key holding the label column's name.
        label: String,
    },
    /// A cluster number per row, whatever the configuration says.
    Cluster,
    /// A vector per row, as long as the configuration key says.
    Embedding {
        /// The configuration key holding the number of components.
        components: String,
    },
    /// A hypothesis test: no per-row output.
    Test,
    /// A posterior (Stan TODO §1): fitted by
    /// [`fit_posterior`](ModelProvider::fit_posterior), not by `fit`.
    Posterior {
        /// The configuration key naming the generated quantity a prediction
        /// reads, for a provider that can predict at all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prediction: Option<String>,
    },
}

impl OutcomeSpec {
    /// The [`Outcome`] this declaration comes to for one configuration over one
    /// dataset.
    ///
    /// Every failure names the configuration key *and* what was wrong with it,
    /// because the admin is looking at a form with that key on it: "the label
    /// `price` is not a column of this dataset" is actionable and "invalid
    /// configuration" is not.
    pub fn resolve(&self, shape: &DatasetShape, config: &Attrs) -> Result<Outcome> {
        match self {
            OutcomeSpec::Supervised { label } => {
                let (name, ty) = label_column(shape, config, label)?;
                if ty.is_numeric() {
                    Ok(Outcome::Regression { label: name })
                } else {
                    Ok(Outcome::Classification {
                        label: name,
                        classes: None,
                    })
                }
            }
            OutcomeSpec::Regression { label } => {
                let (name, ty) = label_column(shape, config, label)?;
                if !ty.is_numeric() {
                    return Err(Error::invalid(format!(
                        "`{label}`: column `{name}` is {} and a regression needs a number",
                        ty.name()
                    )));
                }
                Ok(Outcome::Regression { label: name })
            }
            OutcomeSpec::Classification { label } => {
                let (name, _) = label_column(shape, config, label)?;
                Ok(Outcome::Classification {
                    label: name,
                    classes: None,
                })
            }
            OutcomeSpec::Cluster => Ok(Outcome::Cluster),
            OutcomeSpec::Embedding { components } => {
                let dimensions = config
                    .get(components)
                    .and_then(Json::as_u64)
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "`{components}`: the number of components must be a positive whole \
                             number"
                        ))
                    })?;
                Ok(Outcome::Embedding {
                    dimensions: dimensions as usize,
                })
            }
            OutcomeSpec::Test => Ok(Outcome::Test),
            OutcomeSpec::Posterior { prediction } => Ok(Outcome::Posterior {
                prediction: prediction
                    .as_ref()
                    .and_then(|key| config.get(key))
                    .and_then(Json::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            }),
        }
    }

    /// The prediction types a fit of this declaration could produce, **without a
    /// dataset to resolve it against**.
    ///
    /// One entry for every declaration but [`Supervised`](OutcomeSpec::Supervised),
    /// which has two because that is the whole reason it exists: a random forest
    /// is a regressor or a classifier depending on the type of the column its
    /// configuration names, and no amount of reading the configuration decides
    /// which without the data.
    ///
    /// It exists for `predict()`'s save-time check (§12): the target field has
    /// to be able to hold what the model will produce, and refusing that on the
    /// form is worth an answer that is sometimes two possibilities wide. The
    /// definitive check is still made at fire time, against the outcome the
    /// instance actually recorded.
    ///
    /// Empty for [`Test`](OutcomeSpec::Test), which produces nothing per row —
    /// so a target of any type is wrong, and the caller says so in those words
    /// — and for a posterior that names no prediction.
    pub fn possible_prediction_types(&self) -> Vec<BasicType> {
        match self {
            OutcomeSpec::Supervised { .. } => vec![BasicType::Float, BasicType::Text],
            OutcomeSpec::Regression { .. } => vec![BasicType::Float],
            OutcomeSpec::Classification { .. } => vec![BasicType::Text],
            OutcomeSpec::Cluster => vec![BasicType::Int],
            OutcomeSpec::Embedding { .. } => vec![BasicType::Json],
            OutcomeSpec::Test => Vec::new(),
            // Only a posterior whose declaration names a prediction. None
            // does while prediction from a posterior is carried past the Stan
            // milestone (Stan TODO §19), so a `predict()` over one is
            // refused when it is saved, not at every fire.
            OutcomeSpec::Posterior { prediction: None } => Vec::new(),
            OutcomeSpec::Posterior {
                prediction: Some(_),
            } => vec![BasicType::Float],
        }
    }
}

/// The dataset column a configuration key names, and its type.
fn label_column(shape: &DatasetShape, config: &Attrs, key: &str) -> Result<(String, ColumnType)> {
    let name = config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{key}`: no column is named as the label")))?;
    let ty = shape.column(name).ok_or_else(|| {
        Error::invalid(format!(
            "`{key}`: `{name}` is not a column of this dataset (it has {})",
            if shape.columns.is_empty() {
                "none".to_owned()
            } else {
                shape
                    .columns
                    .iter()
                    .map(|c| format!("`{}`", c.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ))
    })?;
    if ty == ColumnType::Null {
        return Err(Error::invalid(format!(
            "`{key}`: every value of column `{name}` is null, so nothing can be predicted from it"
        )));
    }
    Ok((name.to_owned(), ty))
}

/// One row of a [`ParameterBlock::Table`] — a term and its numbers.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ParameterRow {
    /// The cells, one per declared column and in that order.
    pub cells: Vec<Json>,
}

impl ParameterRow {
    /// A row of these cells.
    pub fn new(cells: impl IntoIterator<Item = impl Into<Json>>) -> ParameterRow {
        ParameterRow {
            cells: cells.into_iter().map(Into::into).collect(),
        }
    }
}

/// A fitted parameter, in the shape the screen renders it in (§7).
///
/// Structured rather than free JSON so that the admin UI has exactly three
/// renderings to write and never has to know what a coefficient, a cluster
/// centre or an explained-variance ratio is. [`Text`](ParameterBlock::Text)
/// exists for a provider whose own output is a summary nobody should reformat —
/// statsmodels' `summary()` is the case — and it means a fourth kind of
/// parameter can arrive without a schema change.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum ParameterBlock {
    /// One number: R̄², the intercept, the number of iterations.
    Scalar {
        /// What it is called on the screen.
        name: String,
        /// The number.
        value: f64,
    },
    /// A table: a coefficient table (estimate, std. error, *t*, *p*), cluster
    /// centres, feature importances.
    Table {
        /// What it is called on the screen.
        name: String,
        /// The column headings.
        columns: Vec<String>,
        /// The rows, each as wide as `columns`.
        rows: Vec<ParameterRow>,
    },
    /// Text a provider produced and nobody should reformat.
    Text {
        /// What it is called on the screen.
        name: String,
        /// The text.
        body: String,
    },
}

impl ParameterBlock {
    /// A scalar parameter.
    pub fn scalar(name: impl Into<String>, value: f64) -> ParameterBlock {
        ParameterBlock::Scalar {
            name: name.into(),
            value,
        }
    }

    /// A text parameter.
    pub fn text(name: impl Into<String>, body: impl Into<String>) -> ParameterBlock {
        ParameterBlock::Text {
            name: name.into(),
            body: body.into(),
        }
    }

    /// A table parameter, **checked**: a row that is not as wide as the headings
    /// is refused here rather than rendered against the wrong column.
    ///
    /// A ragged coefficient table would put a standard error under *p*, which is
    /// a wrong answer on the screen and not a crash — the class of failure this
    /// whole milestone is most careful about.
    pub fn table(
        name: impl Into<String>,
        columns: impl IntoIterator<Item = impl Into<String>>,
        rows: Vec<ParameterRow>,
    ) -> Result<ParameterBlock> {
        let name = name.into();
        let columns: Vec<String> = columns.into_iter().map(Into::into).collect();
        for (i, row) in rows.iter().enumerate() {
            if row.cells.len() != columns.len() {
                return Err(Error::msg(format!(
                    "parameter table `{name}`: row {} has {} cells but the table has {} columns",
                    i + 1,
                    row.cells.len(),
                    columns.len()
                )));
            }
        }
        Ok(ParameterBlock::Table {
            name,
            columns,
            rows,
        })
    }

    /// What this block is called on the screen.
    pub fn name(&self) -> &str {
        match self {
            ParameterBlock::Scalar { name, .. }
            | ParameterBlock::Table { name, .. }
            | ParameterBlock::Text { name, .. } => name,
        }
    }
}

/// What one fit produced: the state that can be applied again, and the
/// parameters worth looking at.
///
/// **No metrics** — see the module docs. `state` is whatever the provider needs
/// to predict with and is opaque to everything else; a provider wanting to store
/// bytes stores base64, because a system table with a `bytea` column would be
/// the only one (§15).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct FitResult {
    /// The serialised fit, as the provider will be handed it back.
    pub state: Json,
    /// The parameters, in the order they should be shown.
    #[serde(default)]
    pub parameters: Vec<ParameterBlock>,
    /// What the provider thinks the admin should know before trusting this
    /// fit ("the optimiser stopped after 100 iterations without converging:
    /// raise `max_iter`"), as sentences that say what to do. The fit job
    /// writes them to `ATTR_WARNINGS`, beside a posterior's diagnostics, so
    /// "fitted cleanly" means the same thing for every provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl FitResult {
    /// A fit with this state and no parameters yet.
    pub fn new(state: Json) -> FitResult {
        FitResult {
            state,
            parameters: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Append a parameter block, returning `self` for chaining.
    pub fn parameter(mut self, block: ParameterBlock) -> FitResult {
        self.parameters.push(block);
        self
    }

    /// Append a warning, returning `self` for chaining.
    pub fn warning(mut self, sentence: impl Into<String>) -> FitResult {
        self.warnings.push(sentence.into());
        self
    }
}

/// What a fitted instance answers for one row.
///
/// Two of the variants are a **classification's answer at different points in
/// its journey**, and the split is deliberate. A provider works in class
/// *indices*, because that is what the target encoding handed it and what its
/// arithmetic produces; a caller wants the class *name*, because the index is an
/// implementation detail of an encoding and nobody's row wants to hold a `2`.
/// So a provider answers [`ClassIndex`](Prediction::ClassIndex),
/// `sc_model::predict` maps it through the instance's encoding, and
/// [`Class`](Prediction::Class) is what leaves the host. Having one variant do
/// both jobs would make "is this a name or an index" a question about where you
/// are in the call stack.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "prediction", rename_all = "snake_case")]
pub enum Prediction {
    /// A number — a regression's answer.
    Number {
        /// The predicted value.
        value: f64,
    },
    /// A class **index** into the fitted target encoding: what a provider
    /// answers, and never what leaves the host.
    ClassIndex {
        /// The index into the encoding's class list.
        index: usize,
        /// The probability the provider gave it, where it has one — the
        /// prediction's uncertainty, which is most of what a logistic regression
        /// is for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probability: Option<f64>,
    },
    /// A named class: what the host answers, once the target encoding has been
    /// undone.
    Class {
        /// The class name, as the fit saw it in the data.
        class: String,
        /// The probability the provider gave it, where it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probability: Option<f64>,
    },
    /// A cluster number.
    Cluster {
        /// Which cluster.
        cluster: usize,
    },
    /// A vector — an embedding, or a PCA projection.
    Vector {
        /// The components, in the fitted order.
        values: Vec<f64>,
    },
}

impl Prediction {
    /// A regression's answer.
    pub fn number(value: f64) -> Prediction {
        Prediction::Number { value }
    }

    /// A provider's answer for a classification: an index into the encoding.
    pub fn class_index(index: usize, probability: Option<f64>) -> Prediction {
        Prediction::ClassIndex { index, probability }
    }

    /// The host's answer for a classification.
    pub fn class(class: impl Into<String>, probability: Option<f64>) -> Prediction {
        Prediction::Class {
            class: class.into(),
            probability,
        }
    }

    /// The value this prediction writes into a row (§12) — the number, the class
    /// name, the cluster index or the vector.
    ///
    /// A [`ClassIndex`](Prediction::ClassIndex) has **no** such value: it is a
    /// number that means a category, and writing it would be the silent wrong
    /// answer the two variants exist to prevent.
    pub fn to_json(&self) -> Result<Json> {
        match self {
            Prediction::Number { value } => Ok(serde_json::Number::from_f64(*value)
                .map(Json::Number)
                .unwrap_or(Json::Null)),
            Prediction::Class { class, .. } => Ok(Json::String(class.clone())),
            Prediction::Cluster { cluster } => Ok(Json::Number((*cluster).into())),
            Prediction::Vector { values } => Ok(Json::Array(
                values
                    .iter()
                    .map(|v| {
                        serde_json::Number::from_f64(*v)
                            .map(Json::Number)
                            .unwrap_or(Json::Null)
                    })
                    .collect(),
            )),
            Prediction::ClassIndex { index, .. } => Err(Error::msg(format!(
                "a class index ({index}) reached a caller: it should have been mapped back \
                 through the instance's target encoding first"
            ))),
        }
    }
}

/// One model provider the picker offers, and everything about it that can be
/// described without running Rust code.
///
/// It is [`TableProviderKind`](sc_catalog::TableProviderKind)'s twin, and for
/// the same reason: it is what crosses the module seam, and it is what the admin
/// UI lists.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelProviderKind {
    /// The name it is registered and stored under — `linear_regression`.
    pub name: String,
    /// One line for the picker.
    pub description: String,
    /// The package supplying it, or `None` for a built-in.
    pub module: Option<String>,
    /// The settings it takes, **before** a dataset resolves the column pickers
    /// (see [`resolve_column_options`]).
    pub config_spec: Vec<FormField>,
    /// The hyperparameters it takes. A model stores, per hyperparameter, either
    /// a value or a list of values, and a fit runs the grid of the lists (§11).
    pub hyperparameters: Vec<FormField>,
    /// What a fit of it produces, as a declaration.
    pub outcome: OutcomeSpec,
    /// Whether the host should **standardise** the numeric features before
    /// handing them over (§6).
    ///
    /// A declaration rather than something the provider does itself, because the
    /// constants have to be stored on the instance and applied identically at
    /// predict time — and a provider that standardised privately would be a
    /// second, unrecorded encoding. A k-means or a PCA says yes (an unscaled fit
    /// is dominated by whichever column happens to be measured in larger units);
    /// a regression says no, because a coefficient in the data's own units is
    /// what somebody is reading it for.
    pub standardise: bool,
    /// Whether the provider takes **bound data** — a program's declared
    /// variables tied to datasets — rather than one encoded frame (Stan TODO
    /// §18).
    ///
    /// A capability rather than a provider name, so the model form renders the
    /// binding editor for any provider that declares it and none of the admin
    /// UI names Stan.
    pub binds_data: bool,
    /// Whether a running fit of it can be **cancelled** (Stan TODO §13) — a
    /// provider whose fit is a subprocess it can kill. `cancelModelFit` is
    /// refused by name for every other, since stopping a `smartcore` or a
    /// Python call mid-flight is not something the host can do.
    pub cancellable: bool,
}

impl ModelProviderKind {
    /// A built-in provider's kind: no module, no settings, no hyperparameters.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        outcome: OutcomeSpec,
    ) -> ModelProviderKind {
        ModelProviderKind {
            name: name.into(),
            description: description.into(),
            module: None,
            config_spec: Vec::new(),
            hyperparameters: Vec::new(),
            outcome,
            standardise: false,
            binds_data: false,
            cancellable: false,
        }
    }

    /// The settings it takes.
    pub fn config(mut self, spec: Vec<FormField>) -> ModelProviderKind {
        self.config_spec = spec;
        self
    }

    /// The hyperparameters it takes.
    pub fn hyperparameters(mut self, spec: Vec<FormField>) -> ModelProviderKind {
        self.hyperparameters = spec;
        self
    }

    /// Ask the host to standardise the numeric features.
    pub fn standardised(mut self) -> ModelProviderKind {
        self.standardise = true;
        self
    }

    /// Declare that it takes bound data.
    pub fn binding_data(mut self) -> ModelProviderKind {
        self.binds_data = true;
        self
    }

    /// The module that supplies it.
    pub fn module(mut self, module: impl Into<String>) -> ModelProviderKind {
        self.module = Some(module.into());
        self
    }

    /// Where this provider comes from, as a phrase an error message can use:
    /// "the built-in providers", or "the module `@saltcorn/sklearn`".
    pub fn source(&self) -> String {
        match &self.module {
            Some(module) => format!("the module `{module}`"),
            None => "the built-in providers".to_owned(),
        }
    }
}

/// Code that can fit something (§10).
///
/// Object-safe and dynamically dispatched, for the reason an `Action` is: which
/// provider a model uses is decided at runtime from a stored name, and the set
/// is meant to grow from outside this crate.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// The name it is registered and stored under. Stable: it is what a saved
    /// model references.
    fn name(&self) -> &str;

    /// One line for the picker.
    fn description(&self) -> &str;

    /// The settings this provider takes, **before** a dataset exists to resolve
    /// its column pickers against.
    ///
    /// Declared separately from [`config_spec`](ModelProvider::config_spec)
    /// because there are two moments: the picker lists providers before any
    /// dataset has been chosen, and a module supplies a declaration rather than
    /// code. A field that means "a column of the dataset" declares
    /// [`COLUMNS_QUERY`] (or one of its two narrower siblings) and is filled in
    /// by the default `config_spec`.
    fn config_declaration(&self) -> Vec<FormField>;

    /// The form, given the dataset's columns — a provider naming a label needs
    /// to offer *these* columns as its options.
    ///
    /// The default resolves the column queries in
    /// [`config_declaration`](ModelProvider::config_declaration), which is
    /// enough for every provider that asks for columns and constants. Override
    /// it for a form whose *shape* depends on the data.
    fn config_spec(&self, shape: &DatasetShape) -> Vec<FormField> {
        resolve_column_options(self.config_declaration(), shape)
    }

    /// The hyperparameters, as form fields. A model stores either a value or a
    /// list of values against each, and a fit runs the grid (§11).
    fn hyperparameters(&self) -> Vec<FormField> {
        Vec::new()
    }

    /// What a fit of this provider produces, as a declaration.
    fn outcome_spec(&self) -> OutcomeSpec;

    /// Whether the host should standardise the numeric features before handing
    /// them over — see [`ModelProviderKind::standardise`].
    fn standardise(&self) -> bool {
        false
    }

    /// Whether this provider takes bound data — see
    /// [`ModelProviderKind::binds_data`].
    fn binds_data(&self) -> bool {
        false
    }

    /// Whether a running fit honours [`FitContext::cancelled`] — see
    /// [`ModelProviderKind::cancellable`].
    fn cancellable(&self) -> bool {
        false
    }

    /// What a fit of *this configuration* over *this dataset* will produce.
    ///
    /// Not a constant: a random forest is a regressor or a classifier depending
    /// on its label's type (§10).
    fn outcome(&self, shape: &DatasetShape, config: &Attrs) -> Result<Outcome> {
        self.outcome_spec().resolve(shape, config)
    }

    /// Check a configuration beyond what
    /// [`config_spec`](ModelProvider::config_spec) can express — the part only
    /// this provider knows.
    ///
    /// Runs where the generic check runs: on save, in front of the admin, and
    /// again before a fit.
    fn validate(&self, shape: &DatasetShape, config: &Attrs) -> Result<()> {
        let _ = (shape, config);
        Ok(())
    }

    /// Everything this provider is, as the picker and the module seam see it.
    ///
    /// The default assembles it from the methods above, so a built-in declares
    /// each thing once; a provider that *came* from a module returns the kind it
    /// arrived as.
    fn kind(&self) -> ModelProviderKind {
        ModelProviderKind {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            module: None,
            config_spec: self.config_declaration(),
            hyperparameters: self.hyperparameters(),
            outcome: self.outcome_spec(),
            standardise: self.standardise(),
            binds_data: self.binds_data(),
            cancellable: self.cancellable(),
        }
    }

    /// Fit `frame` — already encoded to numbers by the host (Phase 3) — with
    /// this configuration and this point of the hyperparameter grid.
    async fn fit(&self, frame: &Frame, config: &Attrs, hyper: &Attrs) -> Result<FitResult>;

    /// Apply a fitted [`state`](FitResult::state) to a frame, answering one
    /// prediction per row **in row order**.
    ///
    /// A frame, not a row: a single row is a frame of one, and batching is what
    /// makes a provider in another language usable at all.
    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>>;

    /// What the program this configuration names declares (Stan TODO §5), or
    /// `None` for a provider with no program.
    ///
    /// Async because answering reads the program — out of a file store, for a
    /// Stan model. A provider that answers `None` gets no binding: its
    /// [`fit_posterior`](ModelProvider::fit_posterior) is handed the datasets and
    /// an empty data object.
    async fn interface(&self, config: &Attrs) -> Result<Option<Interface>> {
        let _ = config;
        Ok(None)
    }

    /// Sample a posterior (Stan TODO §2): the datasets and the data bound from
    /// them in, the draws out.
    ///
    /// Called instead of [`fit`](ModelProvider::fit) for a provider whose
    /// outcome is a [`Posterior`](Outcome::Posterior), and never otherwise. The
    /// default refuses by name, which is what every provider that is not a
    /// sampler wants.
    async fn fit_posterior(
        &self,
        input: &PosteriorInput,
        config: &Attrs,
        ctx: &FitContext<'_>,
    ) -> Result<PosteriorResult> {
        let _ = (input, config, ctx);
        Err(Error::invalid(format!(
            "the model provider `{}` does not sample a posterior",
            self.name()
        )))
    }

    /// How many draws a posterior fit of `config` will store, before it runs —
    /// what the host checks the draws' size against `--stan-max-draws-bytes`
    /// with (Stan TODO §14). `None` (the default) when the provider cannot say,
    /// and then only the draws that come back are measured.
    fn draw_plan(&self, config: &Attrs) -> Result<Option<DrawPlan>> {
        let _ = config;
        Ok(None)
    }

    /// The files of a fit's raw run, when `state` says it kept one outside the
    /// database — what `downloadModelRun` zips (Stan TODO §16), as `(path in
    /// the run, bytes)`. `None` (the default) when there is none, and the host
    /// builds the download from the stored draws instead.
    async fn run_files(&self, state: &Json) -> Result<Option<Vec<(String, Vec<u8>)>>> {
        let _ = state;
        Ok(None)
    }

    /// Whether the program `config` names now differs from the one a fitted
    /// `state` snapshotted — what the instance screen says as "the program has
    /// changed since this fit" (Stan TODO §§6, 18). `None` (the default) when
    /// the provider has no program, the state holds no snapshot, or the
    /// program cannot be read to compare: "cannot tell" is not "unchanged".
    async fn program_changed(&self, config: &Attrs, state: &Json) -> Option<bool> {
        let _ = (config, state);
        None
    }

    /// Release whatever a fitted `state` holds outside the database — a raw run
    /// directory in a file store (Stan TODO §14). Called when the instance is
    /// deleted, after its rows are gone. The default holds nothing.
    async fn discard(&self, state: &Json) -> Result<()> {
        let _ = state;
        Ok(())
    }
}

/// The model providers a module supplies: the seam `sc-module` and `sc-python`
/// implement (§4, §14).
///
/// Three questions, split the way [`TableProviderHost`](sc_catalog::
/// TableProviderHost)'s are: enumerating is synchronous because a form renders it
/// in one expression, and the two that reach a module are async because they
/// reach a module.
///
/// Routing is by the `(module, provider)` pair rather than by the provider name
/// alone — as a table provider's is — because one host serves every module of
/// its language, and two of them may well supply a `random_forest`. The registry
/// refuses the second by name; the host still has to be able to tell them apart.
#[async_trait]
pub trait ModelProviderHost: Send + Sync {
    /// Every provider every loaded module supplies.
    fn providers(&self) -> Vec<ModelProviderKind>;

    /// Fit one, on the far side of the seam.
    ///
    /// The frame crosses as **columns, not rows of objects** (§14): a
    /// 50 000 × 12 dataset is twelve JSON arrays and not 50 000 objects with the
    /// same twelve keys repeated, and on the Python side it lands as something
    /// `numpy.asarray` takes directly.
    async fn fit(
        &self,
        module: &str,
        provider: &str,
        frame: &Frame,
        config: &Attrs,
        hyper: &Attrs,
    ) -> Result<FitResult>;

    /// Predict with one, on the far side of the seam.
    async fn predict(
        &self,
        module: &str,
        provider: &str,
        state: &Json,
        frame: &Frame,
    ) -> Result<Vec<Prediction>>;
}

/// A module's provider, as a [`ModelProvider`].
///
/// The adapter that lets the registry hold one kind of thing. Everything
/// declarative comes off the [`ModelProviderKind`] the module supplied; `fit` and
/// `predict` route back to the host with the module name the kind carries.
pub struct HostProvider {
    kind: ModelProviderKind,
    module: String,
    host: std::sync::Arc<dyn ModelProviderHost>,
}

impl HostProvider {
    /// Wrap one supplied kind. Fails when the kind names no module, because a
    /// provider that arrived through a host and cannot say which module it came
    /// from cannot be routed back to.
    pub fn new(
        kind: ModelProviderKind,
        host: std::sync::Arc<dyn ModelProviderHost>,
    ) -> Result<HostProvider> {
        let module = kind.module.clone().ok_or_else(|| {
            Error::config(format!(
                "the model provider `{}` was supplied by a module host but names no module",
                kind.name
            ))
        })?;
        Ok(HostProvider { kind, module, host })
    }
}

#[async_trait]
impl ModelProvider for HostProvider {
    fn name(&self) -> &str {
        &self.kind.name
    }

    fn description(&self) -> &str {
        &self.kind.description
    }

    fn config_declaration(&self) -> Vec<FormField> {
        self.kind.config_spec.clone()
    }

    fn hyperparameters(&self) -> Vec<FormField> {
        self.kind.hyperparameters.clone()
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        self.kind.outcome.clone()
    }

    fn standardise(&self) -> bool {
        self.kind.standardise
    }

    fn binds_data(&self) -> bool {
        self.kind.binds_data
    }

    fn kind(&self) -> ModelProviderKind {
        self.kind.clone()
    }

    async fn fit(&self, frame: &Frame, config: &Attrs, hyper: &Attrs) -> Result<FitResult> {
        self.host
            .fit(&self.module, &self.kind.name, frame, config, hyper)
            .await
    }

    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
        self.host
            .predict(&self.module, &self.kind.name, state, frame)
            .await
    }
}

/// A field restricted to the dataset's columns — the declaration a label picker
/// is written as.
pub fn column_field(name: impl Into<String>, label: impl Into<String>) -> FormField {
    FormField::new(name, BasicType::Text)
        .label(label)
        .server_query(COLUMNS_QUERY)
}

/// A field restricted to the dataset's **numeric** columns.
pub fn numeric_column_field(name: impl Into<String>, label: impl Into<String>) -> FormField {
    FormField::new(name, BasicType::Text)
        .label(label)
        .server_query(NUMERIC_COLUMNS_QUERY)
}

/// A field restricted to the dataset's **categorical** columns.
pub fn categorical_column_field(name: impl Into<String>, label: impl Into<String>) -> FormField {
    FormField::new(name, BasicType::Text)
        .label(label)
        .server_query(CATEGORICAL_COLUMNS_QUERY)
}

/// Whether a field's options come from the dataset — what tells the admin API
/// that this spec has to be resolved before it is shown.
pub fn is_column_query(field: &FormField) -> bool {
    matches!(
        field.query(),
        Some(COLUMNS_QUERY | NUMERIC_COLUMNS_QUERY | CATEGORICAL_COLUMNS_QUERY)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::DatasetColumnShape;
    use sc_types::OptionsSource;
    use serde_json::json;

    fn shape() -> DatasetShape {
        DatasetShape {
            table: "houses".to_owned(),
            columns: vec![
                DatasetColumnShape {
                    name: "price".to_owned(),
                    ty: ColumnType::Float,
                },
                DatasetColumnShape {
                    name: "region".to_owned(),
                    ty: ColumnType::Str,
                },
                DatasetColumnShape {
                    name: "nothing".to_owned(),
                    ty: ColumnType::Null,
                },
            ],
        }
    }

    fn config(pairs: &[(&str, Json)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn a_supervised_outcome_is_decided_by_the_labels_type() {
        let spec = OutcomeSpec::Supervised {
            label: "label".to_owned(),
        };
        assert_eq!(
            spec.resolve(&shape(), &config(&[("label", json!("price"))]))
                .unwrap(),
            Outcome::Regression {
                label: "price".to_owned()
            }
        );
        // The same provider, the same configuration key, a different column —
        // and it is a classifier. This is the whole reason `outcome` is a
        // function rather than a constant.
        assert_eq!(
            spec.resolve(&shape(), &config(&[("label", json!("region"))]))
                .unwrap(),
            Outcome::Classification {
                label: "region".to_owned(),
                classes: None
            }
        );
    }

    #[test]
    fn a_label_that_is_not_a_column_names_the_key_and_the_alternatives() {
        let spec = OutcomeSpec::Regression {
            label: "label".to_owned(),
        };
        let err = spec
            .resolve(&shape(), &config(&[("label", json!("bedrooms"))]))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("label") && msg.contains("bedrooms"), "{msg}");
        assert!(msg.contains("price") && msg.contains("region"), "{msg}");
    }

    #[test]
    fn a_regression_refuses_a_text_label_and_an_all_null_one() {
        let spec = OutcomeSpec::Regression {
            label: "label".to_owned(),
        };
        let err = spec
            .resolve(&shape(), &config(&[("label", json!("region"))]))
            .unwrap_err();
        assert!(err.to_string().contains("needs a number"), "{err}");

        let err = spec
            .resolve(&shape(), &config(&[("label", json!("nothing"))]))
            .unwrap_err();
        assert!(err.to_string().contains("null"), "{err}");
    }

    #[test]
    fn an_embedding_takes_its_width_from_the_configuration() {
        let spec = OutcomeSpec::Embedding {
            components: "n_components".to_owned(),
        };
        assert_eq!(
            spec.resolve(&shape(), &config(&[("n_components", json!(3))]))
                .unwrap(),
            Outcome::Embedding { dimensions: 3 }
        );
        let err = spec
            .resolve(&shape(), &config(&[("n_components", json!(0))]))
            .unwrap_err();
        assert!(err.to_string().contains("n_components"), "{err}");
    }

    #[test]
    fn a_column_picker_is_resolved_against_the_dataset() {
        let spec = vec![
            numeric_column_field("label", "Label"),
            categorical_column_field("group", "Group"),
            column_field("weight", "Weight"),
            FormField::new("intercept", BasicType::Bool),
        ];
        assert!(spec.iter().take(3).all(is_column_query));
        let resolved = resolve_column_options(spec, &shape());
        assert_eq!(resolved[0].static_options(), [json!("price")]);
        assert_eq!(resolved[1].static_options(), [json!("region")]);
        assert_eq!(
            resolved[2].static_options(),
            [json!("price"), json!("region"), json!("nothing")]
        );
        // A field that is not a column picker is left exactly as declared.
        assert!(resolved[3].static_options().is_empty());
        assert_eq!(resolved[3].options_source, OptionsSource::None);
    }

    #[test]
    fn a_ragged_parameter_table_is_refused_rather_than_rendered() {
        let err = ParameterBlock::table(
            "Coefficients",
            ["term", "estimate", "std. error"],
            vec![ParameterRow::new([json!("price"), json!(1.0)])],
        )
        .unwrap_err();
        assert!(err.to_string().contains("Coefficients"), "{err}");

        let ok = ParameterBlock::table(
            "Coefficients",
            ["term", "estimate"],
            vec![ParameterRow::new([json!("price"), json!(1.0)])],
        )
        .unwrap();
        assert_eq!(ok.name(), "Coefficients");
    }

    #[test]
    fn a_class_index_cannot_be_written_into_a_row() {
        // The variant split's whole purpose: an index that escaped the mapping
        // is caught rather than written, because a row holding `2` where a
        // category belongs is a silent wrong answer.
        let err = Prediction::class_index(2, Some(0.9)).to_json().unwrap_err();
        assert!(err.to_string().contains("class index"), "{err}");
        assert_eq!(
            Prediction::class("chair", Some(0.9)).to_json().unwrap(),
            json!("chair")
        );
        assert_eq!(Prediction::number(1.5).to_json().unwrap(), json!(1.5));
    }

    #[test]
    fn an_outcome_says_what_kind_of_field_can_hold_it() {
        assert_eq!(
            Outcome::Regression {
                label: "price".to_owned()
            }
            .prediction_type(),
            Some(BasicType::Float)
        );
        assert_eq!(Outcome::Cluster.prediction_type(), Some(BasicType::Int));
        // A hypothesis test answers nothing per row, so there is nothing for
        // `predict()` to answer and it is refused.
        assert_eq!(Outcome::Test.prediction_type(), None);
        assert!(!Outcome::Test.predicts());
        assert_eq!(Outcome::Cluster.name(), "cluster");
    }

    #[test]
    fn a_posterior_predicts_only_when_it_names_the_quantity_a_prediction_reads() {
        let inspected = Outcome::Posterior { prediction: None };
        assert!(!inspected.predicts());
        assert_eq!(inspected.prediction_type(), None);
        assert!(inspected.is_posterior());
        assert_eq!(inspected.name(), "posterior");

        let applied = Outcome::Posterior {
            prediction: Some("y_new".to_owned()),
        };
        assert!(applied.predicts());
        assert_eq!(applied.prediction_type(), Some(BasicType::Float));

        // The declaration names the key; the configuration names the variable.
        let spec = OutcomeSpec::Posterior {
            prediction: Some("prediction".to_owned()),
        };
        assert_eq!(
            spec.resolve(&shape(), &config(&[("prediction", json!("y_new"))]))
                .unwrap(),
            applied
        );
        assert_eq!(
            spec.resolve(&shape(), &config(&[("prediction", json!("  "))]))
                .unwrap(),
            inspected
        );
        assert_eq!(
            serde_json::to_value(&applied).unwrap(),
            json!({ "outcome": "posterior", "prediction": "y_new" })
        );
        assert_eq!(
            serde_json::from_value::<Outcome>(json!({ "outcome": "posterior" })).unwrap(),
            inspected
        );
    }
}
