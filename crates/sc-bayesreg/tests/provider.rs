//! R1.7 without CmdStan: the provider's configuration, its lowering into the
//! generated program and bindings, and the host binding that lowering exactly
//! as it binds a hand-written Stan model.

use std::sync::Arc;

use sc_bayesreg::{BayesRegProvider, keys};
use sc_model::{
    Column, Dataset, DatasetShape, Frame, LowerInput, MAIN_DATASET, Model, ModelProvider,
    NamedDataset, bind_data,
};
use sc_stan::StanProvider;
use sc_types::Attrs;
use serde_json::json;

pub(crate) fn provider(
    stan: Result<sc_stan::cmdstan::CmdStan, sc_error::Error>,
) -> BayesRegProvider {
    let stores = Arc::new(
        |name: &str| -> sc_error::Result<Arc<dyn sc_files::FileStore>> {
            Err(sc_error::Error::invalid(format!(
                "no file store `{name}` here"
            )))
        },
    );
    BayesRegProvider::new(Arc::new(StanProvider::new(stores, stan)))
}

fn no_cmdstan() -> BayesRegProvider {
    provider(Err(sc_error::Error::config("not looked for in this test")))
}

/// Twelve homes in four counties: `floor` numeric, `region` text, `county`
/// text, `w` weights, `log_radon` the response; one home with no reading and
/// one with no `floor`.
fn homes() -> Frame {
    let n = 12;
    let county = ["Aitkin", "Anoka", "Benton", "Clay"];
    let region = ["North", "South", "East"];
    Frame::new(
        vec![
            (
                "log_radon".into(),
                Column::Float(
                    (0..n)
                        .map(|i| (i != 5).then_some(1.0 + 0.1 * i as f64))
                        .collect(),
                ),
            ),
            (
                "floor".into(),
                Column::Float(
                    (0..n)
                        .map(|i| (i != 7).then_some(f64::from(u8::from(i % 3 == 0))))
                        .collect(),
                ),
            ),
            (
                "region".into(),
                Column::Str((0..n).map(|i| Some(region[i % 3].to_owned())).collect()),
            ),
            (
                "county".into(),
                Column::Str((0..n).map(|i| Some(county[i % 4].to_owned())).collect()),
            ),
            (
                "w".into(),
                Column::Float((0..n).map(|_| Some(1.0)).collect()),
            ),
        ],
        Vec::new(),
    )
    .unwrap()
}

fn radon_model(formula: &str) -> Model {
    let mut homes = Dataset::new("homes");
    for c in ["log_radon", "floor", "region", "county", "w"] {
        homes = homes.column(c, c);
    }
    Model::new("Radon", sc_bayesreg::PROVIDER, homes)
        .config(keys::FORMULA, formula)
        .config(keys::FAMILY, "gaussian")
}

fn lowered(model: &Model, frame: &Frame) -> Attrs {
    let datasets = vec![(MAIN_DATASET.to_owned(), frame.clone())];
    no_cmdstan()
        .lower(
            &model.configuration,
            LowerInput::Frames {
                model,
                datasets: &datasets,
            },
        )
        .unwrap()
        .unwrap()
}

#[test]
fn the_configuration_is_the_formula_and_the_samplers_settings() {
    let p = no_cmdstan();
    let names: Vec<String> = p
        .config_declaration()
        .iter()
        .map(|f| f.name().to_owned())
        .collect();
    for want in [
        "formula",
        "family",
        "links",
        "priors",
        "center",
        "chains",
        "iter_sampling",
        "seed",
    ] {
        assert!(names.contains(&want.to_owned()), "{want} in {names:?}");
    }
    for derived in [
        "program_store",
        "program",
        "bindings",
        "dimensions",
        "policies",
        "labels",
    ] {
        assert!(
            !names.contains(&derived.to_owned()),
            "{derived} in {names:?}"
        );
    }
    assert!(p.description().contains("CmdStan was not found"));
}

#[tokio::test]
async fn a_lowered_formula_binds_as_a_stan_model_does() {
    let frame = homes();
    let model = radon_model("log_radon | weights(w) ~ floor + region + (1 + floor | county)");
    let config = lowered(&model, &frame);
    assert!(
        config[keys::GENERATED_PROGRAM]
            .as_str()
            .unwrap()
            .starts_with("// Generated")
    );
    assert_eq!(config["bindings"]["X_1"]["kind"], "terms");
    assert_eq!(config["bindings"]["J_1"]["dimension"], "county");
    assert_eq!(config["dimensions"]["county"]["kind"], "values");
    assert_eq!(config["policies"]["main"]["nulls"], "drop");
    assert_eq!(config["exclude_variables"], json!(["z_1"]));

    let p = no_cmdstan();
    let interface = p.interface(&config).await.unwrap().unwrap();
    let datasets = vec![(MAIN_DATASET.to_owned(), frame)];
    let bound = bind_data(&interface, &config, &datasets, 1_000_000).unwrap();
    let data = &bound.json;
    // The home with no reading and the one with no `floor` are dropped, and
    // said so — and the coefficient names and the default priors were worked
    // out over the rows that are left.
    assert_eq!(data["N"], 10);
    assert_eq!(bound.report.dropped(MAIN_DATASET), 2);
    // `floor`, then `region` treatment-coded against its first level.
    assert_eq!(data["K_1"], 3);
    assert_eq!(
        bound.coordinates.designs["X_1"].columns,
        ["floor", "regionNorth", "regionSouth"]
    );
    assert_eq!(
        bound.coordinates.designs["Z_1_1"].columns,
        ["Intercept", "floor"]
    );
    assert_eq!(data["N_1"], 4);
    // Every `b` is flat by default (code 0); the intercept and the SDs are
    // student-t computed from the response (code 2).
    assert_eq!(data["prior_b_1_code"], json!([0, 0, 0]));
    assert_eq!(data["prior_Intercept_1_code"], 2);
    assert_eq!(data["prior_sd_1_code"], json!([2, 2]));
    assert_eq!(data["prior_sd_1_args"][0][2], 2.5);
    assert_eq!(data["prior_cor_1_eta"], 1.0);
    assert_eq!(data["prior_sigma_code"], 2);
}

#[tokio::test]
async fn a_rows_prior_reaches_the_program_as_numbers_and_a_bound_as_bounds() {
    let frame = homes();
    let model = radon_model("log_radon ~ floor + region").config(
        keys::PRIORS,
        json!([
            {"class": "b", "prior": "normal(0, 2)"},
            {"class": "b", "coef": "regionSouth", "prior": "normal(1, 0.5)", "lb": 0},
            {"class": "sigma", "prior": "exponential(1)"},
        ]),
    );
    let config = lowered(&model, &frame);
    let p = no_cmdstan();
    let interface = p.interface(&config).await.unwrap().unwrap();
    let bound = bind_data(
        &interface,
        &config,
        &[(MAIN_DATASET.to_owned(), frame)],
        1_000_000,
    )
    .unwrap();
    let data = &bound.json;
    assert_eq!(data["prior_b_1_code"], json!([1, 1, 1]));
    assert_eq!(data["prior_b_1_args"][2], json!([1.0, 0.5, 0.0]));
    assert_eq!(data["prior_b_1_args"][0], json!([0.0, 2.0, 0.0]));
    assert_eq!(data["lb_b_1"][2], 0.0);
    assert_eq!(data["prior_sigma_code"], 6);
    assert!(
        config[keys::GENERATED_PROGRAM]
            .as_str()
            .unwrap()
            .contains("vector<lower=lb_b_1, upper=ub_b_1>[K_1] b_1;")
    );
}

#[tokio::test]
async fn at_save_the_shape_is_enough_to_check_the_bindings() {
    let frame = homes();
    let shape = DatasetShape::of_frame("homes", &frame);
    let model = radon_model("log_radon ~ floor + region + (1 | county)");
    let p = no_cmdstan();
    let config = p
        .lower(
            &model.configuration,
            LowerInput::Shape {
                model: &model,
                shape: Some(&shape),
            },
        )
        .unwrap()
        .unwrap();
    let interface = p.interface(&config).await.unwrap().unwrap();
    sc_model::check_bindings_with(&interface, &model, &config).unwrap();
    p.validate(&shape, &model.configuration).unwrap();

    // A typo is a sentence at save, naming the near column.
    let bad = radon_model("log_radon ~ flor + (1 | county)");
    let err = p
        .validate(&shape, &bad.configuration)
        .unwrap_err()
        .to_string();
    assert!(err.contains("did you mean `floor`?"), "{err}");
    // A prior row for a class the formula does not have, too.
    let bad = radon_model("log_radon ~ floor").config(
        keys::PRIORS,
        json!([{"class": "sd", "group": "county", "prior": "normal(0, 1)"}]),
    );
    let err = p
        .validate(&shape, &bad.configuration)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no group-level terms"), "{err}");
    // The program and its bindings are derived, never set.
    let bad = radon_model("log_radon ~ floor").config("program", "x.stan");
    assert!(p.validate(&shape, &bad.configuration).is_err());
    // And with no shape at all (a model listed at load), it still lowers.
    assert!(
        p.lower(
            &model.configuration,
            LowerInput::Shape {
                model: &model,
                shape: None
            }
        )
        .unwrap()
        .is_some()
    );
}

#[tokio::test]
async fn a_level_row_is_refused_at_fit_when_the_levels_do_not_have_it() {
    let frame = homes();
    let model = radon_model("log_radon ~ region").config(
        keys::PRIORS,
        json!([{"class": "b", "coef": "regionWest", "prior": "normal(0, 1)"}]),
    );
    // At save, `regionWest` could be a level of `region`.
    let shape = DatasetShape::of_frame("homes", &frame);
    no_cmdstan().validate(&shape, &model.configuration).unwrap();
    // With the rows, it is not.
    let datasets = vec![(MAIN_DATASET.to_owned(), frame)];
    let err = no_cmdstan()
        .lower(
            &model.configuration,
            LowerInput::Frames {
                model: &model,
                datasets: &datasets,
            },
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("`regionWest`") && err.contains("regionNorth"),
        "{err}"
    );
}

#[tokio::test]
async fn the_program_changes_with_the_structure_and_not_with_a_rename() {
    let p = no_cmdstan();
    let model = radon_model("log_radon ~ floor + (1 | county)");
    let text = p
        .lower(
            &model.configuration,
            LowerInput::Shape {
                model: &model,
                shape: None,
            },
        )
        .unwrap()
        .unwrap()[keys::GENERATED_PROGRAM]
        .as_str()
        .unwrap()
        .to_owned();
    let program = sc_bayesreg::program_of(&text).unwrap();
    let state = json!({ "program": { "files": [ { "sha256": program.files[0].sha256 } ] } });
    let renamed = Model::new("Prices", sc_bayesreg::PROVIDER, Dataset::new("houses"))
        .config(keys::FORMULA, "price ~ area + (1 | region)");
    assert_eq!(
        p.program_changed(&renamed.configuration, &state).await,
        Some(false)
    );
    let other = radon_model("log_radon ~ floor + (1 || county)\nsigma ~ floor");
    assert_eq!(
        p.program_changed(&other.configuration, &state).await,
        Some(true)
    );
}

#[test]
fn a_dimension_is_named_after_its_column_unless_a_related_dataset_is() {
    let frame = homes();
    let mut model = radon_model("log_radon ~ floor + (1 | county)");
    model
        .related
        .push(NamedDataset::new("county", Dataset::new("counties")));
    let config = lowered(&model, &frame);
    assert_eq!(config["bindings"]["J_1"]["dimension"], "group_1");
    assert!(config["dimensions"].get("county").is_none());
}

#[test]
fn what_this_release_cannot_generate_is_refused_by_name() {
    let p = no_cmdstan();
    let shape = DatasetShape::of_frame("homes", &homes());
    let err = p
        .validate(
            &shape,
            &radon_model("log_radon ~ floor + (1 | county:region)").configuration,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("`county:region`") && err.contains("Calculated column"),
        "{err}"
    );
    let err = p
        .validate(
            &shape,
            &radon_model("log_radon ~ floor")
                .config(
                    keys::PRIORS,
                    json!([{"class": "b", "prior": "constant(1)"}]),
                )
                .configuration,
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("constant()"), "{err}");
}
