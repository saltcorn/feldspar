//! The [`Model`]: a dataset, a provider, its configuration and its
//! hyperparameter space (TODO §1, §15).
//!
//! Pure data, like a `Trigger` or an `Agent` — the row it is stored as lives in
//! [`store`](crate::store), the validation it must pass in
//! [`validate`](crate::validate), and the fitting in Phase 3. A model knows
//! nothing about how it is fitted, which is what lets the same record be fitted
//! from the admin screen, from an API call and (later) from a trigger without
//! reshaping anything.
//!
//! **The dataset is a named one** (analytics TODO A1.8): the model holds a
//! reference, resolved when it is loaded, and each fit records the definition
//! it read — so editing the dataset flags the fits rather than silently
//! changing what they mean.
//!
//! **A model is edited and refitted**; each fit leaves a
//! [`ModelInstance`](crate::ModelInstance) behind, so the instances of one model
//! are its history and are comparable: same dataset, same split, different
//! settings.

use sc_types::Attrs;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::dataset::Dataset;
use crate::split::Split;

/// What bindings call the model's own dataset (Stan TODO §7), and therefore the
/// one name a related dataset may not have.
pub const MAIN_DATASET: &str = "main";

/// A dataset **beside** the model's main one, under a name bindings address it
/// by — `counties` beside `homes` (Stan TODO §7).
///
/// The model's [`dataset`](Model::dataset) is one rectangle and every provider
/// until the posterior one wanted exactly that. A hierarchical model wants the
/// groups as well as the observations, and the groups have a table of their
/// own: a county with no homes is a row of `counties`, and a numbering built
/// from the homes would drop exactly the county partial pooling is most
/// informative about.
///
/// Stored as `{ name, dataset_id, label }`: the dataset is a named one, like
/// the main dataset (analytics TODO A1.8).
#[derive(Debug, Clone, PartialEq)]
pub struct NamedDataset {
    /// What bindings call it: an identifier, unique among the model's related
    /// datasets, and never [`MAIN_DATASET`].
    pub name: String,
    /// The named dataset, resolved.
    pub dataset: Dataset,
    /// A formula over the dataset's last stage whose value names a row on the
    /// screen — `name` for `counties`. `None` names a row by its primary key.
    pub label: Option<String>,
}

impl NamedDataset {
    /// A related dataset called `name`, labelled by its primary key.
    pub fn new(name: impl Into<String>, dataset: Dataset) -> NamedDataset {
        NamedDataset {
            name: name.into(),
            dataset,
            label: None,
        }
    }

    /// Label its rows by `formula`.
    pub fn labelled(mut self, formula: impl Into<String>) -> NamedDataset {
        self.label = Some(formula.into());
        self
    }
}

/// Identifies a model: the UUID primary key of its `_fd_models` row (§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ModelId(pub Uuid);

impl ModelId {
    /// Mint an id for a new model.
    pub fn new() -> ModelId {
        ModelId(Uuid::new_v4())
    }
}

impl Default for ModelId {
    fn default() -> Self {
        ModelId::new()
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A saved question about a table: which rows and which derived values make up
/// the data, which provider answers it, and with what settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// Stable identity: the UUID of its `_fd_models` row.
    pub id: ModelId,
    /// The unique, human-facing name — what a `predict("…")` formula and the admin
    /// screen address it by, so renaming one breaks those references
    /// deliberately rather than silently.
    pub name: String,
    /// Human-readable description (§9 requires one on every metadata row; the
    /// empty string means "none given").
    pub description: String,
    /// The registered [`ModelProvider`](crate::ModelProvider) name.
    pub provider: String,
    /// Which rows and which derived values (§2). Stored as the `dataset` JSON
    /// column. Bindings call it [`MAIN_DATASET`].
    pub dataset: Dataset,
    /// The datasets beside it, in the order the form shows them (Stan TODO §7).
    /// Stored as the nullable `related` JSON column; every provider but a
    /// posterior one ignores them.
    pub related: Vec<NamedDataset>,
    /// The provider's configuration, keyed by its
    /// [`config_spec`](crate::ModelProvider::config_spec) field names.
    pub configuration: Attrs,
    /// The hyperparameter **space**: per hyperparameter either a value or a
    /// *list* of values, and a fit runs the grid of the lists (§11).
    ///
    /// One column rather than two ("values" and "grids"), because a list of one
    /// and a scalar are the same search and nothing downstream should have to
    /// ask which shape a setting was typed in.
    pub hyperparameters: Attrs,
    /// The fractions a fit divides the rows by, and the seed the hash is salted
    /// with (§5). On the **model**, not the instance, because that is what makes
    /// two instances of one model comparable.
    pub split: Split,
    /// Sparse per-model values (§9).
    pub attributes: Attrs,
}

impl Model {
    /// A **new** model with a fresh id, over `dataset` with `provider`.
    pub fn new(name: impl Into<String>, provider: impl Into<String>, dataset: Dataset) -> Model {
        Model::with_id(ModelId::new(), name, provider, dataset)
    }

    /// Reconstruct an existing model, which already has an id — what
    /// [`load_model`](crate::load_model) and an update path use.
    pub fn with_id(
        id: ModelId,
        name: impl Into<String>,
        provider: impl Into<String>,
        dataset: Dataset,
    ) -> Model {
        Model {
            id,
            name: name.into(),
            description: String::new(),
            provider: provider.into(),
            dataset,
            related: Vec::new(),
            configuration: Attrs::new(),
            hyperparameters: Attrs::new(),
            split: Split::default(),
            attributes: Attrs::new(),
        }
    }

    /// The table this model is over — the one its dataset's rows start from,
    /// and **only** that.
    ///
    /// `_fd_models` carries a `table_name` column so the list can be filtered by
    /// table without reading every dataset, derived from here on every save. A
    /// dataset's base cannot be changed, so the two cannot drift apart.
    pub fn table(&self) -> &str {
        &self.dataset.table
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> Model {
        self.description = description.into();
        self
    }

    /// Set one configuration value, returning `self` for chaining.
    pub fn config(mut self, key: impl Into<String>, value: impl Into<Json>) -> Model {
        self.configuration.insert(key.into(), value.into());
        self
    }

    /// Set one hyperparameter — a value, or a list of values to search over.
    pub fn hyperparameter(mut self, key: impl Into<String>, value: impl Into<Json>) -> Model {
        self.hyperparameters.insert(key.into(), value.into());
        self
    }

    /// Add a related dataset, returning `self` for chaining.
    pub fn related(mut self, related: NamedDataset) -> Model {
        self.related.push(related);
        self
    }

    /// The related dataset called `name`.
    pub fn related_dataset(&self, name: &str) -> Option<&NamedDataset> {
        self.related.iter().find(|r| r.name == name)
    }

    /// Set the split.
    pub fn split(mut self, split: Split) -> Model {
        self.split = split;
        self
    }

    /// Set one attribute, returning `self` for chaining.
    pub fn attribute(mut self, key: impl Into<String>, value: impl Into<Json>) -> Model {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Whether any hyperparameter carries a **list**, which is what makes a fit
    /// a search (§11).
    ///
    /// With no lists there is no search, the validation split is empty, and a
    /// fit is a fit — the common case, which must not pay for the uncommon one.
    pub fn searches(&self) -> bool {
        self.hyperparameters.values().any(Json::is_array)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_models_table_is_its_datasets_and_there_is_no_second_copy() {
        let model = Model::new(
            "house prices",
            "linear_regression",
            Dataset::new("houses").column("price", "price"),
        );
        assert_eq!(model.table(), "houses");
    }

    #[test]
    fn a_list_of_hyperparameter_values_is_what_makes_a_fit_a_search() {
        let model = Model::new("m", "random_forest", Dataset::new("houses"));
        assert!(!model.searches());
        assert!(!model.clone().hyperparameter("n_trees", 100).searches());
        assert!(
            model
                .hyperparameter("n_trees", serde_json::json!([100, 200]))
                .searches()
        );
    }
}
