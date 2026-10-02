//! The hypothesis tests of the goals document's table (analytics TODO
//! A2.12), as pure functions: from sufficient statistics where the test
//! allows it (counts, means and deviations, the centred sums of two columns —
//! what SQL computes over any number of rows), and from the values where it
//! does not (the rank tests, Shapiro–Wilk, logistic regression — read from a
//! sample above [`TEST_SAMPLE`](super::TEST_SAMPLE)).
//!
//! Every test answers the same [`TestResult`]: the statistic, its degrees of
//! freedom, the two-sided p-value, an estimate with its confidence interval,
//! and an effect size. A test that cannot be computed on what it is given
//! answers a sentence saying why.
//!
//! The conventions are R's, so a result here is the one `t.test`,
//! `wilcox.test`, `TukeyHSD` and the rest give for the same data
//! (`tests/r/test_reference.R` records them):
//!
//! - **t-tests** are Welch's for two groups (`t.test`'s default); a mean's
//!   interval is Student's.
//! - **Rank tests** are exact below 50 values (each sample, for the rank-sum
//!   test) when there are no ties (or zeros, for the signed-rank test), and
//!   otherwise the normal approximation with a continuity correction. Their
//!   estimate is the Hodges–Lehmann estimate — the median of the pairwise
//!   differences (rank-sum) or of the Walsh averages (signed-rank) — with
//!   R's exact interval or, with ties or many values, R's interval from the
//!   approximation, found by bisection where R uses `uniroot`.
//! - **The chi-square test of independence** has no continuity correction (as
//!   JMP and SPSS report it, `chisq.test(correct = FALSE)` in R): Fisher's
//!   exact test is beside it for the small tables the correction is for.
//! - **Fisher's exact test** sums the probabilities of the tables no more
//!   likely than the one observed, as R does; for a 2 × 2 table it estimates
//!   the conditional maximum-likelihood odds ratio, with R's interval.
//! - **Logistic regression** is tested by the likelihood ratio (the drop in
//!   deviance), and its odds ratio has the Wald interval (`confint.default`).
//!
//! Effect sizes are the conventional ones, named in [`Effect::kind`]:
//! Cohen's d (with the pooled deviation for two groups, of the differences
//! when paired), η² for an analysis of variance, ε² = H/(n − 1) for
//! Kruskal–Wallis, the rank-biserial correlation for the rank tests,
//! Cohen's w and h, Cramér's V, R² and McFadden's pseudo-R².

use serde::Serialize;
use statrs::distribution::{ChiSquared, ContinuousCDF, FisherSnedecor, StudentsT};
use statrs::function::beta::beta_reg;
use statrs::function::gamma::ln_gamma;

use super::dist::{
    RankSum, SignedRank, pnorm, pnorm_upper, prho, ptukey, qnorm, qtukey, shapiro_wilk,
};
use crate::plot::LinearSums;

/// Which test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestKind {
    /// Student's one-sample t-test.
    OneSampleT,
    /// Shapiro and Wilk's test of normality.
    ShapiroWilk,
    /// The Wilcoxon signed-rank test of one sample's location.
    SignedRank,
    /// The chi-square test that every value is as common.
    ChiSquareFit,
    /// The exact binomial test of a proportion.
    Binomial,
    /// Welch's two-sample t-test.
    WelchT,
    /// The Wilcoxon rank-sum (Mann–Whitney) test.
    MannWhitney,
    /// One-way analysis of variance.
    Anova,
    /// The Kruskal–Wallis rank-sum test.
    KruskalWallis,
    /// Levene's test of equal variances (Brown and Forsythe's, about the
    /// medians).
    Levene,
    /// Pearson's chi-square test of independence.
    ChiSquareIndependence,
    /// Fisher's exact test.
    FisherExact,
    /// Pearson's correlation.
    Pearson,
    /// Spearman's rank correlation.
    Spearman,
    /// Simple linear regression.
    LinearRegression,
    /// Simple logistic regression.
    LogisticRegression,
    /// The paired t-test.
    PairedT,
    /// The Wilcoxon signed-rank test of paired differences.
    PairedSignedRank,
}

impl TestKind {
    /// Its English name (the Analytics UI translates by the JSON name).
    pub fn label(self) -> &'static str {
        match self {
            TestKind::OneSampleT => "One-sample t-test",
            TestKind::ShapiroWilk => "Shapiro-Wilk normality test",
            TestKind::SignedRank => "Wilcoxon signed-rank test",
            TestKind::ChiSquareFit => "Chi-square goodness of fit",
            TestKind::Binomial => "Binomial test",
            TestKind::WelchT => "Welch's t-test",
            TestKind::MannWhitney => "Mann-Whitney test",
            TestKind::Anova => "One-way ANOVA",
            TestKind::KruskalWallis => "Kruskal-Wallis test",
            TestKind::Levene => "Levene's test",
            TestKind::ChiSquareIndependence => "Chi-square test of independence",
            TestKind::FisherExact => "Fisher's exact test",
            TestKind::Pearson => "Pearson correlation",
            TestKind::Spearman => "Spearman correlation",
            TestKind::LinearRegression => "Linear regression",
            TestKind::LogisticRegression => "Logistic regression",
            TestKind::PairedT => "Paired t-test",
            TestKind::PairedSignedRank => "Wilcoxon signed-rank test (paired)",
        }
    }
}

/// A test statistic and its usual symbol.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Statistic {
    /// `t`, `W`, `V`, `F`, `H`, `X²`, `r`, `ρ`, `G²`.
    pub symbol: &'static str,
    /// Its value.
    pub value: f64,
}

/// What a test estimates, with its confidence interval.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Estimate {
    /// What it is: `mean`, `mean_difference`, `location_shift`,
    /// `pseudomedian`, `proportion`, `odds_ratio`, `correlation`, `slope`.
    pub of: &'static str,
    /// Its value.
    pub value: f64,
    /// The interval's lower end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lower: Option<f64>,
    /// The interval's upper end; absent with `lower` present when it is
    /// infinite (an odds ratio's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upper: Option<f64>,
    /// The interval's confidence level — less than asked for when an exact
    /// interval cannot reach it, as R says.
    pub level: f64,
}

/// An effect size.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Effect {
    /// Which: `cohens_d`, `eta_squared`, `epsilon_squared`, `rank_biserial`,
    /// `cohens_w`, `cohens_h`, `cramers_v`, `r_squared`, `mcfadden_r_squared`.
    pub kind: &'static str,
    /// Its value.
    pub value: f64,
}

/// A further number a test reports: a regression's intercept, a slope's
/// standard error.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Detail {
    /// What it is: `intercept`, `standard_error`, `z`.
    pub name: &'static str,
    /// Its value.
    pub value: f64,
}

/// What a test answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestResult {
    /// Which test.
    pub test: TestKind,
    /// The statistic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistic: Option<Statistic>,
    /// Its degrees of freedom: none, one, or two (an F statistic's).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub df: Vec<f64>,
    /// The two-sided p-value.
    pub p_value: f64,
    /// What it estimates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<Estimate>,
    /// The size of the effect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<Effect>,
    /// Further numbers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<Detail>,
    /// How many values (or rows) it was computed from.
    pub n: u64,
    /// How the p-value was found, where there is a choice: `exact`,
    /// `normal approximation`, `t approximation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<&'static str>,
    /// Whether it read a sample of the rows rather than all of them.
    pub sampled: bool,
}

impl TestResult {
    fn new(test: TestKind, n: u64, p_value: f64) -> TestResult {
        TestResult {
            test,
            statistic: None,
            df: Vec::new(),
            p_value: p_value.clamp(0.0, 1.0),
            estimate: None,
            effect: None,
            details: Vec::new(),
            n,
            method: None,
            sampled: false,
        }
    }

    fn statistic(mut self, symbol: &'static str, value: f64) -> TestResult {
        self.statistic = Some(Statistic { symbol, value });
        self
    }

    fn df(mut self, df: &[f64]) -> TestResult {
        self.df = df.to_vec();
        self
    }

    fn estimate(
        mut self,
        of: &'static str,
        value: f64,
        interval: Option<(f64, f64)>,
        level: f64,
    ) -> TestResult {
        self.estimate = Some(Estimate {
            of,
            value,
            lower: interval.map(|i| i.0).filter(|v| v.is_finite()),
            upper: interval.map(|i| i.1).filter(|v| v.is_finite()),
            level,
        });
        self
    }

    fn effect(mut self, kind: &'static str, value: f64) -> TestResult {
        if value.is_finite() {
            self.effect = Some(Effect { kind, value });
        }
        self
    }

    fn detail(mut self, name: &'static str, value: f64) -> TestResult {
        self.details.push(Detail { name, value });
        self
    }

    fn method(mut self, method: &'static str) -> TestResult {
        self.method = Some(method);
        self
    }
}

/// Why a test could not be computed, as a sentence.
pub type Refusal = String;

/// What SQL says about one group of numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moments {
    /// How many values are not missing.
    pub n: u64,
    /// Their mean.
    pub mean: f64,
    /// Their standard deviation (`n − 1` in the denominator); NaN for fewer
    /// than two.
    pub sd: f64,
}

impl Moments {
    /// The moments of `values`, as SQL would give them.
    pub fn of(values: &[f64]) -> Moments {
        let n = values.len();
        let mean = values.iter().sum::<f64>() / n as f64;
        let ss: f64 = values.iter().map(|v| (v - mean) * (v - mean)).sum();
        Moments {
            n: n as u64,
            mean,
            sd: if n >= 2 {
                (ss / (n - 1) as f64).sqrt()
            } else {
                f64::NAN
            },
        }
    }

    fn var(&self) -> f64 {
        self.sd * self.sd
    }
}

/// One pairwise comparison of Tukey's honest significant differences.
#[derive(Debug, Clone, PartialEq)]
pub struct Pairwise {
    /// The first group's index.
    pub a: usize,
    /// The second group's index.
    pub b: usize,
    /// The second group's mean less the first's (R's `diff`).
    pub difference: f64,
    /// The simultaneous interval's lower end.
    pub lower: f64,
    /// Its upper end.
    pub upper: f64,
    /// The adjusted p-value.
    pub p_value: f64,
}

// --- distributions' tails --------------------------------------------------------

fn t_two_sided(t: f64, df: f64) -> f64 {
    StudentsT::new(0.0, 1.0, df).map_or(f64::NAN, |d| (2.0 * d.sf(t.abs())).min(1.0))
}

fn t_quantile(p: f64, df: f64) -> f64 {
    StudentsT::new(0.0, 1.0, df).map_or(f64::NAN, |d| d.inverse_cdf(p))
}

fn chisq_upper(x: f64, df: f64) -> f64 {
    ChiSquared::new(df).map_or(f64::NAN, |d| d.sf(x))
}

fn f_upper(x: f64, df1: f64, df2: f64) -> f64 {
    FisherSnedecor::new(df1, df2).map_or(f64::NAN, |d| d.sf(x))
}

fn normal_two_sided(z: f64) -> f64 {
    (2.0 * pnorm(z).min(pnorm_upper(z))).min(1.0)
}

/// Whether `x` is more than `bound` — false for NaN, which a spread or a
/// determinant computed from too few values can be.
fn above(x: f64, bound: f64) -> bool {
    x > bound
}

/// The ranks of `values` (1-based, ties given their average rank) and the
/// sizes of the groups of ties.
pub fn ranks(values: &[f64]) -> (Vec<f64>, Vec<usize>) {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut out = vec![0.0; values.len()];
    let mut ties = Vec::new();
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && values[order[j + 1]] == values[order[i]] {
            j += 1;
        }
        let rank = (i + j) as f64 / 2.0 + 1.0;
        for &k in &order[i..=j] {
            out[k] = rank;
        }
        if j > i {
            ties.push(j - i + 1);
        }
        i = j + 1;
    }
    (out, ties)
}

fn tie_sum(ties: &[usize]) -> f64 {
    ties.iter().map(|&t| (t * t * t - t) as f64).sum()
}

fn median_of_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// The median of `values`.
pub fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    median_of_sorted(&sorted)
}

/// The root of a decreasing step function `f` between `lo` (where it is
/// positive) and `hi` (negative), to well within R's `tol.root` of 1e-4.
fn bisect(mut lo: f64, mut hi: f64, f: impl Fn(f64) -> f64) -> f64 {
    let tolerance = 1e-10 * (1.0 + lo.abs().max(hi.abs()));
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        if hi - lo <= tolerance {
            return mid;
        }
        if f(mid) > 0.0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

/// The most pairwise differences (or Walsh averages) a rank test's estimate
/// is the median of; with more it is found by bisection, as R finds it.
const MAX_PAIRS: usize = 2_000_000;

// --- one number ---------------------------------------------------------------------

/// Student's t-test that the mean is `mu`, from the values' moments.
pub fn one_sample_t(m: &Moments, mu: f64, level: f64) -> Result<TestResult, Refusal> {
    let (t, df, se) = one_t(m, mu)?;
    let half = t_quantile(0.5 + level / 2.0, df) * se;
    Ok(
        TestResult::new(TestKind::OneSampleT, m.n, t_two_sided(t, df))
            .statistic("t", t)
            .df(&[df])
            .estimate("mean", m.mean, Some((m.mean - half, m.mean + half)), level)
            .effect("cohens_d", (m.mean - mu) / m.sd),
    )
}

/// The paired t-test, from the moments of the differences.
pub fn paired_t(differences: &Moments, level: f64) -> Result<TestResult, Refusal> {
    let mut result = one_sample_t(differences, 0.0, level)?;
    result.test = TestKind::PairedT;
    if let Some(e) = &mut result.estimate {
        e.of = "mean_difference";
    }
    Ok(result)
}

fn one_t(m: &Moments, mu: f64) -> Result<(f64, f64, f64), Refusal> {
    if m.n < 2 {
        return Err("a t-test needs at least two values".to_owned());
    }
    let se = m.sd / (m.n as f64).sqrt();
    if !above(se, 10.0 * f64::EPSILON * m.mean.abs()) {
        return Err("every value is the same, so there is no spread to test against".to_owned());
    }
    Ok(((m.mean - mu) / se, m.n as f64 - 1.0, se))
}

/// Shapiro and Wilk's test that the values come from a normal distribution
/// (3 to 5,000 of them).
pub fn shapiro(values: &[f64]) -> Result<TestResult, Refusal> {
    if values.len() < 3 {
        return Err("a normality test needs at least three values".to_owned());
    }
    if values.len() > 5000 {
        return Err("a normality test reads at most 5,000 values".to_owned());
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let (w, p) = shapiro_wilk(&sorted)
        .ok_or_else(|| "every value is the same, so normality cannot be tested".to_owned())?;
    Ok(TestResult::new(TestKind::ShapiroWilk, values.len() as u64, p).statistic("W", w))
}

/// The Wilcoxon signed-rank test that the values' location is `mu`.
pub fn signed_rank(values: &[f64], mu: f64, level: f64) -> Result<TestResult, Refusal> {
    let shifted: Vec<f64> = values.iter().map(|v| v - mu).collect();
    let zeros = shifted.contains(&0.0);
    let x: Vec<f64> = shifted.into_iter().filter(|v| *v != 0.0).collect();
    let n = x.len();
    if n == 0 {
        return Err(
            "every value equals the one tested against, so there is nothing to rank".to_owned(),
        );
    }
    let abs: Vec<f64> = x.iter().map(|v| v.abs()).collect();
    let (r, ties) = ranks(&abs);
    let v: f64 = x
        .iter()
        .zip(&r)
        .filter(|(x, _)| **x > 0.0)
        .map(|(_, r)| r)
        .sum();
    let nf = n as f64;
    let total = nf * (nf + 1.0) / 2.0;
    let original: Vec<f64> = x.iter().map(|v| v + mu).collect();
    let alpha = 1.0 - level;
    let exact = n < 50 && ties.is_empty() && !zeros;
    let (p, method, estimate, interval, achieved) = if exact {
        let dist = SignedRank::new(n);
        let p = if v > nf * (nf + 1.0) / 4.0 {
            dist.upper(v - 1.0)
        } else {
            dist.lower(v)
        };
        let walsh = walsh_averages(&original);
        let mut qu = dist.quantile(alpha / 2.0);
        if qu == 0 {
            qu = 1;
        }
        let ql = n * (n + 1) / 2 - qu;
        let achieved_alpha = 2.0 * dist.lower(qu as f64 - 1.0);
        let level = if achieved_alpha - alpha > alpha / 2.0 {
            1.0 - signif2(achieved_alpha)
        } else {
            level
        };
        (
            (2.0 * p).min(1.0),
            "exact",
            median_of_sorted(&walsh),
            Some((walsh[qu - 1], walsh[ql])),
            level,
        )
    } else {
        let sigma = (nf * (nf + 1.0) * (2.0 * nf + 1.0) / 24.0 - tie_sum(&ties) / 48.0).sqrt();
        let z = v - nf * (nf + 1.0) / 4.0;
        let z = (z - z.signum() * 0.5) / sigma;
        let w = |d: f64, correct: bool| signed_rank_z(&original, d, correct);
        let (lo, hi) = (
            original.iter().copied().fold(f64::INFINITY, f64::min),
            original.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        );
        let (interval, level) = signed_rank_interval(lo, hi, alpha, level, &original, &w);
        let estimate = if n * (n + 1) / 2 <= MAX_PAIRS {
            median_of_sorted(&walsh_averages(&original))
        } else {
            bisect(lo, hi, |d| w(d, false))
        };
        (
            normal_two_sided(z),
            "normal approximation",
            estimate,
            interval,
            level,
        )
    };
    Ok(TestResult::new(TestKind::SignedRank, n as u64, p)
        .statistic("V", v)
        .estimate("pseudomedian", estimate, interval, achieved)
        .effect("rank_biserial", 2.0 * v / total - 1.0)
        .method(method))
}

/// The signed-rank test of paired differences.
pub fn paired_signed_rank(differences: &[f64], level: f64) -> Result<TestResult, Refusal> {
    let mut result = signed_rank(differences, 0.0, level)?;
    result.test = TestKind::PairedSignedRank;
    if let Some(e) = &mut result.estimate {
        e.of = "pseudomedian_difference";
    }
    Ok(result)
}

/// R's `signif(x, 2)`.
fn signif2(x: f64) -> f64 {
    if x == 0.0 {
        return 0.0;
    }
    let magnitude = 10f64.powf(x.abs().log10().floor() - 1.0);
    (x / magnitude).round() * magnitude
}

fn walsh_averages(x: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(x.len() * (x.len() + 1) / 2);
    for i in 0..x.len() {
        for j in i..x.len() {
            out.push((x[i] + x[j]) / 2.0);
        }
    }
    out.sort_by(f64::total_cmp);
    out
}

/// The standardised signed-rank statistic of `x − d`: R's `W(d)`.
fn signed_rank_z(x: &[f64], d: f64, correct: bool) -> f64 {
    let xd: Vec<f64> = x.iter().map(|v| v - d).filter(|v| *v != 0.0).collect();
    let nx = xd.len() as f64;
    if xd.is_empty() {
        return 0.0;
    }
    let abs: Vec<f64> = xd.iter().map(|v| v.abs()).collect();
    let (r, ties) = ranks(&abs);
    let zd: f64 = xd
        .iter()
        .zip(&r)
        .filter(|(x, _)| **x > 0.0)
        .map(|(_, r)| r)
        .sum::<f64>()
        - nx * (nx + 1.0) / 4.0;
    let sigma = (nx * (nx + 1.0) * (2.0 * nx + 1.0) / 24.0 - tie_sum(&ties) / 48.0).sqrt();
    if sigma == 0.0 {
        return 0.0;
    }
    let correction = if correct { zd.signum() * 0.5 } else { 0.0 };
    (zd - correction) / sigma
}

/// R's asymptotic interval for the signed-rank test: alpha doubled until the
/// ends can be reached, then each end by bisection.
fn signed_rank_interval(
    lo: f64,
    hi: f64,
    alpha: f64,
    level: f64,
    x: &[f64],
    w: &dyn Fn(f64, bool) -> f64,
) -> (Option<(f64, f64)>, f64) {
    let (w_lo, w_hi) = (w(lo, true), w(hi, true));
    let mut alpha = alpha;
    loop {
        let mindiff = w_lo - qnorm(1.0 - alpha / 2.0);
        let maxdiff = w_hi - qnorm(alpha / 2.0);
        if mindiff < 0.0 || maxdiff > 0.0 {
            alpha *= 2.0;
            if alpha >= 1.0 {
                break;
            }
        } else {
            break;
        }
    }
    let level = if alpha >= 1.0 || 1.0 - level < alpha * 0.75 {
        1.0 - alpha.min(1.0)
    } else {
        level
    };
    if alpha >= 1.0 {
        let m = median(x);
        return (Some((m, m)), level);
    }
    let root = |zq: f64| bisect(lo, hi, |d| w(d, true) - zq);
    (
        Some((root(qnorm(1.0 - alpha / 2.0)), root(qnorm(alpha / 2.0)))),
        level,
    )
}

// --- one category ------------------------------------------------------------------

/// The chi-square test that every value is as common, from each value's
/// count.
pub fn chisq_fit(counts: &[u64]) -> Result<TestResult, Refusal> {
    if counts.len() < 2 {
        return Err("a goodness-of-fit test needs at least two values to compare".to_owned());
    }
    let n: u64 = counts.iter().sum();
    let expected = n as f64 / counts.len() as f64;
    let x2: f64 = counts
        .iter()
        .map(|&c| (c as f64 - expected).powi(2) / expected)
        .sum();
    let df = counts.len() as f64 - 1.0;
    Ok(
        TestResult::new(TestKind::ChiSquareFit, n, chisq_upper(x2, df))
            .statistic("X²", x2)
            .df(&[df])
            .effect("cohens_w", (x2 / n as f64).sqrt()),
    )
}

fn ln_choose(n: f64, k: f64) -> f64 {
    ln_gamma(n + 1.0) - ln_gamma(k + 1.0) - ln_gamma(n - k + 1.0)
}

fn ln_dbinom(x: u64, n: u64, p: f64) -> f64 {
    let (xf, nf) = (x as f64, n as f64);
    let a = if x == 0 { 0.0 } else { xf * p.ln() };
    let b = if x == n {
        0.0
    } else {
        (nf - xf) * (1.0 - p).ln()
    };
    ln_choose(nf, xf) + a + b
}

/// P(X ≤ x) for a binomial of `n` trials with chance `p`.
fn pbinom(x: i64, n: u64, p: f64) -> f64 {
    if x < 0 {
        return 0.0;
    }
    if x as u64 >= n {
        return 1.0;
    }
    // P(X ≤ x) = I_{1−p}(n − x, x + 1).
    beta_reg((n - x as u64) as f64, x as f64 + 1.0, 1.0 - p)
}

/// P(X > x) for a binomial of `n` trials with chance `p`, computed directly
/// so that a tiny tail is not lost to `1 − P(X ≤ x)`.
fn pbinom_upper(x: i64, n: u64, p: f64) -> f64 {
    if x < 0 {
        return 1.0;
    }
    if x as u64 >= n {
        return 0.0;
    }
    // P(X > x) = I_p(x + 1, n − x).
    beta_reg(x as f64 + 1.0, (n - x as u64) as f64, p)
}

/// The `p` quantile of a beta distribution, by bisection on the regularised
/// incomplete beta function: the ends of a Clopper–Pearson interval.
fn qbeta(p: f64, a: f64, b: f64) -> f64 {
    let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        if beta_reg(a, b, mid) < p {
            lo = mid;
        } else {
            hi = mid;
        }
        if hi - lo < 1e-17 {
            break;
        }
    }
    (lo + hi) / 2.0
}

/// The exact binomial test that the chance of a success is `p0`, from `x`
/// successes in `n` trials, with the Clopper–Pearson interval — R's
/// `binom.test`.
pub fn binomial(x: u64, n: u64, p0: f64, level: f64) -> Result<TestResult, Refusal> {
    if n == 0 {
        return Err("a binomial test needs at least one value".to_owned());
    }
    let rel_err: f64 = 1.0 + 1e-7;
    let d = ln_dbinom(x, n, p0);
    let m = n as f64 * p0;
    let xf = x as f64;
    let p = if xf == m {
        1.0
    } else if xf < m {
        // How many of ceil(m)..=n are no more likely than x: dbinom falls
        // from the mode, so find where it drops below.
        let from = m.ceil() as u64;
        let first = partition_point(from, n, |i| ln_dbinom(i, n, p0) > d + rel_err.ln());
        let y = n + 1 - first;
        pbinom(x as i64, n, p0) + pbinom_upper((n - y) as i64, n, p0)
    } else {
        let to = m.floor() as u64;
        // dbinom rises to the mode on 0..=floor(m).
        let first_above = partition_point(0, to, |i| ln_dbinom(i, n, p0) <= d + rel_err.ln());
        let y = first_above;
        pbinom(y as i64 - 1, n, p0) + pbinom_upper(x as i64 - 1, n, p0)
    };
    let alpha = (1.0 - level) / 2.0;
    let lower = if x == 0 {
        0.0
    } else {
        qbeta(alpha, xf, (n - x) as f64 + 1.0)
    };
    let upper = if x == n {
        1.0
    } else {
        qbeta(1.0 - alpha, xf + 1.0, (n - x) as f64)
    };
    let estimate = xf / n as f64;
    Ok(TestResult::new(TestKind::Binomial, n, p.min(1.0))
        .estimate("proportion", estimate, Some((lower, upper)), level)
        .effect(
            "cohens_h",
            2.0 * estimate.sqrt().asin() - 2.0 * p0.sqrt().asin(),
        )
        .method("exact"))
}

/// The first `i` in `from..=to` for which `holds` is false (`to + 1` when it
/// always holds), for a predicate that holds then stops holding.
fn partition_point(from: u64, to: u64, holds: impl Fn(u64) -> bool) -> u64 {
    let (mut lo, mut hi) = (from, to + 1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if holds(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

// --- a number by groups ---------------------------------------------------------------

/// Welch's t-test that two groups' means are equal, from their moments: the
/// estimate is the first's mean less the second's.
pub fn welch_t(a: &Moments, b: &Moments, level: f64) -> Result<TestResult, Refusal> {
    if a.n < 2 || b.n < 2 {
        return Err("a t-test needs at least two values in each group".to_owned());
    }
    let (sa, sb) = (a.var() / a.n as f64, b.var() / b.n as f64);
    let se = (sa + sb).sqrt();
    if !above(se, 10.0 * f64::EPSILON * a.mean.abs().max(b.mean.abs())) {
        return Err("every value is the same, so there is no spread to test against".to_owned());
    }
    let df = (sa + sb).powi(2) / (sa * sa / (a.n as f64 - 1.0) + sb * sb / (b.n as f64 - 1.0));
    let difference = a.mean - b.mean;
    let t = difference / se;
    let half = t_quantile(0.5 + level / 2.0, df) * se;
    let pooled = (((a.n as f64 - 1.0) * a.var() + (b.n as f64 - 1.0) * b.var())
        / (a.n + b.n - 2) as f64)
        .sqrt();
    Ok(
        TestResult::new(TestKind::WelchT, a.n + b.n, t_two_sided(t, df))
            .statistic("t", t)
            .df(&[df])
            .estimate(
                "mean_difference",
                difference,
                Some((difference - half, difference + half)),
                level,
            )
            .effect("cohens_d", difference / pooled),
    )
}

/// The Wilcoxon rank-sum (Mann–Whitney) test that two groups' values are
/// alike: `W` is the first group's rank sum less its least possible, and the
/// estimate is the shift of the first group's values from the second's.
pub fn mann_whitney(x: &[f64], y: &[f64], level: f64) -> Result<TestResult, Refusal> {
    let (m, n) = (x.len(), y.len());
    if m == 0 || n == 0 {
        return Err("a rank-sum test needs values in each group".to_owned());
    }
    let (mf, nf) = (m as f64, n as f64);
    let combined: Vec<f64> = x.iter().chain(y).copied().collect();
    let (r, ties) = ranks(&combined);
    let w = r[..m].iter().sum::<f64>() - mf * (mf + 1.0) / 2.0;
    let alpha = 1.0 - level;
    let exact = m < 50 && n < 50 && ties.is_empty();
    let pairs = |x: &[f64]| -> Vec<f64> {
        let mut d: Vec<f64> = x
            .iter()
            .flat_map(|a| y.iter().map(move |b| a - b))
            .collect();
        d.sort_by(f64::total_cmp);
        d
    };
    let (p, method, estimate, interval, achieved) = if exact {
        let dist = RankSum::new(m, n);
        let p = if w > mf * nf / 2.0 {
            dist.upper(w - 1.0)
        } else {
            dist.lower(w)
        };
        let diffs = pairs(x);
        let mut qu = dist.quantile(alpha / 2.0);
        if qu == 0 {
            qu = 1;
        }
        let ql = m * n - qu;
        let achieved_alpha = 2.0 * dist.lower(qu as f64 - 1.0);
        let level = if achieved_alpha - alpha > alpha / 2.0 {
            1.0 - achieved_alpha
        } else {
            level
        };
        (
            (2.0 * p).min(1.0),
            "exact",
            median_of_sorted(&diffs),
            Some((diffs[qu - 1], diffs[ql])),
            level,
        )
    } else {
        let sigma = rank_sum_sigma(mf, nf, &ties);
        let z = w - mf * nf / 2.0;
        let z = (z - z.signum() * 0.5) / sigma;
        let wd = |d: f64, correct: bool| rank_sum_z(x, y, d, correct);
        let lo = x.iter().copied().fold(f64::INFINITY, f64::min)
            - y.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let hi = x.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - y.iter().copied().fold(f64::INFINITY, f64::min);
        let (w_lo, w_hi) = (wd(lo, true), wd(hi, true));
        let root = |zq: f64| {
            if w_lo - zq <= 0.0 {
                lo
            } else if w_hi - zq >= 0.0 {
                hi
            } else {
                bisect(lo, hi, |d| wd(d, true) - zq)
            }
        };
        let interval = (root(qnorm(1.0 - alpha / 2.0)), root(qnorm(alpha / 2.0)));
        let estimate = if m * n <= MAX_PAIRS {
            median_of_sorted(&pairs(x))
        } else {
            bisect(lo, hi, |d| wd(d, false))
        };
        (
            normal_two_sided(z),
            "normal approximation",
            estimate,
            Some(interval),
            level,
        )
    };
    Ok(TestResult::new(TestKind::MannWhitney, (m + n) as u64, p)
        .statistic("W", w)
        .estimate("location_shift", estimate, interval, achieved)
        .effect("rank_biserial", 2.0 * w / (mf * nf) - 1.0)
        .method(method))
}

fn rank_sum_sigma(m: f64, n: f64, ties: &[usize]) -> f64 {
    ((m * n / 12.0) * ((m + n + 1.0) - tie_sum(ties) / ((m + n) * (m + n - 1.0)))).sqrt()
}

/// The standardised rank-sum statistic of `x − d` against `y`: R's `W(d)`.
fn rank_sum_z(x: &[f64], y: &[f64], d: f64, correct: bool) -> f64 {
    let (m, n) = (x.len() as f64, y.len() as f64);
    let combined: Vec<f64> = x.iter().map(|v| v - d).chain(y.iter().copied()).collect();
    let (r, ties) = ranks(&combined);
    let dz = r[..x.len()].iter().sum::<f64>() - m * (m + 1.0) / 2.0 - m * n / 2.0;
    let correction = if correct { dz.signum() * 0.5 } else { 0.0 };
    let sigma = rank_sum_sigma(m, n, &ties);
    if sigma == 0.0 {
        return 0.0;
    }
    (dz - correction) / sigma
}

/// The sums of squares of a one-way layout: (between, within).
fn sums_of_squares(groups: &[Moments]) -> (f64, f64) {
    let n: f64 = groups.iter().map(|g| g.n as f64).sum();
    let grand = groups.iter().map(|g| g.n as f64 * g.mean).sum::<f64>() / n;
    let between = groups
        .iter()
        .map(|g| g.n as f64 * (g.mean - grand).powi(2))
        .sum();
    let within = groups
        .iter()
        .filter(|g| g.n >= 2)
        .map(|g| (g.n as f64 - 1.0) * g.var())
        .sum();
    (between, within)
}

/// One-way analysis of variance of the groups' means, from their moments.
pub fn anova(groups: &[Moments]) -> Result<TestResult, Refusal> {
    let k = groups.len();
    let n: u64 = groups.iter().map(|g| g.n).sum();
    if k < 2 {
        return Err("an analysis of variance needs at least two groups".to_owned());
    }
    if n <= k as u64 {
        return Err("an analysis of variance needs more values than groups".to_owned());
    }
    let (between, within) = sums_of_squares(groups);
    let (df1, df2) = (k as f64 - 1.0, (n - k as u64) as f64);
    if !above(within, 0.0) {
        return Err(
            "every group's values are all the same, so there is no spread to test against"
                .to_owned(),
        );
    }
    let f = (between / df1) / (within / df2);
    Ok(TestResult::new(TestKind::Anova, n, f_upper(f, df1, df2))
        .statistic("F", f)
        .df(&[df1, df2])
        .effect("eta_squared", between / (between + within)))
}

/// Tukey's honest significant differences between every pair of groups,
/// from their moments: the pairs in R's order (each later group against
/// each earlier one).
pub fn tukey_hsd(groups: &[Moments], level: f64) -> Result<Vec<Pairwise>, Refusal> {
    let k = groups.len();
    let n: u64 = groups.iter().map(|g| g.n).sum();
    if k < 2 || n <= k as u64 {
        return Err(
            "pairwise comparisons need at least two groups and more values than groups".to_owned(),
        );
    }
    let (_, within) = sums_of_squares(groups);
    let df = (n - k as u64) as f64;
    if df < 2.0 {
        return Err("pairwise comparisons need at least two more values than groups".to_owned());
    }
    let mse = within / df;
    let q = qtukey(level, k as f64, df)
        .ok_or_else(|| "the studentized range has no quantile here".to_owned())?;
    let mut out = Vec::new();
    for a in 0..k {
        for b in a + 1..k {
            let se = ((mse / 2.0) * (1.0 / groups[a].n as f64 + 1.0 / groups[b].n as f64)).sqrt();
            let difference = groups[b].mean - groups[a].mean;
            let p =
                ptukey((difference / se).abs(), 1.0, k as f64, df).map_or(f64::NAN, |p| 1.0 - p);
            out.push(Pairwise {
                a,
                b,
                difference,
                lower: difference - q * se,
                upper: difference + q * se,
                p_value: p.clamp(0.0, 1.0),
            });
        }
    }
    // R orders the pairs by the second group, then the first.
    out.sort_by_key(|p| (p.a, p.b));
    Ok(out)
}

/// The Kruskal–Wallis test that the groups' values are alike.
pub fn kruskal_wallis(groups: &[Vec<f64>]) -> Result<TestResult, Refusal> {
    let groups: Vec<&Vec<f64>> = groups.iter().filter(|g| !g.is_empty()).collect();
    let k = groups.len();
    if k < 2 {
        return Err("a Kruskal-Wallis test needs at least two groups".to_owned());
    }
    let all: Vec<f64> = groups.iter().flat_map(|g| g.iter().copied()).collect();
    let n = all.len() as f64;
    if n < 2.0 {
        return Err("a Kruskal-Wallis test needs at least two values".to_owned());
    }
    let (r, ties) = ranks(&all);
    let mut at = 0;
    let mut sum = 0.0;
    for g in &groups {
        let s: f64 = r[at..at + g.len()].iter().sum();
        sum += s * s / g.len() as f64;
        at += g.len();
    }
    let denominator = 1.0 - tie_sum(&ties) / (n * n * n - n);
    if !above(denominator, 0.0) {
        return Err("every value is the same, so there is nothing to rank".to_owned());
    }
    let h = (12.0 * sum / (n * (n + 1.0)) - 3.0 * (n + 1.0)) / denominator;
    let df = k as f64 - 1.0;
    Ok(
        TestResult::new(TestKind::KruskalWallis, n as u64, chisq_upper(h, df))
            .statistic("H", h)
            .df(&[df])
            .effect("epsilon_squared", h / (n - 1.0)),
    )
}

/// Levene's test that the groups' variances are equal, in Brown and
/// Forsythe's form: an analysis of variance of each value's distance from its
/// group's median.
pub fn levene(groups: &[Vec<f64>]) -> Result<TestResult, Refusal> {
    let deviations: Vec<Moments> = groups
        .iter()
        .filter(|g| !g.is_empty())
        .map(|g| {
            let m = median(g);
            Moments::of(&g.iter().map(|v| (v - m).abs()).collect::<Vec<_>>())
        })
        .collect();
    let mut result = anova(&deviations)?;
    result.test = TestKind::Levene;
    result.effect = None;
    Ok(result)
}

// --- two categories ---------------------------------------------------------------------

/// A contingency table's rows and columns with a total of zero taken out.
fn trimmed(table: &[Vec<u64>]) -> Vec<Vec<u64>> {
    let columns = table.first().map_or(0, Vec::len);
    let keep: Vec<usize> = (0..columns)
        .filter(|&j| table.iter().any(|row| row[j] > 0))
        .collect();
    table
        .iter()
        .filter(|row| row.iter().any(|&c| c > 0))
        .map(|row| keep.iter().map(|&j| row[j]).collect())
        .collect()
}

/// Each cell's count expected were rows and columns independent.
pub fn expected_counts(table: &[Vec<u64>]) -> Vec<Vec<f64>> {
    let n: u64 = table.iter().flatten().sum();
    let columns = table.first().map_or(0, Vec::len);
    let col_sums: Vec<u64> = (0..columns)
        .map(|j| table.iter().map(|r| r[j]).sum())
        .collect();
    table
        .iter()
        .map(|row| {
            let r: u64 = row.iter().sum();
            col_sums
                .iter()
                .map(|&c| r as f64 * c as f64 / n as f64)
                .collect()
        })
        .collect()
}

/// Pearson's chi-square test that rows and columns are independent, without
/// a continuity correction.
pub fn chisq_independence(table: &[Vec<u64>]) -> Result<TestResult, Refusal> {
    let table = trimmed(table);
    let (r, c) = (table.len(), table.first().map_or(0, Vec::len));
    if r < 2 || c < 2 {
        return Err("a test of independence needs at least two values of each column".to_owned());
    }
    let n: u64 = table.iter().flatten().sum();
    let expected = expected_counts(&table);
    let x2: f64 = table
        .iter()
        .zip(&expected)
        .flat_map(|(o, e)| o.iter().zip(e).map(|(&o, &e)| (o as f64 - e).powi(2) / e))
        .sum();
    let df = ((r - 1) * (c - 1)) as f64;
    let k = r.min(c) as f64;
    Ok(
        TestResult::new(TestKind::ChiSquareIndependence, n, chisq_upper(x2, df))
            .statistic("X²", x2)
            .df(&[df])
            .effect("cramers_v", (x2 / (n as f64 * (k - 1.0))).sqrt()),
    )
}

/// The most partial tables Fisher's exact test visits before it gives up.
pub const MAX_FISHER_TABLES: u64 = 2_000_000;

/// Fisher's exact test that rows and columns are independent. For a 2 × 2
/// table, with the conditional maximum-likelihood odds ratio of the first
/// row's first column, and its interval.
pub fn fisher_exact(table: &[Vec<u64>], level: f64) -> Result<TestResult, Refusal> {
    let table = trimmed(table);
    let (r, c) = (table.len(), table.first().map_or(0, Vec::len));
    if r < 2 || c < 2 {
        return Err("a test of independence needs at least two values of each column".to_owned());
    }
    let n: u64 = table.iter().flatten().sum();
    if r == 2 && c == 2 {
        return Ok(fisher_2x2(&table, level));
    }
    let p = fisher_rxc(&table).ok_or_else(|| {
        "the table has too many possible arrangements for an exact test; the chi-square test applies"
            .to_owned()
    })?;
    Ok(TestResult::new(TestKind::FisherExact, n, p).method("exact"))
}

fn fisher_2x2(t: &[Vec<u64>], level: f64) -> TestResult {
    let x = t[0][0] as i64;
    let m = (t[0][0] + t[1][0]) as i64;
    let nn = (t[0][1] + t[1][1]) as i64;
    let k = (t[0][0] + t[0][1]) as i64;
    let lo = 0.max(k - nn);
    let hi = k.min(m);
    let support: Vec<i64> = (lo..=hi).collect();
    let logdc: Vec<f64> = support
        .iter()
        .map(|&s| {
            ln_choose(m as f64, s as f64) + ln_choose(nn as f64, (k - s) as f64)
                - ln_choose((m + nn) as f64, k as f64)
        })
        .collect();
    // The noncentral hypergeometric distribution with odds ratio exp(θ).
    let dnhyper = |theta: f64| -> Vec<f64> {
        let d: Vec<f64> = logdc
            .iter()
            .zip(&support)
            .map(|(l, &s)| l + theta * s as f64)
            .collect();
        let max = d.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let e: Vec<f64> = d.iter().map(|v| (v - max).exp()).collect();
        let sum: f64 = e.iter().sum();
        e.into_iter().map(|v| v / sum).collect()
    };
    let mean = |theta: f64| -> f64 {
        dnhyper(theta)
            .iter()
            .zip(&support)
            .map(|(p, &s)| p * s as f64)
            .sum()
    };
    let lower_tail = |theta: f64| -> f64 {
        dnhyper(theta)
            .iter()
            .zip(&support)
            .filter(|(_, s)| **s <= x)
            .map(|(p, _)| p)
            .sum()
    };
    let upper_tail = |theta: f64| -> f64 {
        dnhyper(theta)
            .iter()
            .zip(&support)
            .filter(|(_, s)| **s >= x)
            .map(|(p, _)| p)
            .sum()
    };
    let d = dnhyper(0.0);
    let observed = d[(x - lo) as usize] * (1.0 + 1e-7);
    let p: f64 = d.iter().filter(|v| **v <= observed).sum();
    // On θ = log odds ratio, each function is monotone: solve by bisection.
    let solve = |f: &dyn Fn(f64) -> f64, target: f64, increasing: bool| -> f64 {
        let (mut a, mut b) = (-60.0_f64, 60.0_f64);
        for _ in 0..200 {
            let mid = (a + b) / 2.0;
            let above = f(mid) > target;
            if above == increasing {
                b = mid;
            } else {
                a = mid;
            }
        }
        ((a + b) / 2.0).exp()
    };
    let estimate = if x == lo {
        0.0
    } else if x == hi {
        f64::INFINITY
    } else {
        solve(&mean, x as f64, true)
    };
    let alpha = (1.0 - level) / 2.0;
    let upper = if x == hi {
        f64::INFINITY
    } else {
        solve(&lower_tail, alpha, false)
    };
    let lower = if x == lo {
        0.0
    } else {
        solve(&upper_tail, alpha, true)
    };
    let mut result =
        TestResult::new(TestKind::FisherExact, (m + nn) as u64, p.min(1.0)).method("exact");
    result.estimate = Some(Estimate {
        of: "odds_ratio",
        value: estimate,
        lower: Some(lower),
        upper: upper.is_finite().then_some(upper),
        level,
    });
    result
}

/// The p-value of Fisher's exact test of an r × c table: the total
/// probability of the tables with its margins that are no more likely than
/// it. `None` when the search would visit more than [`MAX_FISHER_TABLES`]
/// partial tables.
///
/// The search is a simplified form of the network algorithm R's FEXACT uses
/// (Mehta & Patel 1983): the table is filled a column at a time, and the
/// partial tables that leave the rows the same totals (in any order) are
/// merged, keeping each distinct value of their probability once with its
/// multiplicity. At each merged node, bounds on what the remaining columns can
/// add — each column on its own, ignoring the others — settle at once every
/// past value whose completions are all in the tail (their whole probability
/// is added in closed form) or all out of it (dropped).
fn fisher_rxc(table: &[Vec<u64>]) -> Option<f64> {
    use std::collections::HashMap;
    // Rows are the state, so the shorter side goes there.
    let table: Vec<Vec<u64>> = if table.len() > table[0].len() {
        (0..table[0].len())
            .map(|j| table.iter().map(|row| row[j]).collect())
            .collect()
    } else {
        table.to_vec()
    };
    let c = table[0].len();
    let rows: Vec<u64> = table.iter().map(|row| row.iter().sum()).collect();
    let mut cols: Vec<u64> = (0..c)
        .map(|j| table.iter().map(|row| row[j]).sum())
        .collect();
    // The two largest columns last: they are settled together, in closed
    // form, rather than enumerated for every past value.
    cols.sort_unstable();
    let n: u64 = rows.iter().sum();
    let mut lf = vec![0.0_f64; n as usize + 1];
    for i in 1..=n as usize {
        lf[i] = lf[i - 1] + (i as f64).ln();
    }
    let lfac = |v: u64| lf[v as usize];
    // log P(table) = constant − Σ log n_ij!
    let constant: f64 = rows.iter().map(|&v| lfac(v)).sum::<f64>()
        + cols.iter().map(|&v| lfac(v)).sum::<f64>()
        - lfac(n);
    let observed = constant - table.iter().flatten().map(|&v| lfac(v)).sum::<f64>();
    let threshold = observed + (1.0 + 1e-7_f64).ln();

    // The most and least that a column of `total` can add to −Σ log n!, with
    // at most `caps[i]` in row i: spread as evenly as the caps allow, or
    // piled into the largest rows first.
    let most = |total: u64, caps: &[u64]| -> f64 {
        let mut sorted = caps.to_vec();
        sorted.sort_unstable();
        let mut left = total;
        let mut out = 0.0;
        for (i, &cap) in sorted.iter().enumerate() {
            let share = left / (sorted.len() - i) as u64;
            if cap <= share {
                out -= lfac(cap);
                left -= cap;
            } else {
                let rest = (sorted.len() - i) as u64;
                let extra = left - share * rest;
                out -= extra as f64 * lfac(share + 1) + (rest - extra) as f64 * lfac(share);
                break;
            }
        }
        out
    };
    let least = |total: u64, caps: &[u64]| -> f64 {
        let mut sorted = caps.to_vec();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        let mut left = total;
        let mut out = 0.0;
        for cap in sorted {
            let take = cap.min(left);
            out -= lfac(take);
            left -= take;
        }
        out
    };

    type Paths = Vec<(f64, f64)>;
    let mut stage: HashMap<Vec<u64>, Paths> = HashMap::new();
    let mut start = rows.clone();
    start.sort_unstable();
    stage.insert(start, vec![(0.0, 1.0)]);
    let mut visited: u64 = 0;
    let mut total = 0.0_f64;
    for j in 0..c - 1 {
        let mut next: HashMap<Vec<u64>, Paths> = HashMap::new();
        for (remaining, mut paths) in stage {
            merge_paths(&mut paths);
            visited += paths.len() as u64;
            if visited > MAX_FISHER_TABLES {
                return None;
            }
            let left: u64 = remaining.iter().sum();
            if j == c - 2 {
                // The last two columns: every placement of the first fixes
                // the second, so their values are listed once for the node,
                // sorted, and each past value takes the ones in the tail.
                let mut futures: Vec<f64> = Vec::new();
                let mut placement = vec![0_u64; remaining.len()];
                let budget = MAX_FISHER_TABLES.saturating_sub(visited);
                let finished = place_column(&remaining, cols[j], 0, &mut placement, &mut |cells| {
                    if futures.len() as u64 >= budget {
                        return false;
                    }
                    futures.push(
                        -cells
                            .iter()
                            .zip(&remaining)
                            .map(|(&v, &r)| lfac(v) + lfac(r - v))
                            .sum::<f64>(),
                    );
                    true
                });
                visited += futures.len() as u64;
                if !finished {
                    return None;
                }
                futures.sort_by(f64::total_cmp);
                let top = futures.last().copied().unwrap_or(0.0);
                let mut prefix = Vec::with_capacity(futures.len() + 1);
                prefix.push(0.0);
                for f in &futures {
                    prefix.push(prefix.last().copied().unwrap_or(0.0) + (f - top).exp());
                }
                for (v, w) in paths {
                    let bound = threshold - constant - v;
                    let k = futures.partition_point(|f| *f <= bound);
                    total += w * (constant + v + top).exp() * prefix[k];
                }
                continue;
            }
            let (hi, lo) = cols[j..].iter().fold((0.0, 0.0), |(h, l), &t| {
                (h + most(t, &remaining), l + least(t, &remaining))
            });
            // Σ over every completion of exp(−Σ log n!), in closed form.
            let all = lfac(left)
                - remaining.iter().map(|&v| lfac(v)).sum::<f64>()
                - cols[j..].iter().map(|&v| lfac(v)).sum::<f64>();
            let mut open: Paths = Vec::new();
            for (v, w) in paths {
                if constant + v + hi <= threshold {
                    total += w * (constant + v + all).exp();
                } else if constant + v + lo > threshold {
                    // No completion is in the tail.
                } else {
                    open.push((v, w));
                }
            }
            if open.is_empty() {
                continue;
            }
            // Every way of placing column j.
            let mut placement = vec![0_u64; remaining.len()];
            let mut placed: u64 = 0;
            let budget = MAX_FISHER_TABLES.saturating_sub(visited);
            let finished = place_column(&remaining, cols[j], 0, &mut placement, &mut |cells| {
                placed += open.len() as u64;
                if placed > budget {
                    return false;
                }
                let add: f64 = -cells.iter().map(|&v| lfac(v)).sum::<f64>();
                let mut rest: Vec<u64> = remaining.iter().zip(cells).map(|(r, v)| r - v).collect();
                rest.sort_unstable();
                let entry = next.entry(rest).or_default();
                entry.extend(open.iter().map(|(v, w)| (v + add, *w)));
                true
            });
            visited += placed;
            if !finished {
                return None;
            }
        }
        stage = next;
    }
    Some(total.min(1.0))
}

/// Merge the paths whose values are equal to within rounding, adding their
/// multiplicities.
fn merge_paths(paths: &mut Vec<(f64, f64)>) {
    paths.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(paths.len());
    for &(v, w) in paths.iter() {
        match out.last_mut() {
            Some(last) if (v - last.0).abs() <= 1e-9 * (1.0 + v.abs()) => last.1 += w,
            _ => out.push((v, w)),
        }
    }
    *paths = out;
}

/// Call `each` with every way of putting `left` into the rows from `i` on, at
/// most `caps[k]` in row k, until it answers false; false if it did.
fn place_column(
    caps: &[u64],
    left: u64,
    i: usize,
    cells: &mut Vec<u64>,
    each: &mut dyn FnMut(&[u64]) -> bool,
) -> bool {
    if i == caps.len() - 1 {
        if left <= caps[i] {
            cells[i] = left;
            return each(cells);
        }
        return true;
    }
    let room: u64 = caps[i + 1..].iter().sum();
    for v in left.saturating_sub(room)..=left.min(caps[i]) {
        cells[i] = v;
        if !place_column(caps, left - v, i + 1, cells, each) {
            return false;
        }
    }
    true
}

// --- two numbers -----------------------------------------------------------------------------

/// Pearson's correlation of two columns and its test, from their centred
/// sums, with the interval from Fisher's z.
pub fn pearson(s: &LinearSums, level: f64) -> Result<TestResult, Refusal> {
    if s.n < 3 {
        return Err("a correlation needs at least three pairs of values".to_owned());
    }
    if !(s.sxx > 0.0 && s.syy > 0.0) {
        return Err("one column's values are all the same, so there is no correlation".to_owned());
    }
    let r = (s.sxy / (s.sxx * s.syy).sqrt()).clamp(-1.0, 1.0);
    let df = s.n as f64 - 2.0;
    let t = df.sqrt() * r / (1.0 - r * r).sqrt();
    let interval = (s.n > 3).then(|| {
        let z = r.atanh();
        let sigma = 1.0 / (s.n as f64 - 3.0).sqrt();
        let q = qnorm((1.0 + level) / 2.0);
        ((z - sigma * q).tanh(), (z + sigma * q).tanh())
    });
    Ok(TestResult::new(TestKind::Pearson, s.n, t_two_sided(t, df))
        .statistic("t", t)
        .df(&[df])
        .estimate("correlation", r, interval, level)
        .effect("r_squared", r * r))
}

/// Simple linear regression of Y on X, from their centred sums: the slope
/// with its interval, tested against 0.
pub fn linear_regression(s: &LinearSums, level: f64) -> Result<TestResult, Refusal> {
    if s.n < 3 {
        return Err("a regression needs at least three pairs of values".to_owned());
    }
    if !above(s.sxx, 0.0) {
        return Err("X's values are all the same, so there is no slope to fit".to_owned());
    }
    let slope = s.sxy / s.sxx;
    let intercept = s.mean_y - slope * s.mean_x;
    let df = s.n as f64 - 2.0;
    let sse = (s.syy - slope * s.sxy).max(0.0);
    let se = (sse / df / s.sxx).sqrt();
    let half = t_quantile(0.5 + level / 2.0, df) * se;
    let (t, p) = if se > 0.0 {
        let t = slope / se;
        (t, t_two_sided(t, df))
    } else {
        (f64::INFINITY, 0.0)
    };
    let r2 = if s.syy > 0.0 {
        s.sxy * s.sxy / (s.sxx * s.syy)
    } else {
        f64::NAN
    };
    let mut result = TestResult::new(TestKind::LinearRegression, s.n, p)
        .df(&[df])
        .estimate("slope", slope, Some((slope - half, slope + half)), level)
        .effect("r_squared", r2)
        .detail("intercept", intercept)
        .detail("standard_error", se);
    if t.is_finite() {
        result = result.statistic("t", t);
    }
    Ok(result)
}

/// Spearman's rank correlation and its test: exact (AS 89) up to 1,290
/// pairs without ties, else by Student's t.
pub fn spearman(x: &[f64], y: &[f64]) -> Result<TestResult, Refusal> {
    let n = x.len();
    if n < 3 || y.len() != n {
        return Err("a correlation needs at least three pairs of values".to_owned());
    }
    let (rx, tx) = (ranks(x).0, distinct(x));
    let (ry, ty) = (ranks(y).0, distinct(y));
    let rho = {
        let s = Moments::of(&rx);
        let t = Moments::of(&ry);
        let sxy: f64 = rx
            .iter()
            .zip(&ry)
            .map(|(a, b)| (a - s.mean) * (b - t.mean))
            .sum();
        let sxx: f64 = rx.iter().map(|a| (a - s.mean).powi(2)).sum();
        let syy: f64 = ry.iter().map(|b| (b - t.mean).powi(2)).sum();
        if !(sxx > 0.0 && syy > 0.0) {
            return Err(
                "one column's values are all the same, so there is no correlation".to_owned(),
            );
        }
        (sxy / (sxx * syy).sqrt()).clamp(-1.0, 1.0)
    };
    let nf = n as f64;
    let den = (nf * nf * nf - nf) / 6.0;
    let q = den * (1.0 - rho);
    let ties = tx.min(ty) < n;
    let exact = n <= 1290 && !ties;
    let p = if exact {
        let p = if q > den {
            prho(n, q.round(), false)
        } else {
            prho(n, q.round() + 2.0, true)
        };
        (2.0 * p).min(1.0)
    } else {
        let t = rho / ((1.0 - rho * rho) / (nf - 2.0)).sqrt();
        t_two_sided(t, nf - 2.0)
    };
    Ok(TestResult::new(TestKind::Spearman, n as u64, p)
        .statistic("S", q)
        .estimate("correlation", rho, None, 0.0)
        .method(if exact { "exact" } else { "t approximation" }))
}

fn distinct(values: &[f64]) -> usize {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted.dedup();
    sorted.len()
}

/// Simple logistic regression of a two-valued `y` on `x`, by iteratively
/// reweighted least squares as R's `glm` fits it: tested by the likelihood
/// ratio, with the odds ratio per unit of X and its Wald interval.
pub fn logistic_regression(x: &[f64], y: &[bool], level: f64) -> Result<TestResult, Refusal> {
    let n = x.len();
    if n < 3 || y.len() != n {
        return Err("a logistic regression needs at least three values".to_owned());
    }
    let events = y.iter().filter(|v| **v).count();
    if events == 0 || events == n {
        return Err("every row has the same outcome, so there is nothing to explain".to_owned());
    }
    let mx = x.iter().sum::<f64>() / n as f64;
    if x.iter().all(|v| *v == x[0]) {
        return Err("X's values are all the same, so there is no slope to fit".to_owned());
    }
    let yv: Vec<f64> = y.iter().map(|v| if *v { 1.0 } else { 0.0 }).collect();
    let deviance = |mu: &[f64]| -> f64 {
        -2.0 * yv
            .iter()
            .zip(mu)
            .map(|(y, m)| if *y > 0.5 { m.ln() } else { (1.0 - m).ln() })
            .sum::<f64>()
    };
    // R's start: μ = (y + ½)/2.
    let mut eta: Vec<f64> = yv
        .iter()
        .map(|y| {
            let m = (y + 0.5) / 2.0;
            (m / (1.0 - m)).ln()
        })
        .collect();
    let mut mu: Vec<f64> = eta.iter().map(|e| 1.0 / (1.0 + (-e).exp())).collect();
    let mut dev_old = deviance(&mu);
    let (mut b0, mut b1) = (0.0, 0.0);
    let mut cov = (0.0, 0.0, 0.0);
    let mut converged = false;
    for _ in 0..25 {
        // Weighted least squares of the working response on X.
        let (mut sw, mut swx, mut swxx, mut swz, mut swxz) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for i in 0..n {
            let w = mu[i] * (1.0 - mu[i]);
            let z = eta[i] + (yv[i] - mu[i]) / w;
            let xc = x[i] - mx;
            sw += w;
            swx += w * xc;
            swxx += w * xc * xc;
            swz += w * z;
            swxz += w * xc * z;
        }
        let det = sw * swxx - swx * swx;
        if !above(det, 0.0) {
            break;
        }
        let c1 = (sw * swxz - swx * swz) / det;
        let c0 = (swxx * swz - swx * swxz) / det;
        // The inverse of XᵀWX, for X centred; converted below.
        cov = (swxx / det, -swx / det, sw / det);
        b1 = c1;
        b0 = c0 - c1 * mx;
        for i in 0..n {
            eta[i] = b0 + b1 * x[i];
            mu[i] = 1.0 / (1.0 + (-eta[i]).exp());
        }
        let dev = deviance(&mu);
        if (dev - dev_old).abs() / (dev.abs() + 0.1) < 1e-8 {
            converged = true;
            break;
        }
        dev_old = dev;
    }
    let eps = 10.0 * f64::EPSILON;
    if !converged || mu.iter().any(|m| *m < eps || *m > 1.0 - eps) {
        return Err(
            "X separates the two outcomes (almost) completely, so the odds ratio cannot be estimated"
                .to_owned(),
        );
    }
    let p_bar = events as f64 / n as f64;
    let null_dev = -2.0 * (events as f64 * p_bar.ln() + (n - events) as f64 * (1.0 - p_bar).ln());
    let dev = deviance(&mu);
    let lr = (null_dev - dev).max(0.0);
    // var(b1) is the centred slope's variance; var(b0) follows from b0 = c0 − b1·x̄.
    let se1 = cov.2.sqrt();
    let se0 = (cov.0 - 2.0 * mx * cov.1 + mx * mx * cov.2).sqrt();
    let q = qnorm(0.5 + level / 2.0);
    Ok(
        TestResult::new(TestKind::LogisticRegression, n as u64, chisq_upper(lr, 1.0))
            .statistic("G²", lr)
            .df(&[1.0])
            .estimate(
                "odds_ratio",
                b1.exp(),
                Some(((b1 - q * se1).exp(), (b1 + q * se1).exp())),
                level,
            )
            .effect("mcfadden_r_squared", 1.0 - dev / null_dev)
            .detail("intercept", b0)
            .detail("intercept_standard_error", se0)
            .detail("slope", b1)
            .detail("standard_error", se1)
            .detail("z", b1 / se1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::reference::{close, nums, reference};
    use serde_json::Value;

    fn f(v: &Value) -> f64 {
        v.as_f64().unwrap()
    }

    fn sums(x: &[f64], y: &[f64]) -> LinearSums {
        let (mx, my) = (Moments::of(x).mean, Moments::of(y).mean);
        LinearSums {
            n: x.len() as u64,
            mean_x: mx,
            mean_y: my,
            sxx: x.iter().map(|a| (a - mx).powi(2)).sum(),
            sxy: x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum(),
            syy: y.iter().map(|b| (b - my).powi(2)).sum(),
            min_x: x.iter().copied().fold(f64::INFINITY, f64::min),
            max_x: x.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        }
    }

    fn interval(e: &Estimate) -> (f64, f64) {
        (e.lower.unwrap(), e.upper.unwrap_or(f64::INFINITY))
    }

    #[test]
    fn one_sample_t_is_rs() {
        for case in reference()["one_sample_t"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let x = nums(&case["x"]);
            let t = one_sample_t(&Moments::of(&x), f(&case["mu"]), 0.95).unwrap();
            close(
                t.statistic.as_ref().unwrap().value,
                f(&case["t"]),
                1e-10,
                name,
            );
            close(t.df[0], f(&case["df"]), 1e-12, name);
            close(t.p_value, f(&case["p"]), 1e-10, name);
            let e = t.estimate.as_ref().unwrap();
            close(e.value, f(&case["mean"]), 1e-12, name);
            let ci = nums(&case["ci"]);
            close(interval(e).0, ci[0], 1e-10, name);
            close(interval(e).1, ci[1], 1e-10, name);
            close(t.effect.as_ref().unwrap().value, f(&case["d"]), 1e-10, name);
        }
        assert!(one_sample_t(&Moments::of(&[3.0]), 0.0, 0.95).is_err());
        assert!(one_sample_t(&Moments::of(&[3.0, 3.0, 3.0]), 0.0, 0.95).is_err());
    }

    #[test]
    fn paired_t_is_rs() {
        for case in reference()["paired_t"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let (x, y) = (nums(&case["x"]), nums(&case["y"]));
            let d: Vec<f64> = x.iter().zip(&y).map(|(a, b)| a - b).collect();
            let t = paired_t(&Moments::of(&d), 0.95).unwrap();
            assert_eq!(t.test, TestKind::PairedT);
            close(
                t.statistic.as_ref().unwrap().value,
                f(&case["t"]),
                1e-10,
                name,
            );
            close(t.p_value, f(&case["p"]), 1e-10, name);
            let e = t.estimate.as_ref().unwrap();
            assert_eq!(e.of, "mean_difference");
            close(e.value, f(&case["mean"]), 1e-12, name);
            let ci = nums(&case["ci"]);
            close(interval(e).0, ci[0], 1e-10, name);
            close(interval(e).1, ci[1], 1e-10, name);
            close(t.effect.as_ref().unwrap().value, f(&case["d"]), 1e-10, name);
        }
    }

    #[test]
    fn signed_rank_is_rs() {
        for case in reference()["signed_rank"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let x = nums(&case["x"]);
            let t = signed_rank(&x, f(&case["mu"]), 0.95).unwrap();
            close(
                t.statistic.as_ref().unwrap().value,
                f(&case["v"]),
                1e-12,
                name,
            );
            close(t.p_value, f(&case["p"]), 1e-10, name);
            let exact = case["method"].as_str().unwrap().contains("exact");
            assert_eq!(
                t.method,
                Some(if exact {
                    "exact"
                } else {
                    "normal approximation"
                }),
                "{name}"
            );
            let e = t.estimate.as_ref().unwrap();
            let ci = nums(&case["ci"]);
            // R finds the approximate interval with `uniroot(tol = 1e-4)`, and
            // the approximate estimate as a root of the statistic rather than
            // the Hodges-Lehmann median it approximates.
            let (tol_ci, tol_est) = if exact { (1e-12, 1e-12) } else { (2e-4, 0.05) };
            close(interval(e).0, ci[0], tol_ci, name);
            close(interval(e).1, ci[1], tol_ci, name);
            close(e.value, f(&case["estimate"]), tol_est, name);
        }
    }

    #[test]
    fn mann_whitney_is_rs() {
        for case in reference()["mann_whitney"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let (x, y) = (nums(&case["x"]), nums(&case["y"]));
            let t = mann_whitney(&x, &y, 0.95).unwrap();
            close(
                t.statistic.as_ref().unwrap().value,
                f(&case["w"]),
                1e-12,
                name,
            );
            close(t.p_value, f(&case["p"]), 1e-10, name);
            let exact = case["method"].as_str().unwrap().contains("exact");
            let e = t.estimate.as_ref().unwrap();
            let ci = nums(&case["ci"]);
            let (tol_ci, tol_est) = if exact { (1e-12, 1e-12) } else { (2e-4, 0.05) };
            close(interval(e).0, ci[0], tol_ci, name);
            close(interval(e).1, ci[1], tol_ci, name);
            close(e.value, f(&case["estimate"]), tol_est, name);
            close(t.effect.as_ref().unwrap().value, f(&case["r"]), 1e-12, name);
        }
    }

    #[test]
    fn welch_t_is_rs() {
        for case in reference()["welch_t"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let (x, y) = (nums(&case["x"]), nums(&case["y"]));
            let t = welch_t(&Moments::of(&x), &Moments::of(&y), 0.95).unwrap();
            close(
                t.statistic.as_ref().unwrap().value,
                f(&case["t"]),
                1e-10,
                name,
            );
            close(t.df[0], f(&case["df"]), 1e-10, name);
            close(t.p_value, f(&case["p"]), 1e-10, name);
            let e = t.estimate.as_ref().unwrap();
            close(e.value, f(&case["difference"]), 1e-12, name);
            let ci = nums(&case["ci"]);
            close(interval(e).0, ci[0], 1e-10, name);
            close(interval(e).1, ci[1], 1e-10, name);
            close(t.effect.as_ref().unwrap().value, f(&case["d"]), 1e-10, name);
        }
    }

    #[test]
    fn several_groups_are_rs() {
        for case in reference()["groups"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let groups: Vec<Vec<f64>> = case["y"].as_array().unwrap().iter().map(nums).collect();
            let moments: Vec<Moments> = groups.iter().map(|g| Moments::of(g)).collect();
            let a = anova(&moments).unwrap();
            close(
                a.statistic.as_ref().unwrap().value,
                f(&case["f"]),
                1e-10,
                name,
            );
            assert_eq!(a.df, nums(&case["df"]), "{name}");
            close(a.p_value, f(&case["p"]), 1e-10, name);
            close(
                a.effect.as_ref().unwrap().value,
                f(&case["eta2"]),
                1e-10,
                name,
            );
            let kw = kruskal_wallis(&groups).unwrap();
            close(
                kw.statistic.as_ref().unwrap().value,
                f(&case["h"]),
                1e-10,
                name,
            );
            close(kw.df[0], f(&case["kw_df"]), 1e-12, name);
            close(kw.p_value, f(&case["kw_p"]), 1e-10, name);
            close(
                kw.effect.as_ref().unwrap().value,
                f(&case["epsilon2"]),
                1e-10,
                name,
            );
            let lev = levene(&groups).unwrap();
            close(
                lev.statistic.as_ref().unwrap().value,
                f(&case["levene_f"]),
                1e-10,
                name,
            );
            close(lev.p_value, f(&case["levene_p"]), 1e-10, name);
            let levels: Vec<&str> = case["levels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap())
                .collect();
            let pairs = tukey_hsd(&moments, 0.95).unwrap();
            let expected = case["tukey"].as_array().unwrap();
            assert_eq!(pairs.len(), expected.len(), "{name}");
            for want in expected {
                let a = levels
                    .iter()
                    .position(|l| *l == want["a"].as_str().unwrap())
                    .unwrap();
                let b = levels
                    .iter()
                    .position(|l| *l == want["b"].as_str().unwrap())
                    .unwrap();
                let got = pairs.iter().find(|p| p.a == a && p.b == b).unwrap();
                close(got.difference, f(&want["diff"]), 1e-10, name);
                close(got.lower, f(&want["lower"]), 1e-8, name);
                close(got.upper, f(&want["upper"]), 1e-8, name);
                close(got.p_value, f(&want["p"]), 1e-8, name);
            }
        }
    }

    #[test]
    fn goodness_of_fit_is_rs() {
        for case in reference()["chisq_fit"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let counts: Vec<u64> = nums(&case["counts"])
                .into_iter()
                .map(|c| c as u64)
                .collect();
            let t = chisq_fit(&counts).unwrap();
            close(
                t.statistic.as_ref().unwrap().value,
                f(&case["x2"]),
                1e-12,
                name,
            );
            close(t.df[0], f(&case["df"]), 1e-12, name);
            close(t.p_value, f(&case["p"]), 1e-10, name);
            close(t.effect.as_ref().unwrap().value, f(&case["w"]), 1e-12, name);
        }
        assert!(chisq_fit(&[5]).is_err());
    }

    #[test]
    fn binomial_is_rs() {
        for case in reference()["binomial"].as_array().unwrap() {
            let (x, n) = (f(&case["x"]) as u64, f(&case["n"]) as u64);
            let name = format!("{x} of {n}");
            let t = binomial(x, n, 0.5, 0.95).unwrap();
            // statrs's incomplete beta is less precise than R's TOMS 708 for
            // a million trials (2e-9 of the p-value).
            close(t.p_value, f(&case["p"]), 1e-8, &name);
            let e = t.estimate.as_ref().unwrap();
            close(e.value, f(&case["estimate"]), 1e-12, &name);
            let ci = nums(&case["ci"]);
            close(interval(e).0, ci[0], 1e-9, &name);
            close(interval(e).1, ci[1], 1e-9, &name);
            close(
                t.effect.as_ref().unwrap().value,
                f(&case["h"]),
                1e-12,
                &name,
            );
        }
    }

    #[test]
    fn contingency_tables_are_rs() {
        for case in reference()["contingency"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let rows = f(&case["rows"]) as usize;
            let flat: Vec<u64> = nums(&case["table"]).into_iter().map(|c| c as u64).collect();
            let table: Vec<Vec<u64>> = flat
                .chunks(flat.len() / rows)
                .map(<[u64]>::to_vec)
                .collect();
            let t = chisq_independence(&table).unwrap();
            close(
                t.statistic.as_ref().unwrap().value,
                f(&case["x2"]),
                1e-10,
                name,
            );
            close(t.df[0], f(&case["df"]), 1e-12, name);
            close(t.p_value, f(&case["p"]), 1e-10, name);
            close(t.effect.as_ref().unwrap().value, f(&case["v"]), 1e-10, name);
            let min_expected = expected_counts(&table)
                .into_iter()
                .flatten()
                .fold(f64::INFINITY, f64::min);
            close(min_expected, f(&case["expected_min"]), 1e-12, name);
            match case["fisher_p"].as_f64() {
                Some(p) => {
                    let fisher = fisher_exact(&table, 0.95).unwrap();
                    close(fisher.p_value, p, 1e-9, name);
                    if let Some(or) = case.get("odds_ratio_tight") {
                        // `fisher.test`'s own computation at a tight
                        // tolerance (its default leaves the upper end of the
                        // tea-tasting interval 0.7% short), and `fisher.test`
                        // itself to within that.
                        let e = fisher.estimate.as_ref().unwrap();
                        let ci = nums(&case["odds_ci_tight"]);
                        close(e.value, f(or), 1e-8, name);
                        close(interval(e).0, ci[0], 1e-8, name);
                        if ci[1] < 1e300 {
                            close(interval(e).1, ci[1], 1e-8, name);
                        }
                        let loose = nums(&case["odds_ci"]);
                        close(e.value, f(&case["odds_ratio"]), 1e-2, name);
                        close(interval(e).1.min(1e308), loose[1].min(1e308), 1e-2, name);
                    }
                }
                None => assert!(fisher_exact(&table, 0.95).is_err(), "{name}"),
            }
        }
    }

    #[test]
    fn correlations_and_regression_are_rs() {
        for case in reference()["correlation"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let (x, y) = (nums(&case["x"]), nums(&case["y"]));
            let s = sums(&x, &y);
            let pe = pearson(&s, 0.95).unwrap();
            close(
                pe.estimate.as_ref().unwrap().value,
                f(&case["r"]),
                1e-12,
                name,
            );
            close(
                pe.statistic.as_ref().unwrap().value,
                f(&case["r_t"]),
                1e-10,
                name,
            );
            close(pe.df[0], f(&case["r_df"]), 1e-12, name);
            close(pe.p_value, f(&case["r_p"]), 1e-10, name);
            let ci = nums(&case["r_ci"]);
            let e = pe.estimate.as_ref().unwrap();
            close(interval(e).0, ci[0], 1e-10, name);
            close(interval(e).1, ci[1], 1e-10, name);
            let sp = spearman(&x, &y).unwrap();
            close(
                sp.estimate.as_ref().unwrap().value,
                f(&case["rho"]),
                1e-12,
                name,
            );
            close(
                sp.statistic.as_ref().unwrap().value,
                f(&case["rho_s"]),
                1e-8,
                name,
            );
            close(sp.p_value, f(&case["rho_p"]), 1e-10, name);
            let lm = linear_regression(&s, 0.95).unwrap();
            let e = lm.estimate.as_ref().unwrap();
            close(e.value, f(&case["slope"]), 1e-12, name);
            close(
                lm.statistic.as_ref().unwrap().value,
                f(&case["slope_t"]),
                1e-10,
                name,
            );
            close(lm.p_value, f(&case["slope_p"]), 1e-10, name);
            let ci = nums(&case["slope_ci"]);
            close(interval(e).0, ci[0], 1e-10, name);
            close(interval(e).1, ci[1], 1e-10, name);
            close(
                lm.effect.as_ref().unwrap().value,
                f(&case["r2"]),
                1e-12,
                name,
            );
            let intercept = lm.details.iter().find(|d| d.name == "intercept").unwrap();
            close(intercept.value, f(&case["intercept"]), 1e-10, name);
            let se = lm
                .details
                .iter()
                .find(|d| d.name == "standard_error")
                .unwrap();
            close(se.value, f(&case["slope_se"]), 1e-10, name);
        }
    }

    #[test]
    fn logistic_regression_is_rs() {
        for case in reference()["logistic"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let x = nums(&case["x"]);
            let y: Vec<bool> = nums(&case["y"]).into_iter().map(|v| v > 0.5).collect();
            let t = logistic_regression(&x, &y, 0.95).unwrap();
            let detail = |n: &str| t.details.iter().find(|d| d.name == n).unwrap().value;
            close(detail("intercept"), f(&case["intercept"]), 1e-6, name);
            close(detail("slope"), f(&case["slope"]), 1e-6, name);
            close(detail("standard_error"), f(&case["slope_se"]), 1e-6, name);
            close(detail("z"), f(&case["slope_z"]), 1e-6, name);
            close(
                t.statistic.as_ref().unwrap().value,
                f(&case["lr"]),
                1e-6,
                name,
            );
            close(t.p_value, f(&case["lr_p"]), 1e-6, name);
            let e = t.estimate.as_ref().unwrap();
            close(e.value, f(&case["odds_ratio"]), 1e-6, name);
            let ci = nums(&case["odds_ci"]);
            close(interval(e).0, ci[0], 1e-6, name);
            close(interval(e).1, ci[1], 1e-6, name);
            close(
                t.effect.as_ref().unwrap().value,
                f(&case["mcfadden"]),
                1e-6,
                name,
            );
        }
        // Perfect separation is refused, not answered with a huge odds ratio.
        let x = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let y = [false, false, false, true, true, true];
        assert!(
            logistic_regression(&x, &y, 0.95)
                .unwrap_err()
                .contains("separates")
        );
    }

    #[test]
    fn shapiro_reads_three_to_five_thousand_values() {
        assert!(shapiro(&[1.0, 2.0]).is_err());
        assert!(shapiro(&vec![1.0; 5001]).is_err());
        let t = shapiro(&[2.0, 1.0, 4.0]).unwrap();
        assert_eq!(t.n, 3);
    }

    #[test]
    fn ranks_average_ties() {
        let (r, ties) = ranks(&[3.0, 1.0, 3.0, 2.0]);
        assert_eq!(r, vec![3.5, 1.0, 3.5, 2.0]);
        assert_eq!(ties, vec![2]);
    }
}
