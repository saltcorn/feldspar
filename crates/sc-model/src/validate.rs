//! Validating a [`Model`] before it is stored — and again when it is loaded
//! (TODO task 2.5).
//!
//! Every way a model can be wrong is checked in one place, and checked **on
//! save**, because that is when the admin is standing in front of the form: a
//! provider nothing implements, a dataset column whose formula no longer
//! resolves, a hyperparameter the provider does not declare, split fractions
//! that do not sum to 1. Discovering any of those inside a fit means an instance
//! id that was already returned and a job that fails a second later — which is
//! the shape §8 chose, and precisely why the checks belong in front of it.
//!
//! The same function runs at **load** ([`Models`]), where a model that no longer
//! validates is listed with its reason and **stays editable**, because editing
//! it is the repair. That is the agent rule and the trigger rule, for the same
//! reason: a model that disappeared from the screen because a column was renamed
//! would take its dataset with it.
//!
//! ## Why the shape is optional
//!
//! A provider's form is a function of the dataset's columns and their *types*
//! (§10), and a [`DatasetShape`] is built from a materialised frame — because a
//! `SchemaShape` carries no types at all, and the type of `price / area`, of a
//! join path or of an aggregation is not derivable from one. So the half of
//! validation that needs the shape can only run where something has read the
//! data.
//!
//! Rather than pretend otherwise, the shape is a parameter and it may be absent:
//!
//! - The **admin UI's save** has one, because the dataset builder previews the
//!   dataset before it offers the provider's form; so does the fit, which
//!   materialises first and validates second.
//! - The **load** path does not, and must not: listing forty models would mean
//!   forty dataset reads, at a moment when nobody has asked for any of them.
//!
//! With no shape, every check that does not need the data still runs, and the
//! configuration is checked against the provider's *declaration* — same names,
//! same types, only the column option lists unresolved. What is skipped is
//! narrow and it is checked again before any fit.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::validate_attrs;
use serde_json::Value as Json;

use crate::bind::check_bindings;
use crate::dataset::DatasetShape;
use crate::model::{MAIN_DATASET, Model};
use crate::provider::{ModelProvider, OutcomeSpec};
use crate::registry::ModelRegistry;
use crate::store::{MODELS_TABLE, list_models};

/// Check everything about `model` that can be checked without fitting it.
///
/// Called by [`save_model`](crate::save_model) and by [`Models::load`]. The
/// error names the model first, then the problem, because the admin is looking
/// at a list of models when they see it.
///
/// `shape` is the dataset's columns and their types — see the module docs for
/// why it is optional and what is skipped without it.
pub async fn validate_model(
    catalog: &Catalog,
    registry: &ModelRegistry,
    model: &Model,
    shape: Option<&DatasetShape>,
) -> Result<()> {
    let name = model.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a model needs a name"));
    }
    let problem = |msg: String| Error::invalid(format!("model `{name}`: {msg}"));

    // The dataset first, because everything else is about it: the provider's
    // form is over its columns, and the split is over its rows. A named
    // dataset that does not read — deleted, or with an operation marked
    // invalid — is the model's problem to report, by the dataset's sentence.
    let schema = sc_dataset::Schema::of_catalog(catalog)?;
    model
        .dataset
        .readable()
        .map_err(|e| problem(e.to_string()))?;
    if model.dataset.columns.is_empty() {
        return Err(problem(format!(
            "the dataset `{}` has no columns",
            model.dataset.name
        )));
    }

    // The split's fractions, and the identity its hash needs: a row of a
    // table, or a group. Rows that are neither — a stack, a union — read fine
    // and cannot be split (§5), and refusing that here rather than at fit time
    // is the difference between a form that will not save and a job that fails.
    model.split.validate().map_err(|e| problem(e.to_string()))?;
    if let Some(refusal) = model.dataset.split_refusal() {
        return Err(problem(refusal));
    }

    let provider = registry
        .require(model.provider.trim())
        .map_err(|e| problem(e.to_string()))?;

    // Stan's data is bound by rows of tables: a dimension is a table's rows
    // and an index a key into one (analytics TODO A1.9 — A8 lifts this where
    // it can).
    if provider.binds_data() && !model.dataset.keeps_table_grain() {
        return Err(problem(grain_refusal(&model.dataset)));
    }
    validate_related(model, &schema).map_err(|e| problem(e.to_string()))?;

    // The configuration: against the resolved form where a shape is at hand, and
    // against the declaration where it is not (same names and types, unrestricted
    // options).
    let spec = match shape {
        Some(shape) => provider.config_spec(shape),
        None => provider.config_declaration(),
    };
    validate_attrs(&spec, &model.configuration).map_err(|e| problem(e.to_string()))?;

    validate_hyperparameters(provider.as_ref(), model).map_err(|e| problem(e.to_string()))?;

    // A posterior is sampled, not searched: there is no validation split to
    // score a grid point on and no primary metric to rank one by. Known from the
    // declaration alone, so it is refused here rather than inside the job.
    if matches!(provider.outcome_spec(), OutcomeSpec::Posterior { .. }) && model.searches() {
        return Err(problem(NO_POSTERIOR_SEARCH.to_owned()));
    }

    // A provider that binds data: its program's `data` block against the
    // bindings — every variable bound, every name a binding uses real, every
    // kind able to produce its declaration's rank and type, and an order to
    // resolve the datasets in (Stan TODO §10, the save-time half). Reading the program is not
    // reading the data, so this runs with or without a shape — and a program
    // that is gone from its store lists the model with that reason.
    if provider.binds_data() {
        let interface = provider
            .interface(&model.configuration)
            .await
            .map_err(|e| problem(e.to_string()))?;
        if let Some(interface) = interface {
            check_bindings(&interface, model).map_err(|e| problem(e.to_string()))?;
        }
    }

    if let Some(shape) = shape {
        // What the fit will produce — computed rather than assumed, because a
        // configuration that names no label, or one that is not a column of this
        // dataset, is a model that cannot be fitted and the admin is looking at
        // the form that says so.
        provider
            .outcome(shape, &model.configuration)
            .map_err(|e| problem(e.to_string()))?;
        provider
            .validate(shape, &model.configuration)
            .map_err(|e| problem(e.to_string()))?;
    }

    Ok(())
}

/// Why a posterior refuses a hyperparameter list — here, and again in the fit.
pub(crate) const NO_POSTERIOR_SEARCH: &str = "a posterior is sampled, not searched: there is no held-out score to rank grid points by, \
     so give each hyperparameter one value (the sampler's settings are the provider's \
     configuration)";

/// Each related dataset validates as the main one does, against its own table,
/// under a name bindings can use (Stan TODO §7).
///
/// The primary key is required of each, as it is of the main one, for the
/// reason a posterior needs related datasets at all: a related dataset is a
/// **dimension**, its positions are recorded as keys, and a write-back matches
/// by them. Rows that cannot be told apart cannot be written back to.
fn validate_related(model: &Model, schema: &sc_dataset::Schema) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for related in &model.related {
        let name = related.name.trim();
        if !is_identifier(name) {
            return Err(Error::invalid(format!(
                "related dataset `{name}`: a name must be an identifier — letters, digits and \
                 `_`, not starting with a digit — because bindings address it by name"
            )));
        }
        if name == MAIN_DATASET {
            return Err(Error::invalid(format!(
                "a related dataset cannot be called `{MAIN_DATASET}`: that is what bindings call \
                 the model's own dataset"
            )));
        }
        if !seen.insert(name) {
            return Err(Error::invalid(format!(
                "two related datasets are called `{name}`"
            )));
        }
        let at = |e: Error| Error::invalid(format!("related dataset `{name}`: {e}"));
        related.dataset.readable().map_err(at)?;
        if !related.dataset.keeps_table_grain() {
            return Err(at(Error::invalid(grain_refusal(&related.dataset))));
        }
        if let Some(label) = &related.label {
            related.dataset.check_label(schema, label).map_err(at)?;
        }
    }
    Ok(())
}

/// Why a dataset whose rows are not rows of a table cannot be bound to a
/// Stan program.
pub(crate) fn grain_refusal(dataset: &crate::Dataset) -> String {
    format!(
        "a Stan program's data is bound from rows of tables — a dimension is a table's rows \
         and an index a key into one — and in the dataset `{}` {}",
        dataset.name,
        dataset.grain.as_ref().map_or_else(
            || "rows are not known".to_owned(),
            sc_dataset::Grain::describe
        )
    )
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Every hyperparameter names one the provider declares, and every value is of
/// the declared type — as a value or as a **list** of them (§11).
///
/// A list is checked element by element against the same declaration, so
/// `n_trees: [100, "many"]` is refused with the same message a scalar would get.
/// An empty list is refused too: it is a search over nothing, which would fit no
/// model at all and is much more likely a half-typed grid than an intention.
fn validate_hyperparameters(provider: &dyn ModelProvider, model: &Model) -> Result<()> {
    let spec = provider.hyperparameters();
    for (key, value) in &model.hyperparameters {
        let field = spec.iter().find(|f| f.name() == key).ok_or_else(|| {
            Error::invalid(format!(
                "unknown hyperparameter `{key}`; `{}` takes {}",
                provider.name(),
                if spec.is_empty() {
                    "none".to_owned()
                } else {
                    spec.iter()
                        .map(|f| format!("`{}`", f.name()))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ))
        })?;
        let points: Vec<&Json> = match value {
            Json::Array(values) if values.is_empty() => {
                return Err(Error::invalid(format!(
                    "hyperparameter `{key}` is an empty list, so there is nothing to search over"
                )));
            }
            Json::Array(values) => values.iter().collect(),
            single => vec![single],
        };
        for point in points {
            let mut one = sc_types::Attrs::new();
            one.insert(key.clone(), point.clone());
            validate_attrs(std::slice::from_ref(field), &one)?;
        }
    }
    Ok(())
}

/// Why one stored model is not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelIssue {
    /// The model's name, as stored.
    pub model: String,
    /// What is wrong with it, in the words the validator used.
    pub problem: String,
}

/// The models that can be fitted, plus the ones that cannot and why.
///
/// `Agents`' and `Triggers`' twin. A model that fails validation is **not**
/// deleted and **not** hidden: it stays stored, stays listed and stays editable,
/// because editing it is the repair. What it does not do is fit.
#[derive(Debug, Clone, Default)]
pub struct Models {
    models: Vec<Model>,
    issues: Vec<ModelIssue>,
}

impl Models {
    /// An empty set — a catalog with no `_fd_models` table, and the starting
    /// point for a test.
    pub fn empty() -> Models {
        Models::default()
    }

    /// Load and validate every stored model.
    ///
    /// A catalog with no `_fd_models` table yields an empty set rather than an
    /// error: that table's absence *means* "no models have ever been defined".
    ///
    /// No dataset is read here (see the module docs), so what is checked is the
    /// shape-free half.
    pub async fn load(catalog: &Catalog, registry: &ModelRegistry) -> Result<Models> {
        if catalog.get(MODELS_TABLE)?.is_none() {
            return Ok(Models::empty());
        }
        let mut out = Models::default();
        for model in list_models(catalog).await? {
            match validate_model(catalog, registry, &model, None).await {
                Ok(()) => out.models.push(model),
                Err(e) => out.issues.push(ModelIssue {
                    model: model.name.clone(),
                    problem: e.to_string(),
                }),
            }
        }
        Ok(out)
    }

    /// Reload in place, so a live handle picks up a save or a delete.
    pub async fn reload(&mut self, catalog: &Catalog, registry: &ModelRegistry) -> Result<()> {
        *self = Models::load(catalog, registry).await?;
        Ok(())
    }

    /// The models that can be fitted, ordered by name.
    pub fn all(&self) -> &[Model] {
        &self.models
    }

    /// The stored models that cannot, with their reasons.
    pub fn issues(&self) -> &[ModelIssue] {
        &self.issues
    }

    /// The model named `name`, if it is in the usable set.
    pub fn by_name(&self, name: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.name == name)
    }

    /// The model named `name`, or an error that distinguishes the two ways it
    /// can be missing — never defined, or defined and invalid.
    ///
    /// The distinction is the whole point: "no model named `house prices`" sends
    /// the admin looking for a typo, and "model `house prices` is not usable:
    /// unknown model provider `sklearn_gbm`" sends them to the module they
    /// uninstalled.
    pub fn require(&self, name: &str) -> Result<&Model> {
        if let Some(model) = self.by_name(name) {
            return Ok(model);
        }
        match self.issues.iter().find(|i| i.model == name) {
            Some(issue) => Err(Error::invalid(format!(
                "model `{name}` is not usable: {}",
                issue.problem
            ))),
            None => Err(Error::not_found(format!("no model named `{name}`"))),
        }
    }
}
