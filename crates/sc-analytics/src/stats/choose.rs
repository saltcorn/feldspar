//! Choosing the tests (analytics TODO A2.13): from the columns on the Y, X
//! and Wrap drop zones and their types, as JMP's "Fit Y by X" does — the
//! person never picks a test by name. The goals document's table:
//!
//! | Y | X | design | tests (alternative) |
//! |---|---|---|---|
//! | number | — | [`Design::OneNumber`] | one-sample t, Shapiro–Wilk (signed-rank) |
//! | category | — | [`Design::OneCategory`] | chi-square fit, binomial for two values |
//! | number | category | [`Design::NumberByGroups`] | Welch t (Mann–Whitney) for two groups; ANOVA and Tukey (Kruskal–Wallis) for more |
//! | category | category | [`Design::TwoCategories`] | chi-square independence (Fisher's exact) |
//! | number | number | [`Design::TwoNumbers`] | Pearson, linear regression (Spearman) |
//! | category | number | [`Design::CategoryByNumber`] | logistic regression |
//! | two numbers, paired | — | [`Design::Paired`] | paired t (signed-rank) |
//!
//! A **number** is an integer, number or decimal column that is not a foreign
//! key and is not binned; a **category** is text, a boolean, a foreign key or
//! a binned number. Dates are neither: a test compares numbers or groups.
//! Wrap repeats the analysis for each of its values.
//!
//! What each test assumes is checked when the data is read ([`Check`]), and
//! where an assumption is doubtful the alternative is the one the sentence
//! reports ([`Section::preferred`](super::Section::preferred)); both are
//! always shown.

use sc_dataset::{ColType, StageShape};
use serde::{Deserialize, Serialize};

use super::hypothesis::TestKind;
use crate::plot::validate::field_check;
use crate::plot::{Channel, DataRef, FieldDef};

/// What the explorer asks to test: the columns on Y, X and Wrap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestSpec {
    /// The dataset.
    pub data: DataRef,
    /// The response: one column, or two in paired mode.
    #[serde(default)]
    pub y: Vec<FieldDef>,
    /// The factor, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<FieldDef>,
    /// The column the analysis is repeated for each value of (Wrap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<FieldDef>,
    /// Whether two numbers on Y are paired measurements of the same rows.
    #[serde(default)]
    pub paired: bool,
    /// The mean (or location) a single number is tested against.
    #[serde(default)]
    pub mu: f64,
    /// The confidence level of the intervals.
    #[serde(default = "default_level")]
    pub level: f64,
}

fn default_level() -> f64 {
    0.95
}

/// Which analysis the roles make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Design {
    /// A number on its own.
    OneNumber,
    /// A category on its own.
    OneCategory,
    /// A number by the groups of a category.
    NumberByGroups,
    /// A category by a category.
    TwoCategories,
    /// A number by a number.
    TwoNumbers,
    /// A category with two values by a number.
    CategoryByNumber,
    /// Two numbers measured on the same rows.
    Paired,
}

impl Design {
    /// The tests it runs, the main ones first; [`alternatives`](Self::alternatives)
    /// are among them. Which apply to the data (two groups or more, a 2 × 2
    /// table) is settled when it is read.
    pub fn tests(self) -> &'static [TestKind] {
        match self {
            Design::OneNumber => &[
                TestKind::OneSampleT,
                TestKind::ShapiroWilk,
                TestKind::SignedRank,
            ],
            Design::OneCategory => &[TestKind::ChiSquareFit, TestKind::Binomial],
            Design::NumberByGroups => &[
                TestKind::WelchT,
                TestKind::Anova,
                TestKind::MannWhitney,
                TestKind::KruskalWallis,
            ],
            Design::TwoCategories => &[TestKind::ChiSquareIndependence, TestKind::FisherExact],
            Design::TwoNumbers => &[
                TestKind::Pearson,
                TestKind::LinearRegression,
                TestKind::Spearman,
            ],
            Design::CategoryByNumber => &[TestKind::LogisticRegression],
            Design::Paired => &[TestKind::PairedT, TestKind::PairedSignedRank],
        }
    }

    /// The tests shown alongside as the alternative when the main one's
    /// assumptions are doubtful.
    pub fn alternatives(self) -> &'static [TestKind] {
        match self {
            Design::OneNumber => &[TestKind::SignedRank],
            Design::NumberByGroups => &[TestKind::MannWhitney, TestKind::KruskalWallis],
            Design::TwoCategories => &[TestKind::FisherExact],
            Design::TwoNumbers => &[TestKind::Spearman],
            Design::Paired => &[TestKind::PairedSignedRank],
            Design::OneCategory | Design::CategoryByNumber => &[],
        }
    }
}

/// How a column takes part in a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A number to summarise.
    Number,
    /// Values to group by.
    Category,
}

/// What the roles make: the design and the type of each role's column.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Choice {
    pub design: Design,
    pub y: Vec<(FieldDef, ColType)>,
    pub x: Option<(FieldDef, ColType)>,
    pub by: Option<(FieldDef, ColType)>,
}

/// How a column on `channel` takes part: a number or a category, or the
/// sentence saying why it cannot.
fn kind_of(channel: Channel, f: &FieldDef, shape: &StageShape) -> Result<(Kind, ColType), String> {
    let ty = field_check(channel, f, shape, false)?;
    let key = shape.column(&f.field).is_some_and(|c| c.key.is_some());
    if key || f.bin.is_some() {
        return Ok((Kind::Category, ty));
    }
    match ty {
        ColType::Int | ColType::Float | ColType::Decimal | ColType::Unknown => {
            Ok((Kind::Number, ty))
        }
        ColType::Text | ColType::Bool | ColType::Uuid => Ok((Kind::Category, ty)),
        ColType::Date | ColType::Time | ColType::Timestamp => Err(format!(
            "`{}` is {}, and the tests compare numbers and groups; make a column of the year or month in the dataset to compare periods",
            f.field,
            crate::plot::validate::article(ty)
        )),
        ColType::Json | ColType::Bytes | ColType::Geometry => Err(format!(
            "`{}` is {}, which cannot be tested",
            f.field,
            crate::plot::validate::article(ty)
        )),
    }
}

/// The design the roles make, or the sentence saying why there is no test
/// for them.
pub(crate) fn choose(spec: &TestSpec, shape: &StageShape) -> Result<Choice, String> {
    if !(spec.level > 0.0 && spec.level < 1.0) {
        return Err("a confidence level is between 0 and 1, such as 0.95".to_owned());
    }
    if !spec.mu.is_finite() {
        return Err("the value a mean is tested against must be a number".to_owned());
    }
    let by = match &spec.by {
        None => None,
        Some(f) => {
            let ty = field_check(Channel::Wrap, f, shape, true)?;
            if matches!(ty, ColType::Json | ColType::Bytes | ColType::Geometry) {
                return Err(format!("`{}` cannot be a Wrap group", f.field));
            }
            Some((f.clone(), ty))
        }
    };
    let x = match &spec.x {
        None => None,
        Some(f) => Some((f.clone(), kind_of(Channel::X, f, shape)?)),
    };
    let mut y = Vec::new();
    for f in &spec.y {
        y.push((f.clone(), kind_of(Channel::Y, f, shape)?));
    }
    let typed = |v: &[(FieldDef, (Kind, ColType))]| -> Vec<(FieldDef, ColType)> {
        v.iter().map(|(f, (_, t))| (f.clone(), *t)).collect()
    };
    let x_typed = x.as_ref().map(|(f, (_, t))| (f.clone(), *t));
    if spec.paired {
        if y.len() != 2 {
            return Err(
                "paired mode compares two numbers measured on the same rows; put two columns on Y"
                    .to_owned(),
            );
        }
        if x.is_some() {
            return Err(
                "paired mode compares the two columns on Y with each other; take the column off X"
                    .to_owned(),
            );
        }
        if let Some((f, _)) = y.iter().find(|(_, (k, _))| *k != Kind::Number) {
            return Err(format!(
                "paired mode compares numbers, and `{}` is not one",
                f.field
            ));
        }
        return Ok(Choice {
            design: Design::Paired,
            y: typed(&y),
            x: None,
            by,
        });
    }
    let (yf, (yk, _)) = match y.as_slice() {
        [] => {
            return Err("put a column on Y to test it".to_owned());
        }
        [one] => one.clone(),
        _ => {
            return Err(
                "a test compares one column on Y; take the others off, or choose paired mode for two measurements of the same rows"
                    .to_owned(),
            );
        }
    };
    if let Some((xf, _)) = &x {
        if xf.field == yf.field {
            return Err(format!(
                "`{}` is on both X and Y; a test needs two different columns",
                xf.field
            ));
        }
    }
    let design = match (yk, x.as_ref().map(|(_, (k, _))| *k)) {
        (Kind::Number, None) => Design::OneNumber,
        (Kind::Category, None) => Design::OneCategory,
        (Kind::Number, Some(Kind::Category)) => Design::NumberByGroups,
        (Kind::Category, Some(Kind::Category)) => Design::TwoCategories,
        (Kind::Number, Some(Kind::Number)) => Design::TwoNumbers,
        (Kind::Category, Some(Kind::Number)) => Design::CategoryByNumber,
    };
    Ok(Choice {
        design,
        y: typed(&y),
        x: x_typed,
        by,
    })
}

/// Below this many values, a group is too small to judge (or to trust a test
/// that assumes normality on).
pub const SMALL_GROUP: u64 = 10;
/// From this many values on, a mean is close enough to normal (by the central
/// limit theorem) that a test of normality failing is not a reason to prefer
/// the rank test.
pub const LARGE_GROUP: u64 = 50;
/// The p-value below which an assumption is doubtful.
pub const ASSUMPTION_LEVEL: f64 = 0.05;
/// The smallest expected count a chi-square test is trusted with.
pub const MIN_EXPECTED: f64 = 5.0;
/// The fewest of the rarer outcome a logistic regression is trusted with.
pub const MIN_EVENTS: u64 = 10;

/// What an assumption check found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    /// Which assumption: `group_size`, `normality`, `equal_variances`,
    /// `expected_counts`, `events`.
    pub check: &'static str,
    /// Whether it holds well enough.
    pub ok: bool,
    /// What it is about: a group's value (for a group's size or normality),
    /// or `values`, `differences` or `residuals` for normality.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub of: Option<serde_json::Value>,
    /// The number of values it judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    /// The p-value of the test it used (Shapiro–Wilk, Levene).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p_value: Option<f64>,
    /// The number it compared: the smallest expected count, the number of
    /// the rarer outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

impl Check {
    pub(crate) fn new(check: &'static str, ok: bool) -> Check {
        Check {
            check,
            ok,
            of: None,
            n: None,
            p_value: None,
            value: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_dataset::{ForeignKey, Grain, StageColumn};

    fn shape() -> StageShape {
        let col = |name: &str, ty: ColType| StageColumn {
            name: name.to_owned(),
            ty,
            key: None,
        };
        let mut columns = vec![
            col("price", ColType::Float),
            col("area", ColType::Float),
            col("bedrooms", ColType::Int),
            col("street", ColType::Text),
            col("sold", ColType::Bool),
            col("listed", ColType::Date),
            col("before", ColType::Float),
            col("after", ColType::Float),
        ];
        let mut fk = col("neighbourhood", ColType::Int);
        fk.key = Some(ForeignKey {
            table: "neighbourhoods".to_owned(),
            field: "id".to_owned(),
        });
        columns.push(fk);
        StageShape {
            columns,
            grain: Grain::Derived,
        }
    }

    fn spec(y: &[FieldDef], x: Option<FieldDef>) -> TestSpec {
        TestSpec {
            data: DataRef::Dataset {
                dataset: sc_dataset::DatasetId::new(),
            },
            y: y.to_vec(),
            x,
            by: None,
            paired: false,
            mu: 0.0,
            level: 0.95,
        }
    }

    fn design(y: &[&str], x: Option<FieldDef>) -> Result<Design, String> {
        let y: Vec<FieldDef> = y.iter().map(|f| FieldDef::of(*f)).collect();
        choose(&spec(&y, x), &shape()).map(|c| c.design)
    }

    #[test]
    fn the_table_of_the_goals_document() {
        assert_eq!(design(&["price"], None), Ok(Design::OneNumber));
        assert_eq!(design(&["street"], None), Ok(Design::OneCategory));
        assert_eq!(design(&["sold"], None), Ok(Design::OneCategory));
        assert_eq!(
            design(&["price"], Some(FieldDef::of("neighbourhood"))),
            Ok(Design::NumberByGroups)
        );
        assert_eq!(
            design(&["street"], Some(FieldDef::of("sold"))),
            Ok(Design::TwoCategories)
        );
        assert_eq!(
            design(&["price"], Some(FieldDef::of("area"))),
            Ok(Design::TwoNumbers)
        );
        // An integer that is not a key is a number.
        assert_eq!(
            design(&["price"], Some(FieldDef::of("bedrooms"))),
            Ok(Design::TwoNumbers)
        );
        assert_eq!(
            design(&["sold"], Some(FieldDef::of("area"))),
            Ok(Design::CategoryByNumber)
        );
        // A binned number is a category.
        assert_eq!(
            design(&["price"], Some(FieldDef::binned("area"))),
            Ok(Design::NumberByGroups)
        );
        assert_eq!(design(&["neighbourhood"], None), Ok(Design::OneCategory));
    }

    #[test]
    fn paired_mode_takes_two_numbers() {
        let mut s = spec(&[FieldDef::of("before"), FieldDef::of("after")], None);
        s.paired = true;
        assert_eq!(choose(&s, &shape()).map(|c| c.design), Ok(Design::Paired));
        s.y.pop();
        assert!(
            choose(&s, &shape())
                .unwrap_err()
                .contains("two columns on Y")
        );
        s.y = vec![FieldDef::of("before"), FieldDef::of("street")];
        assert!(
            choose(&s, &shape())
                .unwrap_err()
                .contains("`street` is not one")
        );
        s.y = vec![FieldDef::of("before"), FieldDef::of("after")];
        s.x = Some(FieldDef::of("street"));
        assert!(
            choose(&s, &shape())
                .unwrap_err()
                .contains("take the column off X")
        );
    }

    #[test]
    fn what_has_no_test_is_said_in_a_sentence() {
        assert_eq!(
            design(&[], None).unwrap_err(),
            "put a column on Y to test it"
        );
        assert!(
            design(&["price", "area"], None)
                .unwrap_err()
                .contains("paired mode")
        );
        assert!(
            design(&["listed"], None)
                .unwrap_err()
                .contains("`listed` is a date")
        );
        assert!(
            design(&["price"], Some(FieldDef::of("price")))
                .unwrap_err()
                .contains("both X and Y")
        );
        assert!(
            design(&["nope"], None)
                .unwrap_err()
                .contains("`nope` is not a column")
        );
        assert!(
            design(&["price"], Some(FieldDef::binned("street")))
                .unwrap_err()
                .contains("only numbers can be binned")
        );
        let mut s = spec(&[FieldDef::of("price")], None);
        s.by = Some(FieldDef::of("area"));
        assert!(choose(&s, &shape()).unwrap_err().contains("bin it"));
        s.by = Some(FieldDef::binned("area"));
        assert!(choose(&s, &shape()).is_ok());
        s.level = 1.5;
        assert!(
            choose(&s, &shape())
                .unwrap_err()
                .contains("confidence level")
        );
    }

    #[test]
    fn every_alternative_is_one_of_the_designs_tests() {
        for d in [
            Design::OneNumber,
            Design::OneCategory,
            Design::NumberByGroups,
            Design::TwoCategories,
            Design::TwoNumbers,
            Design::CategoryByNumber,
            Design::Paired,
        ] {
            for a in d.alternatives() {
                assert!(d.tests().contains(a), "{d:?}");
            }
        }
    }
}
