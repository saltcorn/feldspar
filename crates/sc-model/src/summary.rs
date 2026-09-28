//! The posterior summary of one element: mean, sd, MCSE, quantiles, R̂ and the
//! effective sample sizes (Stan TODO §15).
//!
//! The definitions are those of Vehtari, Gelman, Simpson, Carpenter & Bürkner
//! (2021), "Rank-normalization, folding, and localization: an improved R̂ for
//! assessing convergence of MCMC" — the ones R's `posterior`, ArviZ and
//! CmdStan's `stansummary` implement — so that the numbers here agree with the
//! numbers an admin gets by downloading the run and asking any of them:
//!
//! - **R̂** is the rank-normalised split-R̂: the larger of the classic R̂ over
//!   the rank-normalised split chains (the *bulk*) and over the rank-normalised
//!   *folded* split chains (`|x − median|`, the *tails*);
//! - **bulk-ESS** is the effective sample size of the rank-normalised split
//!   chains;
//! - **tail-ESS** is the smaller of the effective sample sizes of the
//!   indicators `x ≤ q5` and `x ≤ q95` — or the one that is defined, when the
//!   other's indicator is constant (a quantity that is almost always 0), as
//!   Stan does;
//! - the **MCSE of the mean** is `sd / √ESS` with the ESS of the split chains
//!   as they are (not rank-normalised) — `posterior`'s and ArviZ's definition;
//!   `stansummary` divides by the ESS of the unsplit chains, which differs in
//!   the third significant figure;
//! - the **quantiles** interpolate linearly between order statistics (R's type
//!   7, NumPy's default).
//!
//! The effective sample size is Geyer's initial monotone sequence over the
//! autocorrelations, which come from **biased** autocovariances computed with a
//! small radix-2 FFT here rather than a new dependency (§15).
//!
//! A statistic that is not defined — the R̂ of a constant, the ESS of a chain
//! with a NaN in it — is NaN, which the stored JSON holds as `null`: "not
//! defined" is a different answer from any number.

use statrs::distribution::{ContinuousCDF, Normal};

/// The summary of one element of one variable across its chains.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ElementSummary {
    /// The posterior mean.
    pub mean: f64,
    /// The posterior standard deviation (over every draw of every chain).
    pub sd: f64,
    /// The Monte Carlo standard error of the mean.
    pub mcse_mean: f64,
    /// The 5 % quantile.
    pub q5: f64,
    /// The median.
    pub q50: f64,
    /// The 95 % quantile.
    pub q95: f64,
    /// Rank-normalised split-R̂.
    pub rhat: f64,
    /// Bulk effective sample size.
    pub ess_bulk: f64,
    /// Tail effective sample size.
    pub ess_tail: f64,
}

impl ElementSummary {
    /// Every statistic of `chains` — one slice per chain, in iteration order.
    ///
    /// Chains of different lengths are cut to the shortest: every definition
    /// here is of an `iterations × chains` matrix, and CmdStan's chains of one
    /// fit are always the same length.
    pub fn of(chains: &[&[f64]]) -> ElementSummary {
        let chains = equal_length(chains);
        let all: Vec<f64> = chains.iter().flat_map(|c| c.iter().copied()).collect();
        let (q5, q50, q95) = if all.iter().any(|x| x.is_nan()) || all.is_empty() {
            (f64::NAN, f64::NAN, f64::NAN)
        } else {
            let sorted = sorted(&all);
            (
                quantile(&sorted, 0.05),
                quantile(&sorted, 0.5),
                quantile(&sorted, 0.95),
            )
        };
        let sd = sd(&all);
        ElementSummary {
            mean: mean(&all),
            sd,
            mcse_mean: sd / ess_mean(&chains).sqrt(),
            q5,
            q50,
            q95,
            rhat: rhat(&chains),
            ess_bulk: ess_bulk(&chains),
            ess_tail: ess_tail(&chains),
        }
    }

    /// The same summary with no R̂ — for draws that are not chains (Pathfinder's
    /// approximation), where "do the chains agree" has no meaning.
    pub fn without_rhat(mut self) -> ElementSummary {
        self.rhat = f64::NAN;
        self
    }
}

/// The chains cut to a common length.
fn equal_length<'a>(chains: &[&'a [f64]]) -> Vec<&'a [f64]> {
    let n = chains.iter().map(|c| c.len()).min().unwrap_or(0);
    chains.iter().map(|c| &c[..n]).collect()
}

/// Rank-normalised split-R̂ (see the module docs); NaN when it is not defined.
pub fn rhat(chains: &[&[f64]]) -> f64 {
    let split = split_chains(chains);
    if should_be_nan(&split) {
        return f64::NAN;
    }
    let bulk = rhat_basic(&z_scale(&split));
    let tail = rhat_basic(&z_scale(&fold(&split)));
    bulk.max(tail)
}

/// Bulk effective sample size: the ESS of the rank-normalised split chains.
pub fn ess_bulk(chains: &[&[f64]]) -> f64 {
    let split = split_chains(chains);
    if should_be_nan(&split) {
        return f64::NAN;
    }
    ess_basic(&z_scale(&split))
}

/// Tail effective sample size: the smaller of the ESS of the 5 % and the 95 %
/// quantile indicators, or the defined one when only one is (`f64::min`
/// ignores a NaN).
pub fn ess_tail(chains: &[&[f64]]) -> f64 {
    ess_quantile(chains, 0.05).min(ess_quantile(chains, 0.95))
}

/// The effective sample size of the split chains as they are — what the MCSE
/// of the mean is divided by.
pub fn ess_mean(chains: &[&[f64]]) -> f64 {
    let split = split_chains(chains);
    if should_be_nan(&split) {
        return f64::NAN;
    }
    ess_basic(&split)
}

/// The ESS of the indicator `x ≤ quantile(x, prob)` over the split chains.
fn ess_quantile(chains: &[&[f64]], prob: f64) -> f64 {
    let split = split_chains(chains);
    if should_be_nan(&split) {
        return f64::NAN;
    }
    let all: Vec<f64> = split.iter().flatten().copied().collect();
    let q = quantile(&sorted(&all), prob);
    let indicator: Vec<Vec<f64>> = split
        .iter()
        .map(|c| c.iter().map(|x| f64::from(u8::from(*x <= q))).collect())
        .collect();
    if should_be_nan(&indicator) {
        return f64::NAN;
    }
    ess_basic(&indicator)
}

/// Each chain cut in two halves, the middle iteration of an odd chain dropped
/// — so a chain that drifts disagrees with itself.
fn split_chains(chains: &[&[f64]]) -> Vec<Vec<f64>> {
    let chains = equal_length(chains);
    let n = chains.first().map_or(0, |c| c.len());
    if n < 2 {
        return chains.iter().map(|c| c.to_vec()).collect();
    }
    let half = n / 2;
    let mut out = Vec::with_capacity(chains.len() * 2);
    for chain in chains {
        out.push(chain[..half].to_vec());
        out.push(chain[n - half..].to_vec());
    }
    out
}

/// Whether a statistic of these chains is undefined: no draws, a non-finite
/// draw, or every draw the same.
fn should_be_nan(chains: &[Vec<f64>]) -> bool {
    let mut all = chains.iter().flatten();
    let Some(first) = all.next() else {
        return true;
    };
    if chains.iter().flatten().any(|x| !x.is_finite()) {
        return true;
    }
    let (lo, hi) = chains
        .iter()
        .flatten()
        .fold((*first, *first), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
    (hi - lo).abs() < f64::EPSILON
}

/// Every draw replaced by the normal quantile of its fractional rank across
/// all chains, ties averaged: `Φ⁻¹((r − 3/8) / (S + 1/4))` (Blom's offset).
fn z_scale(chains: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let all: Vec<f64> = chains.iter().flatten().copied().collect();
    let ranks = average_ranks(&all);
    let s = all.len() as f64;
    let normal = Normal::standard();
    let mut ranks = ranks.into_iter();
    chains
        .iter()
        .map(|c| {
            c.iter()
                .map(|_| {
                    let r = ranks.next().unwrap_or(f64::NAN);
                    normal.inverse_cdf((r - 0.375) / (s + 0.25))
                })
                .collect()
        })
        .collect()
}

/// The 1-based ranks of `values`, ties given the mean of the ranks they span.
fn average_ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| values[*a].total_cmp(&values[*b]));
    let mut ranks = vec![0.0; values.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && values[order[j + 1]] == values[order[i]] {
            j += 1;
        }
        // Positions i..=j (0-based) are ranks i+1..=j+1; their mean.
        let rank = (i + j) as f64 / 2.0 + 1.0;
        for k in &order[i..=j] {
            ranks[*k] = rank;
        }
        i = j + 1;
    }
    ranks
}

/// `|x − median|`, the median taken over every draw.
fn fold(chains: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let all: Vec<f64> = chains.iter().flatten().copied().collect();
    let median = quantile(&sorted(&all), 0.5);
    chains
        .iter()
        .map(|c| c.iter().map(|x| (x - median).abs()).collect())
        .collect()
}

/// The classic R̂ of equal-length chains: `√((B/W + n − 1) / n)`, with `B/n`
/// the variance of the chain means and `W` the mean within-chain variance.
fn rhat_basic(chains: &[Vec<f64>]) -> f64 {
    let n = chains.first().map_or(0, Vec::len);
    if n < 2 || chains.len() < 2 {
        return f64::NAN;
    }
    let means: Vec<f64> = chains.iter().map(|c| mean(c)).collect();
    let within = mean(&chains.iter().map(|c| variance(c)).collect::<Vec<_>>());
    let between = n as f64 * variance(&means);
    let n = n as f64;
    ((between / within + n - 1.0) / n).sqrt()
}

/// The effective sample size of equal-length chains: Geyer's initial monotone
/// sequence over the combined autocorrelations, with the "improved" last term
/// and the floor of `1 / log10(S)` on τ that every reference implementation
/// shares.
///
/// Written as `posterior`'s `.ess`. The reference implementations agree to
/// the last digit whenever the sequence stops at its first negative pair, which
/// is every chain that has mixed; they differ in the last term when it runs to
/// the lag bound instead — a chain so autocorrelated that its ESS is a handful
/// and no digit of it matters.
fn ess_basic(chains: &[Vec<f64>]) -> f64 {
    let m = chains.len();
    let n = chains.first().map_or(0, Vec::len);
    if n < 3 || should_be_nan(chains) {
        return f64::NAN;
    }
    let acov: Vec<Vec<f64>> = chains.iter().map(|c| autocovariance(c)).collect();
    let mean_at = |t: usize| acov.iter().map(|a| a[t]).sum::<f64>() / m as f64;
    let nf = n as f64;
    let mean_var = mean_at(0) * nf / (nf - 1.0);
    let mut var_plus = mean_var * (nf - 1.0) / nf;
    if m > 1 {
        let means: Vec<f64> = chains.iter().map(|c| mean(c)).collect();
        var_plus += variance(&means);
    }

    let mut rho = vec![0.0; n];
    let mut t = 0;
    let mut rho_even = 1.0;
    rho[0] = rho_even;
    let mut rho_odd = 1.0 - (mean_var - mean_at(1)) / var_plus;
    rho[1] = rho_odd;
    // Geyer's initial positive sequence: pairs of lags while their sum is
    // positive.
    while t + 5 < n && !(rho_even + rho_odd).is_nan() && rho_even + rho_odd > 0.0 {
        t += 2;
        rho_even = 1.0 - (mean_var - mean_at(t)) / var_plus;
        rho_odd = 1.0 - (mean_var - mean_at(t + 1)) / var_plus;
        if rho_even + rho_odd >= 0.0 {
            rho[t] = rho_even;
            rho[t + 1] = rho_odd;
        }
    }
    let max_t = t;
    // The improved estimate: a positive last even lag counts half.
    if rho_even > 0.0 {
        rho[max_t] = rho_even;
    }
    // Geyer's initial monotone sequence: no pair larger than the one before.
    let mut t = 0;
    while t + 4 <= max_t {
        t += 2;
        if rho[t] + rho[t + 1] > rho[t - 2] + rho[t - 1] {
            rho[t] = (rho[t - 2] + rho[t - 1]) / 2.0;
            rho[t + 1] = rho[t];
        }
    }
    let draws = (m * n) as f64;
    let tau = (-1.0 + 2.0 * rho[..max_t].iter().sum::<f64>() + rho[max_t]).max(1.0 / draws.log10());
    draws / tau
}

/// The biased autocovariance of `x` at every lag `0..n` — `Σ (xᵢ − x̄)(xᵢ₊ₜ −
/// x̄) / n` — through an FFT of the zero-padded, centred series.
fn autocovariance(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    let size = (2 * n).next_power_of_two();
    let centre = mean(x);
    let mut data: Vec<(f64, f64)> = x
        .iter()
        .map(|v| (v - centre, 0.0))
        .chain(std::iter::repeat((0.0, 0.0)))
        .take(size)
        .collect();
    fft(&mut data, false);
    for z in &mut data {
        *z = (z.0 * z.0 + z.1 * z.1, 0.0);
    }
    fft(&mut data, true);
    // The inverse transform is unnormalised: divide by its length, then by n
    // for the biased estimate.
    data[..n]
        .iter()
        .map(|z| z.0 / size as f64 / n as f64)
        .collect()
}

/// An in-place iterative radix-2 FFT of a power-of-two length; `inverse`
/// conjugates the twiddles and leaves the result unnormalised.
fn fft(data: &mut [(f64, f64)], inverse: bool) {
    let n = data.len();
    debug_assert!(n.is_power_of_two());
    // Bit-reversal permutation.
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            data.swap(i, j);
        }
    }
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut len = 2;
    while len <= n {
        let angle = sign * 2.0 * std::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                // The twiddle computed per k rather than by repeated
                // multiplication, which drifts over long transforms.
                let (s, c) = (angle * k as f64).sin_cos();
                let (a, b) = (data[start + k], data[start + k + len / 2]);
                let t = (b.0 * c - b.1 * s, b.0 * s + b.1 * c);
                data[start + k] = (a.0 + t.0, a.1 + t.1);
                data[start + k + len / 2] = (a.0 - t.0, a.1 - t.1);
            }
        }
        len <<= 1;
    }
}

/// A copy of `values` in ascending order.
fn sorted(values: &[f64]) -> Vec<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted
}

/// The `p` quantile of ascending `sorted` values, interpolating linearly
/// between order statistics (R's type 7); NaN for no values.
pub fn quantile(sorted: &[f64], p: f64) -> f64 {
    match sorted.len() {
        0 => f64::NAN,
        1 => sorted[0],
        n => {
            let h = (n - 1) as f64 * p;
            let lo = h.floor() as usize;
            let hi = (lo + 1).min(n - 1);
            sorted[lo] + (h - lo as f64) * (sorted[hi] - sorted[lo])
        }
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// The sample variance (`n − 1`); NaN for fewer than two values.
fn variance(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return f64::NAN;
    }
    let m = mean(values);
    values.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (values.len() - 1) as f64
}

fn sd(values: &[f64]) -> f64 {
    variance(values).sqrt()
}

#[cfg(test)]
mod tests;
