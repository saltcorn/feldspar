//! Datasets (layer 5; analytics TODO A1 — the goals document's "Dataset
//! operations").
//!
//! A dataset is a **base** (a table, or another dataset) followed by an
//! **ordered list of operations**, each taking the rows the one before it
//! produced — a pipeline of tidyverse verbs adapted to Feldspar's relational
//! model. It is persistent and named ([`DatasetDef`], stored in
//! `_fd_datasets`), shared by the models, panels and other datasets that read
//! it, and never materialised: [`compile`] turns it into one `sc-query`
//! statement, nested only where an operation needs the one before it as a
//! subquery.
//!
//! The pieces:
//!
//! - [`def`](DatasetDef) — the definition, as the JSON the Analytics UI edits.
//! - [`compile`] — the stages: each operation's columns, their types and the
//!   grain ([`StageShape`]), its errors as sentences, and the query each stage
//!   is. Formulas are `sc-expr` formulas, checked against the stage the way a
//!   calculated field is checked against its table.
//! - [`read_stage`] — a page of a stage's rows, with the total.
//! - [`save_dataset`] and its neighbours — the store.
//! - [`Snapshot`] — what a model fit records: the resolved definitions and a
//!   hash of what they mean.

mod compile;
mod def;
mod infer;
mod read;
mod shape;
mod snapshot;
mod store;
mod walk;

pub use compile::{
    BaseReport, Compilation, Library, MAX_RANGE_VALUES, OpStatus, OperationReport, Options,
    ROW_KEY, Restriction, Stage, compile, scramble,
};
pub use def::{
    AggregateOp, Base, CalculatedOp, CompleteColumn, CompleteOp, CompleteValues, DatasetDef,
    DatasetId, FillValue, FilterOp, GroupKey, JoinKey, JoinKind, JoinOp, LimitMode, LimitOp, Op,
    Operation, OrderKey, Other, SelectColumn, SelectOp, SortKey, SortOp, SpatialJoinOp,
    SpatialRelation, SplitOp, SplitSummary, StackOp, Summary, SummaryFunction, UnionOp,
    WindowFunction, WindowOp,
};
pub use read::{
    MAX_PAGE, Page, Rows, StagePage, column_values, count, last_stage, read_page, read_rows,
    read_stage, value_json,
};
pub use shape::{ColType, ForeignKey, Grain, Schema, StageColumn, StageShape, TableInfo};
pub use snapshot::Snapshot;
pub use store::{
    DATASETS_TABLE, bootstrap_datasets, clone_dataset, datasets_using, delete_dataset,
    list_datasets, load_dataset, load_dataset_by_name, load_library, require_dataset, save_dataset,
};
