//! How a dataset becomes rows: the seam, and its bound (TODO §4, §9).
//!
//! This crate is at layer 6 so a module can supply a model provider (see the
//! crate docs), and the row layer is at layer 8 — so reading is a **seam** that
//! somebody above the row layer fills in. `sc_server::models::CatalogDatasetSource`
//! is that somebody, over `sc_api::rows`, which is what makes a dataset see the
//! non-stored calculated fields, the ownership rule, row-level security and a
//! provided table. Going around the row layer would mean a dataset that saw none
//! of those.
//!
//! The seam takes a **cap** rather than reading it from a configuration this
//! crate cannot see. A dataset is a `SELECT` an admin wrote and the server has
//! to hold the answer in memory, so a materialisation that would exceed the cap
//! is refused by name — "the dataset selects more than 200 000 rows; add a
//! filter or raise `--model-max-rows`" — rather than by the OOM killer. The
//! count is asked for **before** the rows, so the refusal costs one `COUNT(*)`
//! and not a partial read.
//!
//! ## Why a read is three things and not one
//!
//! A fit reads the whole dataset, and that was the only reader Phase 3 had. Two
//! more arrived with the API (Phase 5) and neither is that read:
//!
//! - **A prediction** wants the dataset's derived columns for *some* rows — the
//!   row a trigger just wrote, or the rows a filter selects. It cannot compute
//!   them itself: a join path and an aggregation are the row layer's answer, so
//!   the restriction has to go *into* the read rather than be applied to what
//!   comes back. Hence [`Read::restricted_to`], which the source ands into the
//!   `WHERE`. A prediction also reads [`unfiltered`](Read::unfiltered), and that
//!   is the point of the flag: a dataset's filter says which rows the model was
//!   **fitted from**, not which rows it may be asked about. `sold` is the
//!   motivating case — a model of what houses sell for is fitted on the sold
//!   ones and asked about the unsold one a trigger just inserted, and a read
//!   that kept the filter would answer "this row is not in the dataset" for
//!   every row anybody actually wants a prediction for.
//! - **A preview** wants the first few rows and their types, on a table that may
//!   be far over the cap — that is the whole point of previewing before fitting.
//!   Hence [`Read::first`], which is a `LIMIT` and therefore needs no count: the
//!   answer is bounded by construction, so refusing it for being over the cap
//!   would refuse the one screen that exists to say "your dataset is too big".

use std::sync::Arc;

use async_trait::async_trait;
use sc_catalog::Catalog;
use sc_dataset::{Grain, Options, Restriction, Schema, compile, count, read_rows};
use sc_error::{Context, Error, Result};
use sc_query::{Expr, Value};

use crate::dataset::Dataset;
use crate::frame::{Column, Frame, canonical_key};

/// The default ceiling on a dataset's rows (`--model-max-rows`).
pub const DEFAULT_MAX_ROWS: u64 = 200_000;

/// The alias a dataset read projects each row's primary key under, so the split
/// can hash it (§5).
///
/// A reserved name rather than the primary-key column's own, because a dataset
/// column may legitimately be *called* `id` while computing something else — and
/// a split that hashed that would be a split over the wrong thing.
pub const SPLIT_KEY: &str = "_fd_split_key";

/// What one read of a dataset asks for: the bound it must stay under, the rows
/// it is restricted to, and how many of them it wants.
///
/// An options value rather than three parameters because two of the three are
/// absent in the common case, and `materialise(ds, None, None, cap)` at every
/// call site would say nothing about which `None` was which.
#[derive(Debug, Clone, Copy)]
pub struct Read<'a> {
    /// The ceiling on the rows this read may return — `--model-max-rows`.
    pub cap: u64,
    /// An extra predicate, anded with the dataset's own filter.
    ///
    /// A `sc_query::Expr` rather than a formula, because the two callers build
    /// it differently and both already have what they need: a prediction over
    /// one row has the primary key's *value*, and a prediction over a filter has
    /// a formula it translated with
    /// [`translate_filter`](crate::translate_filter).
    pub restrict: Option<&'a Expr>,
    /// At most this many rows, and **no count**: a limited read is bounded by
    /// construction.
    pub limit: Option<u64>,
    /// Whether the dataset's **own** filter applies.
    ///
    /// True for a fit and a preview, which are looking at the sample the model
    /// is *about*. False for a prediction, which is looking at rows the caller
    /// named: the filter is a statement about what was fitted, and reusing it to
    /// decide what may be predicted would make a model fitted on `sold` houses
    /// unable to answer about an unsold one — which is the only question anybody
    /// asks it.
    pub filtered: bool,
}

impl<'a> Read<'a> {
    /// Every row the dataset selects, up to `cap`.
    pub fn all(cap: u64) -> Read<'a> {
        Read {
            cap,
            restrict: None,
            limit: None,
            filtered: true,
        }
    }

    /// The same read, **without** the dataset's own filter — what a prediction
    /// does. See [`filtered`](Read::filtered).
    pub fn unfiltered(mut self) -> Read<'a> {
        self.filtered = false;
        self
    }

    /// The same read, restricted to the rows `expr` selects.
    pub fn restricted_to(mut self, expr: &'a Expr) -> Read<'a> {
        self.restrict = Some(expr);
        self
    }

    /// The same read, stopping after `limit` rows.
    pub fn first(mut self, limit: u64) -> Read<'a> {
        self.limit = Some(limit);
        self
    }

    /// How many rows this read may return at most — the limit where there is
    /// one, and the cap otherwise.
    pub fn ceiling(&self) -> u64 {
        self.limit.map_or(self.cap, |n| n.min(self.cap))
    }
}

/// How a [`Dataset`] becomes a [`Frame`]. [`CompiledSource`] is the real
/// one; tests hand in frames of their own.
#[async_trait]
pub trait DatasetSource: Send + Sync {
    /// Read `ds` as `how` asks, refusing by name if an unlimited read would
    /// exceed [`Read::cap`].
    ///
    /// The frame's [`keys`](Frame::keys) are filled in when the dataset's table
    /// has a single primary key and left empty when it does not: reading is
    /// unaffected by that, and only [`Frame::split`](crate::Frame::split)
    /// refuses.
    async fn read(&self, ds: &Dataset, how: &Read<'_>) -> Result<Frame>;

    /// Read every row of `ds`, refusing by name if it selects more than `cap` —
    /// what a fit does.
    async fn materialise(&self, ds: &Dataset, cap: u64) -> Result<Frame> {
        self.read(ds, &Read::all(cap)).await
    }
}

/// The [`DatasetSource`] that reads a named dataset the way everything else
/// does: compiled by `sc-dataset` into one query and run on the primary
/// database (analytics TODO A1.8).
///
/// As the admin, for now (A1.7): a model is the admin's, and A9 is where a
/// restricted reader's permissions enter. A table's non-stored calculated
/// fields are columns of a dataset over it where they become SQL, which is
/// what the dataset compiler does with them.
pub struct CompiledSource {
    catalog: Arc<Catalog>,
}

impl CompiledSource {
    /// A source reading from `catalog`.
    pub fn new(catalog: Arc<Catalog>) -> CompiledSource {
        CompiledSource { catalog }
    }
}

#[async_trait]
impl DatasetSource for CompiledSource {
    async fn read(&self, ds: &Dataset, how: &Read<'_>) -> Result<Frame> {
        let snapshot = ds.readable()?;
        let schema = Schema::of_catalog(&self.catalog)?;
        // A prediction reads past the filters: they say which rows the model
        // was fitted from, not which rows it may be asked about.
        let options = Options {
            skip_filters: !how.filtered,
        };
        let compiled = compile(&schema, &snapshot.library(), snapshot.def(), options);
        let stage = compiled
            .last()
            .map_err(|e| Error::invalid(format!("the dataset `{}` does not read: {e}", ds.name)))?;
        let restriction = match how.restrict {
            Some(filter) => {
                let key = schema
                    .tables
                    .get(&ds.table)
                    .and_then(|t| t.primary_key.clone())
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "`{}` has no single primary key, so its rows cannot be asked for one \
                             by one",
                            ds.table
                        ))
                    })?;
                Some(Restriction {
                    table: ds.table.clone(),
                    key,
                    filter: filter.clone(),
                })
            }
            None => None,
        };
        // The count first, and on purpose: a dataset is held in memory, so
        // the refusal costs one `COUNT(*)` rather than a partial read. A
        // limited read is bounded by construction and skips it.
        if how.limit.is_none() {
            let n = count(&self.catalog, stage, restriction.as_ref())
                .await
                .with_context(|| format!("counting the rows of the dataset `{}`", ds.name))?;
            if how.cap < n {
                return Err(Error::invalid(format!(
                    "the dataset selects more than {} rows (it selects {n}); add a filter or \
                     raise `--model-max-rows`",
                    how.cap
                )));
            }
        }
        let limit = how.limit.map(|_| how.ceiling());
        let rows = read_rows(&self.catalog, stage, restriction.as_ref(), limit)
            .await
            .with_context(|| format!("reading the dataset `{}`", ds.name))?;
        let n = rows.rows.len();
        let mut columns: Vec<(String, Column)> = Vec::with_capacity(rows.columns.len());
        for (i, column) in rows.columns.iter().enumerate() {
            let values: Vec<Value> = rows
                .rows
                .iter()
                .map(|r| r.get(i).cloned().unwrap_or(Value::Null))
                .collect();
            columns.push((
                column.name.clone(),
                if n == 0 {
                    Column::Null(0)
                } else {
                    Column::from_values(values)
                },
            ));
        }
        // Each row's identity, for the split to hash and a prediction to be
        // matched back by: the table's primary key, or the group's keys.
        let keys: Vec<String> = if !rows.keys.is_empty() {
            rows.keys.iter().map(canonical_key).collect()
        } else if let Grain::Group { keys } = &rows.grain {
            let at: Vec<usize> = keys
                .iter()
                .filter_map(|k| rows.columns.iter().position(|c| &c.name == k))
                .collect();
            if at.is_empty() {
                Vec::new()
            } else {
                rows.rows
                    .iter()
                    .map(|r| {
                        at.iter()
                            .map(|i| canonical_key(r.get(*i).unwrap_or(&Value::Null)))
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .collect()
            }
        } else {
            Vec::new()
        };
        Frame::new(columns, keys)
    }
}
