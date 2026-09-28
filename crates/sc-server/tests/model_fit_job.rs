//! A posterior fit as a job whose row is its registry (Stan TODO 4.3): the
//! provider's progress reaches the instance's `attributes.progress` (at most
//! once a second), and a cancel set on the **row** — which is what
//! `cancelModelFit` will do, from any node — is read back by the job and
//! honoured by the provider, and the instance fails saying so.
//!
//! The provider is a stub that samples nothing: what is under test is the
//! server's half, the upkeep of the row. The provider's half — killing its
//! chains when asked — is `sc-stan`'s `runner.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_model::{
    ATTR_ERROR, ATTR_PROGRESS, ChainPhase, ChainProgress, Dataset, FitContext, FitResult, FitStage,
    FitStatus, Frame, Model, ModelInstance, ModelProvider, OutcomeSpec, PosteriorInput,
    PosteriorResult, Prediction, Progress, request_fit_cancel, require_model_instance,
};
use sc_test_harness::TestDb;
use sc_types::{Attrs, FormField};
use serde_json::Value as Json;

/// A sampler that reports an iteration every 20 ms until it is cancelled —
/// or, after 30 seconds, gives up, which the test would notice.
struct Endless;

#[async_trait::async_trait]
impl ModelProvider for Endless {
    fn name(&self) -> &str {
        "endless"
    }
    fn description(&self) -> &str {
        "samples until it is cancelled"
    }
    fn config_declaration(&self) -> Vec<FormField> {
        Vec::new()
    }
    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Posterior { prediction: None }
    }
    async fn fit(&self, _f: &Frame, _c: &Attrs, _h: &Attrs) -> Result<FitResult> {
        Err(Error::msg("never fitted"))
    }
    async fn predict(&self, _s: &Json, _f: &Frame) -> Result<Vec<Prediction>> {
        Err(Error::msg("never predicts"))
    }
    async fn fit_posterior(
        &self,
        _input: &PosteriorInput,
        _config: &Attrs,
        ctx: &FitContext<'_>,
    ) -> Result<PosteriorResult> {
        let started = Instant::now();
        let mut iteration = 0;
        while started.elapsed() < Duration::from_secs(30) {
            if ctx.cancelled() {
                return Err(Error::msg("the fit was cancelled"));
            }
            iteration += 1;
            ctx.report(&Progress {
                stage: FitStage::Sampling,
                chains: vec![ChainProgress {
                    chain: 1,
                    iteration,
                    total: 1_000_000,
                    phase: ChainPhase::Warmup,
                }],
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(PosteriorResult::default())
    }
}

async fn eventually(
    catalog: &Catalog,
    instance: &ModelInstance,
    what: &str,
    ok: impl Fn(&ModelInstance) -> bool,
) -> ModelInstance {
    let started = Instant::now();
    loop {
        let row = require_model_instance(catalog, instance.id).await.unwrap();
        if ok(&row) {
            return row;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{what} did not happen: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_running_fit_reports_progress_to_its_row_and_stops_when_the_row_says_so() -> Result<()> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE homes (id bigint primary key, y numeric);
             INSERT INTO homes VALUES (1, 1.5), (2, 2.5);",
        )
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    catalog.reload().await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let mut registry = models.base_registry()?;
    registry.register(Arc::new(Endless))?;
    models.set_registry(Arc::new(registry));

    let model = Model::new("endless", "endless", Dataset::new("homes").column("y", "y"));
    let instance = models
        .start_fit(&model, ModelInstance::starting(model.id))
        .await?;

    // Progress reaches the row — the latest report, not every one of them.
    let row = eventually(&catalog, &instance, "a progress write", |row| {
        row.attributes.contains_key(ATTR_PROGRESS)
    })
    .await;
    assert_eq!(row.status, FitStatus::Fitting);
    let progress: Progress = serde_json::from_value(row.attributes[ATTR_PROGRESS].clone()).unwrap();
    assert_eq!(progress.stage, FitStage::Sampling);
    assert!(progress.chains[0].iteration > 10, "{progress:?}");

    // A cancel on the row stops the fit, which fails saying why.
    let asked = Instant::now();
    assert!(request_fit_cancel(&catalog, instance.id).await?);
    let row = eventually(&catalog, &instance, "the cancel", |row| {
        row.status != FitStatus::Fitting
    })
    .await;
    assert!(asked.elapsed() < Duration::from_secs(5));
    assert_eq!(row.status, FitStatus::Failed);
    let error = row.attributes[ATTR_ERROR].as_str().unwrap_or_default();
    assert!(error.contains("the fit was cancelled"), "{error}");
    // The finished row is the job's final save: no progress left on it, and a
    // late write of the upkeep cannot put one back.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let row = require_model_instance(&catalog, instance.id).await?;
    assert_eq!(row.status, FitStatus::Failed);
    assert!(!row.attributes.contains_key(ATTR_PROGRESS));
    Ok(())
}
