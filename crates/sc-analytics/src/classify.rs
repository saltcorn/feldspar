//! **Classification** for a map's graduated colours (analytics TODO A5.9):
//! where the classes of a number column break, by quantiles, by equal
//! intervals or by natural breaks.
//!
//! Pure functions over numbers. The values come from
//! [`layer_sketch`](crate::layer::layer_sketch): every value when a layer is
//! small, and otherwise values evenly spaced in rank through the sorted column
//! — a sorted sample, the same every time it is read, so a map's classes do not
//! move between two renders of the same data.
//!
//! **The breaks** are `k + 1` numbers for `k` classes: the smallest value, the
//! lower bound of each class after the first, and the largest value. Class `i`
//! holds the values from `breaks[i]` up to, not including, `breaks[i + 1]`;
//! the last class includes the largest value. A column with fewer distinct
//! values than classes gets fewer classes, never an empty one.

use serde::{Deserialize, Serialize};

/// How a number column is cut into classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// The same number of features in each class.
    Quantile,
    /// Classes of the same width, from the smallest value to the largest.
    EqualInterval,
    /// Jenks' natural breaks: the classes whose values are closest to their
    /// class's mean (the least sum of squared deviations), found exactly by
    /// Fisher's dynamic programme.
    NaturalBreaks,
}

impl Classification {
    /// Its name in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            Classification::Quantile => "quantiles",
            Classification::EqualInterval => "equal intervals",
            Classification::NaturalBreaks => "natural breaks",
        }
    }
}

/// The fewest and most classes a graduated layer may have: the sequential
/// ramp has seven colours.
pub const MIN_CLASSES: u32 = 2;
/// See [`MIN_CLASSES`].
pub const MAX_CLASSES: u32 = 7;

/// The breaks of `values` (sorted ascending, no missing values) into `classes`
/// classes by `method`. Empty for no values; `[v, v]` for one distinct value.
pub fn breaks(values: &[f64], classes: u32, method: Classification) -> Vec<f64> {
    let values: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    let (Some(&lo), Some(&hi)) = (values.first(), values.last()) else {
        return Vec::new();
    };
    if hi <= lo {
        return vec![lo, hi];
    }
    let k = classes.clamp(MIN_CLASSES, MAX_CLASSES) as usize;
    let inner: Vec<f64> = match method {
        Classification::EqualInterval => (1..k)
            .map(|i| lo + (hi - lo) * i as f64 / k as f64)
            .collect(),
        Classification::Quantile => (1..k)
            .map(|i| quantile7(&values, i as f64 / k as f64))
            .collect(),
        Classification::NaturalBreaks => natural_breaks(&values, k),
    };
    let mut out = vec![lo];
    for b in inner {
        // A break on the smallest value, or on the one before it, would make
        // an empty class. One on the largest value makes a class of the
        // largest values alone, which is not empty.
        if b > *out.last().unwrap_or(&lo) && b <= hi {
            out.push(b);
        }
    }
    out.push(hi);
    out
}

/// The `p` quantile of sorted values, R's type 7 (as `quantile()`).
fn quantile7(sorted: &[f64], p: f64) -> f64 {
    let h = (sorted.len() - 1) as f64 * p;
    let lo = h.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    sorted[lo] + (h - h.floor()) * (sorted[hi] - sorted[lo])
}

/// The lower bounds of classes 2…k of the sorted `values` that minimise the
/// sum of squared deviations from each class's mean.
///
/// `cost[c][j]` is the least total for the first `j + 1` values in `c + 1`
/// classes; `start[c][j]` is where its last class begins. A class boundary
/// never splits a run of equal values, so equal values share a class.
fn natural_breaks(values: &[f64], k: usize) -> Vec<f64> {
    let n = values.len();
    // Where each value's run of equal values begins: a class may start only
    // there.
    let can_start: Vec<bool> = (0..n)
        .map(|i| i == 0 || values[i] > values[i - 1])
        .collect();
    let distinct = can_start.iter().filter(|s| **s).count();
    let k = k.min(distinct);
    if k < 2 {
        return Vec::new();
    }
    let mut sum = vec![0.0; n + 1];
    let mut sq = vec![0.0; n + 1];
    for (i, v) in values.iter().enumerate() {
        sum[i + 1] = sum[i] + v;
        sq[i + 1] = sq[i] + v * v;
    }
    // The squared deviations of values[i..=j] from their mean.
    let ssd = |i: usize, j: usize| {
        let m = (j - i + 1) as f64;
        let s = sum[j + 1] - sum[i];
        ((sq[j + 1] - sq[i]) - s * s / m).max(0.0)
    };
    let mut cost = vec![vec![f64::INFINITY; n]; k];
    let mut start = vec![vec![0usize; n]; k];
    for (j, c) in cost[0].iter_mut().enumerate() {
        *c = ssd(0, j);
    }
    for c in 1..k {
        for j in c..n {
            // The last class is values[i..=j]; the classes before it end at i − 1.
            for i in c..=j {
                if !can_start[i] || cost[c - 1][i - 1].is_infinite() {
                    continue;
                }
                let total = cost[c - 1][i - 1] + ssd(i, j);
                if total < cost[c][j] {
                    cost[c][j] = total;
                    start[c][j] = i;
                }
            }
        }
    }
    // The last value must end a run, so walk back from the end.
    let mut out = Vec::with_capacity(k - 1);
    let mut j = n - 1;
    for c in (1..k).rev() {
        let i = start[c][j];
        out.push(values[i]);
        j = i - 1;
    }
    out.reverse();
    out
}

/// The class `value` falls in, `0..breaks.len() - 1`; `None` outside the
/// breaks.
pub fn class_of(breaks: &[f64], value: f64) -> Option<usize> {
    if breaks.len() < 2 || value < breaks[0] || value > breaks[breaks.len() - 1] {
        return None;
    }
    let last = breaks.len() - 2;
    Some(
        breaks[1..breaks.len() - 1]
            .iter()
            .take_while(|b| value >= **b)
            .count()
            .min(last),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(values: &[f64], breaks: &[f64]) -> Vec<usize> {
        let mut out = vec![0; breaks.len() - 1];
        for v in values {
            out[class_of(breaks, *v).expect("inside")] += 1;
        }
        out
    }

    #[test]
    fn natural_breaks_find_the_gaps() {
        let values = [1.0, 2.0, 3.0, 10.0, 11.0, 12.0, 20.0, 21.0, 22.0];
        assert_eq!(
            breaks(&values, 3, Classification::NaturalBreaks),
            vec![1.0, 10.0, 20.0, 22.0]
        );
        // Two classes: the widest gap.
        assert_eq!(
            breaks(
                &[1.0, 1.5, 2.0, 50.0, 51.0],
                2,
                Classification::NaturalBreaks
            ),
            vec![1.0, 50.0, 51.0]
        );
    }

    #[test]
    fn natural_breaks_match_jenks_on_a_known_series() {
        // Four tight groups split at the three widest gaps (6, 6 and 22):
        // {0, 1, 2}, {8, 9}, {15, 16, 17, 18}, {40} — the last a class of one.
        let values = [0.0, 1.0, 2.0, 8.0, 9.0, 15.0, 16.0, 17.0, 18.0, 40.0];
        let b = breaks(&values, 4, Classification::NaturalBreaks);
        assert_eq!(b, vec![0.0, 8.0, 15.0, 40.0, 40.0]);
        assert_eq!(counts(&values, &b), vec![3, 2, 4, 1]);
    }

    #[test]
    fn equal_values_share_a_class() {
        let values = [1.0, 1.0, 1.0, 1.0, 2.0, 2.0, 9.0];
        let b = breaks(&values, 5, Classification::NaturalBreaks);
        // Three distinct values: three classes, however many were asked for.
        assert_eq!(b, vec![1.0, 2.0, 9.0, 9.0]);
        assert_eq!(counts(&values, &b), vec![4, 2, 1]);
    }

    #[test]
    fn quantiles_put_as_many_in_each_class() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        let b = breaks(&values, 4, Classification::Quantile);
        // R: quantile(1:100, c(.25, .5, .75)) = 25.75, 50.5, 75.25.
        assert_eq!(b, vec![1.0, 25.75, 50.5, 75.25, 100.0]);
        assert_eq!(counts(&values, &b), vec![25, 25, 25, 25]);
    }

    #[test]
    fn equal_intervals_are_as_wide() {
        let b = breaks(&[0.0, 3.0, 10.0], 5, Classification::EqualInterval);
        assert_eq!(b, vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0]);
    }

    #[test]
    fn edge_cases() {
        assert!(breaks(&[], 5, Classification::Quantile).is_empty());
        assert_eq!(
            breaks(&[4.0, 4.0], 5, Classification::Quantile),
            vec![4.0, 4.0]
        );
        // Skewed: most quantiles land on the same value, which makes one class
        // rather than empty ones.
        let skewed = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 5.0];
        let b = breaks(&skewed, 4, Classification::Quantile);
        assert_eq!(b.first(), Some(&0.0));
        assert_eq!(b.last(), Some(&5.0));
        assert!(b[..b.len() - 1].windows(2).all(|w| w[0] < w[1]), "{b:?}");
        assert!(counts(&skewed, &b).iter().all(|n| *n > 0), "{b:?}");
        // Classes are clamped to what the ramp can colour.
        assert_eq!(
            breaks(
                &(0..100).map(f64::from).collect::<Vec<_>>(),
                20,
                Classification::EqualInterval
            )
            .len(),
            MAX_CLASSES as usize + 1
        );
        assert_eq!(class_of(&[0.0, 1.0, 2.0], 2.0), Some(1));
        assert_eq!(class_of(&[0.0, 1.0, 2.0], 1.0), Some(1));
        assert_eq!(class_of(&[0.0, 1.0, 2.0], 0.5), Some(0));
        assert_eq!(class_of(&[0.0, 1.0, 2.0], 3.0), None);
    }
}
