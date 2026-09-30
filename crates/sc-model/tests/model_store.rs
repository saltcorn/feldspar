//! `_fd_models` and `_fd_model_instances` against a **real Postgres**
//! (principle 4): the rows *are* the definition and the fit, so what a save
//! writes and a load reads back is the whole of whether a configured model — and
//! an hour of fitting — survives a restart.
//!
//! Validation is tested here rather than in a unit test because it is not a
//! property of the [`Model`] value: whether that table exists, whether that
//! column still resolves, whether that table has a primary key to split on are
//! questions about the world the model is stored in, and only real against a
//! real catalog.

use std::sync::Arc;

use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_model::{
    Dataset, DatasetOrder, FitResult, FitStatus, Frame, INSTANCES_TABLE, MODELS_TABLE, Model,
    ModelInstance, ModelProvider, ModelProviderKind, ModelRegistry, Models, NamedDataset,
    OutcomeSpec, ParameterBlock, ParameterRow, Prediction, RESTARTED, Split, active_model_instance,
    bootstrap_model_instances, bootstrap_models, delete_model, delete_model_instance,
    list_model_instances, list_models, load_model, load_model_by_name, models_for_table,
    numeric_column_field, reap_fitting_instances, require_model, save_model, save_model_instance,
};
use sc_query::{Assignment, Expr, Statement, Update, Value};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::{Value as Json, json};

/// A catalog over a per-test database with both model tables bootstrapped and a
/// `houses` table for datasets to be written over.
async fn setup(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_models(&cat).await?;
    bootstrap_model_instances(&cat).await?;
    cat.create_table(
        "houses",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("price", TypeRef::Basic(BasicType::Float)),
            DataField::plain("bedrooms", TypeRef::Basic(BasicType::Int)),
            DataField::plain("area", TypeRef::Basic(BasicType::Float)),
            DataField::plain("sold", TypeRef::Basic(BasicType::Bool)),
        ],
    )
    .await?;
    Ok(cat)
}

/// A provider with one required setting and one hyperparameter, so a
/// configuration and a grid can each be right or wrong.
struct Ols;

#[async_trait::async_trait]
impl ModelProvider for Ols {
    fn name(&self) -> &str {
        "linear_regression"
    }
    fn description(&self) -> &str {
        "Ordinary least squares"
    }
    fn config_declaration(&self) -> Vec<FormField> {
        vec![numeric_column_field("label", "Label").required()]
    }
    fn hyperparameters(&self) -> Vec<FormField> {
        vec![FormField::new("ridge", BasicType::Float)]
    }
    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Regression {
            label: "label".to_owned(),
        }
    }
    async fn fit(&self, _f: &Frame, _c: &Attrs, _h: &Attrs) -> Result<FitResult> {
        Ok(FitResult::new(Json::Null))
    }
    async fn predict(&self, _s: &Json, _f: &Frame) -> Result<Vec<Prediction>> {
        Ok(Vec::new())
    }
}

fn registry() -> Result<ModelRegistry> {
    let mut reg = ModelRegistry::new();
    reg.register(Arc::new(Ols))?;
    Ok(reg)
}

/// The milestone's own example, minus the join and the aggregation (this
/// database has one table).
fn house_prices() -> Model {
    Model::new(
        "house prices",
        "linear_regression",
        Dataset::new("houses")
            .column("price", "price")
            .column("bedrooms", "bedrooms")
            .column("per_room", "area / bedrooms")
            .filtered("sold === true"),
    )
    .description("what a house goes for")
    .config("label", "price")
    .hyperparameter("ridge", 0.1)
    .split(Split::new(0.6, 0.2, 0.2, 42))
    .attribute("note", "fitted from the tutorial")
}

#[tokio::test]
async fn a_model_round_trips_through_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;

    let loaded = load_model(&cat, model.id)
        .await?
        .expect("the row that was just written");
    assert_same(&loaded, &model);

    // And by name, which is how `predict_row` resolves it.
    assert_same(
        &load_model_by_name(&cat, "house prices")
            .await?
            .expect("by name"),
        &model,
    );
    assert_same(&require_model(&cat, "house prices").await?, &model);

    // The dataset is a named one now, stored beside the model and resolved
    // with it: the columns in order, and the formula that is not a bare field
    // name.
    assert_eq!(loaded.dataset.columns.len(), 3);
    assert_eq!(loaded.dataset.columns[2].expr, "area / bedrooms");
    let def = sc_dataset::load_dataset(&cat, model.dataset.id)
        .await?
        .expect("the dataset was saved as a named one");
    assert!(
        def.operations
            .iter()
            .any(|o| o.op == sc_dataset::Op::filter("sold === true"))
    );
    // As does the split, which is what makes two instances comparable.
    assert_eq!(loaded.split, Split::new(0.6, 0.2, 0.2, 42));
    assert_eq!(loaded.attributes["note"], json!("fitted from the tutorial"));
    Ok(())
}

#[tokio::test]
async fn the_table_column_is_derived_so_the_models_on_a_table_can_be_asked_for() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    save_model(&cat, &reg, &house_prices(), None).await?;

    let on_houses = models_for_table(&cat, "houses").await?;
    assert_eq!(on_houses.len(), 1);
    assert_eq!(on_houses[0].table(), "houses");
    assert!(models_for_table(&cat, "books").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn an_edit_updates_in_place_and_a_delete_removes_it() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    let mut model = house_prices();
    save_model(&cat, &reg, &model, None).await?;
    model.description = "revised".to_owned();
    model.dataset = model.dataset.clone().column("area", "area");
    save_model(&cat, &reg, &model, None).await?;

    assert_eq!(list_models(&cat).await?.len(), 1);
    let loaded = require_model(&cat, "house prices").await?;
    assert_eq!(loaded.description, "revised");
    assert_eq!(loaded.dataset.columns.len(), 4);

    assert!(delete_model(&cat, &reg, model.id).await?);
    assert!(!delete_model(&cat, &reg, model.id).await?);
    assert!(list_models(&cat).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn two_models_cannot_claim_one_name() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    save_model(&cat, &reg, &house_prices(), None).await?;

    let clash = Model::new(
        "house prices",
        "linear_regression",
        Dataset::new("houses").column("price", "price"),
    )
    .config("label", "price");
    let msg = save_model(&cat, &reg, &clash, None)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(msg.contains("already used"), "{msg}");
    Ok(())
}

#[tokio::test]
async fn every_way_a_model_cannot_be_fitted_is_refused_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;

    /// The sentence saving this model is refused with.
    async fn refused(cat: &Catalog, reg: &ModelRegistry, model: Model) -> String {
        save_model(cat, reg, &model, None)
            .await
            .expect_err("this model should not save")
            .to_string()
    }

    // A provider nothing implements.
    let mut model = house_prices();
    model.provider = "sklearn_gbm".to_owned();
    let msg = refused(&cat, &reg, model).await;
    assert!(
        msg.contains("sklearn_gbm") && msg.contains("linear_regression"),
        "{msg}"
    );

    // A dataset column whose formula names a field that is not there.
    let mut model = house_prices();
    model.dataset = Dataset::new("houses").column("x", "no_such_field");
    let msg = refused(&cat, &reg, model).await;
    assert!(msg.contains("no_such_field"), "{msg}");

    // `user` in a dataset formula: a dataset has no caller (§2).
    let mut model = house_prices();
    model.dataset = Dataset::new("houses").column("x", "user.id");
    let msg = refused(&cat, &reg, model).await;
    assert!(msg.contains("user"), "{msg}");

    // A table this database does not have.
    let mut model = house_prices();
    model.dataset = Dataset::new("flats").column("price", "price");
    let msg = refused(&cat, &reg, model).await;
    assert!(msg.contains("flats"), "{msg}");

    // Split fractions that do not sum to 1 — an instance would otherwise say it
    // held out a different fraction than it did.
    let mut model = house_prices();
    model.split = Split::new(0.6, 0.2, 0.4, 0);
    let msg = refused(&cat, &reg, model).await;
    assert!(msg.contains("must sum to 1"), "{msg}");

    // A hyperparameter the provider does not declare, and one whose list holds
    // the wrong type.
    let msg = refused(&cat, &reg, house_prices().hyperparameter("n_trees", 100)).await;
    assert!(msg.contains("n_trees") && msg.contains("ridge"), "{msg}");
    let msg = refused(
        &cat,
        &reg,
        house_prices().hyperparameter("ridge", json!([0.1, "lots"])),
    )
    .await;
    assert!(msg.contains("ridge"), "{msg}");
    let msg = refused(
        &cat,
        &reg,
        house_prices().hyperparameter("ridge", json!([])),
    )
    .await;
    assert!(msg.contains("nothing to search over"), "{msg}");

    // A setting the provider does not declare.
    let msg = refused(&cat, &reg, house_prices().config("penalty", "l2")).await;
    assert!(msg.contains("penalty"), "{msg}");

    // No name at all.
    let mut model = house_prices();
    model.name = "  ".to_owned();
    let msg = refused(&cat, &reg, model).await;
    assert!(msg.contains("needs a name"), "{msg}");
    Ok(())
}

#[tokio::test]
async fn a_table_with_no_single_primary_key_cannot_be_split_and_says_so() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    cat.create_table(
        "readings",
        &[
            DataField::plain("sensor", TypeRef::Basic(BasicType::Text)),
            DataField::plain("value", TypeRef::Basic(BasicType::Float)),
        ],
    )
    .await?;

    let model = Model::new(
        "sensor",
        "linear_regression",
        Dataset::new("readings").column("value", "value"),
    )
    .config("label", "value");
    let msg = save_model(&cat, &reg, &model, None)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(msg.contains("nothing stable to hash"), "{msg}");
    Ok(())
}

#[tokio::test]
async fn a_mangled_column_is_reported_and_not_defaulted() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;

    // The derived table column edited out from under the dataset — a
    // hand-edited row, or a bug in something else that writes here. Reading it
    // either way would make one of "which models are on `houses`" and "what is
    // this fitted over" answer wrong.
    let update = Update::new(
        MODELS_TABLE,
        vec![Assignment::new("table_name".to_owned(), Expr::lit("flats"))],
    )
    .filter(Expr::col("id").eq(Expr::Lit(Value::Uuid(model.id.0))));
    cat.primary().query(&Statement::from(update)).await?;

    let err = list_models(&cat).await.err().unwrap().to_string();
    assert!(err.contains("house prices"), "{err}");
    assert!(err.contains("flats") && err.contains("houses"), "{err}");

    // And a dataset that is not a dataset at all.
    let update = Update::new(
        MODELS_TABLE,
        vec![
            Assignment::new("table_name".to_owned(), Expr::lit("houses")),
            Assignment::new(
                "dataset".to_owned(),
                Expr::Lit(Value::Json(
                    json!({"table": "houses", "columns": "all of them"}),
                )),
            ),
        ],
    )
    .filter(Expr::col("id").eq(Expr::Lit(Value::Uuid(model.id.0))));
    cat.primary().query(&Statement::from(update)).await?;

    let err = list_models(&cat).await.err().unwrap().to_string();
    assert!(
        err.contains("house prices") && err.contains("dataset"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn a_model_that_stopped_validating_stays_listed_with_its_reason() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    save_model(&cat, &reg, &house_prices(), None).await?;

    let live = Models::load(&cat, &reg).await?;
    assert_eq!(live.all().len(), 1);
    assert!(live.issues().is_empty());
    assert_eq!(live.require("house prices")?.table(), "houses");

    // The module supplying the provider is uninstalled: the model is still
    // stored, still listed and still editable — editing it is the repair.
    let empty = ModelRegistry::new();
    let live = Models::load(&cat, &empty).await?;
    assert!(live.all().is_empty());
    assert_eq!(live.issues().len(), 1);
    assert!(
        live.issues()[0].problem.contains("linear_regression"),
        "{:?}",
        live.issues()
    );
    assert_eq!(list_models(&cat).await?.len(), 1);

    // And the two ways of being missing read differently.
    let msg = live.require("house prices").err().unwrap().to_string();
    assert!(msg.contains("not usable"), "{msg}");
    let msg = live.require("flats").err().unwrap().to_string();
    assert!(msg.contains("no model named"), "{msg}");
    Ok(())
}

/// A finished fit of `model`, with a coefficient table on it.
fn a_fit(model: &Model) -> Result<ModelInstance> {
    let mut instance = ModelInstance::starting(model.id).name("first");
    instance.status = FitStatus::Fitted;
    instance.state = json!({"coefficients": [1.5, -0.25]});
    instance.parameters = vec![
        ParameterBlock::scalar("intercept", 12.0),
        ParameterBlock::table(
            "Coefficients",
            ["term", "estimate", "p"],
            vec![ParameterRow::new([
                json!("bedrooms"),
                json!(1.5),
                json!(0.01),
            ])],
        )?,
    ];
    instance.metrics = json!({"test": {"r2": 0.81}});
    instance.encoding = json!({"columns": []});
    instance
        .hyperparameters
        .insert("ridge".to_owned(), json!(0.1));
    Ok(instance)
}

#[tokio::test]
async fn an_instance_round_trips_through_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;

    let instance = a_fit(&model)?;
    save_model_instance(&cat, &instance).await?;

    let loaded = list_model_instances(&cat, model.id).await?;
    assert_eq!(loaded.len(), 1);
    // The whole of a fit, including the structured parameters that are what an
    // admin actually looks at. The creation instant is compared separately
    // because `timestamptz` keeps microseconds and `Utc::now()` has
    // nanoseconds: the row is the authority on what was stored, and losing the
    // last three digits of an instant nothing sorts finer than is not a defect
    // worth a column type to avoid.
    let mut expected = instance.clone();
    assert!((loaded[0].created - instance.created).num_microseconds() == Some(0));
    expected.created = loaded[0].created;
    assert_eq!(loaded[0], expected);
    assert_eq!(loaded[0].parameters[1].name(), "Coefficients");
    assert_eq!(loaded[0].hyperparameters["ridge"], json!(0.1));
    assert!(loaded[0].is_usable());
    Ok(())
}

#[tokio::test]
async fn at_most_one_instance_per_model_is_active() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;

    let first = a_fit(&model)?.activated();
    save_model_instance(&cat, &first).await?;
    assert_eq!(
        active_model_instance(&cat, model.id).await?.map(|i| i.id),
        Some(first.id)
    );

    // Refitting and activating the new one deactivates the old, so a trigger
    // naming the *model* follows without being edited.
    let second = a_fit(&model)?.name("second").activated();
    save_model_instance(&cat, &second).await?;
    let active = active_model_instance(&cat, model.id).await?.expect("one");
    assert_eq!(active.id, second.id);
    let all = list_model_instances(&cat, model.id).await?;
    assert_eq!(all.len(), 2);
    assert_eq!(all.iter().filter(|i| i.active).count(), 1);

    // A fit that has not finished cannot be the one a trigger predicts with.
    let running = ModelInstance::starting(model.id).activated();
    let msg = save_model_instance(&cat, &running)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(msg.contains("fitting"), "{msg}");
    Ok(())
}

#[tokio::test]
async fn boot_fails_every_fit_that_was_running_when_the_server_died() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;

    let stranded = ModelInstance::starting(model.id).name("interrupted");
    save_model_instance(&cat, &stranded).await?;
    let finished = a_fit(&model)?;
    save_model_instance(&cat, &finished).await?;

    assert_eq!(reap_fitting_instances(&cat).await?, 1);

    let all = list_model_instances(&cat, model.id).await?;
    let reaped = all.iter().find(|i| i.id == stranded.id).expect("stranded");
    assert_eq!(reaped.status, FitStatus::Failed);
    assert_eq!(reaped.error(), Some(RESTARTED));
    // The one that finished is untouched.
    let kept = all.iter().find(|i| i.id == finished.id).expect("finished");
    assert_eq!(kept.status, FitStatus::Fitted);
    // And it is idempotent: a second boot finds nothing to reap.
    assert_eq!(reap_fitting_instances(&cat).await?, 0);
    Ok(())
}

#[tokio::test]
async fn deleting_a_model_takes_its_instances_with_it() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;
    let instance = a_fit(&model)?;
    save_model_instance(&cat, &instance).await?;

    assert!(delete_model(&cat, &reg, model.id).await?);
    // An instance whose model is gone is a row nothing can list, read or apply
    // — unlike a run, which is a transcript of something that happened.
    assert!(list_model_instances(&cat, model.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn an_instance_can_be_deleted_on_its_own() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;
    let instance = a_fit(&model)?;
    save_model_instance(&cat, &instance).await?;

    assert!(delete_model_instance(&cat, &reg, instance.id).await?);
    assert!(!delete_model_instance(&cat, &reg, instance.id).await?);
    assert!(list_model_instances(&cat, model.id).await?.is_empty());
    assert!(load_model(&cat, model.id).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn an_instance_with_an_unreadable_status_is_refused_rather_than_guessed_at() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;
    let instance = a_fit(&model)?;
    save_model_instance(&cat, &instance).await?;

    let update = Update::new(
        INSTANCES_TABLE,
        vec![Assignment::new("status".to_owned(), Expr::lit("done"))],
    )
    .filter(Expr::col("id").eq(Expr::Lit(Value::Uuid(instance.id.0))));
    cat.primary().query(&Statement::from(update)).await?;

    // "Probably failed" and "probably fitted" differ in whether this row may
    // serve a prediction, so neither is guessed.
    let err = list_model_instances(&cat, model.id)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("done"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_modules_provider_is_saved_against_like_any_other() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;

    /// A host supplying one estimator, the way `plugins/sklearn` will.
    struct Sklearn;
    #[async_trait::async_trait]
    impl sc_model::ModelProviderHost for Sklearn {
        fn providers(&self) -> Vec<ModelProviderKind> {
            vec![
                ModelProviderKind::new(
                    "gradient_boosting",
                    "scikit-learn's gradient boosting",
                    OutcomeSpec::Supervised {
                        label: "label".to_owned(),
                    },
                )
                .module("@saltcorn/sklearn")
                .config(vec![numeric_column_field("label", "Label").required()]),
            ]
        }
        async fn fit(
            &self,
            _m: &str,
            _p: &str,
            _f: &Frame,
            _c: &Attrs,
            _h: &Attrs,
        ) -> Result<FitResult> {
            Ok(FitResult::new(Json::Null))
        }
        async fn predict(
            &self,
            _m: &str,
            _p: &str,
            _s: &Json,
            _f: &Frame,
        ) -> Result<Vec<Prediction>> {
            Ok(Vec::new())
        }
    }

    let mut reg = registry()?;
    reg.register_host(Arc::new(Sklearn))?;

    let model = Model::new(
        "house prices, boosted",
        "gradient_boosting",
        Dataset::new("houses").column("price", "price"),
    )
    .config("label", "price");
    save_model(&cat, &reg, &model, None).await?;
    // Nothing in the store knows that this one's provider lives in Python.
    assert_eq!(
        require_model(&cat, "house prices, boosted").await?.provider,
        "gradient_boosting"
    );
    assert_eq!(Models::load(&cat, &reg).await?.all().len(), 1);
    Ok(())
}

/// Two models store the same thing: every field of the row, the dataset by
/// its id. (A model built in a test carries its dataset's formulas; one loaded
/// carries the dataset resolved, so they are not `==`.)
#[track_caller]
fn assert_same(a: &Model, b: &Model) {
    assert_eq!(a.id, b.id);
    assert_eq!(a.name, b.name);
    assert_eq!(a.description, b.description);
    assert_eq!(a.provider, b.provider);
    assert_eq!(a.dataset.id, b.dataset.id);
    assert_eq!(a.configuration, b.configuration);
    assert_eq!(a.hyperparameters, b.hyperparameters);
    assert_eq!(a.split, b.split);
    assert_eq!(a.attributes, b.attributes);
    let related = |m: &Model| -> Vec<(String, sc_dataset::DatasetId, Option<String>)> {
        m.related
            .iter()
            .map(|r| (r.name.clone(), r.dataset.id, r.label.clone()))
            .collect()
    };
    assert_eq!(related(a), related(b));
}

/// A `counties` table beside `houses`, for related datasets to be over.
async fn with_counties(cat: &Catalog) -> Result<()> {
    cat.create_table(
        "counties",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("name", TypeRef::Basic(BasicType::Text)),
            DataField::plain("log_uranium", TypeRef::Basic(BasicType::Float)),
        ],
    )
    .await?;
    cat.reload().await
}

fn counties() -> NamedDataset {
    NamedDataset::new(
        "counties",
        Dataset::new("counties")
            .column("u", "log_uranium")
            .ordered(DatasetOrder::asc("name")),
    )
    .labelled("name")
}

#[tokio::test]
async fn related_datasets_round_trip_and_validate_against_their_own_tables() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    with_counties(&cat).await?;
    let reg = registry()?;

    let model = house_prices().related(counties());
    save_model(&cat, &reg, &model, None).await?;
    let loaded = load_model(&cat, model.id).await?.expect("stored");
    assert_eq!(loaded.related.len(), 1);
    assert_eq!(loaded.related[0].name, "counties");
    assert_eq!(loaded.related[0].label.as_deref(), Some("name"));
    assert_eq!(loaded.related[0].dataset.columns[0].name, "u");
    assert_same(&loaded, &model);

    // Each way a related dataset can be wrong, refused by name.
    let refused = |model: Model, expected: &'static str| {
        let cat = &cat;
        let reg = &reg;
        async move {
            let err = save_model(cat, reg, &model, None)
                .await
                .expect_err(expected);
            assert!(err.to_string().contains(expected), "{err}");
        }
    };
    let rename = |name: &str| {
        let mut related = counties();
        related.name = name.to_owned();
        related
    };
    refused(
        house_prices().related(rename("main")),
        "cannot be called `main`",
    )
    .await;
    refused(
        house_prices().related(rename("2nd")),
        "must be an identifier",
    )
    .await;
    refused(
        house_prices().related(counties()).related(counties()),
        "two related datasets are called `counties`",
    )
    .await;
    refused(
        house_prices().related(counties().labelled("no_such_field")),
        "the label `no_such_field`",
    )
    .await;
    // Validated against its *own* table: `price` is a column of houses, not
    // of counties.
    refused(
        house_prices().related(NamedDataset::new(
            "counties",
            Dataset::new("counties").column("p", "price"),
        )),
        "unknown identifier `price`",
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn an_existing_models_table_gains_the_related_column_on_boot() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let reg = registry()?;
    let model = house_prices();
    save_model(&cat, &reg, &model, None).await?;

    // An installation from before related datasets: the column is not there,
    // and the table has rows.
    cat.primary()
        .query(&Statement::raw(
            r#"ALTER TABLE "_fd_models" DROP COLUMN "related""#,
            Vec::new(),
        ))
        .await?
        .try_collect()
        .await?;
    cat.reload().await?;
    assert!(cat.require(MODELS_TABLE)?.field("related").is_none());
    // Read before the boot has caught up: the absent column is "none".
    assert!(
        load_model(&cat, model.id)
            .await?
            .expect("stored")
            .related
            .is_empty()
    );

    // Boot adds it — nullable, so a table with rows can take it …
    bootstrap_models(&cat).await?;
    assert!(cat.require(MODELS_TABLE)?.field("related").is_some());
    // … and the old row reads back with no related datasets.
    let loaded = load_model(&cat, model.id).await?.expect("stored");
    assert!(loaded.related.is_empty());
    assert_same(&loaded, &model);
    Ok(())
}
