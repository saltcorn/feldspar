//! Predictive models (layer 6; TODO "Predictive models", which replaces the
//! technical design's §14.2).
//!
//! Everything else in this system *retrieves*. This crate is the half that
//! answers what the data **implies**: a [`Dataset`] is a saved question about a
//! table — which rows, and which derived values — and a model provider answers
//! it, leaving a fitted instance behind that is both inspected (the
//! coefficients, the test statistic) and applied (a predicted price on a row a
//! trigger just inserted).
//!
//! Phase 1 was the data half of that: what a dataset is, what it becomes as
//! SQL, what comes back ([`Frame`]), and how the rows divide into train,
//! validation and test ([`Split`]). Phase 2 is the **vocabulary and the
//! store**: what a model provider is ([`ModelProvider`]), how the built-ins and
//! a module's are assembled into one set ([`ModelRegistry`]), and what a
//! [`Model`] and a [`ModelInstance`] are as rows. Phase 3 is the **work**: what
//! turns a frame into numbers ([`Encoding`]), what scores the result
//! ([`Metrics`]), the order the two go in ([`run_fit`]), and how a fitted
//! instance is applied to a row it has never seen ([`predict_rows`]). Phase 4
//! is the **algorithms**: the seven built-in providers
//! ([`builtin_providers`]), five of them smartcore's behind the `smartcore`
//! feature and two of them hypothesis tests that are there either way (see
//! [`BUILTINS_COMPILED_OUT`]).
//!
//! The Bayesian milestone (the Stan TODO) widens the seam without adding a
//! second one: a model gains **related datasets** ([`NamedDataset`]) and a
//! dataset an **order** ([`DatasetOrder`]); a provider may declare what its
//! program needs ([`Interface`]) and sample a posterior
//! ([`ModelProvider::fit_posterior`]); and the draws that come back get a table
//! of their own ([`DrawsReader`]), written in the transaction that marks the
//! instance fitted.
//!
//! ## Layering: why this is at layer 6 and not above the row layer
//!
//! Its data comes from `sc-api::rows`, which is layer 8, so the obvious place
//! for this crate is above it. It is here instead for the reason `sc-action` is
//! here: **a module supplies model providers** the way it supplies actions and
//! table providers, and `sc-module` (layer 6) can only implement a trait
//! declared *below* it. `TableProviderHost` — declared in `sc-catalog` at layer
//! 4, implemented in `sc-module` at layer 6 — is the same shape.
//!
//! The price is that this crate cannot read a row, and it does not pretend
//! otherwise: reading is a **seam** ([`DatasetSource`]) that somebody above the
//! row layer fills in (`sc_server::models::CatalogDatasetSource`), exactly as
//! `sc-agent` declares `ProviderConnector` and `sc-server` supplies it. Going
//! around the row layer instead would mean a dataset that ignored non-stored
//! calculated fields, ownership and row-level security, and that could not read
//! a provided table at all.
//!
//! ## The decisions this crate fixes
//!
//! - **A dataset is a list of formulas, and that is the whole of it.** There is
//!   no second vocabulary of "field / joinfield / aggregation" with three shapes
//!   in the JSON and three code paths behind it: a column is an `sc-expr`
//!   formula, validated against the same [`SchemaShape`](sc_expr::SchemaShape)
//!   as a calculated field and translated by the same `translate_value`. The
//!   admin UI's picker is sugar that *writes* one.
//! - **The split is a hash of the primary key, not a shuffle.** So a refit after
//!   new rows arrive keeps every old row on the side it was on, and the test
//!   metric of instance 7 is comparable with the test metric of instance 3 —
//!   which is the entire reason anybody looks at two instances of one model.
//!   See [`Split`].
//! - **The encoding belongs to the instance.** Fitted once, on the training rows
//!   only, and stored — so a prediction is encoded the way its fit was, or it
//!   fails by name. Re-deriving the one-hot column order at predict time would
//!   put every coefficient against the wrong column and return confident
//!   nonsense. See [`Encoding`].
//! - **Metrics are the host's; parameters are the provider's.** The same code
//!   scores every provider on the same rows, so two instances' numbers mean one
//!   thing — and a provider in another language does not have to reimplement R²
//!   to be a citizen here. See [`Metrics`].
//! - **The frame is columnar, and it is bounded.** Every consumer wants a
//!   column, and a dataset is a `SELECT` an admin wrote that the server has to
//!   hold in memory — so [`DatasetSource::materialise`] takes a cap and refuses
//!   by name rather than by the OOM killer. See [`DEFAULT_MAX_ROWS`].

mod bind;
mod dataset;
mod diagnose;
mod draws;
mod encode;
mod fit;
mod frame;
mod instance;
mod instance_store;
mod interface;
mod metrics;
mod model;
mod posterior;
mod predict;
mod provider;
mod providers;
mod reading;
mod registry;
mod source;
mod split;
mod store;
mod summary;
mod validate;

pub use bind::{
    Aggregate, Along, Axis, BINDINGS_KEY, BindReport, Binding, BoundData, Coordinates,
    DEFAULT_MAX_DATA_VALUES, DIMENSIONS_KEY, DataPreview, DatasetReport, DesignCoordinates,
    DimensionCoordinates, DimensionKind, DimensionSpec, DropReport, EXCLUDE_VARIABLES_KEY, Edges,
    KEEP_DRAWS_KEY, LABEL_COLUMN, LABELS_KEY, Labeller, MAX_DISTANCE_SITES, MAX_GRID_STEPS,
    MAX_ICAR_NODES, POLICIES_KEY, Points, Policies, Policy, RecordedAxes, Suggestions, Symmetric,
    TimeScale, VariablePreview, VariableReport, bind_data, binding_dataset, check_bindings,
    check_bindings_declared, element_label, excluded_variables, keeps_draws, named_axes,
    preview_data, recorded_axes, suggest_bindings,
};
pub use dataset::{
    ATTR_DATASETS, Dataset, DatasetColumn, DatasetColumnShape, DatasetOrder, DatasetShape,
    dataset_changed, datasets_hash, datasets_record, fitted_dataset, translate_filter,
};
pub use diagnose::{
    EBFMI_THRESHOLD, ESS_PER_CHAIN_THRESHOLD, PosteriorReport, RHAT_THRESHOLD, ebfmi,
    report as diagnose_posterior,
};
pub use draws::{
    BYTES_PER_DRAW, BYTES_PER_ROW, DRAWS_TABLE, DrawsQuery, DrawsReader, PlannedDraws,
    bootstrap_model_draws, check_planned_draws, declared_elements, human_bytes, plan_draws,
    stored_bytes,
};
pub use encode::{
    ColumnEncoding, Encoded, Encoding, Matrix, TargetEncoding, apply_encoding,
    apply_encoding_dropping, fit_encoding,
};
pub use fit::{
    ATTR_AXES, ATTR_BINDING, ATTR_CANCEL_REQUESTED, ATTR_COORDINATES, ATTR_OUTCOME, ATTR_PROGRESS,
    ATTR_ROWS, ATTR_SEARCH, ATTR_WARNINGS, Activation, Fit, FitStarter, GridPoint, MAX_GRID_POINTS,
    RowCounts, fit_model, fit_model_with, fitted_cleanly, grid, run_fit, run_fit_with,
};
pub use frame::{Column, ColumnType, Frame, canonical_key};
pub use instance::{ATTR_ERROR, FitStatus, InstanceId, ModelInstance, RESTARTED};
pub use instance_store::{
    INSTANCES_TABLE, ProgressWrite, active_model_instance, bootstrap_model_instances,
    cancel_requested, delete_model_instance, fitted, list_model_instances, load_model_instance,
    reap_fitting_instances, record_fit_progress, request_fit_cancel, require_model_instance,
    save_fitted_instance, save_model_instance,
};
pub use interface::{Declaration, Element, Interface, SizeExpr, SizeOp, SizeTree};
pub use metrics::{
    ApproximationMetrics, ClassMetrics, Metrics, ModeMetrics, PosteriorMetrics, SplitMetrics,
};
pub use model::{MAIN_DATASET, Model, ModelId, NamedDataset};
pub use posterior::{
    ChainPhase, ChainProgress, DEFAULT_MAX_DRAWS_BYTES, DEFAULT_SUMMARY_MAX_ELEMENTS, DrawPlan,
    DrawSeries, FitContext, FitProgress, FitStage, NoProgress, PosteriorInput, PosteriorLimits,
    PosteriorMethod, PosteriorResult, PosteriorRun, Progress,
};
pub use predict::{
    Predictions, Subject, name_classes, no_per_row_prediction, predict_rows, predict_subject,
    prediction_values,
};
pub use provider::{
    CATEGORICAL_COLUMNS_QUERY, COLUMNS_QUERY, FitResult, HostProvider, ModelProvider,
    ModelProviderHost, ModelProviderKind, NUMERIC_COLUMNS_QUERY, Outcome, OutcomeSpec,
    ParameterBlock, ParameterRow, Prediction, categorical_column_field, column_field,
    is_column_query, numeric_column_field, resolve_column_options,
};
pub use providers::{BUILTINS_COMPILED_OUT, SMARTCORE, builtin_providers, builtin_registry};
pub use reading::{
    ChainDraws, CoordinatePart, CoordinateWrite, DEFAULT_MAX_DRAWS_RESPONSE, DrawsRequest,
    PlannedRow, PosteriorView, PosteriorWrite, Selection, VariableDraws, VariableSummary,
    WriteMode, WritePlan, draws_csv, instance_coordinates, plan_write, posterior_method,
    posterior_variables, read_draws, statistic_names, summarise_variable,
};
pub use registry::ModelRegistry;
pub use source::{CompiledSource, DEFAULT_MAX_ROWS, DatasetSource, Read, SPLIT_KEY};
pub use split::{Part, Split, SplitCounts, Splits};
pub use store::{
    MODELS_QUERY, MODELS_TABLE, bootstrap_models, dataset_ref, delete_model, list_models,
    load_model, load_model_by_name, models_for_table, related_json, require_model, save_model,
};
pub use summary::{ElementSummary, ess_bulk, ess_mean, ess_tail, quantile, rhat};
pub use validate::{ModelIssue, Models, validate_model};
