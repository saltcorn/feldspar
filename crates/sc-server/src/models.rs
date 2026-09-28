//! Predictive models: the pieces `sc-model` declares but cannot supply, and the
//! services a running server holds them in (TODO "Predictive models", §4, §8).
//!
//! `sc-model` is at layer 6 so a module can supply a model provider — the same
//! placement argument `sc-action` carries — and the price is that it cannot read
//! a row: `sc_api::rows` is layer 8. So it declares
//! [`DatasetSource`](sc_model::DatasetSource) and this is where the seam is
//! filled in, exactly as `sc-agent` declares `ProviderConnector` and
//! [`agents`](crate::agents) supplies it.
//!
//! Reading **through the row layer** rather than around it is the whole point of
//! the seam. A dataset that issued its own `SELECT` would see no non-stored
//! calculated field, would ignore ownership and row-level security, and could
//! not read a table a module provides at all — three ways for a fit to be
//! computed over rows that are not the rows the application has.
//!
//! [`ModelServices`] is the assembly [`AgentServices`](crate::AgentServices) and
//! the trigger dispatcher already are: the registry of providers, the source, the
//! row cap this process was started with, and the one method that **starts a
//! fit** ([`ModelServices::start_fit`]). It rides on
//! [`AppMounts`](crate::AppMounts) with the other four for the reason they do —
//! the admin handlers and the action registry both already hold that handle.
//!
//! ## A fit is a job, and the row is the registry (§8)
//!
//! [`start_fit`](ModelServices::start_fit) writes the instance row saying
//! `fitting`, returns it, and spawns the work. There is no in-memory job
//! registry, so nothing survives a restart — which is why
//! [`install_models`] reaps every row still `fitting` at boot rather than
//! leaving an admin looking at a fit in progress that is not.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use sc_api::rows::{RowQuery, count_rows_where, list_row_values};
use sc_catalog::Catalog;
use sc_error::{Context, Error, Result};
use sc_model::{
    Column, Dataset, DatasetSource, Frame, InstanceId, Model, ModelInstance, ModelRegistry, Read,
    SPLIT_KEY, bootstrap_model_draws, bootstrap_model_instances, bootstrap_models,
    builtin_registry, canonical_key, fit_model, reap_fitting_instances, save_model_instance,
};
use sc_query::{Expr, Projection, Value};

/// The [`DatasetSource`] a running server has: the catalog, read through
/// `sc_api::rows`.
pub struct CatalogDatasetSource {
    catalog: Arc<Catalog>,
}

impl CatalogDatasetSource {
    /// A source over `catalog`.
    pub fn new(catalog: Arc<Catalog>) -> CatalogDatasetSource {
        CatalogDatasetSource { catalog }
    }
}

#[async_trait]
impl DatasetSource for CatalogDatasetSource {
    async fn read(&self, ds: &Dataset, how: &Read<'_>) -> Result<Frame> {
        let table = self.catalog.require(&ds.table)?;
        let shape = self.catalog.schema_shape()?;
        // The dataset's own filter, and the caller's restriction anded onto it:
        // a prediction about one row is that row's primary key in the `WHERE`,
        // not a full read filtered afterwards, because "afterwards" would mean
        // materialising the whole table to answer about one row of it.
        //
        // A prediction reads `unfiltered`, and that is deliberate: the dataset's
        // filter says which rows the model was *fitted from*, not which rows it
        // may be asked about. A model of what houses sell for is fitted on the
        // sold ones and asked about the unsold one a trigger just inserted.
        let own = if how.filtered {
            ds.filter_expr(&shape)?
        } else {
            None
        };
        let filter = match (own, how.restrict) {
            (Some(own), Some(extra)) => Some(own.and(extra.clone())),
            (Some(own), None) => Some(own),
            (None, Some(extra)) => Some(extra.clone()),
            (None, None) => None,
        };

        // The count first, and on purpose: a dataset is a `SELECT` an admin
        // wrote and the server has to hold the answer in memory, so the refusal
        // costs one `COUNT(*)` rather than a partial read that has already
        // allocated most of what it would have refused.
        //
        // A **limited** read skips it, because it is bounded by construction:
        // the preview screen exists to show an admin the dataset that is too
        // big to fit, and refusing it for being too big would refuse the one
        // answer they came for.
        if how.limit.is_none() {
            let count = count_rows_where(&self.catalog, &table, filter.clone(), None)
                .await
                .with_context(|| format!("counting the rows of dataset table `{}`", ds.table))?;
            if count > 0 && how.cap < count as u64 {
                return Err(Error::invalid(format!(
                    "the dataset selects more than {} rows (it selects {count}); \
                     add a filter or raise `--model-max-rows`",
                    how.cap
                )));
            }
        }

        // The split key rides along as a reserved projection rather than as the
        // primary-key column's own name: a dataset column may legitimately be
        // *called* `id` while computing something else, and a split that hashed
        // that would be a split over the wrong thing. A table with a composite
        // or absent primary key simply has no key column — reads are unaffected,
        // and only the split refuses (§5).
        let mut projections = ds.projections(&shape)?;
        let key_column = ds.primary_key(&shape).ok();
        if let Some(pk) = &key_column {
            projections.push(Projection::expr_as(
                Expr::qcol(ds.table.clone(), pk.clone()),
                SPLIT_KEY,
            ));
        }

        // The dataset's order, then its primary key, on every read — a fit, a
        // preview and a prediction alike. A posterior needs it (the same seed
        // over the same rows in another order is another set of draws, Stan
        // TODO §7), and a hash-split provider is indifferent to it.
        let mut query = RowQuery::new()
            .where_(filter)
            .projecting(projections)
            .order_by(ds.order_by(&shape)?);
        if how.limit.is_some() {
            // Never above the cap, even when the caller asked for more: the cap
            // is what this process can hold, and a limit is what this caller
            // wants.
            query = query.limit(how.ceiling());
        }
        let rows = list_row_values(&self.catalog, &table, &query, None)
            .await
            .with_context(|| format!("reading dataset table `{}`", ds.table))?;

        // A frame of no rows is a real answer — a filter that matched nothing —
        // and it keeps its columns: a frame with none would fail later as a
        // shape error rather than here as an empty fit. There is also nothing to
        // check translation against, since a column is "present" only by having
        // arrived on some row.
        if rows.is_empty() {
            return Frame::new(
                ds.columns
                    .iter()
                    .map(|c| (c.name.clone(), Column::Null(0)))
                    .collect(),
                Vec::new(),
            );
        }

        let mut columns = Vec::with_capacity(ds.columns.len());
        let mut arrived: BTreeSet<&str> = BTreeSet::new();
        for column in &ds.columns {
            // A column absent from every row is one whose formula did not
            // translate: `Dataset::projections` leaves those to the reified
            // evaluator, which the read path does not yet run.
            if rows.iter().all(|row| !row.contains_key(&column.name)) {
                continue;
            }
            arrived.insert(column.name.as_str());
            columns.push((
                column.name.clone(),
                Column::from_values(
                    rows.iter()
                        .map(|row| row.get(&column.name).cloned().unwrap_or(Value::Null))
                        .collect(),
                ),
            ));
        }
        // Saying which column and why, rather than handing back a frame that is
        // quietly missing it — a fit over the columns that happened to arrive
        // would not be the fit the model asked for.
        let missing = ds.missing(&arrived);
        if !missing.is_empty() {
            return Err(Error::invalid(format!(
                "dataset on `{}`: the formula for {} does not translate to SQL, and the \
                 dataset read has no reified fallback",
                ds.table,
                missing
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        let keys = match &key_column {
            Some(_) => rows
                .iter()
                .map(|row| canonical_key(row.get(SPLIT_KEY).unwrap_or(&Value::Null)))
                .collect(),
            None => Vec::new(),
        };
        Frame::new(columns, keys)
    }
}

/// The model machinery a running server holds: the provider registry, the
/// dataset seam, and the bound a read must stay under.
///
/// Cloneable and cheap, like [`AgentServices`](crate::AgentServices), because
/// four places need the same one: the admin handlers, the `predict_row` action
/// in the trigger registry, the module reload that rebuilds the registry, and
/// the spawned fit itself.
#[derive(Clone)]
pub struct ModelServices {
    catalog: Arc<Catalog>,
    /// The providers, **replaced whole** rather than mutated when the module set
    /// changes — so a fit that is running keeps the registry it started with,
    /// which is the same rule the action registry follows.
    registry: Arc<RwLock<Arc<ModelRegistry>>>,
    source: Arc<dyn DatasetSource>,
    max_rows: u64,
}

impl ModelServices {
    /// The services over `catalog`, with the built-in providers and the catalog
    /// as the dataset source.
    pub fn new(catalog: &Arc<Catalog>, max_rows: u64) -> Result<ModelServices> {
        Ok(ModelServices {
            catalog: Arc::clone(catalog),
            registry: Arc::new(RwLock::new(Arc::new(
                builtin_registry().context("registering the built-in model providers")?,
            ))),
            source: Arc::new(CatalogDatasetSource::new(Arc::clone(catalog))),
            max_rows,
        })
    }

    /// The provider registry as it stands.
    pub fn registry(&self) -> Arc<ModelRegistry> {
        match self.registry.read() {
            Ok(guard) => Arc::clone(&guard),
            // A poisoned lock means a panic while a *read* was in progress,
            // which cannot have left the value half-written: the registry is
            // replaced whole. Recovering is therefore correct and refusing
            // would take the models tab down for the life of the process.
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Replace the provider registry — what a module change does (Phase 7).
    pub fn set_registry(&self, registry: Arc<ModelRegistry>) {
        match self.registry.write() {
            Ok(mut guard) => *guard = registry,
            Err(poisoned) => *poisoned.into_inner() = registry,
        }
    }

    /// How a dataset becomes rows here.
    pub fn source(&self) -> Arc<dyn DatasetSource> {
        Arc::clone(&self.source)
    }

    /// The ceiling on one dataset read (`--model-max-rows`).
    pub fn max_rows(&self) -> u64 {
        self.max_rows
    }

    /// **Start a fit** (§8): write the instance row saying `fitting`, and spawn
    /// the work.
    ///
    /// Returns as soon as the row exists, because that is the contract the whole
    /// milestone is built on — a fit reads every row of a dataset and runs an
    /// optimiser over it, which is seconds at best and minutes at worst, and it
    /// must not be an HTTP request a proxy times out halfway through while the
    /// work carries on invisibly. The screen polls the row.
    ///
    /// The spawned task's only failure mode worth handling is "the failure could
    /// not be recorded", which [`fit_model`] reports as `Err`; a fit that simply
    /// did not work is `Ok` carrying a failed instance. So the task logs the
    /// former and nothing else: there is nobody left to return it to.
    pub async fn start_fit(&self, model: &Model, instance: ModelInstance) -> Result<ModelInstance> {
        save_model_instance(&self.catalog, &instance)
            .await
            .context("recording the start of the fit")?;
        let id: InstanceId = instance.id;
        let catalog = Arc::clone(&self.catalog);
        let registry = self.registry();
        let source = Arc::clone(&self.source);
        let model = model.clone();
        let cap = self.max_rows;
        tokio::spawn(async move {
            if let Err(e) = fit_model(&catalog, &registry, source.as_ref(), &model, id, cap).await {
                eprintln!(
                    "feldspar: the fit of model `{}` could not be recorded: {}",
                    model.name,
                    sc_error::format_chain(&e)
                );
            }
        });
        Ok(instance)
    }
}

/// Ensure the three model tables exist, **reap every fit that was running when
/// this process last stopped** (§8), and assemble the services.
///
/// A fit is a job whose registry is its row: `fitModel` writes the instance
/// first, returns its id, and runs the work on a spawned task. Nothing survives
/// a restart, so an instance still saying `fitting` at boot is one nothing will
/// ever finish — and leaving it that way would show an admin a fit in progress
/// that is not. It is failed by name instead, with the sentence saying what
/// happened. Making a fit durable is the workflow engine's job and would mean
/// expressing a fit as a workflow, which is a bigger claim than this milestone
/// makes.
///
/// Runs before anything can read an instance, for that reason.
pub async fn install_models(catalog: &Arc<Catalog>, max_rows: u64) -> Result<ModelServices> {
    bootstrap_models(catalog)
        .await
        .context("ensuring the models table exists")?;
    bootstrap_model_instances(catalog)
        .await
        .context("ensuring the model instances table exists")?;
    bootstrap_model_draws(catalog)
        .await
        .context("ensuring the model draws table exists")?;
    let reaped = reap_fitting_instances(catalog)
        .await
        .context("failing the fits that were running at the last shutdown")?;
    if reaped > 0 {
        eprintln!(
            "feldspar: {reaped} model fit(s) were running when the server last stopped and \
             have been marked failed"
        );
    }
    ModelServices::new(catalog, max_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_key_is_a_reserved_alias_and_not_a_dataset_column_name() {
        // A dataset column called `id` computes whatever its formula says; the
        // key is projected separately so the two cannot be confused.
        assert!(SPLIT_KEY.starts_with("_fd_"));
        let ds = Dataset::new("houses").column("id", "bedrooms");
        assert!(ds.columns.iter().all(|c| c.name != SPLIT_KEY));
    }
}
