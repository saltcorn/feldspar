//! Plots rendered on both backends (analytics TODO A2.3–A2.6), against bins,
//! quartiles and summaries worked out by hand from ten houses:
//!
//! | id | price | area | hood | year | sold  |
//! |----|-------|------|------|------|-------|
//! | 1  | 100   | 50   | 1    | 1990 | true  |
//! | 2  | 200   | 60   | 1    | 1995 | false |
//! | 3  | 300   | 70   | 1    | 2000 | true  |
//! | 4  | 400   | 80   | 1    | 2005 | true  |
//! | 5  | 150   | 55   | 2    | 1990 | false |
//! | 6  | 250   | 65   | 2    | 1992 | true  |
//! | 7  | 350   | 75   | 2    | 2001 | false |
//! | 8  | 1000  | 90   | 2    | 2010 | true  |
//! | 9  | 120   | 40   | 3    | 1985 | —     |
//! | 10 | 220   | 45   | 3    | 1999 | true  |

use std::sync::Arc;

use sc_analytics::plot::{
    AggregateFn, Bin, Channel, DataRef, FieldDef, Fold, Layer, Mark, PlotData, PlotSpec, Rendered,
    Scale, ScaleKind, SmoothMethod, Stat, render_plot,
};
use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId, TableId};
use sc_dataset::{Base, DatasetDef, DatasetId, Op, Operation, save_dataset};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};

pub(crate) struct Fixture {
    pub(crate) cat: Catalog,
    pub(crate) backend: &'static str,
    pub(crate) houses: DatasetId,
    _db: Option<TestDb>,
}

fn column(name: &str, ty: BasicType) -> DataField {
    DataField::plain(name, TypeRef::Basic(ty))
}

pub(crate) async fn fill(cat: &Catalog) -> Result<DatasetId> {
    sc_dataset::bootstrap_datasets(cat).await?;
    let id = || column("id", BasicType::Int).required().primary_key();
    cat.create_table("neighbourhoods", &[id(), column("name", BasicType::Text)])
        .await?;
    let mut hood = column("neighbourhood", BasicType::Int);
    hood.kind = DataFieldKind::Key {
        target_table: TableId("neighbourhoods".into()),
        target_field: FieldId("id".into()),
        summary_field: None,
    };
    cat.create_table(
        "houses",
        &[
            id(),
            column("price", BasicType::Float),
            column("area", BasicType::Float),
            hood,
            column("year_built", BasicType::Int),
            column("sold", BasicType::Bool),
        ],
    )
    .await?;
    let insert = |table: &str, columns: &[&str], rows: Vec<Vec<Value>>| Insert {
        table: table.into(),
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        rows: rows
            .into_iter()
            .map(|r| r.into_iter().map(Expr::Lit).collect())
            .collect(),
        returning: Vec::new(),
    };
    let run = |i: Insert| async move {
        cat.primary()
            .query(&Statement::from(i))
            .await?
            .try_collect()
            .await
    };
    run(insert(
        "neighbourhoods",
        &["id", "name"],
        ["North", "South", "East"]
            .iter()
            .enumerate()
            .map(|(i, n)| vec![Value::Int(i as i64 + 1), Value::Text((*n).into())])
            .collect(),
    ))
    .await?;
    let rows: [(i64, f64, f64, i64, i64, Option<bool>); 10] = [
        (1, 100., 50., 1, 1990, Some(true)),
        (2, 200., 60., 1, 1995, Some(false)),
        (3, 300., 70., 1, 2000, Some(true)),
        (4, 400., 80., 1, 2005, Some(true)),
        (5, 150., 55., 2, 1990, Some(false)),
        (6, 250., 65., 2, 1992, Some(true)),
        (7, 350., 75., 2, 2001, Some(false)),
        (8, 1000., 90., 2, 2010, Some(true)),
        (9, 120., 40., 3, 1985, None),
        (10, 220., 45., 3, 1999, Some(true)),
    ];
    run(insert(
        "houses",
        &["id", "price", "area", "neighbourhood", "year_built", "sold"],
        rows.iter()
            .map(|(id, p, a, h, y, s)| {
                vec![
                    Value::Int(*id),
                    Value::Float(*p),
                    Value::Float(*a),
                    Value::Int(*h),
                    Value::Int(*y),
                    s.map_or(Value::Null, Value::Bool),
                ]
            })
            .collect(),
    ))
    .await?;
    let def = DatasetDef {
        id: DatasetId::new(),
        name: "Houses".into(),
        description: String::new(),
        base: Base::Table {
            table: "houses".into(),
        },
        operations: Vec::new(),
    };
    save_dataset(cat, &def).await?;
    Ok(def.id)
}

pub(crate) async fn both() -> Result<Vec<Fixture>> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let pg = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    let pg_houses = fill(&pg).await?;
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let lite = Catalog::init(driver).await?;
    let lite_houses = fill(&lite).await?;
    Ok(vec![
        Fixture {
            cat: pg,
            backend: "postgres",
            houses: pg_houses,
            _db: Some(db),
        },
        Fixture {
            cat: lite,
            backend: "sqlite",
            houses: lite_houses,
            _db: None,
        },
    ])
}

pub(crate) fn spec(fx: &Fixture, layer: Layer) -> PlotSpec {
    PlotSpec::single(DataRef::Dataset { dataset: fx.houses }, layer)
}

pub(crate) async fn draw(fx: &Fixture, spec: &PlotSpec) -> PlotData {
    match render_plot(&fx.cat, spec).await.expect("renders") {
        Rendered::Plot(data) => data,
        Rendered::Refused { problems, .. } => {
            panic!("on {}: refused: {problems:?}", fx.backend)
        }
    }
}

pub(crate) async fn refused(fx: &Fixture, spec: &PlotSpec) -> String {
    match render_plot(&fx.cat, spec).await.expect("answers") {
        Rendered::Refused { error, .. } => error,
        Rendered::Plot(_) => panic!("on {}: drew {spec:?}", fx.backend),
    }
}

/// Assert two tables of JSON are equal, numbers compared as numbers.
#[track_caller]
pub(crate) fn assert_rows(backend: &str, actual: &[Vec<Json>], expected: &[Vec<Json>]) {
    let same = actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(a, e)| {
            a.len() == e.len()
                && a.iter()
                    .zip(e)
                    .all(|(x, y)| match (x.as_f64(), y.as_f64()) {
                        (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * (1.0 + y.abs()),
                        _ => x == y,
                    })
        });
    assert!(
        same,
        "on {backend}:\n  got      {}\n  expected {}",
        serde_json::to_string(actual).unwrap_or_default(),
        serde_json::to_string(expected).unwrap_or_default()
    );
}

#[tokio::test]
async fn histograms_bin_by_a_width_or_by_freedman_diaconis() -> Result<()> {
    for fx in both().await? {
        let fixed = spec(
            &fx,
            Layer::new(Mark::Bar, Stat::Count).with(
                Channel::X,
                FieldDef {
                    field: "price".into(),
                    bin: Some(Bin {
                        width: Some(100.0),
                        bins: None,
                    }),
                },
            ),
        );
        let data = draw(&fx, &fixed).await;
        let layer = &data.layers[0];
        assert_eq!(layer.columns, vec!["x", "x_end", "y"]);
        assert_rows(
            fx.backend,
            &layer.rows,
            &[
                vec![json!(100), json!(200), json!(3)],
                vec![json!(200), json!(300), json!(3)],
                vec![json!(300), json!(400), json!(2)],
                vec![json!(400), json!(500), json!(1)],
                vec![json!(1000), json!(1100), json!(1)],
            ],
        );
        assert_eq!(layer.total, 10);
        assert_eq!(layer.stat, "count");

        // Freedman–Diaconis: the type-7 quartiles of price are 162.5 and
        // 337.5, so 2·175·10^(−1/3) = 162.4, rounded to 200, from 0.
        let fd = spec(
            &fx,
            Layer::new(Mark::Bar, Stat::Count).with(Channel::X, FieldDef::binned("price")),
        );
        let data = draw(&fx, &fd).await;
        assert_eq!(data.bins["price"].width, 200.0, "on {}", fx.backend);
        assert_eq!(data.bins["price"].origin, 0.0);
        assert_rows(
            fx.backend,
            &data.layers[0].rows,
            &[
                vec![json!(0), json!(200), json!(3)],
                vec![json!(200), json!(400), json!(5)],
                vec![json!(400), json!(600), json!(1)],
                vec![json!(1000), json!(1200), json!(1)],
            ],
        );
        // The domains span the bins' edges and the counts.
        assert_eq!(data.domains["x"].min, Some(json!(0.0)));
        assert_eq!(data.domains["x"].max, Some(json!(1200.0)));
        assert_eq!(data.domains["y"].kind, "continuous");
        assert_eq!(data.domains["y"].max, Some(json!(5)));
    }
    Ok(())
}

#[tokio::test]
async fn counts_and_aggregates_group_by_category() -> Result<()> {
    for fx in both().await? {
        let bars = spec(
            &fx,
            Layer::new(Mark::Bar, Stat::Count).with(Channel::X, FieldDef::of("neighbourhood")),
        );
        let data = draw(&fx, &bars).await;
        assert_rows(
            fx.backend,
            &data.layers[0].rows,
            &[
                vec![json!(1), json!(4)],
                vec![json!(2), json!(4)],
                vec![json!(3), json!(2)],
            ],
        );
        assert_eq!(
            data.domains["x"].values,
            Some(vec![json!(1), json!(2), json!(3)])
        );

        // The mean price by whether it sold, the missing group last; a
        // boolean is a boolean on SQLite too.
        let means = spec(
            &fx,
            Layer::new(Mark::Bar, Stat::aggregate(AggregateFn::Mean))
                .with(Channel::X, FieldDef::of("sold"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        let data = draw(&fx, &means).await;
        assert_rows(
            fx.backend,
            &data.layers[0].rows,
            &[
                vec![json!(false), json!(700.0 / 3.0)],
                vec![json!(true), json!(2270.0 / 6.0)],
                vec![Json::Null, json!(120.0)],
            ],
        );

        // Medians and quantiles come from the same percentile query.
        let medians = spec(
            &fx,
            Layer::new(Mark::Point, Stat::aggregate(AggregateFn::Median))
                .with(Channel::X, FieldDef::of("neighbourhood"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        let data = draw(&fx, &medians).await;
        assert_rows(
            fx.backend,
            &data.layers[0].rows,
            &[
                vec![json!(1), json!(250)],
                vec![json!(2), json!(300)],
                vec![json!(3), json!(170)],
            ],
        );
        let quartiles = spec(
            &fx,
            Layer::new(
                Mark::Point,
                Stat::Quantiles {
                    probabilities: vec![0.25, 0.75],
                },
            )
            .with(Channel::Y, FieldDef::of("price")),
        );
        let data = draw(&fx, &quartiles).await;
        assert_eq!(data.layers[0].columns, vec!["n", "y_p25", "y_p75"]);
        assert_rows(
            fx.backend,
            &data.layers[0].rows,
            &[vec![json!(10), json!(162.5), json!(337.5)]],
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_box_plot_has_quartiles_whiskers_and_outliers() -> Result<()> {
    for fx in both().await? {
        let boxes = spec(
            &fx,
            Layer::new(Mark::Box, Stat::boxplot())
                .with(Channel::X, FieldDef::of("neighbourhood"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        let data = draw(&fx, &boxes).await;
        let layer = &data.layers[0];
        assert_eq!(
            layer.columns,
            vec!["x", "n", "y_lower", "y_q1", "y_median", "y_q3", "y_upper"]
        );
        // South: quartiles 225, 300, 512.5, so the upper fence is
        // 512.5 + 1.5·287.5 = 943.75 and 1000 is beyond it: the whisker stops
        // at 350.
        assert_rows(
            fx.backend,
            &layer.rows,
            &[
                vec![
                    json!(1),
                    json!(4),
                    json!(100),
                    json!(175),
                    json!(250),
                    json!(325),
                    json!(400),
                ],
                vec![
                    json!(2),
                    json!(4),
                    json!(150),
                    json!(225),
                    json!(300),
                    json!(512.5),
                    json!(350),
                ],
                vec![
                    json!(3),
                    json!(2),
                    json!(120),
                    json!(145),
                    json!(170),
                    json!(195),
                    json!(220),
                ],
            ],
        );
        let outliers = layer.outliers.as_ref().expect("outliers");
        assert_eq!(outliers.columns, vec!["x", "y"]);
        assert_rows(fx.backend, &outliers.rows, &[vec![json!(2), json!(1000)]]);
        // The Y domain reaches the outlier.
        assert_eq!(data.domains["y"].max, Some(json!(1000.0)));
    }
    Ok(())
}

#[tokio::test]
async fn a_summary_has_a_t_interval() -> Result<()> {
    for fx in both().await? {
        let s = spec(
            &fx,
            Layer::new(Mark::Errorbar, Stat::summary())
                .with(Channel::X, FieldDef::of("neighbourhood"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        let data = draw(&fx, &s).await;
        let layer = &data.layers[0];
        assert_eq!(layer.columns, vec!["x", "n", "y", "y_lower", "y_upper"]);
        // North: mean 250, sd √(50000/3), t(0.975, 3) = 3.182446.
        let half = 3.182_446_305 * (50_000.0f64 / 3.0).sqrt() / 2.0;
        let north = &layer.rows[0];
        assert_eq!(north[1], json!(4));
        assert!((north[2].as_f64().unwrap() - 250.0).abs() < 1e-9);
        assert!(
            (north[3].as_f64().unwrap() - (250.0 - half)).abs() < 1e-5,
            "{north:?}"
        );
        assert!(
            (north[4].as_f64().unwrap() - (250.0 + half)).abs() < 1e-5,
            "{north:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn rows_are_sampled_above_the_limit_and_the_same_each_time() -> Result<()> {
    for fx in both().await? {
        let mut scatter = spec(
            &fx,
            Layer::new(Mark::Point, Stat::Identity)
                .with(Channel::X, FieldDef::of("area"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        let all = draw(&fx, &scatter).await;
        assert!(!all.layers[0].sampled);
        assert_eq!(all.layers[0].rows.len(), 10);
        scatter.layers[0].sample = Some(4);
        let first = draw(&fx, &scatter).await;
        let layer = &first.layers[0];
        assert!(layer.sampled, "on {}", fx.backend);
        assert_eq!((layer.rows.len(), layer.total), (4, 10));
        for row in &layer.rows {
            assert!(all.layers[0].rows.contains(row), "{row:?} is not a house");
        }
        let again = draw(&fx, &scatter).await;
        assert_eq!(again.layers[0].rows, layer.rows);
        // A line's rows are in order of X.
        let line = spec(
            &fx,
            Layer::new(Mark::Line, Stat::Identity)
                .with(Channel::X, FieldDef::of("area"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        let data = draw(&fx, &line).await;
        let xs: Vec<f64> = data.layers[0]
            .rows
            .iter()
            .map(|r| r[0].as_f64().unwrap())
            .collect();
        assert!(xs.windows(2).all(|w| w[0] <= w[1]), "{xs:?}");
    }
    Ok(())
}

#[tokio::test]
async fn facets_folds_and_log_scales() -> Result<()> {
    for fx in both().await? {
        // Wrap by decade: one plot per bin of year_built.
        let mut wrapped = spec(
            &fx,
            Layer::new(Mark::Bar, Stat::Count).with(Channel::X, FieldDef::of("neighbourhood")),
        );
        wrapped.facet.wrap = Some(FieldDef {
            field: "year_built".into(),
            bin: Some(Bin {
                width: Some(10.0),
                bins: None,
            }),
        });
        let data = draw(&fx, &wrapped).await;
        assert_eq!(
            data.facets["wrap"],
            vec![json!(1980.0), json!(1990.0), json!(2000.0), json!(2010.0)],
            "on {}",
            fx.backend
        );
        assert_eq!(data.layers[0].columns, vec!["x", "wrap", "wrap_end", "y"]);
        // 1990s: North 1990 and 1995, South 1990 and 1992, East 1999.
        let nineties: Vec<&Vec<Json>> = data.layers[0]
            .rows
            .iter()
            .filter(|r| r[1] == json!(1990.0))
            .collect();
        assert_rows(
            fx.backend,
            &nineties.into_iter().cloned().collect::<Vec<_>>(),
            &[
                vec![json!(1), json!(1990), json!(2000), json!(2)],
                vec![json!(2), json!(1990), json!(2000), json!(2)],
                vec![json!(3), json!(1990), json!(2000), json!(1)],
            ],
        );

        // Price and area compared as one variable: twenty rows, ten of each.
        let mut folded = spec(
            &fx,
            Layer::new(Mark::Bar, Stat::Count).with(Channel::X, FieldDef::of("variable")),
        );
        folded.fold = Some(Fold::of(["price", "area"]));
        let data = draw(&fx, &folded).await;
        assert_rows(
            fx.backend,
            &data.layers[0].rows,
            &[
                vec![json!("area"), json!(10)],
                vec![json!("price"), json!(10)],
            ],
        );

        // A log scale leaves out what it cannot show, and says so.
        let mut def = sc_dataset::load_dataset(&fx.cat, fx.houses).await?.unwrap();
        def.id = DatasetId::new();
        def.name = "Above 150".into();
        def.operations.push(Operation {
            id: "c".into(),
            enabled: true,
            op: Op::Calculated(sc_dataset::CalculatedOp {
                name: "over".into(),
                formula: "price - 150".into(),
            }),
        });
        save_dataset(&fx.cat, &def).await?;
        let mut logged = PlotSpec::single(
            DataRef::Dataset { dataset: def.id },
            Layer::new(Mark::Point, Stat::Identity)
                .with(Channel::X, FieldDef::of("area"))
                .with(Channel::Y, FieldDef::of("over")),
        );
        logged.scales.insert(
            Channel::Y,
            Scale {
                kind: ScaleKind::Log,
                ..Scale::default()
            },
        );
        let data = draw(&fx, &logged).await;
        assert_eq!(data.layers[0].rows.len(), 7, "on {}", fx.backend);
        assert_eq!(
            data.warnings,
            vec![
                "3 rows with `over` of 0 or less are left out: a log scale on Y cannot show them"
                    .to_owned()
            ]
        );
    }
    Ok(())
}

#[tokio::test]
async fn densities_and_smoothers_are_computed_on_the_server() -> Result<()> {
    for fx in both().await? {
        let density = spec(
            &fx,
            Layer::new(Mark::Area, Stat::density()).with(Channel::X, FieldDef::of("price")),
        );
        let data = draw(&fx, &density).await;
        let layer = &data.layers[0];
        assert_eq!(layer.columns, vec!["x", "y"]);
        assert_eq!(layer.rows.len(), 512);
        assert_eq!(layer.info["exact"], json!(true));
        let step = layer.rows[1][0].as_f64().unwrap() - layer.rows[0][0].as_f64().unwrap();
        let area: f64 = layer
            .rows
            .iter()
            .map(|r| r[1].as_f64().unwrap())
            .sum::<f64>()
            * step;
        assert!((area - 1.0).abs() < 0.01, "on {}: {area}", fx.backend);

        // A density per colour group: two curves.
        let mut by_sold = density.clone();
        by_sold.layers[0].encoding.color = Some(FieldDef::of("sold"));
        let data = draw(&fx, &by_sold).await;
        // Three groups, but the missing one has a single house: not drawn.
        assert_eq!(data.layers[0].rows.len(), 2 * 512, "on {}", fx.backend);
        assert_eq!(data.warnings.len(), 1);

        // The linear smoother agrees with least squares worked out here.
        let (xs, ys): (Vec<f64>, Vec<f64>) = (
            vec![50., 60., 70., 80., 55., 65., 75., 90., 40., 45.],
            vec![100., 200., 300., 400., 150., 250., 350., 1000., 120., 220.],
        );
        let mx = xs.iter().sum::<f64>() / 10.0;
        let my = ys.iter().sum::<f64>() / 10.0;
        let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
        let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
        let slope = sxy / sxx;
        let line = spec(
            &fx,
            Layer::new(Mark::Line, Stat::smooth(SmoothMethod::Linear))
                .with(Channel::X, FieldDef::of("area"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        let data = draw(&fx, &line).await;
        let layer = &data.layers[0];
        assert_eq!(layer.columns, vec!["x", "y", "y_lower", "y_upper"]);
        assert_eq!(layer.rows.len(), 80);
        let first = &layer.rows[0];
        assert_eq!(first[0], json!(40.0));
        let fitted = my + slope * (40.0 - mx);
        assert!(
            (first[1].as_f64().unwrap() - fitted).abs() < 1e-6,
            "on {}: {first:?}",
            fx.backend
        );
        assert!(first[2].as_f64().unwrap() < fitted && fitted < first[3].as_f64().unwrap());

        let loess = spec(
            &fx,
            Layer::new(Mark::Line, Stat::smooth(SmoothMethod::Loess))
                .with(Channel::X, FieldDef::of("area"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        let data = draw(&fx, &loess).await;
        assert_eq!(data.layers[0].rows.len(), 80);
        assert!(!data.layers[0].sampled);
    }
    Ok(())
}

#[tokio::test]
async fn what_cannot_be_drawn_is_said_in_a_sentence() -> Result<()> {
    for fx in both().await? {
        let missing = spec(
            &fx,
            Layer::new(Mark::Point, Stat::Identity).with(Channel::X, FieldDef::of("colour")),
        );
        assert!(
            refused(&fx, &missing)
                .await
                .starts_with("X: `colour` is not a column of the dataset"),
        );
        let gone = PlotSpec::single(
            DataRef::Dataset {
                dataset: DatasetId::new(),
            },
            Layer::new(Mark::Point, Stat::Identity).with(Channel::X, FieldDef::of("price")),
        );
        assert_eq!(
            refused(&fx, &gone).await,
            "the dataset this plot reads is gone; pick another"
        );
        // A dataset whose last operation has an error.
        let broken = DatasetDef {
            id: DatasetId::new(),
            name: "Broken".into(),
            description: String::new(),
            base: Base::Table {
                table: "houses".into(),
            },
            operations: vec![Operation {
                id: "f".into(),
                enabled: true,
                op: Op::Filter(sc_dataset::FilterOp {
                    formula: "nope > 1".into(),
                }),
            }],
        };
        save_dataset(&fx.cat, &broken).await?;
        let s = PlotSpec::single(
            DataRef::Dataset { dataset: broken.id },
            Layer::new(Mark::Point, Stat::Identity).with(Channel::X, FieldDef::of("price")),
        );
        let sentence = refused(&fx, &s).await;
        assert!(
            sentence.starts_with("the dataset `Broken` does not read:"),
            "{sentence}"
        );
    }
    Ok(())
}
