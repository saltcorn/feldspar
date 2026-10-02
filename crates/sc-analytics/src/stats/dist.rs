//! The distributions the hypothesis tests need that `statrs` does not have,
//! ported from R's own C so that a p-value here is R's p-value
//! (`tests/r/test_reference.R` records R's answers, and the tests below
//! compare):
//!
//! - the studentized range ([`ptukey`], [`qtukey`]: R's `nmath/ptukey.c` and
//!   `qtukey.c`, Copenhaver & Holland 1988), for Tukey's honest significant
//!   differences;
//! - Shapiro and Wilk's W and its p-value ([`shapiro_wilk`]: R's `swilk.c`,
//!   Royston's algorithm AS R94);
//! - the exact null distributions of the Wilcoxon rank-sum and signed-rank
//!   statistics ([`RankSum`], [`SignedRank`]: what R's `cwilcox` and
//!   `csignrank` count, counted here by adding one rank at a time so that no
//!   count is ever a difference);
//! - Spearman's S ([`prho`]: R's `prho.c`, algorithm AS 89, exact for nine
//!   values or fewer and an Edgeworth series above).

// The constants are R's, digit for digit — its quadrature nodes, and its
// truncated √2, 1/√2 and π/3 — so that a result here is R's to the last bit.
#![allow(clippy::excessive_precision, clippy::approx_constant)]

use statrs::distribution::{ContinuousCDF, Normal};
use statrs::function::erf::erfc;
use statrs::function::gamma::ln_gamma;

/// The standard normal distribution's lower tail.
pub(crate) fn pnorm(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

/// The standard normal distribution's upper tail.
pub(crate) fn pnorm_upper(x: f64) -> f64 {
    0.5 * erfc(x / std::f64::consts::SQRT_2)
}

/// The standard normal quantile.
pub(crate) fn qnorm(p: f64) -> f64 {
    // `Normal::new(0, 1)` cannot fail.
    Normal::new(0.0, 1.0).map_or(f64::NAN, |n| n.inverse_cdf(p))
}

// --- the studentized range ---------------------------------------------------

/// The probability integral of Hartley's form of the range of `cc` normal
/// values (`rr` of them), from 0 to `w` — R's `wprob`.
fn wprob(w: f64, rr: f64, cc: f64) -> f64 {
    const NLEG: usize = 12;
    const IHALF: usize = 6;
    const C1: f64 = -30.0;
    const C2: f64 = -50.0;
    const C3: f64 = 60.0;
    const BB: f64 = 8.0;
    const WLAR: f64 = 3.0;
    const WINCR1: f64 = 2.0;
    const WINCR2: f64 = 3.0;
    const XLEG: [f64; IHALF] = [
        0.981_560_634_246_719_250_690_549_090_149,
        0.904_117_256_370_474_856_678_465_866_119,
        0.769_902_674_194_304_687_036_893_833_213,
        0.587_317_954_286_617_447_296_702_418_941,
        0.367_831_498_998_180_193_752_691_536_644,
        0.125_233_408_511_468_915_472_441_369_464,
    ];
    const ALEG: [f64; IHALF] = [
        0.047_175_336_386_511_827_194_615_961_485,
        0.106_939_325_995_318_430_960_254_718_194,
        0.160_078_328_543_346_226_334_652_529_543,
        0.203_167_426_723_065_921_749_064_455_810,
        0.233_492_536_538_354_808_760_849_898_925,
        0.249_147_045_813_402_785_000_562_436_043,
    ];
    let qsqz = w * 0.5;
    if qsqz >= BB {
        return 1.0;
    }
    // (2Φ(w/2) − 1)^cc, the first term of Hartley's form.
    let mut pr_w = 2.0 * pnorm(qsqz) - 1.0;
    pr_w = if pr_w >= (C2 / cc).exp() {
        pr_w.powf(cc)
    } else {
        0.0
    };
    let wincr = if w > WLAR { WINCR1 } else { WINCR2 };
    // The second term, by Legendre quadrature over two or three intervals
    // from w/2 to 8.
    let mut blb = qsqz;
    let binc = (BB - qsqz) / wincr;
    let mut bub = blb + binc;
    let mut einsum = 0.0;
    let cc1 = cc - 1.0;
    let mut wi = 1.0;
    while wi <= wincr {
        let mut elsum = 0.0;
        let a = 0.5 * (bub + blb);
        let b = 0.5 * (bub - blb);
        for jj in 1..=NLEG {
            let (j, xx) = if IHALF < jj {
                let j = NLEG - jj + 1;
                (j, XLEG[j - 1])
            } else {
                (jj, -XLEG[jj - 1])
            };
            let c = b * xx;
            let ac = a + c;
            let qexpo = ac * ac;
            if qexpo > C3 {
                break;
            }
            let pplus = 2.0 * pnorm(ac);
            let pminus = 2.0 * pnorm(ac - w);
            let rinsum = pplus * 0.5 - pminus * 0.5;
            if rinsum >= (C1 / cc1).exp() {
                elsum += ALEG[j - 1] * (-(0.5 * qexpo)).exp() * rinsum.powf(cc1);
            }
        }
        elsum *= 2.0 * b * cc / (2.0 * std::f64::consts::PI).sqrt();
        einsum += elsum;
        blb = bub;
        bub += binc;
        wi += 1.0;
    }
    pr_w += einsum;
    if pr_w <= (C1 / rr).exp() {
        return 0.0;
    }
    pr_w = pr_w.powf(rr);
    pr_w.min(1.0)
}

/// The distribution function of the studentized range of `cc` means with
/// `df` degrees of freedom, `rr` ranges (1 for Tukey's test) — R's `ptukey`,
/// the lower tail; `1 − ` it is the upper. `None` for arguments R refuses.
pub fn ptukey(q: f64, rr: f64, cc: f64, df: f64) -> Option<f64> {
    const NLEGQ: usize = 16;
    const IHALFQ: usize = 8;
    const EPS1: f64 = -30.0;
    const EPS2: f64 = 1.0e-14;
    const DHAF: f64 = 100.0;
    const DQUAR: f64 = 800.0;
    const DEIGH: f64 = 5000.0;
    const DLARG: f64 = 25000.0;
    const XLEGQ: [f64; IHALFQ] = [
        0.989_400_934_991_649_932_596_154_173_450,
        0.944_575_023_073_232_576_077_988_415_535,
        0.865_631_202_387_831_743_880_467_897_712,
        0.755_404_408_355_003_033_895_101_194_847,
        0.617_876_244_402_643_748_446_671_764_049,
        0.458_016_777_657_227_386_342_419_442_984,
        0.281_603_550_779_258_913_230_460_501_460,
        0.950_125_098_376_374_401_853_193_354_250e-1,
    ];
    const ALEGQ: [f64; IHALFQ] = [
        0.271_524_594_117_540_948_517_805_724_560e-1,
        0.622_535_239_386_478_928_628_438_369_944e-1,
        0.951_585_116_824_927_848_099_251_076_022e-1,
        0.124_628_971_255_533_872_052_476_282_192,
        0.149_595_988_816_576_732_081_501_730_547,
        0.169_156_519_395_002_538_189_312_079_030,
        0.182_603_415_044_923_588_866_763_667_969,
        0.189_450_610_455_068_496_285_396_723_208,
    ];
    if q.is_nan() || rr.is_nan() || cc.is_nan() || df.is_nan() {
        return None;
    }
    if q <= 0.0 {
        return Some(0.0);
    }
    if df < 2.0 || rr < 1.0 || cc < 2.0 {
        return None;
    }
    if !q.is_finite() {
        return Some(1.0);
    }
    if df > DLARG {
        return Some(wprob(q, rr, cc));
    }
    let f2 = df * 0.5;
    let mut f2lf = f2 * df.ln() - df * std::f64::consts::LN_2 - ln_gamma(f2);
    let f21 = f2 - 1.0;
    let ff4 = df * 0.25;
    let ulen: f64 = if df <= DHAF {
        1.0
    } else if df <= DQUAR {
        0.5
    } else if df <= DEIGH {
        0.25
    } else {
        0.125
    };
    f2lf += ulen.ln();
    let mut ans = 0.0;
    let mut otsum = 0.0;
    for i in 1..=50 {
        otsum = 0.0;
        let twa1 = f64::from(2 * i - 1) * ulen;
        for jj in 1..=NLEGQ {
            let (j, t1) = if IHALFQ < jj {
                let j = jj - IHALFQ - 1;
                (
                    j,
                    f2lf + f21 * (twa1 + XLEGQ[j] * ulen).ln() - (XLEGQ[j] * ulen + twa1) * ff4,
                )
            } else {
                let j = jj - 1;
                (
                    j,
                    f2lf + f21 * (twa1 - XLEGQ[j] * ulen).ln() + (XLEGQ[j] * ulen - twa1) * ff4,
                )
            };
            if t1 >= EPS1 {
                let qsqz = if IHALFQ < jj {
                    q * ((XLEGQ[j] * ulen + twa1) * 0.5).sqrt()
                } else {
                    q * ((-(XLEGQ[j] * ulen) + twa1) * 0.5).sqrt()
                };
                let wprb = wprob(qsqz, rr, cc);
                otsum += wprb * ALEGQ[j] * t1.exp();
            }
        }
        if f64::from(i) * ulen >= 1.0 && otsum <= EPS2 {
            break;
        }
        ans += otsum;
    }
    // R warns when `otsum > EPS2` here (not converged) and answers anyway.
    let _ = otsum;
    Some(ans.min(1.0))
}

/// R's starting value for [`qtukey`]'s secant search.
fn qinv(p: f64, c: f64, v: f64) -> f64 {
    const P0: f64 = 0.322_232_421_088;
    const Q0: f64 = 0.993_484_626_060e-01;
    const P1: f64 = -1.0;
    const Q1: f64 = 0.588_581_570_495;
    const P2: f64 = -0.342_242_088_547;
    const Q2: f64 = 0.531_103_462_366;
    const P3: f64 = -0.204_231_210_125;
    const Q3: f64 = 0.103_537_752_850;
    const P4: f64 = -0.453_642_210_148e-04;
    const Q4: f64 = 0.385_607_006_34e-02;
    const C1: f64 = 0.8832;
    const C2: f64 = 0.2368;
    const C3: f64 = 1.214;
    const C4: f64 = 1.208;
    const C5: f64 = 1.4142;
    const VMAX: f64 = 120.0;
    let ps = 0.5 - 0.5 * p;
    let yi = (1.0 / (ps * ps)).ln().sqrt();
    let mut t = yi
        + ((((yi * P4 + P3) * yi + P2) * yi + P1) * yi + P0)
            / ((((yi * Q4 + Q3) * yi + Q2) * yi + Q1) * yi + Q0);
    if v < VMAX {
        t += (t * t * t + t) / v / 4.0;
    }
    let mut q = C1 - C2 * t;
    if v < VMAX {
        q += -C3 / v + C4 * t / v;
    }
    t * (q * (c - 1.0).ln() + C5)
}

/// The `p` quantile of the studentized range of `cc` means with `df` degrees
/// of freedom (one range) — R's `qtukey`, by its secant search.
pub fn qtukey(p: f64, cc: f64, df: f64) -> Option<f64> {
    const EPS: f64 = 0.0001;
    const MAXITER: usize = 50;
    if !(0.0..=1.0).contains(&p) || df < 2.0 || cc < 2.0 {
        return None;
    }
    if p == 0.0 {
        return Some(0.0);
    }
    if p == 1.0 {
        return Some(f64::INFINITY);
    }
    let pt = |x: f64| ptukey(x, 1.0, cc, df).unwrap_or(f64::NAN);
    let mut x0 = qinv(p, cc, df);
    let mut valx0 = pt(x0) - p;
    let mut x1 = if valx0 > 0.0 {
        (x0 - 1.0).max(0.0)
    } else {
        x0 + 1.0
    };
    let mut valx1 = pt(x1) - p;
    let mut ans = 0.0;
    for _ in 1..MAXITER {
        ans = x1 - valx1 * (x1 - x0) / (valx1 - valx0);
        valx0 = valx1;
        x0 = x1;
        if ans < 0.0 {
            ans = 0.0;
        }
        valx1 = pt(ans) - p;
        x1 = ans;
        if (x1 - x0).abs() < EPS {
            return Some(ans);
        }
    }
    Some(ans)
}

// --- Shapiro and Wilk -----------------------------------------------------------

/// `cc[0] + cc[1]·x + …` — AS 181.2's `poly`.
fn poly(cc: &[f64], x: f64) -> f64 {
    let nord = cc.len();
    let mut ret = cc[0];
    if nord > 1 {
        let mut p = x * cc[nord - 1];
        for j in (1..nord - 1).rev() {
            p = (p + cc[j]) * x;
        }
        ret += p;
    }
    ret
}

/// Shapiro and Wilk's W of `x`, sorted ascending, and its p-value — R's
/// `swilk` for 3 to 5,000 values (R's `shapiro.test` refuses others, and so
/// does this, with `None`, as it does when every value is the same).
pub fn shapiro_wilk(x: &[f64]) -> Option<(f64, f64)> {
    const SMALL: f64 = 1e-19;
    const G: [f64; 2] = [-2.273, 0.459];
    const C1: [f64; 6] = [0.0, 0.221_157, -0.147_981, -2.071_19, 4.434_685, -2.706_056];
    const C2: [f64; 6] = [
        0.0, 0.042_981, -0.293_762, -1.752_461, 5.682_633, -3.582_633,
    ];
    const C3: [f64; 4] = [0.544, -0.399_78, 0.025_054, -6.714e-4];
    const C4: [f64; 4] = [1.3822, -0.778_57, 0.062_767, -0.002_032_2];
    const C5: [f64; 4] = [-1.5861, -0.310_82, -0.083_751, 0.003_891_5];
    const C6: [f64; 3] = [-0.4803, -0.082_676, 0.003_030_2];
    let n = x.len();
    if !(3..=5000).contains(&n) {
        return None;
    }
    let range0 = x[n - 1] - x[0];
    if range0 <= 0.0 || !range0.is_finite() {
        return None;
    }
    // R rescales a tiny range so that the check below passes.
    let scaled: Vec<f64>;
    let x = if range0 < 1e-10 {
        scaled = x.iter().map(|v| v / range0).collect();
        &scaled[..]
    } else {
        x
    };
    let nn2 = n / 2;
    let mut a = vec![0.0; nn2 + 1]; // 1-based
    let an = n as f64;
    if n == 3 {
        a[1] = 0.707_106_78;
    } else {
        let an25 = an + 0.25;
        let mut summ2 = 0.0;
        for (i, ai) in a.iter_mut().enumerate().skip(1) {
            *ai = qnorm((i as f64 - 0.375) / an25);
            summ2 += *ai * *ai;
        }
        summ2 *= 2.0;
        let ssumm2 = summ2.sqrt();
        let rsn = 1.0 / an.sqrt();
        let a1 = poly(&C1, rsn) - a[1] / ssumm2;
        let (i1, fac) = if n > 5 {
            let a2 = -a[2] / ssumm2 + poly(&C2, rsn);
            let fac = ((summ2 - 2.0 * (a[1] * a[1]) - 2.0 * (a[2] * a[2]))
                / (1.0 - 2.0 * (a1 * a1) - 2.0 * (a2 * a2)))
                .sqrt();
            a[2] = a2;
            (3, fac)
        } else {
            (
                2,
                ((summ2 - 2.0 * (a[1] * a[1])) / (1.0 - 2.0 * (a1 * a1))).sqrt(),
            )
        };
        a[1] = a1;
        for ai in a.iter_mut().skip(i1) {
            *ai /= -fac;
        }
    }
    let range = x[n - 1] - x[0];
    if range < SMALL {
        return None;
    }
    let sign = |v: i64| -> f64 {
        match v.cmp(&0) {
            std::cmp::Ordering::Greater => 1.0,
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
        }
    };
    let mut sx = x[0] / range;
    let mut sa = -a[1];
    let (mut i, mut j) = (1_i64, n as i64 - 1);
    while i < n as i64 {
        let xi = x[i as usize] / range;
        sx += xi;
        i += 1;
        if i != j {
            sa += sign(i - j) * a[i.min(j) as usize];
        }
        j -= 1;
    }
    sa /= an;
    sx /= an;
    let (mut ssa, mut ssx, mut sax) = (0.0, 0.0, 0.0);
    let mut j = n as i64 - 1;
    for (i, xi) in x.iter().enumerate() {
        let i = i as i64;
        let asa = if i != j {
            sign(i - j) * a[1 + i.min(j) as usize] - sa
        } else {
            -sa
        };
        let xsx = xi / range - sx;
        ssa += asa * asa;
        ssx += xsx * xsx;
        sax += asa * xsx;
        j -= 1;
    }
    // 1 − W, computed so as not to lose W near 1.
    let ssassx = (ssa * ssx).sqrt();
    let w1 = (ssassx - sax) * (ssassx + sax) / (ssa * ssx);
    let w = 1.0 - w1;
    if n == 3 {
        let pi6 = 1.909_859_317_102_74;
        let stqr = 1.047_197_551_196_60;
        return Some((w, (pi6 * (w.sqrt().asin() - stqr)).max(0.0)));
    }
    let mut y = w1.ln();
    let xx = an.ln();
    let (m, s) = if n <= 11 {
        let gamma = poly(&G, an);
        if y >= gamma {
            return Some((w, 1e-99));
        }
        y = -(gamma - y).ln();
        (poly(&C3, an), poly(&C4, an).exp())
    } else {
        (poly(&C5, xx), poly(&C6, xx).exp())
    };
    Some((w, pnorm_upper((y - m) / s)))
}

// --- the rank statistics' exact distributions ---------------------------------

/// The exact null distribution of the Wilcoxon rank-sum statistic `W` (the
/// Mann–Whitney U of the first sample) for samples of `m` and `n` values
/// without ties: `W` runs from 0 to `m·n`.
pub struct RankSum {
    /// `probabilities[w]` = P(W = w).
    probabilities: Vec<f64>,
}

impl RankSum {
    /// The distribution for samples of `m` and `n`; R uses it below 50 each.
    pub fn new(m: usize, n: usize) -> RankSum {
        // Ways to choose `size` of the ranks 1..=m+n with the sum `t`: add one
        // rank at a time. W is the first sample's rank sum less m(m+1)/2.
        let total = m + n;
        let max_sum = total * (total + 1) / 2;
        let mut ways = vec![vec![0.0_f64; max_sum + 1]; m + 1];
        ways[0][0] = 1.0;
        for rank in 1..=total {
            for size in (1..=m.min(rank)).rev() {
                let (lower, upper) = ways.split_at_mut(size);
                let (from, to) = (&lower[size - 1], &mut upper[0]);
                for t in (rank..=max_sum).rev() {
                    let add = from[t - rank];
                    if add != 0.0 {
                        to[t] += add;
                    }
                }
            }
        }
        let offset = m * (m + 1) / 2;
        let counts: Vec<f64> = (0..=m * n).map(|w| ways[m][w + offset]).collect();
        let all: f64 = counts.iter().sum();
        RankSum {
            probabilities: counts.into_iter().map(|c| c / all).collect(),
        }
    }

    /// P(W ≤ q).
    pub fn lower(&self, q: f64) -> f64 {
        if q < 0.0 {
            return 0.0;
        }
        let q = (q + 1e-7).floor() as usize;
        self.probabilities.iter().take(q + 1).sum::<f64>().min(1.0)
    }

    /// P(W > q).
    pub fn upper(&self, q: f64) -> f64 {
        if q < 0.0 {
            return 1.0;
        }
        let q = (q + 1e-7).floor() as usize;
        self.probabilities.iter().skip(q + 1).sum::<f64>().min(1.0)
    }

    /// The smallest `q` with P(W ≤ q) ≥ `p` — R's `qwilcox`.
    pub fn quantile(&self, p: f64) -> usize {
        let p = p - 10.0 * f64::EPSILON;
        let mut sum = 0.0;
        for (q, prob) in self.probabilities.iter().enumerate() {
            sum += prob;
            if sum >= p {
                return q;
            }
        }
        self.probabilities.len() - 1
    }
}

/// The exact null distribution of the Wilcoxon signed-rank statistic `V` for
/// `n` values without ties or zeros: `V` runs from 0 to `n(n+1)/2`.
pub struct SignedRank {
    probabilities: Vec<f64>,
}

impl SignedRank {
    /// The distribution for `n` values; R uses it below 50.
    pub fn new(n: usize) -> SignedRank {
        let max = n * (n + 1) / 2;
        let mut ways = vec![0.0_f64; max + 1];
        ways[0] = 1.0;
        for rank in 1..=n {
            for t in (rank..=max).rev() {
                ways[t] += ways[t - rank];
            }
        }
        let all: f64 = ways.iter().sum();
        SignedRank {
            probabilities: ways.into_iter().map(|c| c / all).collect(),
        }
    }

    /// P(V ≤ q).
    pub fn lower(&self, q: f64) -> f64 {
        if q < 0.0 {
            return 0.0;
        }
        let q = (q + 1e-7).floor() as usize;
        self.probabilities.iter().take(q + 1).sum::<f64>().min(1.0)
    }

    /// P(V > q).
    pub fn upper(&self, q: f64) -> f64 {
        if q < 0.0 {
            return 1.0;
        }
        let q = (q + 1e-7).floor() as usize;
        self.probabilities.iter().skip(q + 1).sum::<f64>().min(1.0)
    }

    /// The smallest `q` with P(V ≤ q) ≥ `p` — R's `qsignrank`.
    pub fn quantile(&self, p: f64) -> usize {
        let p = p - 10.0 * f64::EPSILON;
        let mut sum = 0.0;
        for (q, prob) in self.probabilities.iter().enumerate() {
            sum += prob;
            if sum >= p {
                return q;
            }
        }
        self.probabilities.len() - 1
    }
}

// --- Spearman's S ------------------------------------------------------------------

/// P(S ≥ `is`), or P(S < `is`) with `lower`, for Spearman's
/// `S = (n³ − n)(1 − ρ)/6` of `n` values without ties — R's `prho` (AS 89):
/// exact for `n` ≤ 9 and an Edgeworth series above.
pub fn prho(n: usize, is: f64, lower: bool) -> f64 {
    const C: [f64; 12] = [
        0.2274, 0.2531, 0.1745, 0.0758, 0.1033, 0.3932, 0.0879, 0.0151, 0.0072, 0.0831, 0.0131,
        4.6e-4,
    ];
    const N_SMALL: usize = 9;
    let start = if lower { 0.0 } else { 1.0 };
    if n <= 1 || is <= 0.0 {
        return start;
    }
    let nf = n as f64;
    let n3 = nf * (nf * nf - 1.0) / 3.0;
    if is > n3 {
        return 1.0 - start;
    }
    if n <= N_SMALL {
        // Every permutation, in R's order (the order does not matter to the
        // count, but it is R's).
        let mut l: Vec<i64> = (1..=n as i64).collect();
        let nfac: i64 = (1..=n as i64).product();
        let ifr = if is == n3 {
            1
        } else {
            let mut ifr = 0;
            for _ in 0..nfac {
                let ise: i64 = l
                    .iter()
                    .enumerate()
                    .map(|(i, li)| {
                        let d = i as i64 + 1 - li;
                        d * d
                    })
                    .sum();
                if is <= ise as f64 {
                    ifr += 1;
                }
                let mut n1 = n;
                loop {
                    let mt = l[0];
                    for i in 1..n1 {
                        l[i - 1] = l[i];
                    }
                    n1 -= 1;
                    l[n1] = mt;
                    if !(mt == n1 as i64 + 1 && n1 > 1) {
                        break;
                    }
                }
            }
            ifr
        };
        let count = if lower { nfac - ifr } else { ifr };
        return count as f64 / nfac as f64;
    }
    let b = 1.0 / nf;
    let x = (6.0 * (is - 1.0) * b / (nf * nf - 1.0) - 1.0) * (nf - 1.0).sqrt();
    let y = x * x;
    let u = x
        * b
        * (C[0]
            + b * (C[1] + C[2] * b)
            + y * (-C[3] + b * (C[4] + C[5] * b)
                - y * b * (C[6] + C[7] * b - y * (C[8] - C[9] * b + y * b * (C[10] - C[11] * y)))));
    let y = u / (y / 2.0).exp();
    let tail = if lower { pnorm(x) } else { pnorm_upper(x) };
    ((if lower { -y } else { y }) + tail).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::reference::{close, nums, reference};

    #[test]
    fn the_studentized_range_is_rs() {
        let r = reference();
        for case in r["tukey"].as_array().unwrap() {
            let (q, k, df) = (
                case["q"].as_f64().unwrap(),
                case["k"].as_f64().unwrap(),
                case["df"].as_f64().unwrap(),
            );
            let p = ptukey(q, 1.0, k, df).unwrap();
            // R integrates in long double, and its pnorm is not statrs's.
            close(p, case["p"].as_f64().unwrap(), 1e-10, "ptukey");
            close(
                1.0 - p,
                case["upper"].as_f64().unwrap(),
                1e-10,
                "ptukey upper",
            );
        }
        for case in r["qtukey"].as_array().unwrap() {
            let q = qtukey(
                case["p"].as_f64().unwrap(),
                case["k"].as_f64().unwrap(),
                case["df"].as_f64().unwrap(),
            )
            .unwrap();
            close(q, case["q"].as_f64().unwrap(), 1e-9, "qtukey");
        }
    }

    #[test]
    fn shapiro_wilk_is_rs() {
        for case in reference()["shapiro"].as_array().unwrap() {
            let mut x = nums(&case["x"]);
            x.sort_by(f64::total_cmp);
            let (w, p) = shapiro_wilk(&x).unwrap();
            let name = case["name"].as_str().unwrap();
            close(w, case["w"].as_f64().unwrap(), 1e-12, name);
            close(p, case["p"].as_f64().unwrap(), 1e-10, name);
        }
        assert!(shapiro_wilk(&[1.0, 2.0]).is_none());
        assert!(shapiro_wilk(&[3.0, 3.0, 3.0, 3.0]).is_none());
    }

    #[test]
    fn exact_rank_distributions_sum_to_one_and_are_symmetric() {
        let w = RankSum::new(4, 6);
        assert!((w.lower(24.0) - 1.0).abs() < 1e-15);
        // Symmetric about mn/2 = 12.
        assert!((w.lower(5.0) - w.upper(18.0)).abs() < 1e-15);
        // choose(10, 4) = 210 arrangements; W = 0 is one of them.
        assert!((w.lower(0.0) - 1.0 / 210.0).abs() < 1e-15);
        let v = SignedRank::new(5);
        // 2^5 subsets; V = 0 and V = 1 one each.
        assert!((v.lower(1.0) - 2.0 / 32.0).abs() < 1e-15);
        assert!((v.lower(15.0) - 1.0).abs() < 1e-15);
    }

    #[test]
    fn spearman_s_is_exact_for_small_samples() {
        // n = 3: S takes 0, 2, 6, 8 with counts 1, 2, 2, 1.
        assert!((prho(3, 6.0, false) - 3.0 / 6.0).abs() < 1e-15);
        assert!((prho(3, 6.0, true) - 3.0 / 6.0).abs() < 1e-15);
        assert!((prho(3, 8.0, false) - 1.0 / 6.0).abs() < 1e-15);
    }
}
