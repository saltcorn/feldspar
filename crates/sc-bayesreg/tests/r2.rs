//! R2.1–R2.3 without CmdStan: each new kind of term, family and model
//! parsed, resolved, lowered and bound through the host's own `bind_data`
//! exactly as a fit binds it; and the normalisation rules of §5 for the
//! non-linear and multivariate structures (one program, one cache key).

use sc_bayesreg::{Data, build, keys};
use sc_model::{
    Column, Dataset, Frame, LowerInput, MAIN_DATASET, Model, ModelProvider, NamedDataset, bind_data,
};
use serde_json::{Value as Json, json};

use crate::generate::{config, program};

fn frame(columns: Vec<(&str, Column)>) -> Frame {
    Frame::new(
        columns
            .into_iter()
            .map(|(n, c)| (n.to_owned(), c))
            .collect(),
        Vec::new(),
    )
    .unwrap()
}

fn floats(v: &[f64]) -> Column {
    Column::Float(v.iter().copied().map(Some).collect())
}

fn ints(v: &[i64]) -> Column {
    Column::Int(v.iter().copied().map(Some).collect())
}

fn texts(v: &[&str]) -> Column {
    Column::Str(v.iter().map(|s| Some((*s).to_owned())).collect())
}

fn model_of(formula: &str, family: &str, f: &Frame) -> Model {
    let mut ds = Dataset::new("t");
    for (n, _) in &f.columns {
        ds = ds.column(n.clone(), n.clone());
    }
    Model::new("m", sc_bayesreg::PROVIDER, ds)
        .config(keys::FORMULA, formula)
        .config(keys::FAMILY, family)
}

/// The lowered configuration of `model` over `datasets`, and the data the
/// host binds from it.
async fn bind_model(model: &Model, datasets: &[(String, Frame)]) -> (sc_types::Attrs, Json) {
    let p = crate::provider::provider(Err(sc_error::Error::config("not looked for")));
    let config = p
        .lower(&model.configuration, LowerInput::Frames { model, datasets })
        .unwrap_or_else(|e| panic!("{}", sc_error::format_chain(&e)))
        .unwrap();
    let interface = p.interface(&config).await.unwrap().unwrap();
    let bound = bind_data(&interface, &config, datasets, 1_000_000)
        .unwrap_or_else(|e| panic!("{}", sc_error::format_chain(&e)));
    (config, bound.json)
}

async fn bind(formula: &str, family: &str, f: Frame) -> (sc_types::Attrs, Json) {
    let model = model_of(formula, family, &f);
    bind_model(&model, &[(MAIN_DATASET.to_owned(), f)]).await
}

fn twelve() -> Frame {
    let g = ["a", "a", "b", "b", "c", "c", "a", "a", "b", "b", "c", "c"];
    let h = ["x", "y", "x", "y", "x", "y", "x", "y", "x", "y", "x", "y"];
    frame(vec![
        (
            "y",
            floats(&[1.0, 2.0, 1.5, 2.5, 3.0, 1.0, 2.0, 1.2, 0.5, 2.2, 1.8, 2.9]),
        ),
        (
            "x",
            floats(&[0.1, 0.5, 0.2, 0.9, 0.3, 0.1, 0.4, 0.8, 0.2, 0.6, 0.7, 0.3]),
        ),
        ("edu", ints(&[0, 1, 2, 3, 0, 1, 2, 3, 1, 2, 3, 0])),
        ("rating", ints(&[1, 2, 3, 1, 2, 3, 3, 2, 1, 1, 2, 3])),
        ("g", texts(&g)),
        ("h", texts(&h)),
    ])
}

#[tokio::test]
async fn a_combination_of_grouping_columns_is_one_level_per_combination_that_occurs() {
    let (config, data) = bind("y ~ x + (1 | g:h)", "gaussian", twelve()).await;
    assert_eq!(config["derived_columns"]["g:h"]["kind"], "combine");
    assert_eq!(data["N_1"], 6);
    // `g/h` is `(1 | g) + (1 | g:h)`.
    let (_, data) = bind("y ~ x + (1 | g/h)", "gaussian", twelve()).await;
    let sizes: Vec<i64> = [data["N_1"].as_i64(), data["N_2"].as_i64()]
        .into_iter()
        .flatten()
        .collect();
    assert!(sizes.contains(&3) && sizes.contains(&6), "{sizes:?}");
}

#[tokio::test]
async fn a_foreign_key_groups_by_the_referenced_rows_even_those_with_no_rows_here() {
    let f = twelve();
    let mut model = model_of("y ~ x + (1 | g)", "gaussian", &f);
    for c in &mut model.dataset.columns {
        if c.name == "g" {
            c.key = Some(sc_dataset::ForeignKey {
                table: "groups".into(),
                field: "id".into(),
            });
        }
    }
    let groups = Frame::new(
        vec![("id".to_owned(), texts(&["a", "b", "c", "d"]))],
        vec![
            "text:a".into(),
            "text:b".into(),
            "text:c".into(),
            "text:d".into(),
        ],
    )
    .unwrap();
    model.related.push(NamedDataset::new(
        "groups",
        Dataset::new("groups").column("id", "id"),
    ));
    let datasets = vec![(MAIN_DATASET.to_owned(), f), ("groups".to_owned(), groups)];
    let (config, data) = bind_model(&model, &datasets).await;
    assert_eq!(config["bindings"]["J_1"]["dimension"], "groups");
    // `d` has no rows, and still a parameter.
    assert_eq!(data["N_1"], 4);
}

#[tokio::test]
async fn a_monotonic_effect_has_a_simplex_over_its_steps() {
    let (config, data) = bind("y ~ x + mo(edu)", "gaussian", twelve()).await;
    let program = config[keys::GENERATED_PROGRAM].as_str().unwrap();
    assert!(
        program.contains("simplex[Lmo_1_1 - 1] simo_1_1;"),
        "{program}"
    );
    assert_eq!(data["Lmo_1_1"], 4);
    assert_eq!(data["con_simo_1_1"], json!([1.0, 1.0, 1.0]));
    // Levels are positions: `edu = 0` is the first.
    assert_eq!(data["Xmo_1_1"][0], 1);
    assert_eq!(data["Xmo_1_1"][3], 4);
    // A text column has no order to trust.
    let err = build(
        &config_with("y ~ mo(g)", "gaussian"),
        Data::Frame(&twelve()),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("ordinal column of whole numbers"), "{err}");
}

fn config_with(formula: &str, family: &str) -> sc_types::Attrs {
    config(formula, family)
}

#[tokio::test]
async fn an_ordinal_response_counts_its_thresholds_from_the_data_or_from_thres() {
    let (config, data) = bind("rating ~ x + (1 | g)", "cumulative", twelve()).await;
    assert_eq!(data["nthres"], 2);
    assert!(
        config[keys::GENERATED_PROGRAM]
            .as_str()
            .unwrap()
            .contains("ordered[nthres] Intercept_c_1;")
    );
    let (_, data) = bind("rating | thres(4) ~ x", "sratio", twelve()).await;
    assert_eq!(data["nthres"], 4);
    let (config, _) = bind("rating ~ x + cs(x)", "acat", twelve()).await;
    assert!(
        config[keys::GENERATED_PROGRAM]
            .as_str()
            .unwrap()
            .contains("matrix[Kcs_1, nthres] bcs_1;")
    );
}

#[tokio::test]
async fn a_categorical_response_indexes_its_categories() {
    let (_, data) = bind("g ~ x", "categorical", twelve()).await;
    assert_eq!(data["ncat"], 3);
    assert_eq!(data["Y"][2], 2);
}

#[tokio::test]
async fn a_covariance_per_level_of_by_and_a_level_in_two_is_refused() {
    let mut f = twelve();
    // `g` nested in `k`: a, b in one; c in the other.
    f.columns.push((
        "k".to_owned(),
        texts(&["p", "p", "p", "p", "q", "q", "p", "p", "p", "p", "q", "q"]),
    ));
    let (config, data) = bind("y ~ x + (1 + x | gr(g, by = k))", "gaussian", f).await;
    assert_eq!(data["Nby_1"], 2);
    assert!(
        config[keys::GENERATED_PROGRAM]
            .as_str()
            .unwrap()
            .contains("array[Nby_1] cholesky_factor_corr[M_1_1] L_1;")
    );
    let model = model_of("y ~ x + (1 | gr(g, by = h))", "gaussian", &twelve());
    let p = crate::provider::provider(Err(sc_error::Error::config("not looked for")));
    let datasets = vec![(MAIN_DATASET.to_owned(), twelve())];
    let err = p
        .lower(
            &model.configuration,
            LowerInput::Frames {
                model: &model,
                datasets: &datasets,
            },
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("is in two levels of `h`"), "{err}");
}

#[tokio::test]
async fn a_fixed_distributional_parameter_is_data() {
    let (config, data) = bind("y ~ x\nnu = 4", "student", twelve()).await;
    assert_eq!(data["nu"], 4.0);
    let text = config[keys::GENERATED_PROGRAM].as_str().unwrap();
    assert!(text.contains("real<lower=1> nu;"), "{text}");
    assert!(!text.contains("prior_nu_code"), "{text}");
    // `nu = 4` and `constant(4)` are one program.
    let mut c = config_with("y ~ x", "student");
    c.insert(
        keys::PRIORS.into(),
        json!([{"class": "nu", "prior": "constant(4)"}]),
    );
    assert_eq!(
        build(&c, Data::Nothing).unwrap().program,
        program("y ~ x\nnu = 4", "student")
    );
}

#[tokio::test]
async fn a_zero_inflated_and_a_hurdle_model_bind() {
    let f = frame(vec![
        ("n", ints(&[0, 0, 3, 1, 0, 5, 2, 0, 0, 1])),
        (
            "x",
            floats(&[0.1, 0.5, 0.2, 0.9, 0.3, 0.1, 0.4, 0.8, 0.2, 0.6]),
        ),
    ]);
    for family in ["zero_inflated_poisson", "hurdle_negbinomial"] {
        let (config, data) = bind("n ~ x", family, f.clone()).await;
        assert_eq!(data["N"], 10, "{family}");
        let resolved = &config[keys::RESOLVED_PRIORS]["rows"];
        let dpar = if family.starts_with("zero") {
            "zi"
        } else {
            "hu"
        };
        assert!(
            resolved
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["class"] == dpar && r["prior"] == "beta(1, 1)"),
            "{resolved}"
        );
    }
}

#[test]
fn a_non_linear_formula_is_one_program_whatever_its_names() {
    let a = program(
        "y ~ a * (1 - exp(-b * t))\na ~ 1 + (1 | tree)\nb ~ 1",
        "gaussian",
    );
    let b = program(
        "height ~ asym * (1 - exp(-rate * age))\nasym ~ 1 + (1 | plot)\nrate ~ 1",
        "gaussian",
    );
    assert_eq!(a, b);
    assert!(
        a.contains("vector[N] mu = (nlp_1 .* (1.0 - exp(((-nlp_2) .* C_1))));"),
        "{a}"
    );
    // No commutative reordering: another program (a cache miss, never a
    // wrong answer).
    assert_ne!(
        program("y ~ a * x\na ~ 1", "gaussian"),
        program("y ~ x * a\na ~ 1", "gaussian")
    );
    // `nl` is inferred and said.
    let built = build(&config("y ~ a * x\na ~ 1", "gaussian"), Data::Nothing).unwrap();
    assert!(
        built
            .resolved
            .notes
            .iter()
            .any(|n| n.contains("non-linear")),
        "{:?}",
        built.resolved.notes
    );
    // A non-linear parameter needs a proper prior.
    let p = crate::provider::provider(Err(sc_error::Error::config("not looked for")));
    let f = frame(vec![("y", floats(&[1.0, 2.0])), ("x", floats(&[1.0, 2.0]))]);
    let shape = sc_model::DatasetShape::of_frame("t", &f);
    let err = p
        .validate(&shape, &config("y ~ a * x\na ~ 1", "gaussian"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs a proper prior"), "{err}");
    let mut c = config("y ~ a * x\na ~ 1", "gaussian");
    c.insert(
        keys::PRIORS.into(),
        json!([{"class": "b", "nlpar": "a", "prior": "normal(0, 5)"}]),
    );
    p.validate(&shape, &c).unwrap();
}

#[test]
fn the_responses_of_a_multivariate_model_are_sorted_and_correlated_by_default() {
    let one = program("bf(y1 ~ x, sigma ~ x) + bf(y2 ~ x)", "gaussian");
    let two = program("bf(w ~ z) + bf(v ~ z, sigma ~ z)", "gaussian");
    assert_eq!(one, two);
    assert!(one.contains("cholesky_factor_corr[2] Lrescor;"), "{one}");
    let off = program("bf(y1 ~ x) + bf(y2 ~ x) + set_rescor(FALSE)", "gaussian");
    assert!(!off.contains("Lrescor"), "{off}");
    // `mvbind` is the same rhs for each.
    assert_eq!(
        program("mvbind(y1, y2) ~ x + (1 | p | g)", "gaussian"),
        program("y1 ~ x + (1 | p | g)\ny2 ~ x + (1 | p | g)", "gaussian")
    );
    // A shared ID is one block across the responses.
    let built = build(
        &config("y1 ~ x + (1 | p | g)\ny2 ~ x + (1 | p | g)", "gaussian"),
        Data::Nothing,
    )
    .unwrap();
    assert_eq!(built.resolved.blocks.len(), 1);
    assert_eq!(built.resolved.blocks[0].parts.len(), 2);
    // A bare dpar line is ambiguous with two responses.
    let err = build(
        &config("y1 ~ x\ny2 ~ x\nsigma ~ x", "gaussian"),
        Data::Nothing,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("inside the `bf(…)`"), "{err}");
}

#[tokio::test]
async fn a_subset_response_is_measured_on_its_rows_and_drops_none() {
    let f = frame(vec![
        ("y1", floats(&[1.0, 2.0, 1.5, 2.5, 3.0, 1.0])),
        (
            "y2",
            Column::Float(vec![Some(0.5), None, Some(0.7), None, Some(0.1), None]),
        ),
        (
            "s",
            Column::Bool(vec![
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
            ]),
        ),
        ("x", floats(&[0.1, 0.5, 0.2, 0.9, 0.3, 0.1])),
    ]);
    let (config, data) = bind(
        "bf(y1 ~ x) + bf(y2 | subset(s) ~ x) + set_rescor(FALSE)",
        "gaussian",
        f,
    )
    .await;
    assert_eq!(data["N"], 6);
    // Whichever position the subset response sorts to, its rows are those
    // of `s`.
    let suf = if data.get("N_sub").is_some() {
        ""
    } else {
        "_2"
    };
    assert_eq!(data[format!("N_sub{suf}")], 3);
    assert_eq!(data[format!("J_sub{suf}")], json!([1, 3, 5]));
    assert_eq!(data[format!("Y{suf}")], json!([0.5, 0.7, 0.1]));
    assert_eq!(config["derived_columns"]["y2 | subset(s)"]["kind"], "mask");
}

/// R2.4 without CmdStan: a pack made from known draws predicts what those
/// draws imply, and refuses a level the fit did not have, by name.
#[tokio::test]
async fn a_prediction_is_the_mean_over_the_packed_draws() {
    use sc_model::{DrawSeries, Prediction};
    let f = twelve();
    let model = model_of("y ~ x + (1 | g)", "gaussian", &f);
    let datasets = vec![(MAIN_DATASET.to_owned(), f.clone())];
    let p = crate::provider::provider(Err(sc_error::Error::config("not looked for")));
    let config = p
        .lower(
            &model.configuration,
            LowerInput::Frames {
                model: &model,
                datasets: &datasets,
            },
        )
        .unwrap()
        .unwrap();
    let interface = p.interface(&config).await.unwrap().unwrap();
    let bound = bind_data(&interface, &config, &datasets, 1_000_000).unwrap();
    let structure: sc_bayesreg::Structure =
        serde_json::from_value(config[keys::STRUCTURE].clone()).unwrap();
    // Two draws: Intercept 1 and 3, slope 2 and 4; county effects a, b, c.
    let draws = vec![
        DrawSeries::new("Intercept_1", vec![], 1, vec![1.0, 3.0]),
        DrawSeries::new("b_1", vec![1], 1, vec![2.0, 4.0]),
        DrawSeries::new("r_1", vec![1, 1], 1, vec![0.5, 0.5]),
        DrawSeries::new("r_1", vec![2, 1], 1, vec![0.0, 0.0]),
        DrawSeries::new("r_1", vec![3, 1], 1, vec![-1.0, -1.0]),
        DrawSeries::new("sigma", vec![], 1, vec![1.0, 1.0]),
    ];
    let pack = sc_bayesreg::predict::pack(&config, &structure, &bound.coordinates, &draws).unwrap();
    let state = json!({ "prediction": pack });
    let rows = frame(vec![("x", floats(&[1.0, 0.0])), ("g", texts(&["a", "c"]))]);
    let got = p.predict(&state, &rows).await.unwrap();
    // Mean intercept 2, mean slope 3: 2 + 3 + 0.5 and 2 + 0 − 1.
    assert_eq!(got, vec![Prediction::number(5.5), Prediction::number(1.0)]);
    let unknown = frame(vec![("x", floats(&[1.0])), ("g", texts(&["z"]))]);
    let err = p.predict(&state, &unknown).await.unwrap_err().to_string();
    assert!(
        err.contains("`g` is `z` in row 1, which was not a level"),
        "{err}"
    );
    // No draws kept: no pack, and the refusal says why.
    let mut off = config.clone();
    off.insert("keep_draws".into(), Json::Bool(false));
    let why = sc_bayesreg::predict::pack(&off, &structure, &bound.coordinates, &draws).unwrap_err();
    assert!(why.contains("keep_draws"), "{why}");
    let err = p
        .predict(&json!({ "prediction_unavailable": why }), &rows)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cannot predict") && err.contains("keep_draws"),
        "{err}"
    );
    // A multivariate model predicts no one number, and says so.
    let mv = build(&config_with("y ~ x\nx2 ~ x", "gaussian"), Data::Nothing).unwrap();
    assert!(sc_bayesreg::predict::predicts(mv.structure()).is_err());
}
