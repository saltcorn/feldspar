//! A posterior's draws in `_fd_model_draws`, on **Postgres and SQLite** (Stan
//! TODO Phase 1.5–1.6).
//!
//! What is pinned is the promise the table makes: a fitted instance has *all*
//! of its draws and a failed write leaves *none* of them — the instance still
//! `fitting` — because the draws and the row are one transaction; the reader
//! answers one variable, some elements, some chains, with or without warmup;
//! and deleting an instance or its model takes the draws with it and lets the
//! provider discard what the fit kept elsewhere. Each scenario runs on both
//! backends, because atomicity is a property of the backend's transactions and
//! not of this crate's intentions.

use std::sync::{Arc, Mutex};

use sc_catalog::{Catalog, DataField, SchemaStep};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_model::{
    ATTR_CANCEL_REQUESTED, ATTR_PROGRESS, ChainPhase, ChainProgress, DRAWS_TABLE, Dataset,
    DatasetSource, DrawSeries, DrawsQuery, DrawsReader, FitContext, FitResult, FitStage, FitStatus,
    Frame, Model, ModelInstance, ModelProvider, ModelRegistry, OutcomeSpec, PosteriorInput,
    PosteriorResult, Prediction, Progress, ProgressWrite, Read, bootstrap_model_draws,
    bootstrap_model_instances, bootstrap_models, cancel_requested, delete_model,
    delete_model_instance, fit_model, fitted, record_fit_progress, request_fit_cancel,
    require_model_instance, save_fitted_instance, save_model, save_model_instance,
};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::{Value as Json, json};

/// A sampler whose draws are canned and whose discards are recorded.
struct Sampler {
    discarded: Arc<Mutex<Vec<Json>>>,
}

#[async_trait::async_trait]
impl ModelProvider for Sampler {
    fn name(&self) -> &str {
        "sampler"
    }
    fn description(&self) -> &str {
        "answers canned draws"
    }
    fn config_declaration(&self) -> Vec<FormField> {
        Vec::new()
    }
    fn hyperparameters(&self) -> Vec<FormField> {
        vec![FormField::new("bias", BasicType::Float)]
    }
    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Posterior { prediction: None }
    }
    async fn fit(&self, _f: &Frame, _c: &Attrs, _h: &Attrs) -> Result<FitResult> {
        Err(sc_error::Error::msg("never fitted"))
    }
    async fn predict(&self, _s: &Json, _f: &Frame) -> Result<Vec<Prediction>> {
        Err(sc_error::Error::msg("never predicts"))
    }
    async fn fit_posterior(
        &self,
        input: &PosteriorInput,
        _config: &Attrs,
        _ctx: &FitContext<'_>,
    ) -> Result<PosteriorResult> {
        let rows = input.dataset("main").map_or(0, |f| f.rows);
        Ok(PosteriorResult {
            state: json!({ "run": "stan-runs/radon/1" }),
            draws: canned(),
            parameters: vec![sc_model::ParameterBlock::scalar("rows", rows as f64)],
            run: Default::default(),
        })
    }
    async fn discard(&self, state: &Json) -> Result<()> {
        self.discarded.lock().unwrap().push(state.clone());
        Ok(())
    }
}

/// Two chains of `alpha[1..3]`, one of `Sigma[2,1]`, `lp__` per chain, and one
/// warmup series — with the three values JSON has no number for among them.
fn canned() -> Vec<DrawSeries> {
    let mut out = Vec::new();
    for chain in 1..=2u32 {
        for j in 1..=3usize {
            out.push(DrawSeries::new(
                "alpha",
                vec![j],
                chain,
                vec![j as f64 + f64::from(chain) / 10.0, 0.25],
            ));
        }
        out.push(DrawSeries::new("lp__", vec![], chain, vec![-10.5, -11.0]));
    }
    out.push(DrawSeries::new(
        "Sigma",
        vec![2, 1],
        1,
        vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.0e-300],
    ));
    out.push(DrawSeries::new("alpha", vec![1], 1, vec![5.0, 4.0, 3.0]).warmup());
    out
}

fn registry(discarded: &Arc<Mutex<Vec<Json>>>) -> Result<ModelRegistry> {
    let mut reg = ModelRegistry::new();
    reg.register(Arc::new(Sampler {
        discarded: Arc::clone(discarded),
    }))?;
    Ok(reg)
}

fn radon() -> Model {
    Model::new("radon", "sampler", Dataset::new("homes").column("y", "y"))
}

/// Every model table, and a `homes` table for the model to be over.
async fn prepare(cat: &Catalog) -> Result<()> {
    bootstrap_models(cat).await?;
    bootstrap_model_instances(cat).await?;
    bootstrap_model_draws(cat).await?;
    // A second boot is a no-op, index and all.
    bootstrap_model_draws(cat).await?;
    cat.create_table(
        "homes",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("y", TypeRef::Basic(BasicType::Float)),
        ],
    )
    .await?;
    cat.reload().await?;
    Ok(())
}

async fn postgres(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    prepare(&cat).await?;
    Ok(cat)
}

async fn sqlite() -> Result<Catalog> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let cat = Catalog::init(driver).await?;
    prepare(&cat).await?;
    Ok(cat)
}

/// A fitted instance of `model`, as a fit would leave it.
fn a_fit(model: &Model) -> ModelInstance {
    fitted(
        ModelInstance::starting(model.id),
        json!({ "run": "stan-runs/radon/1" }),
        Vec::new(),
    )
}

// ---------------------------------------------------------------------------
// The round trip.

async fn the_draws_round_trip(cat: &Catalog) -> Result<()> {
    let model = radon();
    let instance = a_fit(&model);
    save_fitted_instance(cat, &instance, &canned()).await?;
    let reader = DrawsReader::new(cat, instance.id);
    assert_eq!(reader.count().await?, 10);

    // One variable: every element of every chain, post-warmup only, ordered
    // by element and then chain.
    let alpha = reader.read(&DrawsQuery::variable("alpha")).await?;
    let order: Vec<(Vec<usize>, u32)> =
        alpha.iter().map(|s| (s.element.clone(), s.chain)).collect();
    assert_eq!(
        order,
        vec![
            (vec![1], 1),
            (vec![1], 2),
            (vec![2], 1),
            (vec![2], 2),
            (vec![3], 1),
            (vec![3], 2)
        ]
    );
    assert_eq!(alpha[5].draws, vec![3.2, 0.25]);

    // Some elements, some chains.
    let some = reader
        .read(
            &DrawsQuery::variable("alpha")
                .elements(vec![vec![2], vec![3]])
                .chains(vec![2]),
        )
        .await?;
    assert_eq!(
        some.iter().map(DrawSeries::label).collect::<Vec<_>>(),
        ["alpha[2]", "alpha[3]"]
    );
    assert!(some.iter().all(|s| s.chain == 2));

    // Warmup only on request, and before the iterations that follow it.
    let with_warmup = reader
        .read(
            &DrawsQuery::variable("alpha")
                .elements(vec![vec![1]])
                .chains(vec![1])
                .with_warmup(),
        )
        .await?;
    assert_eq!(with_warmup.len(), 2);
    assert!(with_warmup[0].warmup && !with_warmup[1].warmup);
    assert_eq!(with_warmup[0].draws, vec![5.0, 4.0, 3.0]);

    // A two-index element, and the values JSON has no number for.
    let sigma = reader
        .read(&DrawsQuery::variable("Sigma").elements(vec![vec![2, 1]]))
        .await?;
    let [sigma] = sigma.as_slice() else {
        panic!("one series of Sigma[2,1], got {sigma:?}");
    };
    assert!(sigma.draws[0].is_nan());
    assert_eq!(
        sigma.draws[1..],
        [f64::INFINITY, f64::NEG_INFINITY, 1.0e-300]
    );

    // A scalar's element is `[]`.
    let lp = reader.read(&DrawsQuery::variable("lp__")).await?;
    assert_eq!(lp.len(), 2);
    assert!(lp.iter().all(|s| s.element.is_empty()));

    // What is not there is an empty answer, not an error.
    assert!(reader.read(&DrawsQuery::variable("beta")).await?.is_empty());
    assert!(
        reader
            .read(&DrawsQuery::variable("alpha").chains(vec![]))
            .await?
            .is_empty()
    );

    // Saving again replaces the draws rather than adding to them.
    save_fitted_instance(cat, &instance, &canned()[..3]).await?;
    assert_eq!(reader.count().await?, 3);
    // And another instance's draws are its own.
    let other = a_fit(&model);
    save_fitted_instance(cat, &other, &canned()).await?;
    assert_eq!(reader.count().await?, 3);
    assert_eq!(DrawsReader::new(cat, other.id).count().await?, 10);
    Ok(())
}

#[tokio::test]
async fn the_draws_round_trip_on_postgres() -> Result<()> {
    let db = TestDb::new().await?;
    the_draws_round_trip(&postgres(&db).await?).await
}

#[tokio::test]
async fn the_draws_round_trip_on_sqlite() -> Result<()> {
    the_draws_round_trip(&sqlite().await?).await
}

// ---------------------------------------------------------------------------
// Atomicity.

/// Save a fit whose last draws batch the database refuses, well after the first
/// batch has gone in, and check nothing of it survives.
async fn a_failed_write_leaves_the_instance_fitting_with_no_draws(
    cat: &Catalog,
    poison: &str,
) -> Result<()> {
    cat.apply_schema_batch(&[SchemaStep::Sql(poison.to_owned())])
        .await?;
    let model = radon();
    let started = ModelInstance::starting(model.id);
    save_model_instance(cat, &started).await?;

    // 600 good series (more than two insert batches) and then the poison.
    let mut draws: Vec<DrawSeries> = (1..=600)
        .map(|j| DrawSeries::new("y_rep", vec![j], 1, vec![0.0; 10]))
        .collect();
    draws.push(DrawSeries::new("poison", vec![], 1, vec![0.0]));
    let finished = fitted(started.clone(), json!({}), Vec::new());
    let err = save_fitted_instance(cat, &finished, &draws)
        .await
        .expect_err("the database refuses the last batch");
    assert!(err.to_string().to_lowercase().contains("poison"), "{err}");

    // The row is as it was — still `fitting` — and not one draw was kept.
    assert_eq!(
        require_model_instance(cat, started.id).await?.status,
        FitStatus::Fitting
    );
    assert_eq!(DrawsReader::new(cat, started.id).count().await?, 0);

    // Draws are refused outright on an instance that is not fitted.
    let err = save_fitted_instance(cat, &started, &draws[..1])
        .await
        .expect_err("draws on a running fit");
    assert!(err.to_string().contains("only a fitted one"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_failed_write_is_atomic_on_postgres() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = postgres(&db).await?;
    a_failed_write_leaves_the_instance_fitting_with_no_draws(
        &cat,
        r#"ALTER TABLE "_fd_model_draws" ADD CONSTRAINT "poison" CHECK ("variable" <> 'poison')"#,
    )
    .await
}

#[tokio::test]
async fn a_failed_write_is_atomic_on_sqlite() -> Result<()> {
    let cat = sqlite().await?;
    a_failed_write_leaves_the_instance_fitting_with_no_draws(
        &cat,
        r#"CREATE TRIGGER "poison" BEFORE INSERT ON "_fd_model_draws"
           WHEN NEW."variable" = 'poison'
           BEGIN SELECT RAISE(ABORT, 'poison'); END"#,
    )
    .await
}

// ---------------------------------------------------------------------------
// Deletion, and the provider's discard.

async fn deleting_takes_the_draws_and_discards_the_run(cat: &Catalog) -> Result<()> {
    let discarded = Arc::new(Mutex::new(Vec::new()));
    let reg = registry(&discarded)?;
    let model = radon();
    save_model(cat, &reg, &model, None).await?;

    let one = a_fit(&model);
    let two = fitted(
        ModelInstance::starting(model.id),
        json!({ "run": "stan-runs/radon/2" }),
        Vec::new(),
    );
    save_fitted_instance(cat, &one, &canned()).await?;
    save_fitted_instance(cat, &two, &canned()).await?;
    // A failed fit has no state, and nothing to discard.
    save_model_instance(cat, &ModelInstance::starting(model.id).failed("no")).await?;

    // One instance: its draws go, the other's stay, and its run is discarded.
    assert!(delete_model_instance(cat, &reg, one.id).await?);
    assert_eq!(DrawsReader::new(cat, one.id).count().await?, 0);
    assert_eq!(DrawsReader::new(cat, two.id).count().await?, 10);
    assert_eq!(
        *discarded.lock().unwrap(),
        vec![json!({ "run": "stan-runs/radon/1" })]
    );

    // The model: every instance's draws, and every remaining run.
    assert!(delete_model(cat, &reg, model.id).await?);
    assert_eq!(DrawsReader::new(cat, two.id).count().await?, 0);
    assert_eq!(
        *discarded.lock().unwrap(),
        vec![
            json!({ "run": "stan-runs/radon/1" }),
            json!({ "run": "stan-runs/radon/2" })
        ]
    );
    Ok(())
}

#[tokio::test]
async fn deleting_takes_the_draws_on_postgres() -> Result<()> {
    let db = TestDb::new().await?;
    deleting_takes_the_draws_and_discards_the_run(&postgres(&db).await?).await
}

#[tokio::test]
async fn deleting_takes_the_draws_on_sqlite() -> Result<()> {
    deleting_takes_the_draws_and_discards_the_run(&sqlite().await?).await
}

// ---------------------------------------------------------------------------
// The job, end to end.

/// Twelve homes, whatever is asked.
struct Homes;

#[async_trait::async_trait]
impl DatasetSource for Homes {
    async fn read(&self, _ds: &Dataset, _how: &Read<'_>) -> Result<Frame> {
        Frame::new(
            vec![(
                "y".to_owned(),
                sc_model::Column::Float((0..12).map(|i| Some(f64::from(i))).collect()),
            )],
            (0..12).map(|i| format!("int:{i}")).collect(),
        )
    }
}

async fn a_posterior_fit_job_stores_its_draws_with_its_row(cat: &Catalog) -> Result<()> {
    let discarded = Arc::new(Mutex::new(Vec::new()));
    let reg = registry(&discarded)?;
    let model = radon();
    save_model(cat, &reg, &model, None).await?;
    let started = ModelInstance::starting(model.id);
    save_model_instance(cat, &started).await?;

    let finished = fit_model(cat, &reg, &Homes, &model, started.id, 1000).await?;
    assert_eq!(finished.status, FitStatus::Fitted, "{:?}", finished.error());
    let stored = require_model_instance(cat, started.id).await?;
    assert_eq!(stored.status, FitStatus::Fitted);
    assert_eq!(stored.attributes["outcome"]["outcome"], "posterior");
    // The host's summary tables, one per variable, then the provider's own.
    let names: Vec<&str> = stored.parameters.iter().map(|p| p.name()).collect();
    assert_eq!(names, ["alpha", "Sigma", "rows"]);
    assert_eq!(stored.metrics["train"]["metrics"], "posterior");
    assert_eq!(stored.metrics["train"]["chains"], 2);
    assert_eq!(DrawsReader::new(cat, started.id).count().await?, 10);
    Ok(())
}

#[tokio::test]
async fn a_posterior_fit_job_stores_its_draws_on_postgres() -> Result<()> {
    let db = TestDb::new().await?;
    a_posterior_fit_job_stores_its_draws_with_its_row(&postgres(&db).await?).await
}

#[tokio::test]
async fn a_posterior_fit_job_stores_its_draws_on_sqlite() -> Result<()> {
    a_posterior_fit_job_stores_its_draws_with_its_row(&sqlite().await?).await
}

#[tokio::test]
async fn a_posterior_model_with_a_grid_is_refused_on_save() -> Result<()> {
    let cat = sqlite().await?;
    let reg = registry(&Arc::new(Mutex::new(Vec::new())))?;
    let model = radon().hyperparameter("bias", json!([0.0, 1.0]));
    let err = save_model(&cat, &reg, &model, None)
        .await
        .expect_err("a grid over a posterior");
    assert!(err.to_string().contains("sampled, not searched"), "{err}");
    assert!(cat.get(DRAWS_TABLE)?.is_some());
    Ok(())
}

/// A Stan program's data is bound from rows of tables, so a dataset that
/// changes the grain is refused on save, naming what a row is (analytics TODO
/// A1.9).
#[tokio::test]
async fn a_posterior_over_a_dataset_that_changes_the_grain_is_refused() -> Result<()> {
    /// A provider whose program's data is bound, like Stan's, and which never
    /// gets as far as sampling.
    struct Binder;
    #[async_trait::async_trait]
    impl ModelProvider for Binder {
        fn name(&self) -> &str {
            "binder"
        }
        fn description(&self) -> &str {
            "binds data"
        }
        fn config_declaration(&self) -> Vec<FormField> {
            Vec::new()
        }
        fn outcome_spec(&self) -> OutcomeSpec {
            OutcomeSpec::Posterior { prediction: None }
        }
        fn binds_data(&self) -> bool {
            true
        }
        async fn fit(&self, _f: &Frame, _c: &Attrs, _h: &Attrs) -> Result<FitResult> {
            Err(sc_error::Error::msg("never fitted"))
        }
        async fn predict(&self, _s: &Json, _f: &Frame) -> Result<Vec<Prediction>> {
            Err(sc_error::Error::msg("never predicts"))
        }
    }
    let cat = sqlite().await?;
    let mut reg = ModelRegistry::new();
    reg.register(Arc::new(Binder))?;
    let grouped = sc_dataset::DatasetDef::over_table("by y", "homes").then(
        sc_dataset::Op::Aggregate(sc_dataset::AggregateOp {
            group_by: vec![sc_dataset::GroupKey::column("y")],
            summaries: vec![sc_dataset::Summary::count("n")],
        }),
    );
    sc_dataset::save_dataset(&cat, &grouped).await?;
    let schema = sc_dataset::Schema::of_catalog(&cat)?;
    let library = sc_dataset::load_library(&cat).await?;
    let model = Model::new(
        "grouped",
        "binder",
        Dataset::resolve(&schema, &library, grouped.id),
    );
    let err = save_model(&cat, &reg, &model, None)
        .await
        .expect_err("a grouped dataset under a posterior");
    assert!(
        err.to_string().contains("bound from rows of tables")
            && err
                .to_string()
                .contains("each row is one combination of `y`"),
        "{err}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Progress and cancel go through the row (Stan TODO 4.3).

fn sampling(iteration: u64) -> Progress {
    Progress {
        stage: FitStage::Sampling,
        chains: vec![ChainProgress {
            chain: 1,
            iteration,
            total: 2000,
            phase: ChainPhase::Warmup,
        }],
    }
}

async fn progress_and_cancel_go_through_the_row(cat: &Catalog) -> Result<()> {
    let model = radon();
    let running = ModelInstance::starting(model.id);
    save_model_instance(cat, &running).await?;

    // Progress is written, and only read when there is none to write.
    assert_eq!(
        record_fit_progress(cat, running.id, Some(&sampling(400))).await?,
        ProgressWrite::Running
    );
    assert_eq!(
        record_fit_progress(cat, running.id, None).await?,
        ProgressWrite::Running
    );
    let row = require_model_instance(cat, running.id).await?;
    assert_eq!(
        row.attributes[ATTR_PROGRESS],
        serde_json::to_value(sampling(400)).unwrap()
    );
    assert_eq!(row.status, FitStatus::Fitting);

    // A cancel is set on the row and read back by the next write, which
    // then leaves the row alone.
    assert!(request_fit_cancel(cat, running.id).await?);
    let row = require_model_instance(cat, running.id).await?;
    assert!(cancel_requested(&row));
    assert_eq!(row.attributes[ATTR_CANCEL_REQUESTED], true);
    assert_eq!(
        record_fit_progress(cat, running.id, Some(&sampling(800))).await?,
        ProgressWrite::CancelRequested
    );
    let row = require_model_instance(cat, running.id).await?;
    assert_eq!(
        row.attributes[ATTR_PROGRESS],
        serde_json::to_value(sampling(400)).unwrap()
    );
    assert!(cancel_requested(&row));

    // A finished fit is neither written to nor cancelled.
    save_model_instance(cat, &running.clone().failed("the fit was cancelled")).await?;
    assert_eq!(
        record_fit_progress(cat, running.id, Some(&sampling(900))).await?,
        ProgressWrite::Finished
    );
    assert!(!request_fit_cancel(cat, running.id).await?);
    let row = require_model_instance(cat, running.id).await?;
    assert_eq!(row.status, FitStatus::Failed);
    assert!(!row.attributes.contains_key(ATTR_PROGRESS));
    Ok(())
}

#[tokio::test]
async fn progress_and_cancel_go_through_the_row_on_postgres() -> Result<()> {
    let db = TestDb::new().await?;
    progress_and_cancel_go_through_the_row(&postgres(&db).await?).await
}

#[tokio::test]
async fn progress_and_cancel_go_through_the_row_on_sqlite() -> Result<()> {
    progress_and_cancel_go_through_the_row(&sqlite().await?).await
}
