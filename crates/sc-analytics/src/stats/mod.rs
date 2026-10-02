//! Hypothesis tests in the Data explorer (analytics TODO A2.12–A2.14; the
//! goals document's "Hypothesis tests in the data explorer").
//!
//! - [`hypothesis`](TestResult) — the tests themselves, pure functions over
//!   sufficient statistics or values, each answering a [`TestResult`], with
//!   R's conventions and R's reference values in the tests.
//! - [`dist`] — the distributions R has and `statrs` does not.
//! - [`choose`](TestSpec) — which tests the Y, X and Wrap roles make, from
//!   the columns' types ([`Design`]), and the assumption checks ([`Check`]).
//! - [`run_tests`] — the tests run over a dataset: sufficient statistics in
//!   SQL, values read from a sample, repeated for each value of Wrap.

mod choose;
pub mod dist;
mod hypothesis;
#[cfg(test)]
mod reference;
mod run;

pub use choose::{
    ASSUMPTION_LEVEL, Check, Design, LARGE_GROUP, MIN_EVENTS, MIN_EXPECTED, SMALL_GROUP, TestSpec,
};
pub use hypothesis::{
    Detail, Effect, Estimate, MAX_FISHER_TABLES, Moments, Pairwise, Refusal, Statistic, TestKind,
    TestResult, anova, binomial, chisq_fit, chisq_independence, expected_counts, fisher_exact,
    kruskal_wallis, levene, linear_regression, logistic_regression, mann_whitney, median,
    one_sample_t, paired_signed_rank, paired_t, pearson, ranks, shapiro, signed_rank, spearman,
    tukey_hsd, welch_t,
};
pub use run::{
    Analysis, Comparison, Entry, Level, MAX_GROUPS, MAX_PAIRWISE_GROUPS, MAX_SECTIONS, Section,
    TEST_SAMPLE, TestsAnswer, run_tests,
};
