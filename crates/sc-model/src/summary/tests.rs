use super::*;

/// splitmix64: a generator small enough to write down, so the draws the
/// reference numbers were computed from are the draws these tests compute on.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform on (0, 1).
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal, by Box–Muller (the cosine half only, for simplicity).
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

const CHAINS: usize = 4;
const DRAWS: usize = 1000;

/// The three reference variables, chain by chain, each rounded to nine
/// significant figures as CmdStan would have written them (so the reference
/// tool and this code read the same numbers):
///
/// - `iid`: well mixed, N(1, 2²) independent draws;
/// - `ar`: an AR(1) with coefficient 0.9 — strongly autocorrelated, so its
///   ESS is a small fraction of its 4 000 draws;
/// - `stuck`: three chains N(0, 1) and a fourth stuck near 3.
fn reference_draws() -> Vec<(&'static str, Vec<Vec<f64>>)> {
    let round = |x: f64| format!("{x:.8e}").parse::<f64>().unwrap();
    let mut iid = Vec::new();
    let mut ar = Vec::new();
    let mut stuck = Vec::new();
    for chain in 0..CHAINS {
        let mut rng = Rng(1000 + chain as u64);
        iid.push(
            (0..DRAWS)
                .map(|_| round(1.0 + 2.0 * rng.normal()))
                .collect(),
        );
        let mut x = 0.0;
        ar.push(
            (0..DRAWS)
                .map(|_| {
                    x = 0.9 * x + rng.normal();
                    round(x)
                })
                .collect(),
        );
        stuck.push(
            (0..DRAWS)
                .map(|_| {
                    let z = rng.normal();
                    round(if chain == 3 { 3.0 + 0.1 * z } else { z })
                })
                .collect(),
        );
    }
    vec![("iid", iid), ("ar", ar), ("stuck", stuck)]
}

fn slices(chains: &[Vec<f64>]) -> Vec<&[f64]> {
    chains.iter().map(Vec::as_slice).collect()
}

fn summary_of(chains: &[Vec<f64>]) -> ElementSummary {
    ElementSummary::of(&slices(chains))
}

/// Writes the reference draws as four CmdStan CSVs, for the reference tool.
/// Run with `cargo test -p sc-model --lib summary::tests::write_reference_csvs
/// -- --ignored`, then (CmdStan 2.40.0):
///
/// ```text
/// ~/.cmdstan/cmdstan-2.40.0/bin/stansummary --sig_figs 9 \
///     -c reference.csv $TMPDIR/sc-summary-reference/chain-*.csv
/// ```
#[test]
#[ignore = "writes the reference CSVs; see the doc comment"]
fn write_reference_csvs() {
    let dir = std::env::temp_dir().join("sc-summary-reference");
    std::fs::create_dir_all(&dir).unwrap();
    let vars = reference_draws();
    for chain in 0..CHAINS {
        let mut csv = String::from(
            "# model = reference_model\n# method = sample (Default)\n#   sample\n\
             #     num_samples = 1000 (Default)\n#     num_warmup = 1000 (Default)\n\
             #     save_warmup = false (Default)\n#     thin = 1 (Default)\n",
        );
        csv.push_str(&format!("# id = {}\n", chain + 1));
        csv.push_str("lp__,accept_stat__,stepsize__,treedepth__,n_leapfrog__,divergent__,energy__");
        for (name, _) in &vars {
            csv.push_str(&format!(",{name}"));
        }
        csv.push_str("\n# Adaptation terminated\n# Step size = 0.5\n");
        csv.push_str("# Diagonal elements of inverse mass matrix:\n# 1, 1, 1\n");
        for i in 0..DRAWS {
            csv.push_str(&format!("{},0.9,0.5,2,3,0,{}", -(i as f64), i as f64));
            for (_, draws) in &vars {
                csv.push_str(&format!(",{:.8e}", draws[chain][i]));
            }
            csv.push('\n');
        }
        csv.push_str(
            "\n#  Elapsed Time: 0.1 seconds (Warm-up)\n#                0.1 seconds (Sampling)\n\
             #                0.2 seconds (Total)\n",
        );
        std::fs::write(dir.join(format!("chain-{}.csv", chain + 1)), csv).unwrap();
    }
    eprintln!("wrote {}", dir.display());
}

/// `(mean, sd, MCSE, q5, q50, q95, ess_bulk, ess_tail, rhat)` as
/// `stansummary --sig_figs 9 -c reference.csv` wrote them for the draws above
/// (see [`write_reference_csvs`]), CmdStan 2.40.0.
///
/// Two of the stuck variable's numbers are not a reference for anything, and
/// the test does not compare them: its ESS runs Geyer's sequence to the lag
/// bound, where the implementations truncate differently (see `ess_basic`),
/// and its tail indicator is constant in some split chains, which Stan's
/// autocorrelation turns into NaN and then into `2000` (its `lp__` shows the
/// same `2000`). What that case is for is R̂, which agrees.
const REFERENCE: [(&str, [f64; 9]); 3] = [
    (
        "iid",
        [
            0.961434818,
            1.98862135,
            0.0316111048,
            -2.29039058,
            0.972713962,
            4.22473384,
            3960.13988,
            4050.25137,
            1.00128409,
        ],
    ),
    (
        "ar",
        [
            0.11791418,
            2.28393782,
            0.151506841,
            -3.58585266,
            0.144448702,
            3.7986972,
            228.73422,
            354.10764,
            1.00609594,
        ],
    ),
    (
        "stuck",
        [
            0.758778106,
            1.55474091,
            0.946375922,
            -1.45535982,
            0.422302966,
            3.08695669,
            7.39272746,
            2000.0,
            1.49727135,
        ],
    ),
];

fn close(what: &str, got: f64, want: f64, rel: f64) {
    let ok = (got - want).abs() <= rel * want.abs().max(1e-12);
    assert!(ok, "{what}: got {got}, want {want} (within {rel:e})");
}

#[test]
fn the_summary_agrees_with_stansummary() {
    let draws = reference_draws();
    for ((name, chains), (ref_name, r)) in draws.iter().zip(REFERENCE) {
        assert_eq!(*name, ref_name);
        let s = summary_of(chains);
        let at = |stat: &str| format!("{name}.{stat}");
        close(&at("mean"), s.mean, r[0], 1e-7);
        close(&at("sd"), s.sd, r[1], 1e-7);
        close(&at("q5"), s.q5, r[3], 1e-7);
        close(&at("q50"), s.q50, r[4], 1e-7);
        close(&at("q95"), s.q95, r[5], 1e-7);
        close(&at("rhat"), s.rhat, r[8], 1e-7);
        if *name == "stuck" {
            continue;
        }
        close(&at("ess_bulk"), s.ess_bulk, r[6], 1e-7);
        close(&at("ess_tail"), s.ess_tail, r[7], 1e-7);
        // stansummary's MCSE divides by the ESS of the unsplit chains; ours,
        // posterior's and ArviZ's by the split chains'. Both from one ESS.
        let unsplit = ess_basic(chains);
        close(&at("stansummary mcse"), s.sd / unsplit.sqrt(), r[2], 1e-7);
        close(
            &at("mcse"),
            s.mcse_mean,
            s.sd / ess_mean(&slices(chains)).sqrt(),
            1e-12,
        );
    }
}

#[test]
fn a_stuck_chain_makes_rhat_large_and_a_mixed_one_does_not() {
    let draws = reference_draws();
    let by = |n: &str| summary_of(&draws.iter().find(|(v, _)| *v == n).unwrap().1);
    assert!(by("iid").rhat < 1.01, "{:?}", by("iid"));
    let stuck = by("stuck");
    assert!(stuck.rhat > 1.4, "{stuck:?}");
    // And its ESS is a handful of draws out of 4 000, however it is truncated.
    assert!(stuck.ess_bulk < 50.0, "{stuck:?}");
    // Autocorrelation costs effective draws: an AR(1) at 0.9 keeps about
    // (1 − 0.9) / (1 + 0.9) of them.
    let ar = by("ar");
    assert!(ar.ess_bulk > 100.0 && ar.ess_bulk < 400.0, "{ar:?}");
    assert!(by("iid").ess_bulk > 3000.0);
}

#[test]
fn the_fft_autocovariance_is_the_direct_sum() {
    let mut rng = Rng(7);
    let x: Vec<f64> = (0..37).map(|_| rng.normal()).collect();
    let m = mean(&x);
    let fast = autocovariance(&x);
    for (t, got) in fast.iter().enumerate() {
        let direct: f64 = (0..x.len() - t)
            .map(|i| (x[i] - m) * (x[i + t] - m))
            .sum::<f64>()
            / x.len() as f64;
        assert!((got - direct).abs() < 1e-12, "lag {t}: {got} vs {direct}");
    }
}

#[test]
fn ranks_average_their_ties_and_quantiles_interpolate() {
    assert_eq!(average_ranks(&[3.0, 1.0, 3.0, 2.0]), [3.5, 1.0, 3.5, 2.0]);
    let sorted = [1.0, 2.0, 3.0, 4.0];
    assert_eq!(quantile(&sorted, 0.5), 2.5);
    assert!((quantile(&sorted, 0.05) - 1.15).abs() < 1e-12);
    assert_eq!(quantile(&[5.0], 0.95), 5.0);
    assert!(quantile(&[], 0.5).is_nan());
}

#[test]
fn what_is_undefined_is_nan_and_never_a_panic() {
    // A constant: no spread, so no R̂ and no ESS — but a mean and quantiles.
    let constant = vec![vec![2.0; 10]; 4];
    let s = summary_of(&constant);
    assert_eq!(s.mean, 2.0);
    assert_eq!(s.q50, 2.0);
    assert!(s.rhat.is_nan() && s.ess_bulk.is_nan() && s.ess_tail.is_nan());
    // A NaN draw: nothing but the (NaN) mean.
    let mut nan = vec![vec![1.0, 2.0, 3.0, 4.0]; 2];
    nan[1][2] = f64::NAN;
    let s = summary_of(&nan);
    assert!(s.mean.is_nan() && s.q5.is_nan() && s.rhat.is_nan());
    // One draw (the optimiser's): the value, and nothing about spread.
    let s = summary_of(&[vec![0.25]]);
    assert_eq!(s.mean, 0.25);
    assert_eq!(s.q95, 0.25);
    assert!(s.sd.is_nan() && s.rhat.is_nan() && s.ess_bulk.is_nan());
    // No draws at all.
    let s = summary_of(&[]);
    assert!(s.mean.is_nan());
    // Pathfinder's draws are not chains.
    let draws = reference_draws();
    assert!(summary_of(&draws[0].1).without_rhat().rhat.is_nan());
}
