use sc_types::Attrs;
use serde_json::json;

use super::*;
use crate::bind::{Coordinates, DimensionCoordinates, DimensionKind};
use crate::interface::{Declaration, Element, SizeExpr};

/// splitmix64 and Box–Muller, as in the summary's tests.
struct Rng(u64);

impl Rng {
    fn normal(&mut self) -> f64 {
        let mut next = || {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (((z ^ (z >> 31)) >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let (u, v) = (next(), next());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

const CHAINS: u32 = 4;
const DRAWS: usize = 400;

fn interface() -> Interface {
    Interface {
        data: vec![Declaration::new("J", Element::Int, vec![], "int")],
        parameters: vec![
            Declaration::new("mu", Element::Real, vec![], "real"),
            Declaration::new(
                "alpha",
                Element::Real,
                vec![SizeExpr::var("J")],
                "vector[J]",
            ),
        ],
        transformed: vec![],
        generated: vec![Declaration::new(
            "y_rep",
            Element::Real,
            vec![SizeExpr::literal(5)],
            "vector[5]",
        )],
    }
}

fn coordinates() -> Coordinates {
    Coordinates {
        dimensions: vec![DimensionCoordinates {
            name: "counties".to_owned(),
            kind: DimensionKind::Rows,
            dataset: "counties".to_owned(),
            column: None,
            keys: vec![json!(27001), json!(27003), json!(27005)],
            labels: vec!["Aitkin".into(), "Anoka".into(), "Becker".into()],
        }],
        ..Coordinates::default()
    }
}

fn config() -> Attrs {
    serde_json::from_value(json!({
        "bindings": {"J": {"kind": "size", "dimension": "counties"}},
    }))
    .unwrap()
}

/// Four chains of `mu`, `alpha[1..3]`, `y_rep[1..5]` and the sampler's
/// variables. `stuck` moves `alpha[2]`'s fourth chain away from the rest;
/// `divergent` marks that many iterations of chain 2 divergent; `deep` makes
/// that many of chain 3's iterations hit the tree depth of 10; `sticky_energy`
/// makes chain 1's energy a slow random walk (a low E-BFMI).
fn draws(stuck: bool, divergent: usize, deep: usize, sticky_energy: bool) -> Vec<DrawSeries> {
    let mut out = Vec::new();
    for chain in 1..=CHAINS {
        let mut rng = Rng(u64::from(chain) * 77);
        let mut normal =
            |shift: f64| -> Vec<f64> { (0..DRAWS).map(|_| shift + rng.normal()).collect() };
        out.push(DrawSeries::new("mu", vec![], chain, normal(1.0)));
        for j in 1..=3 {
            let shift = if stuck && j == 2 && chain == 4 {
                5.0
            } else {
                0.0
            };
            out.push(DrawSeries::new("alpha", vec![j], chain, normal(shift)));
        }
        for i in 1..=5 {
            out.push(DrawSeries::new("y_rep", vec![i], chain, normal(0.0)));
        }
        out.push(DrawSeries::new("lp__", vec![], chain, normal(-10.0)));
        let flags = |n: usize, on: f64, off: f64| -> Vec<f64> {
            (0..DRAWS).map(|i| if i < n { on } else { off }).collect()
        };
        out.push(DrawSeries::new(
            "divergent__",
            vec![],
            chain,
            flags(if chain == 2 { divergent } else { 0 }, 1.0, 0.0),
        ));
        out.push(DrawSeries::new(
            "treedepth__",
            vec![],
            chain,
            flags(if chain == 3 { deep } else { 0 }, 10.0, 3.0),
        ));
        let energy = if sticky_energy && chain == 1 {
            let mut e = 0.0;
            (0..DRAWS)
                .map(|_| {
                    e += 0.05 * rng.normal();
                    e
                })
                .collect()
        } else {
            normal(20.0)
        };
        out.push(DrawSeries::new("energy__", vec![], chain, energy));
    }
    out
}

fn mcmc() -> PosteriorRun {
    PosteriorRun {
        method: PosteriorMethod::Mcmc,
        max_treedepth: Some(10),
        wall_seconds: vec![1.0, 1.1, 0.9, 1.2],
        iterations: None,
    }
}

fn table<'a>(rep: &'a PosteriorReport, name: &str) -> (&'a [String], &'a [ParameterRow]) {
    match rep.tables.iter().find(|t| t.name() == name) {
        Some(ParameterBlock::Table { columns, rows, .. }) => (columns.as_slice(), rows.as_slice()),
        other => panic!("no table `{name}`: {other:?}"),
    }
}

#[test]
fn a_mixed_posterior_is_summarised_by_label_and_warns_of_nothing() {
    let coords = coordinates();
    let labeller = Labeller::new(&config(), &coords).unwrap();
    let draws = draws(false, 0, 0, false);
    let rep = report(&draws, &mcmc(), Some(&interface()), &labeller, 1000).unwrap();
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    // The program's order, sampler variables left out.
    assert_eq!(
        rep.tables
            .iter()
            .map(ParameterBlock::name)
            .collect::<Vec<_>>(),
        ["mu", "alpha", "y_rep"]
    );
    let (columns, rows) = table(&rep, "alpha");
    assert_eq!(
        columns,
        [
            "counties", "mean", "sd", "mcse", "q5", "q50", "q95", "rhat", "ess_bulk", "ess_tail"
        ]
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].cells[0], json!("Aitkin"));
    assert_eq!(rows[2].cells[0], json!("Becker"));
    let mean = rows[0].cells[1].as_f64().unwrap();
    assert!(mean.abs() < 0.2, "{mean}");
    // A scalar has no label column; a literal-sized quantity is numbered.
    assert_eq!(table(&rep, "mu").0[0], "mean");
    assert_eq!(table(&rep, "y_rep").1[4].cells[0], json!(5));

    let Metrics::Posterior(m) = &rep.metrics else {
        panic!("{:?}", rep.metrics);
    };
    assert_eq!((m.chains, m.draws_per_chain), (4, DRAWS));
    assert_eq!((m.divergent, m.max_treedepth_hits), (0, 0));
    assert_eq!(m.divergent_per_chain, [0, 0, 0, 0]);
    assert_eq!(m.ebfmi.len(), 4);
    assert!(m.ebfmi.iter().all(|e| *e > 1.0), "{:?}", m.ebfmi);
    assert!(m.max_rhat < 1.01 && m.min_ess_bulk > 400.0, "{m:?}");
    assert_eq!(m.wall_seconds, [1.0, 1.1, 0.9, 1.2]);
}

#[test]
fn every_diagnostic_past_its_threshold_is_a_sentence_saying_what_to_do() {
    let coords = coordinates();
    let labeller = Labeller::new(&config(), &coords).unwrap();
    let draws = draws(true, 12, 3, true);
    let rep = report(&draws, &mcmc(), Some(&interface()), &labeller, 1000).unwrap();
    let Metrics::Posterior(m) = &rep.metrics else {
        panic!();
    };
    assert_eq!(m.divergent, 12);
    assert_eq!(m.divergent_per_chain, [0, 12, 0, 0]);
    assert_eq!(m.max_treedepth_hits, 3);
    assert!(m.ebfmi[0] < 0.3, "{:?}", m.ebfmi);
    assert!(m.max_rhat > 1.5);
    let all = rep.warnings.join("\n");
    for expected in [
        "12 divergent transitions after warmup (in chain 2): the posterior has regions the \
         sampler cannot explore",
        "raise `adapt_delta`",
        "3 iterations hit the maximum tree depth of 10",
        "raise `max_treedepth`",
        "in chain 1 (below 0.3)",
        "for `alpha[Anoka]`, above 1.01: the chains disagree",
        "the bulk effective sample size is",
        "below 400 (100 per chain)",
    ] {
        assert!(all.contains(expected), "no {expected:?} in\n{all}");
    }
    assert_eq!(rep.warnings.len(), 6, "{all}");
}

#[test]
fn a_large_generated_quantity_is_left_to_be_summarised_on_demand() {
    let coords = coordinates();
    let labeller = Labeller::new(&config(), &coords).unwrap();
    let draws = draws(false, 0, 0, false);
    let rep = report(&draws, &mcmc(), Some(&interface()), &labeller, 4).unwrap();
    assert_eq!(rep.unsummarised, [("y_rep".to_owned(), 5)]);
    assert!(rep.tables.iter().all(|t| t.name() != "y_rep"));
    // Parameters are summarised whatever their size.
    assert_eq!(table(&rep, "alpha").1.len(), 3);
}

#[test]
fn a_mode_is_its_estimates_and_an_approximation_has_no_rhat() {
    let coords = coordinates();
    let labeller = Labeller::new(&config(), &coords).unwrap();
    let mode = vec![
        DrawSeries::new("lp__", vec![], 1, vec![-5.25]),
        DrawSeries::new("mu", vec![], 1, vec![0.5]),
        DrawSeries::new("alpha", vec![1], 1, vec![0.1]),
        DrawSeries::new("alpha", vec![2], 1, vec![0.2]),
        DrawSeries::new("alpha", vec![3], 1, vec![0.3]),
    ];
    let run = PosteriorRun {
        method: PosteriorMethod::Mode,
        iterations: Some(17),
        wall_seconds: vec![0.25],
        ..PosteriorRun::default()
    };
    let rep = report(&mode, &run, Some(&interface()), &labeller, 1000).unwrap();
    let (columns, rows) = table(&rep, "alpha");
    assert_eq!(columns, ["counties", "estimate"]);
    assert_eq!(rows[1].cells, [json!("Anoka"), json!(0.2)]);
    assert_eq!(
        rep.metrics,
        Metrics::PosteriorMode(ModeMetrics {
            log_density: -5.25,
            iterations: Some(17),
            wall_seconds: vec![0.25],
        })
    );
    assert!(rep.warnings.is_empty());

    // Pathfinder: one "chain" of independent draws.
    let approx: Vec<DrawSeries> = draws(false, 0, 0, false)
        .into_iter()
        .filter(|s| s.chain == 1 && !s.variable.ends_with("__"))
        .collect();
    let run = PosteriorRun {
        method: PosteriorMethod::Approximation,
        ..PosteriorRun::default()
    };
    let rep = report(&approx, &run, Some(&interface()), &labeller, 1000).unwrap();
    let (columns, rows) = table(&rep, "alpha");
    let rhat = columns.iter().position(|c| c == "rhat").unwrap();
    assert_eq!(rows[0].cells[rhat], Json::Null);
    let Metrics::PosteriorApproximation(m) = &rep.metrics else {
        panic!("{:?}", rep.metrics);
    };
    assert_eq!(m.draws, DRAWS);
    assert!(m.min_ess_bulk > 200.0, "{m:?}");
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
}

#[test]
fn without_an_interface_every_program_variable_is_summarised_and_numbered() {
    let coords = Coordinates::default();
    let labeller = Labeller::new(&Attrs::new(), &coords).unwrap();
    let draws = draws(false, 0, 0, false);
    let rep = report(&draws, &mcmc(), None, &labeller, 1000).unwrap();
    assert_eq!(
        rep.tables
            .iter()
            .map(ParameterBlock::name)
            .collect::<Vec<_>>(),
        ["mu", "alpha", "y_rep"]
    );
    assert_eq!(table(&rep, "alpha").0[0], "index");
}

#[test]
fn ebfmi_is_the_squared_steps_over_the_squared_deviations() {
    // Energies 0, 2, 0, 2: steps² = 4 × 3 = 12, deviations² = 1 × 4 = 4.
    assert!((ebfmi(&[0.0, 2.0, 0.0, 2.0]) - 3.0).abs() < 1e-12);
    assert!(ebfmi(&[1.0]).is_nan());
    assert!(ebfmi(&[1.0, 1.0, 1.0]).is_nan());
}
