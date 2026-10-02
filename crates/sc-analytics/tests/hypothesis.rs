//! Hypothesis tests run over a dataset on both backends (analytics TODO
//! A2.12–A2.14), on the ten houses of `plots.rs`. The expected values are R's
//! for the same rows:
//!
//! ```r
//! price <- c(100,200,300,400,150,250,350,1000,120,220)
//! area  <- c(50,60,70,80,55,65,75,90,40,45)
//! hood  <- c(1,1,1,1,2,2,2,2,3,3)
//! sold  <- c(T,F,T,T,F,T,F,T,NA,T)
//! t.test(price); shapiro.test(price); wilcox.test(price, conf.int = TRUE)
//! summary(aov(price ~ factor(hood))); kruskal.test(price, factor(hood))
//! TukeyHSD(aov(price ~ factor(hood)))
//! t.test(price[!sold], price[sold]); wilcox.test(price[!sold], price[sold], conf.int = TRUE)
//! chisq.test(table(hood, sold), correct = FALSE); fisher.test(table(hood, sold))
//! cor.test(area, price); cor.test(area, price, method = "spearman"); lm(price ~ area)
//! glm(sold ~ area, family = binomial)
//! t.test(price, area, paired = TRUE); wilcox.test(price, area, paired = TRUE)
//! chisq.test(c(4, 4, 2)); binom.test(6, 9)
//! ```

use std::sync::Arc;

use sc_analytics::plot::{DataRef, FieldDef};
use sc_analytics::stats::{
    Analysis, Design, Section, TEST_SAMPLE, TestKind, TestResult, TestSpec, TestsAnswer, run_tests,
};
use sc_catalog::{Catalog, DataField};
use sc_dataset::{Base, DatasetDef, DatasetId, save_dataset};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::json;

use super::plots::{Fixture, both};

fn spec(fx: &Fixture, y: &[&str], x: Option<FieldDef>) -> TestSpec {
    TestSpec {
        data: DataRef::Dataset { dataset: fx.houses },
        y: y.iter().map(|f| FieldDef::of(*f)).collect(),
        x,
        by: None,
        paired: false,
        mu: 0.0,
        level: 0.95,
    }
}

async fn analyse(cat: &Catalog, backend: &str, spec: &TestSpec) -> Analysis {
    match run_tests(cat, spec).await.expect("runs") {
        TestsAnswer::Analysis(a) => *a,
        TestsAnswer::Refused { error, .. } => panic!("on {backend}: refused: {error}"),
    }
}

async fn refused(fx: &Fixture, spec: &TestSpec) -> String {
    match run_tests(&fx.cat, spec).await.expect("answers") {
        TestsAnswer::Refused { error, .. } => error,
        TestsAnswer::Analysis(a) => panic!("on {}: tested {:?}", fx.backend, a.design),
    }
}

#[track_caller]
fn result(section: &Section, test: TestKind) -> &TestResult {
    section
        .tests
        .iter()
        .find(|e| e.test == test)
        .and_then(|e| e.result.as_ref())
        .unwrap_or_else(|| panic!("{test:?} has no result: {:?}", section.tests))
}

#[track_caller]
fn near(backend: &str, what: &str, got: f64, want: f64) {
    assert!(
        (got - want).abs() <= 1e-9 * want.abs().max(1.0),
        "on {backend}: {what}: got {got}, R has {want}"
    );
}

#[tokio::test]
async fn a_number_on_its_own() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let a = analyse(&fx.cat, b, &spec(&fx, &["price"], None)).await;
        assert_eq!(a.design, Design::OneNumber);
        let s = &a.sections[0];
        assert_eq!(s.n, 10);
        assert!(s.by.is_none() && s.sampled.is_none());
        let t = result(s, TestKind::OneSampleT);
        near(
            b,
            "t",
            t.statistic.as_ref().unwrap().value,
            3.73653653909152,
        );
        near(b, "p", t.p_value, 0.00465028849027962);
        let e = t.estimate.as_ref().unwrap();
        near(b, "lower", e.lower.unwrap(), 121.926608373379);
        near(b, "upper", e.upper.unwrap(), 496.073391626621);
        let w = result(s, TestKind::ShapiroWilk);
        near(
            b,
            "W",
            w.statistic.as_ref().unwrap().value,
            0.718074246837093,
        );
        near(b, "Shapiro p", w.p_value, 0.00145945287736296);
        let v = result(s, TestKind::SignedRank);
        near(b, "V", v.statistic.as_ref().unwrap().value, 55.0);
        near(b, "signed-rank p", v.p_value, 0.001953125);
        assert_eq!(v.estimate.as_ref().unwrap().value, 250.0, "on {b}");
        // Ten values and clearly not normal: the rank test is preferred.
        let normality = s.checks.iter().find(|c| c.check == "normality").unwrap();
        assert!(!normality.ok, "on {b}");
        assert_eq!(s.preferred, Some(TestKind::SignedRank), "on {b}");
        assert_eq!(s.tests[2].role, "alternative");
    }
    Ok(())
}

#[tokio::test]
async fn a_number_by_three_groups() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let a = analyse(
            &fx.cat,
            b,
            &spec(&fx, &["price"], Some(FieldDef::of("neighbourhood"))),
        )
        .await;
        assert_eq!(a.design, Design::NumberByGroups);
        let s = &a.sections[0];
        let levels: Vec<_> = s.levels.iter().map(|l| (l.value.clone(), l.n)).collect();
        assert_eq!(
            levels,
            vec![(json!(1), 4), (json!(2), 4), (json!(3), 2)],
            "on {b}"
        );
        near(b, "mean of 2", s.levels[1].mean.unwrap(), 437.5);
        let f = result(s, TestKind::Anova);
        near(
            b,
            "F",
            f.statistic.as_ref().unwrap().value,
            0.835527044025157,
        );
        near(b, "ANOVA p", f.p_value, 0.47270642650755);
        assert_eq!(f.df, vec![2.0, 7.0]);
        let kw = result(s, TestKind::KruskalWallis);
        near(
            b,
            "H",
            kw.statistic.as_ref().unwrap().value,
            1.58181818181818,
        );
        near(b, "KW p", kw.p_value, 0.453432396587182);
        // TukeyHSD: 2-1, 3-1, 3-2.
        let pairs: Vec<_> = s.comparisons.iter().map(|c| (c.a, c.b)).collect();
        assert_eq!(pairs, vec![(0, 1), (0, 2), (1, 2)], "on {b}");
        near(b, "2-1", s.comparisons[0].difference, 187.5);
        near(b, "2-1 lower", s.comparisons[0].lower, -367.321731121938);
        near(b, "3-2 p", s.comparisons[2].p_value, 0.511522626864751);
        assert!(
            s.checks.iter().any(|c| c.check == "equal_variances"),
            "on {b}"
        );
        // Groups of four and two are small: the rank test is preferred.
        assert_eq!(s.preferred, Some(TestKind::KruskalWallis), "on {b}");
    }
    Ok(())
}

#[tokio::test]
async fn a_number_by_two_groups() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let a = analyse(
            &fx.cat,
            b,
            &spec(&fx, &["price"], Some(FieldDef::of("sold"))),
        )
        .await;
        let s = &a.sections[0];
        // The house with no `sold` is left out; false before true.
        assert_eq!(s.n, 9, "on {b}");
        assert_eq!(s.levels[0].value, json!(false), "on {b}");
        assert_eq!(s.levels[1].value, json!(true), "on {b}");
        let t = result(s, TestKind::WelchT);
        near(
            b,
            "t",
            t.statistic.as_ref().unwrap().value,
            -1.00829334693438,
        );
        near(b, "df", t.df[0], 6.60077073264259);
        near(b, "p", t.p_value, 0.348836313313166);
        near(
            b,
            "lower",
            t.estimate.as_ref().unwrap().lower.unwrap(),
            -489.26442414635,
        );
        let w = result(s, TestKind::MannWhitney);
        near(b, "W", w.statistic.as_ref().unwrap().value, 6.0);
        near(b, "MW p", w.p_value, 0.547619047619048);
        assert_eq!(w.method, Some("exact"));
        let e = w.estimate.as_ref().unwrap();
        assert_eq!(
            (e.value, e.lower.unwrap(), e.upper.unwrap()),
            (-60.0, -800.0, 130.0),
            "on {b}"
        );
        assert!(s.comparisons.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn two_categories_two_numbers_and_a_logistic_curve() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let a = analyse(
            &fx.cat,
            b,
            &spec(&fx, &["sold"], Some(FieldDef::of("neighbourhood"))),
        )
        .await;
        assert_eq!(a.design, Design::TwoCategories);
        let s = &a.sections[0];
        assert_eq!(s.levels.len(), 3, "on {b}");
        assert_eq!(s.categories.len(), 2, "on {b}");
        let c = result(s, TestKind::ChiSquareIndependence);
        near(b, "X²", c.statistic.as_ref().unwrap().value, 1.125);
        near(b, "chi-square p", c.p_value, 0.569782824730923);
        near(
            b,
            "Fisher p",
            result(s, TestKind::FisherExact).p_value,
            0.999999999999999,
        );
        let expected = s
            .checks
            .iter()
            .find(|c| c.check == "expected_counts")
            .unwrap();
        near(b, "least expected", expected.value.unwrap(), 1.0 / 3.0);
        assert!(!expected.ok);
        assert_eq!(s.preferred, Some(TestKind::FisherExact), "on {b}");

        let a = analyse(
            &fx.cat,
            b,
            &spec(&fx, &["price"], Some(FieldDef::of("area"))),
        )
        .await;
        assert_eq!(a.design, Design::TwoNumbers);
        let s = &a.sections[0];
        let r = result(s, TestKind::Pearson);
        near(
            b,
            "r",
            r.estimate.as_ref().unwrap().value,
            0.816304376521976,
        );
        near(b, "r p", r.p_value, 0.00396536692591747);
        let rho = result(s, TestKind::Spearman);
        near(
            b,
            "rho",
            rho.estimate.as_ref().unwrap().value,
            0.903030303030303,
        );
        near(b, "rho p", rho.p_value, 0.000880224994754143);
        let lm = result(s, TestKind::LinearRegression);
        near(
            b,
            "slope",
            lm.estimate.as_ref().unwrap().value,
            13.3246753246753,
        );
        assert!(
            s.checks
                .iter()
                .any(|c| c.check == "normality" && c.of == Some(json!("residuals")))
        );

        let a = analyse(
            &fx.cat,
            b,
            &spec(&fx, &["sold"], Some(FieldDef::of("area"))),
        )
        .await;
        assert_eq!(a.design, Design::CategoryByNumber);
        let s = &a.sections[0];
        assert_eq!(s.event, Some(1));
        assert_eq!(s.levels[1].value, json!(true), "on {b}");
        let l = result(s, TestKind::LogisticRegression);
        let slope = l.details.iter().find(|d| d.name == "slope").unwrap().value;
        assert!((slope - 0.0177356231180203).abs() < 1e-6, "on {b}: {slope}");
        assert!((l.p_value - 0.731945004001759).abs() < 1e-6, "on {b}");
        // Three of nine are not sold: too few events to trust.
        assert!(!s.checks.iter().find(|c| c.check == "events").unwrap().ok);
    }
    Ok(())
}

#[tokio::test]
async fn one_category_and_paired_columns() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let a = analyse(&fx.cat, b, &spec(&fx, &["neighbourhood"], None)).await;
        assert_eq!(a.design, Design::OneCategory);
        let s = &a.sections[0];
        let fit = result(s, TestKind::ChiSquareFit);
        near(b, "X²", fit.statistic.as_ref().unwrap().value, 0.8);
        near(b, "fit p", fit.p_value, 0.670320046035639);
        assert!(s.tests.iter().all(|e| e.test != TestKind::Binomial));

        let a = analyse(&fx.cat, b, &spec(&fx, &["sold"], None)).await;
        let s = &a.sections[0];
        let bt = result(s, TestKind::Binomial);
        near(b, "binomial p", bt.p_value, 0.5078125);
        let e = bt.estimate.as_ref().unwrap();
        near(b, "proportion", e.value, 6.0 / 9.0);
        near(b, "lower", e.lower.unwrap(), 0.29929505620854);
        assert_eq!(s.preferred, Some(TestKind::Binomial));

        let mut paired = spec(&fx, &["price", "area"], None);
        paired.paired = true;
        let a = analyse(&fx.cat, b, &paired).await;
        assert_eq!(a.design, Design::Paired);
        let s = &a.sections[0];
        assert_eq!(s.levels[0].value, json!("price"));
        near(b, "mean price", s.levels[0].mean.unwrap(), 309.0);
        near(b, "mean area", s.levels[1].mean.unwrap(), 63.0);
        let t = result(s, TestKind::PairedT);
        near(
            b,
            "paired t",
            t.statistic.as_ref().unwrap().value,
            3.12914186902311,
        );
        near(b, "paired p", t.p_value, 0.0121384711902739);
        near(b, "difference", t.estimate.as_ref().unwrap().value, 246.0);
        let w = result(s, TestKind::PairedSignedRank);
        near(b, "paired V", w.statistic.as_ref().unwrap().value, 55.0);
        near(b, "paired W p", w.p_value, 0.001953125);
    }
    Ok(())
}

#[tokio::test]
async fn wrap_repeats_the_analysis_for_each_group() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let mut s = spec(&fx, &["price"], Some(FieldDef::of("sold")));
        s.by = Some(FieldDef::of("neighbourhood"));
        let a = analyse(&fx.cat, b, &s).await;
        assert_eq!(a.by.as_deref(), Some("neighbourhood"));
        let by: Vec<_> = a.sections.iter().map(|s| s.by.clone()).collect();
        assert_eq!(
            by,
            vec![Some(json!(1)), Some(json!(2)), Some(json!(3))],
            "on {b}"
        );
        // North: one house not sold, so no t-test, but a rank-sum test.
        let north = &a.sections[0];
        let welch = north
            .tests
            .iter()
            .find(|e| e.test == TestKind::WelchT)
            .unwrap();
        assert!(
            welch
                .error
                .as_deref()
                .unwrap()
                .contains("at least two values in each group")
        );
        near(
            b,
            "north W p",
            result(north, TestKind::MannWhitney).p_value,
            1.0,
        );
        assert_eq!(north.preferred, Some(TestKind::MannWhitney), "on {b}");
        // South: two and two.
        let south = &a.sections[1];
        let t = result(south, TestKind::WelchT);
        near(
            b,
            "south t",
            t.statistic.as_ref().unwrap().value,
            -0.966234939601246,
        );
        near(b, "south p", t.p_value, 0.494391303963447);
        near(
            b,
            "south W p",
            result(south, TestKind::MannWhitney).p_value,
            0.666666666666667,
        );
        // East: every house with a `sold` was sold.
        let east = &a.sections[2];
        assert_eq!(
            east.error.as_deref(),
            Some(
                "the tests compare groups of `sold`, and the rows of this group that have a `price` all have the same `sold`"
            ),
            "on {b}"
        );
        assert!(east.tests.is_empty());

        // Binned, the group is a bin with both edges.
        let mut s = spec(&fx, &["price"], None);
        s.by = Some(FieldDef {
            field: "area".into(),
            bin: Some(sc_analytics::plot::Bin {
                width: Some(25.0),
                bins: None,
            }),
        });
        let a = analyse(&fx.cat, b, &s).await;
        let edges: Vec<_> = a
            .sections
            .iter()
            .map(|s| {
                let edge = |v: &Option<serde_json::Value>| v.as_ref().and_then(|v| v.as_f64());
                (edge(&s.by), edge(&s.by_end), s.n)
            })
            .collect();
        assert_eq!(
            edges,
            vec![
                (Some(25.0), Some(50.0), 2),
                (Some(50.0), Some(75.0), 5),
                (Some(75.0), Some(100.0), 3)
            ],
            "on {b}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn what_has_no_test_is_said_in_a_sentence() -> Result<()> {
    for fx in both().await? {
        assert_eq!(
            refused(&fx, &spec(&fx, &[], None)).await,
            "put a column on Y to test it"
        );
        assert!(
            refused(&fx, &spec(&fx, &["price", "area"], None))
                .await
                .contains("paired mode")
        );
        let mut s = spec(&fx, &["price"], None);
        s.by = Some(FieldDef::of("area"));
        assert!(refused(&fx, &s).await.contains("bin it"));
        let mut gone = spec(&fx, &["price"], None);
        gone.data = DataRef::Dataset {
            dataset: DatasetId::new(),
        };
        assert!(refused(&fx, &gone).await.contains("is gone"));
    }
    Ok(())
}

/// A table of 6,000 measurements in two groups, more than the tests that
/// read values read.
async fn measurements(cat: &Catalog) -> Result<DatasetId> {
    sc_dataset::bootstrap_datasets(cat).await?;
    cat.create_table(
        "measurements",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("v", TypeRef::Basic(BasicType::Float)),
            DataField::plain("g", TypeRef::Basic(BasicType::Text)),
        ],
    )
    .await?;
    for chunk in (0..6000_i64).collect::<Vec<_>>().chunks(1000) {
        let insert = Insert {
            table: "measurements".into(),
            columns: vec!["id".into(), "v".into(), "g".into()],
            rows: chunk
                .iter()
                .map(|i| {
                    vec![
                        Expr::Lit(Value::Int(*i)),
                        Expr::Lit(Value::Float(
                            ((i * 7919) % 1000) as f64 / 10.0 + (i % 3) as f64,
                        )),
                        Expr::Lit(Value::Text(if i % 2 == 0 { "a" } else { "b" }.into())),
                    ]
                })
                .collect(),
            returning: Vec::new(),
        };
        cat.primary()
            .query(&Statement::from(insert))
            .await?
            .try_collect()
            .await?;
    }
    let def = DatasetDef {
        id: DatasetId::new(),
        name: "Measurements".into(),
        description: String::new(),
        base: Base::Table {
            table: "measurements".into(),
        },
        operations: Vec::new(),
    };
    save_dataset(cat, &def).await?;
    Ok(def.id)
}

#[tokio::test]
async fn rank_tests_read_a_sample_above_the_limit_and_say_so() -> Result<()> {
    let db = TestDb::new().await?;
    let pg =
        Catalog::init(Arc::new(PgDriver::from_pool(db.pool().clone())) as Arc<dyn DatabaseDriver>)
            .await?;
    let lite =
        Catalog::init(Arc::new(SqliteDriver::open_in_memory()?) as Arc<dyn DatabaseDriver>).await?;
    for (cat, b) in [(&pg, "postgres"), (&lite, "sqlite")] {
        let id = measurements(cat).await?;
        let spec = TestSpec {
            data: DataRef::Dataset { dataset: id },
            y: vec![FieldDef::of("v")],
            x: None,
            by: None,
            paired: false,
            mu: 50.0,
            level: 0.95,
        };
        let a = analyse(cat, b, &spec).await;
        let s = &a.sections[0];
        assert_eq!(s.n, 6000, "on {b}");
        assert_eq!(s.sampled, Some(TEST_SAMPLE), "on {b}");
        // The t-test reads every row, in SQL; the rank tests a sample.
        let t = result(s, TestKind::OneSampleT);
        assert_eq!((t.n, t.sampled), (6000, false), "on {b}");
        let w = result(s, TestKind::SignedRank);
        assert!(w.sampled && w.n <= TEST_SAMPLE, "on {b}");
        assert_eq!(result(s, TestKind::ShapiroWilk).n, TEST_SAMPLE, "on {b}");
        // The same sample each time.
        let again = analyse(cat, b, &spec).await;
        let w2 = result(&again.sections[0], TestKind::SignedRank);
        assert_eq!(w.statistic, w2.statistic, "on {b}");

        // Wrap by the group: 3,000 each, so nothing is sampled.
        let mut wrapped = spec.clone();
        wrapped.by = Some(FieldDef::of("g"));
        let a = analyse(cat, b, &wrapped).await;
        assert_eq!(a.sections.len(), 2);
        assert!(
            a.sections
                .iter()
                .all(|s| s.sampled.is_none() && s.n == 3000),
            "on {b}"
        );
    }
    Ok(())
}
