//! The arithmetic of the stats (analytics TODO A2.3, A2.4), apart from the SQL
//! that feeds it: quantiles from the two rows either side of a percentile,
//! bin widths, confidence intervals, kernel densities and smoothers. Pure
//! functions over numbers, so each is tested without a database against values
//! recorded from R (`tests/r/plot_reference.R`).
//!
//! The conventions are R's, so a plot here agrees with the same plot in
//! ggplot2: quantiles of type 7 (`quantile()`'s default, Postgres's
//! `percentile_cont`), Silverman's rule of thumb for a density's bandwidth
//! (`bw.nrd0`) with `density()`'s grid of 512 points reaching three bandwidths
//! past the data, `lm`'s confidence band for a linear smoother, and
//! `loess(surface = "direct")` with `predict(se = TRUE)`'s band for loess.

use statrs::distribution::{ContinuousCDF, StudentsT};

/// How many points a density is evaluated at (R's `density()` default).
pub const DENSITY_POINTS: usize = 512;
/// How many points a smoother's curve is evaluated at (ggplot2's default).
pub const SMOOTH_POINTS: usize = 80;
/// The most bins a binned column is given; a rule asking for more is widened.
pub const MAX_BINS: f64 = 200.0;

/// The `p` quantile (type 7) of `n` sorted values, given the values at the
/// 1-based positions `floor((n − 1)p) + 1` and the one after it — what the
/// percentile query reads for each probability.
pub fn quantile(n: u64, p: f64, at_lo: f64, at_hi: Option<f64>) -> f64 {
    let h = (n.saturating_sub(1)) as f64 * p;
    let frac = h - h.floor();
    match at_hi {
        Some(hi) if frac > 0.0 => at_lo + frac * (hi - at_lo),
        _ => at_lo,
    }
}

/// What a bin rule needs to know about a column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnStats {
    /// How many values are not missing.
    pub n: u64,
    /// The smallest.
    pub min: f64,
    /// The largest.
    pub max: f64,
    /// The first quartile.
    pub q1: f64,
    /// The third quartile.
    pub q3: f64,
}

/// Where a binned column's bins start, and how wide they are: bin `i` holds
/// `origin + i·width ≤ x < origin + (i + 1)·width`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct BinParams {
    /// The start of bin 0.
    pub origin: f64,
    /// The width of every bin.
    pub width: f64,
}

/// The bins of a column: of `width` when given; else about `bins` of them;
/// else by the Freedman–Diaconis rule, `2·IQR·n^(−1/3)` (Sturges's when the
/// interquartile range is 0). Widths that are not given are rounded to 1, 2,
/// 2.5 or 5 times a power of ten (whole numbers for an integer column), and the
/// origin is a multiple of the width, so bin edges are round numbers.
pub fn bin_params(
    stats: &ColumnStats,
    width: Option<f64>,
    bins: Option<u32>,
    integer: bool,
) -> BinParams {
    let range = stats.max - stats.min;
    let width = match width {
        Some(w) => w,
        None => {
            let raw = match bins {
                Some(b) => range / f64::from(b.max(1)),
                None => {
                    let iqr = stats.q3 - stats.q1;
                    let n = stats.n.max(1) as f64;
                    let fd = 2.0 * iqr * n.powf(-1.0 / 3.0);
                    if fd > 0.0 {
                        fd
                    } else {
                        range / (n.log2().ceil() + 1.0)
                    }
                }
            };
            let raw = if raw > 0.0 && raw.is_finite() {
                raw.max(range / MAX_BINS)
            } else if stats.max.abs() > 0.0 {
                stats.max.abs() / 10.0
            } else {
                1.0
            };
            nice(raw, integer)
        }
    };
    BinParams {
        origin: (stats.min / width).floor() * width,
        width,
    }
}

/// `raw` rounded to the nearest of 1, 2, 2.5 and 5 times a power of ten (not
/// 2.5, and at least 1, for an integer column).
pub fn nice(raw: f64, integer: bool) -> f64 {
    let magnitude = 10f64.powf(raw.log10().floor());
    let steps: &[f64] = if integer && magnitude >= 1.0 {
        &[1.0, 2.0, 5.0, 10.0]
    } else {
        &[1.0, 2.0, 2.5, 5.0, 10.0]
    };
    let best = steps
        .iter()
        .map(|s| s * magnitude)
        .min_by(|a, b| {
            ((a / raw).ln().abs())
                .partial_cmp(&(b / raw).ln().abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(raw);
    if integer { best.round().max(1.0) } else { best }
}

/// Whether `x` is more than 0 — false for NaN, which a spread computed from
/// missing values can be.
pub fn positive(x: f64) -> bool {
    x > 0.0
}

/// The two-sided `level` quantile of Student's t with `df` degrees of
/// freedom: the multiplier of a standard error in a confidence interval.
pub fn t_multiplier(df: f64, level: f64) -> Option<f64> {
    let t = StudentsT::new(0.0, 1.0, df).ok()?;
    Some(t.inverse_cdf(0.5 + level / 2.0))
}

/// A mean's confidence interval from the count, mean and standard deviation.
pub fn mean_interval(n: u64, mean: f64, sd: Option<f64>, level: f64) -> Option<(f64, f64)> {
    if n < 2 {
        return None;
    }
    let sd = sd?;
    let half = t_multiplier(n as f64 - 1.0, level)? * sd / (n as f64).sqrt();
    Some((mean - half, mean + half))
}

/// Silverman's rule of thumb (R's `bw.nrd0`): `0.9·min(sd, IQR/1.34)·n^(−1/5)`,
/// falling back to the standard deviation, then to `|fallback|`, then to 1,
/// when what comes first is 0.
pub fn bandwidth_nrd0(n: u64, sd: f64, iqr: f64, fallback: f64) -> f64 {
    let mut lo = sd.min(iqr / 1.34);
    if !positive(lo) {
        lo = sd;
    }
    if !positive(lo) {
        lo = fallback.abs();
    }
    if !positive(lo) {
        lo = 1.0;
    }
    0.9 * lo * (n.max(1) as f64).powf(-0.2)
}

/// The grid a density is evaluated at: [`DENSITY_POINTS`] points from three
/// bandwidths below the smallest value to three above the largest (R's
/// `cut = 3`).
pub fn density_grid(min: f64, max: f64, bw: f64) -> Vec<f64> {
    let (from, to) = (min - 3.0 * bw, max + 3.0 * bw);
    let step = (to - from) / (DENSITY_POINTS - 1) as f64;
    (0..DENSITY_POINTS)
        .map(|i| from + i as f64 * step)
        .collect()
}

fn gaussian(u: f64) -> f64 {
    (-0.5 * u * u).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// A Gaussian kernel density estimate at each of `grid`, from weighted points
/// (a value and how many rows have it): `Σ wᵢ φ((x − xᵢ)/bw) / (n·bw)`. With
/// every weight 1 it is the exact estimate; with the counts of fine bins at
/// their centres, the binned approximation R's `density()` makes too.
pub fn kde(points: &[(f64, f64)], bw: f64, grid: &[f64]) -> Vec<f64> {
    let n: f64 = points.iter().map(|(_, w)| w).sum();
    if n <= 0.0 || bw <= 0.0 {
        return vec![0.0; grid.len()];
    }
    grid.iter()
        .map(|x| {
            points
                .iter()
                .map(|(xi, w)| {
                    let u = (x - xi) / bw;
                    // Beyond eight bandwidths a kernel adds less than 1e-14.
                    if u.abs() > 8.0 { 0.0 } else { w * gaussian(u) }
                })
                .sum::<f64>()
                / (n * bw)
        })
        .collect()
}

/// The sums a least-squares line is fitted from, each about the group's mean
/// so that large values lose no digits: what the linear smoother's query
/// returns for each group.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearSums {
    /// How many rows have both values.
    pub n: u64,
    /// The mean of X.
    pub mean_x: f64,
    /// The mean of Y.
    pub mean_y: f64,
    /// `Σ(x − x̄)²`.
    pub sxx: f64,
    /// `Σ(x − x̄)(y − ȳ)`.
    pub sxy: f64,
    /// `Σ(y − ȳ)²`.
    pub syy: f64,
    /// The smallest X.
    pub min_x: f64,
    /// The largest X.
    pub max_x: f64,
}

/// A fitted line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearFit {
    /// The value at X = 0.
    pub intercept: f64,
    /// The change in Y per unit of X.
    pub slope: f64,
    /// The residual standard error, when there are more than two rows.
    pub sigma: Option<f64>,
}

impl LinearSums {
    /// The least-squares line, when X varies.
    pub fn fit(&self) -> Option<LinearFit> {
        if self.n < 2 || !positive(self.sxx) {
            return None;
        }
        let slope = self.sxy / self.sxx;
        let intercept = self.mean_y - slope * self.mean_x;
        let sigma = (self.n > 2).then(|| {
            let rss = (self.syy - slope * self.sxy).max(0.0);
            (rss / (self.n as f64 - 2.0)).sqrt()
        });
        Some(LinearFit {
            intercept,
            slope,
            sigma,
        })
    }
}

/// One point of a smoother's curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurvePoint {
    /// Where.
    pub x: f64,
    /// The fitted value.
    pub y: f64,
    /// The confidence band, when asked for and computable.
    pub band: Option<(f64, f64)>,
}

/// `count` evenly spaced points from `from` to `to`.
pub fn spaced(from: f64, to: f64, count: usize) -> Vec<f64> {
    if count < 2 || to <= from {
        return vec![from];
    }
    let step = (to - from) / (count - 1) as f64;
    // The last point is the end itself, not `from + (count − 1)·step`, which
    // can fall a rounding short of it.
    (0..count)
        .map(|i| {
            if i + 1 == count {
                to
            } else {
                from + i as f64 * step
            }
        })
        .collect()
}

/// The linear smoother's curve over the range of X, with `lm`'s confidence
/// band for the mean: `ŷ ± t·σ·√(1/n + (x − x̄)²/Sxx)`.
pub fn linear_curve(sums: &LinearSums, se: bool, level: f64) -> Option<Vec<CurvePoint>> {
    let fit = sums.fit()?;
    let t = if se {
        t_multiplier(sums.n as f64 - 2.0, level)
    } else {
        None
    };
    Some(
        spaced(sums.min_x, sums.max_x, SMOOTH_POINTS)
            .into_iter()
            .map(|x| {
                let y = fit.intercept + fit.slope * x;
                let band = match (t, fit.sigma) {
                    (Some(t), Some(sigma)) => {
                        let d = x - sums.mean_x;
                        let half = t * sigma * (1.0 / sums.n as f64 + d * d / sums.sxx).sqrt();
                        Some((y - half, y + half))
                    }
                    _ => None,
                };
                CurvePoint { x, y, band }
            })
            .collect(),
    )
}

/// The weights of a local fit at `x0`: tricube in the distance, scaled by the
/// distance to the `q`-th nearest point where `q = ⌊n·span⌋` (span below 1),
/// or by the largest distance times √span (span of 1 or more) — Cleveland's
/// loess, as R's `loess(family = "gaussian")` computes it.
fn loess_weights(xs: &[f64], x0: f64, span: f64) -> Vec<f64> {
    let n = xs.len();
    let mut d: Vec<f64> = xs.iter().map(|x| (x - x0).abs()).collect();
    let h = if span < 1.0 {
        // R's `floor(n·span + 1e-5)`, so a span of 0.3 of 10 points is 3.
        let q = ((n as f64 * span + 1e-5).floor() as usize).clamp(1, n);
        let mut sorted = d.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        sorted[q - 1]
    } else {
        // R scales the *squared* largest distance by the span (`ehg127`), so
        // in one variable the radius is √span times it, not the span times it
        // that `?loess` describes; this follows what R does.
        d.iter().copied().fold(0.0, f64::max) * span.sqrt()
    };
    for di in &mut d {
        *di = if h > 0.0 {
            let u = *di / h;
            if u < 1.0 {
                (1.0 - u * u * u).powi(3)
            } else {
                0.0
            }
        } else if *di == 0.0 {
            1.0
        } else {
            0.0
        };
    }
    d
}

/// The row of the loess operator at `x0`: the weights `ℓ` with `ŷ(x0) = ℓ·y`,
/// from a weighted local quadratic (falling back to a line, then to a
/// constant, where too few points carry weight to fix a quadratic).
fn loess_row(xs: &[f64], x0: f64, span: f64) -> Vec<f64> {
    let w = loess_weights(xs, x0, span);
    for degree in (0..=2usize).rev() {
        let k = degree + 1;
        // The normal equations XᵀWX b = XᵀW y, with X's columns
        // 1, (x − x0), (x − x0)².
        let mut m = vec![vec![0.0; k]; k];
        for (xi, wi) in xs.iter().zip(&w) {
            let d = xi - x0;
            let powers = [1.0, d, d * d];
            for r in 0..k {
                for c in 0..k {
                    m[r][c] += wi * powers[r] * powers[c];
                }
            }
        }
        if let Some(inv) = invert(&m) {
            // ℓ_i = w_i · (first row of (XᵀWX)⁻¹) · x_i
            return xs
                .iter()
                .zip(&w)
                .map(|(xi, wi)| {
                    let d = xi - x0;
                    let powers = [1.0, d, d * d];
                    wi * (0..k).map(|c| inv[0][c] * powers[c]).sum::<f64>()
                })
                .collect();
        }
    }
    vec![0.0; xs.len()]
}

/// The inverse of a small symmetric matrix, or `None` when it is singular.
fn invert(m: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let k = m.len();
    let mut a: Vec<Vec<f64>> = m
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut r = row.clone();
            r.extend((0..k).map(|j| if i == j { 1.0 } else { 0.0 }));
            r
        })
        .collect();
    let scale = m
        .iter()
        .flatten()
        .fold(0.0f64, |s, v| s.max(v.abs()))
        .max(f64::MIN_POSITIVE);
    for col in 0..k {
        let pivot = (col..k).max_by(|&i, &j| {
            a[i][col]
                .abs()
                .partial_cmp(&a[j][col].abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
        if a[pivot][col].abs() <= 1e-12 * scale {
            return None;
        }
        a.swap(col, pivot);
        let p = a[col][col];
        for v in &mut a[col] {
            *v /= p;
        }
        for row in 0..k {
            if row != col {
                let f = a[row][col];
                if f != 0.0 {
                    let pivot_row = a[col].clone();
                    for (cell, p) in a[row].iter_mut().zip(&pivot_row) {
                        *cell -= f * p;
                    }
                }
            }
        }
    }
    Some(a.into_iter().map(|r| r[k..].to_vec()).collect())
}

/// The value and slope of R's interpolation table for the exponent of the
/// δ approximation (`ehg176` in R's `loessf.f`), at each of its vertices.
const DELTA_TABLE: [(f64, f64, f64); 10] = [
    (-0.005, -9.0572e-2, 4.4844),
    (0.1204, 9.5807e-2, -0.7978),
    (0.2017, 2.6152e-2, -0.7286),
    (0.2815, -3.1926e-2, -0.4457),
    (0.3705, -5.3718e-2, -0.3495),
    (0.4536, -6.4170e-2, 3.2813e-2),
    (0.5591, -5.8387e-2, 0.1611),
    (0.7132, -2.0636e-2, 0.3350),
    (0.8751, 4.0172e-2, -4.1032e-2),
    (1.005, -1.0856e-2, -0.7736),
];
/// The coefficients of R's δ₁ and δ₂ approximations for a local quadratic in
/// one variable (`ehg141`'s `c(13:15)` and `c(37:39)`).
const DELTA1: [f64; 3] = [0.1611761, 0.3091323, 0.4401023];
const DELTA2: [f64; 3] = [0.2075670, 0.2822574, 0.2369957];

/// R's approximation of a loess fit's `δ₁ = tr((I − L)ᵀ(I − L))` and
/// `δ₂ = tr(((I − L)ᵀ(I − L))²)` from the trace of its operator `L`
/// (`ehg141` in R's `loessf.f`, which `loess()` uses unless asked for
/// `statistics = "exact"`): exact ones cost a product of two n×n matrices.
fn loess_deltas(n: usize, trace: f64) -> (f64, f64) {
    // A local quadratic in one variable has three parameters.
    let k = 3.0;
    let n = n as f64;
    let corx = (k / n).sqrt();
    let z = (((k / trace).sqrt() - corx) / (1.0 - corx)).clamp(0.0, 1.0);
    // The table's cubic Hermite interpolant at z.
    let i = DELTA_TABLE
        .windows(2)
        .position(|w| z <= w[1].0)
        .unwrap_or(DELTA_TABLE.len() - 2);
    let ((v0, g0, s0), (v1, g1, s1)) = (DELTA_TABLE[i], DELTA_TABLE[i + 1]);
    let h = (z - v0) / (v1 - v0);
    let exponent = (1.0 - h).powi(2) * (1.0 + 2.0 * h) * g0
        + h * h * (3.0 - 2.0 * h) * g1
        + (h * (1.0 - h).powi(2) * s0 + h * h * (h - 1.0) * s1) * (v1 - v0);
    let c4 = exponent.exp();
    let delta = |c: [f64; 3]| n - trace * (c[0] * z.powf(c[1]) * (1.0 - z).powf(c[2]) * c4).exp();
    (delta(DELTA1), delta(DELTA2))
}

/// A loess curve (local quadratic, tricube weights, Gaussian family) over the
/// range of the points, evaluated directly at each point of the curve — R's
/// `loess(span = span, degree = 2, surface = "direct")`.
///
/// The band is ggplot2's from `predict(se = TRUE)`: `ŷ ± t·s·‖ℓ(x)‖` with the
/// residual scale `s² = RSS/δ₁` and `δ₁²/δ₂` degrees of freedom, the δs
/// approximated from the trace of the operator as R does by default
/// ([`loess_deltas`]).
pub fn loess_curve(
    xs: &[f64],
    ys: &[f64],
    span: f64,
    se: bool,
    level: f64,
) -> Option<Vec<CurvePoint>> {
    let n = xs.len();
    if n < 3 || ys.len() != n {
        return None;
    }
    let min = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let max = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !positive(max - min) {
        return None;
    }
    let scale_and_df = if se {
        let mut rss = 0.0;
        let mut trace = 0.0;
        for (i, x) in xs.iter().enumerate() {
            let row = loess_row(xs, *x, span);
            let fitted: f64 = row.iter().zip(ys).map(|(l, y)| l * y).sum();
            rss += (ys[i] - fitted).powi(2);
            trace += row[i];
        }
        let (delta1, delta2) = loess_deltas(n, trace);
        (positive(delta1) && positive(delta2))
            .then(|| ((rss / delta1).sqrt(), delta1 * delta1 / delta2))
    } else {
        None
    };
    let t = scale_and_df.and_then(|(_, df)| t_multiplier(df, level));
    Some(
        spaced(min, max, SMOOTH_POINTS)
            .into_iter()
            .map(|x| {
                let row = loess_row(xs, x, span);
                let y: f64 = row.iter().zip(ys).map(|(l, y)| l * y).sum();
                let band = match (t, scale_and_df) {
                    (Some(t), Some((s, _))) => {
                        let half = t * s * row.iter().map(|l| l * l).sum::<f64>().sqrt();
                        Some((y - half, y + half))
                    }
                    _ => None,
                };
                CurvePoint { x, y, band }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Values recorded from R by `tests/r/plot_reference.R`, on R's own data
    /// sets (`cars`, `faithful`).
    fn reference() -> Value {
        serde_json::from_str(include_str!("../../tests/r/plot_reference.json")).unwrap()
    }

    fn nums(v: &Value) -> Vec<f64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect()
    }

    fn num(v: &Value) -> f64 {
        v.as_f64().unwrap()
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol * (1.0 + b.abs())
    }

    /// Each of `ours` within `tol` of R's, relative to the largest of R's.
    fn assert_all_close(what: &str, ours: &[f64], r: &[f64], tol: f64) {
        assert_eq!(ours.len(), r.len(), "{what}");
        let scale = r.iter().fold(0.0f64, |m, v| m.max(v.abs())).max(1.0);
        for (i, (a, b)) in ours.iter().zip(r).enumerate() {
            assert!((a - b).abs() <= tol * scale, "{what}[{i}]: ours {a}, R {b}");
        }
    }

    fn sums(xs: &[f64], ys: &[f64]) -> LinearSums {
        let n = xs.len() as f64;
        let mx = xs.iter().sum::<f64>() / n;
        let my = ys.iter().sum::<f64>() / n;
        LinearSums {
            n: xs.len() as u64,
            mean_x: mx,
            mean_y: my,
            sxx: xs.iter().map(|x| (x - mx).powi(2)).sum(),
            sxy: xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum(),
            syy: ys.iter().map(|y| (y - my).powi(2)).sum(),
            min_x: xs.iter().copied().fold(f64::INFINITY, f64::min),
            max_x: xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        }
    }

    #[test]
    fn quantiles_match_r_type_7() {
        let r = &reference()["quantiles"];
        let mut sorted = nums(&r["x"]);
        sorted.sort_by(f64::total_cmp);
        let n = sorted.len();
        let ours: Vec<f64> = nums(&r["p"])
            .into_iter()
            .map(|p| {
                let lo = ((n - 1) as f64 * p).floor() as usize;
                quantile(n as u64, p, sorted[lo], sorted.get(lo + 1).copied())
            })
            .collect();
        assert_all_close("quantile", &ours, &nums(&r["q"]), 1e-14);
        assert_eq!(quantile(1, 0.5, 7.0, None), 7.0);
    }

    #[test]
    fn bins_are_round_and_cover_the_data() {
        // Freedman–Diaconis for 1000 values with IQR 50: 2·50·1000^(−1/3) = 10.
        let stats = ColumnStats {
            n: 1000,
            min: 3.0,
            max: 197.0,
            q1: 75.0,
            q3: 125.0,
        };
        let b = bin_params(&stats, None, None, false);
        assert_eq!(
            b,
            BinParams {
                origin: 0.0,
                width: 10.0
            }
        );
        // 13.7 rounds to 10 (log-nearest of 10 and 20), and an integer column
        // keeps whole widths.
        assert_eq!(nice(13.7, false), 10.0);
        assert_eq!(nice(2.3, false), 2.5);
        assert_eq!(nice(2.3, true), 2.0);
        assert_eq!(nice(0.3, true), 1.0);
        assert_eq!(bin_params(&stats, Some(7.0), None, false).width, 7.0);
        assert_eq!(bin_params(&stats, None, Some(20), false).width, 10.0);
        // No spread at all: one bin around the value.
        let flat = ColumnStats {
            n: 5,
            min: 4.0,
            max: 4.0,
            q1: 4.0,
            q3: 4.0,
        };
        let b = bin_params(&flat, None, None, false);
        assert!(b.origin <= 4.0 && 4.0 < b.origin + b.width, "{b:?}");
        // A rule never makes more than MAX_BINS bins.
        let wide = ColumnStats {
            n: 1_000_000,
            min: 0.0,
            max: 1e6,
            q1: 1.0,
            q3: 2.0,
        };
        assert!(1e6 / bin_params(&wide, None, None, false).width <= MAX_BINS);
    }

    #[test]
    fn a_mean_interval_matches_t_test() {
        for case in reference()["intervals"].as_array().unwrap() {
            let xs = nums(&case["x"]);
            let n = xs.len() as f64;
            let mean = xs.iter().sum::<f64>() / n;
            let sd = (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
            let (lo, hi) =
                mean_interval(xs.len() as u64, mean, Some(sd), num(&case["level"])).unwrap();
            assert_all_close("t.test", &[lo, hi], &nums(&case["ci"]), 1e-12);
        }
        assert_eq!(mean_interval(1, 3.0, None, 0.95), None);
    }

    #[test]
    fn the_linear_smoother_matches_lm() {
        for case in reference()["linear"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let s = sums(&nums(&case["x"]), &nums(&case["y"]));
            let fit = s.fit().unwrap();
            assert!(
                close(fit.intercept, num(&case["intercept"]), 1e-12),
                "{name}"
            );
            assert!(close(fit.slope, num(&case["slope"]), 1e-12), "{name}");
            assert!(
                close(fit.sigma.unwrap(), num(&case["sigma"]), 1e-12),
                "{name}"
            );
            let curve = linear_curve(&s, true, num(&case["level"])).unwrap();
            let x: Vec<f64> = curve.iter().map(|p| p.x).collect();
            let y: Vec<f64> = curve.iter().map(|p| p.y).collect();
            let lower: Vec<f64> = curve.iter().map(|p| p.band.unwrap().0).collect();
            let upper: Vec<f64> = curve.iter().map(|p| p.band.unwrap().1).collect();
            assert_all_close(name, &x, &nums(&case["grid"]), 1e-14);
            assert_all_close(name, &y, &nums(&case["fit"]), 1e-12);
            assert_all_close(name, &lower, &nums(&case["lower"]), 1e-10);
            assert_all_close(name, &upper, &nums(&case["upper"]), 1e-10);
        }
    }

    #[test]
    fn the_density_matches_r() {
        for case in reference()["densities"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let xs = nums(&case["x"]);
            let bw = bandwidth_nrd0(xs.len() as u64, num(&case["sd"]), num(&case["iqr"]), xs[0]);
            assert!(close(bw, num(&case["bw"]), 1e-14), "{name}: bw {bw}");
            let min = xs.iter().copied().fold(f64::INFINITY, f64::min);
            let max = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let grid = density_grid(min, max, bw);
            assert_all_close(name, &grid, &nums(&case["grid"]), 1e-14);
            // The estimate from the values is the exact Gaussian one…
            let points: Vec<(f64, f64)> = xs.iter().map(|x| (*x, 1.0)).collect();
            let exact = kde(&points, bw, &grid);
            assert_all_close(name, &exact, &nums(&case["exact"]), 1e-12);
            // …which density() approximates by binning to within 0.03% of
            // the peak, …
            let r = nums(&case["density"]);
            assert_all_close(name, &exact, &r, 3e-4);
            // …as the estimate from fine bins (many values) does too.
            let fine = (max - min) / 2048.0;
            let mut binned: std::collections::BTreeMap<i64, f64> = Default::default();
            for x in &xs {
                let b = ((x - min) / fine).floor().min(2047.0);
                *binned.entry(b as i64).or_default() += 1.0;
            }
            let centres: Vec<(f64, f64)> = binned
                .into_iter()
                .map(|(b, c)| (min + (b as f64 + 0.5) * fine, c))
                .collect();
            assert_all_close(name, &kde(&centres, bw, &grid), &r, 5e-4);
        }
        // With no spread, the bandwidth falls back as bw.nrd0 does.
        assert_eq!(
            bandwidth_nrd0(10, 0.0, 0.0, 3.0),
            0.9 * 3.0 * 10f64.powf(-0.2)
        );
    }

    #[test]
    fn loess_matches_r_with_surface_direct() {
        for case in reference()["loess"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let xs = nums(&case["x"]);
            let ys = nums(&case["y"]);
            let curve =
                loess_curve(&xs, &ys, num(&case["span"]), true, num(&case["level"])).unwrap();
            let x: Vec<f64> = curve.iter().map(|p| p.x).collect();
            let y: Vec<f64> = curve.iter().map(|p| p.y).collect();
            let lower: Vec<f64> = curve.iter().map(|p| p.band.unwrap().0).collect();
            let upper: Vec<f64> = curve.iter().map(|p| p.band.unwrap().1).collect();
            assert_all_close(name, &x, &nums(&case["grid"]), 1e-14);
            assert_all_close(name, &y, &nums(&case["fit"]), 1e-10);
            assert_all_close(name, &lower, &nums(&case["lower"]), 1e-8);
            assert_all_close(name, &upper, &nums(&case["upper"]), 1e-8);
            // The δs approximated from the trace, as R's are.
            let (d1, d2) = loess_deltas(xs.len(), num(&case["trace"]));
            assert!(close(d1, num(&case["one_delta"]), 1e-10), "{name}: δ₁ {d1}");
            assert!(close(d2, num(&case["two_delta"]), 1e-10), "{name}: δ₂ {d2}");
        }
    }

    #[test]
    fn loess_reproduces_a_quadratic() {
        // A local quadratic fits a quadratic exactly, whatever the span.
        let xs: Vec<f64> = (0..30).map(f64::from).collect();
        let ys: Vec<f64> = xs.iter().map(|x| 3.0 - 2.0 * x + 0.5 * x * x).collect();
        let curve = loess_curve(&xs, &ys, 0.4, true, 0.95).unwrap();
        for p in &curve {
            let truth = 3.0 - 2.0 * p.x + 0.5 * p.x * p.x;
            assert!(close(p.y, truth, 1e-8), "{p:?}");
            // No residuals, so no band to speak of.
            let (lo, hi) = p.band.unwrap();
            assert!((hi - lo).abs() < 1e-6);
        }
        // With evenly spread points the weights about the middle are
        // symmetric, and every row of the operator sums to 1.
        let row = loess_row(&[0.0, 1.0, 2.0, 3.0, 4.0], 2.0, 2.0);
        assert!(close(row.iter().sum::<f64>(), 1.0, 1e-12));
        assert!(close(row[0], row[4], 1e-12) && close(row[1], row[3], 1e-12));
    }
}
