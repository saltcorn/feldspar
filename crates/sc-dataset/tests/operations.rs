//! The operations, compiled and run on both backends against rows worked out
//! by hand (analytics TODO A1.2–A1.7). See `fixture.rs` for the tables.

use sc_dataset::{
    AggregateOp, Base, ColType, CompleteColumn, CompleteOp, CompleteValues, DatasetDef, FillValue,
    Grain, GroupKey, JoinKey, JoinKind, JoinOp, Library, LimitMode, LimitOp, Op, OpStatus, Options,
    OrderKey, Other, Page, Schema, SelectColumn, SelectOp, SortKey, SortOp, SplitOp, SplitSummary,
    StackOp, Summary, SummaryFunction, UnionOp, WindowFunction, WindowOp, column_values, compile,
    read_page, save_dataset,
};
use sc_error::Result;
use serde_json::{Value as Json, json};

use crate::fixture::{Fixture, assert_rows, both, json_rows, names, read};

fn select(columns: &[&str]) -> Op {
    Op::Select(SelectOp {
        columns: columns.iter().map(|c| SelectColumn::keep(*c)).collect(),
    })
}

fn sort(formula: &str, descending: bool) -> Op {
    Op::Sort(SortOp {
        keys: vec![SortKey {
            formula: formula.into(),
            descending,
        }],
    })
}

fn window(name: &str, function: WindowFunction, column: Option<&str>) -> WindowOp {
    WindowOp {
        name: name.into(),
        function,
        column: column.map(str::to_owned),
        offset: None,
        partition: Vec::new(),
        order: Vec::new(),
    }
}

fn by_hood(mut w: WindowOp) -> Op {
    w.partition = vec!["neighbourhood".into()];
    w.order = vec![OrderKey::asc("year_built")];
    Op::Window(w)
}

fn aggregate(keys: &[&str], summaries: Vec<Summary>) -> Op {
    Op::Aggregate(AggregateOp {
        group_by: keys.iter().map(|k| GroupKey::column(*k)).collect(),
        summaries,
    })
}

async fn rows_of(fx: &Fixture, def: &DatasetDef) -> Result<Vec<Vec<Json>>> {
    Ok(json_rows(&read(fx, def, None).await?))
}

// --- A1.2: stage shapes and grain ------------------------------------------

#[tokio::test]
async fn a_key_stays_a_key_and_grain_decides_what_formulas_may_follow() -> Result<()> {
    for fx in both().await? {
        let schema = Schema::of_catalog(&fx.cat)?;
        // After an Aggregate by the `neighbourhood` key, each row is a
        // neighbourhood: Ⱶ follows the key, and Ↄ aggregates over its houses.
        let def = DatasetDef::over_table("by hood", "houses")
            .then(aggregate(&["neighbourhood"], vec![Summary::count("n")]))
            .then(Op::calculated("name", "neighbourhoodⱵname"))
            .then(Op::calculated("houses", "housesↃneighbourhood.length"));
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        assert!(compiled.is_valid(), "{:?}", compiled.operations);
        let shape = compiled.operations[0].shape.clone().expect("a shape");
        assert_eq!(
            shape.grain,
            Grain::Group {
                keys: vec!["neighbourhood".into()]
            }
        );
        let key = shape.column("neighbourhood").and_then(|c| c.key.clone());
        assert_eq!(key.map(|k| k.table), Some("neighbourhoods".to_owned()));
        assert_eq!(shape.column("n").map(|c| c.ty), Some(ColType::Int));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![json!(1), json!(3), json!("North"), json!(3)],
                vec![json!(2), json!(2), json!("South"), json!(2)],
            ],
        );

        // After an Aggregate by a year, a row is no table's row, and the
        // refusal says what a row is.
        let def = DatasetDef::over_table("by year", "houses")
            .then(aggregate(&["year_built"], vec![Summary::count("n")]))
            .then(Op::calculated("houses", "housesↃneighbourhood.length"));
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        let (i, report) = compiled.first_error().expect("refused");
        assert_eq!(i, 1);
        let error = report.error.clone().unwrap_or_default();
        assert!(
            error.contains("each row is one combination of `year_built`"),
            "{error}"
        );

        // The base's grain is the table's.
        assert_eq!(
            compiled.base.shape.expect("base").grain,
            Grain::Table {
                table: "houses".into(),
                key: "id".into()
            }
        );
    }
    Ok(())
}

// --- A1.3: the operations that keep the grain -------------------------------

#[tokio::test]
async fn calculated_filter_sort_and_select_give_the_rows_worked_out_by_hand() -> Result<()> {
    for fx in both().await? {
        let def = DatasetDef::over_table("d", "houses")
            .then(Op::calculated("ppm", "price / area"))
            .then(Op::calculated("hood", "neighbourhoodⱵname"))
            .then(Op::filter("price > 100000"))
            .then(sort("ppm", true))
            .then(Op::Select(SelectOp {
                columns: vec![
                    SelectColumn::keep("id"),
                    SelectColumn::renamed("hood", "where"),
                    SelectColumn::keep("ppm"),
                ],
            }));
        let page = read(&fx, &def, None).await?;
        assert_eq!(names(&page), ["id", "where", "ppm"]);
        assert_eq!(page.total, 4);
        // 2 and 5 tie on 3000 and keep their order (by id).
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!(2), json!("North"), json!(3000.0)],
                vec![json!(5), json!("North"), json!(3000.0)],
                vec![json!(4), json!("South"), json!(2500.0)],
                vec![json!(1), json!("North"), json!(2000.0)],
            ],
        );
        assert_eq!(page.columns[1].ty, ColType::Text);
        assert_eq!(page.columns[2].ty, ColType::Float);

        // The stage after the first operation: every row, one more column.
        let first = read(&fx, &def, Some(1)).await?;
        assert_eq!(first.total, 5);
        assert_eq!(
            names(&first),
            [
                "id",
                "price",
                "area",
                "neighbourhood",
                "year_built",
                "sold",
                "ppm"
            ]
        );
    }
    Ok(())
}

#[tokio::test]
async fn window_columns_follow_their_groups_and_order() -> Result<()> {
    for fx in both().await? {
        let mut rn = window("rn", WindowFunction::RowNumber, None);
        rn.order = vec![OrderKey::desc("price")];
        let mut share = window("share", WindowFunction::Share, Some("price"));
        share.partition = vec!["neighbourhood".into()];
        let def = DatasetDef::over_table("w", "houses")
            .then(by_hood(window("prev", WindowFunction::Lag, Some("price"))))
            .then(by_hood(window(
                "diff",
                WindowFunction::Difference,
                Some("price"),
            )))
            .then(by_hood(window(
                "cum",
                WindowFunction::CumulativeSum,
                Some("price"),
            )))
            .then(by_hood(window("rk", WindowFunction::Rank, None)))
            .then(Op::Window(rn))
            .then(Op::Window(share))
            .then(select(&["id", "prev", "diff", "cum", "rk", "rn", "share"]));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![
                    json!(1),
                    json!(null),
                    json!(null),
                    json!(200000.0),
                    json!(1),
                    json!(2),
                    json!(200000.0 / 470000.0),
                ],
                vec![
                    json!(2),
                    json!(120000.0),
                    json!(30000.0),
                    json!(470000.0),
                    json!(3),
                    json!(3),
                    json!(150000.0 / 470000.0),
                ],
                vec![
                    json!(3),
                    json!(null),
                    json!(null),
                    json!(90000.0),
                    json!(1),
                    json!(5),
                    json!(90000.0 / 390000.0),
                ],
                vec![
                    json!(4),
                    json!(90000.0),
                    json!(210000.0),
                    json!(390000.0),
                    json!(2),
                    json!(1),
                    json!(300000.0 / 390000.0),
                ],
                vec![
                    json!(5),
                    json!(200000.0),
                    json!(-80000.0),
                    json!(320000.0),
                    json!(1),
                    json!(4),
                    json!(120000.0 / 470000.0),
                ],
            ],
        );

        // The last value that was not missing, the running mean, and the
        // group summaries; a filter on a window column reads it as a subquery.
        let mut fill = window("sold_so_far", WindowFunction::Fill, Some("sold"));
        fill.order = vec![OrderKey::asc("id")];
        let mut mean = window("mean", WindowFunction::GroupMean, Some("price"));
        mean.partition = vec!["neighbourhood".into()];
        let mut count = window("known", WindowFunction::GroupCount, Some("sold"));
        count.partition = vec!["neighbourhood".into()];
        let mut max = window("top", WindowFunction::GroupMax, Some("price"));
        max.partition = vec!["neighbourhood".into()];
        let mut running = window("running", WindowFunction::CumulativeMean, Some("area"));
        running.order = vec![OrderKey::asc("id")];
        let def = DatasetDef::over_table("w2", "houses")
            .then(Op::Window(fill))
            .then(Op::Window(mean))
            .then(Op::Window(count))
            .then(Op::Window(max))
            .then(Op::Window(running))
            .then(Op::filter("top > 250000"))
            .then(select(&[
                "id",
                "sold_so_far",
                "mean",
                "known",
                "top",
                "running",
            ]));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![
                    json!(3),
                    json!(true),
                    json!(195000.0),
                    json!(2),
                    json!(300000.0),
                    json!(70.0),
                ],
                vec![
                    json!(4),
                    json!(true),
                    json!(195000.0),
                    json!(2),
                    json!(300000.0),
                    json!(82.5),
                ],
            ],
        );
        let def = DatasetDef::over_table("w3", "houses")
            .then(Op::Window({
                let mut f = window("sold_so_far", WindowFunction::Fill, Some("sold"));
                f.order = vec![OrderKey::asc("id")];
                f
            }))
            .then(select(&["id", "sold", "sold_so_far"]));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![json!(1), json!(true), json!(true)],
                vec![json!(2), json!(false), json!(false)],
                vec![json!(3), json!(true), json!(true)],
                vec![json!(4), json!(true), json!(true)],
                vec![json!(5), json!(null), json!(true)],
            ],
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_formula_that_the_database_cannot_compute_is_refused_by_name() -> Result<()> {
    for fx in both().await? {
        let schema = Schema::of_catalog(&fx.cat)?;
        let def = DatasetDef::over_table("d", "houses")
            .then(Op::calculated("kind", "typeof price"))
            .then(Op::calculated("me", "user.id"))
            .then(Op::filter("no_such_column > 1"));
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        let error = compiled.operations[0].error.clone().unwrap_or_default();
        assert!(
            error.contains("cannot be computed by the database"),
            "{error}"
        );
        assert_eq!(compiled.operations[1].status, OpStatus::NotReached);
    }
    Ok(())
}

// --- A1.4: the operations that change the grain -----------------------------

#[tokio::test]
async fn an_aggregate_computes_every_summary() -> Result<()> {
    for fx in both().await? {
        let def = DatasetDef::over_table("agg", "houses").then(aggregate(
            &["neighbourhood"],
            vec![
                Summary::count("n"),
                Summary::of("mean", SummaryFunction::Mean, "price"),
                Summary::of("median", SummaryFunction::Median, "price"),
                Summary::of("oldest", SummaryFunction::Min, "year_built"),
                Summary::of("dearest", SummaryFunction::Max, "price"),
                Summary::of("sd", SummaryFunction::Sd, "price"),
                Summary {
                    name: "first".into(),
                    function: SummaryFunction::First,
                    column: Some("price".into()),
                    order: Some(OrderKey::asc("year_built")),
                },
                Summary {
                    name: "last".into(),
                    function: SummaryFunction::Last,
                    column: Some("price".into()),
                    order: Some(OrderKey::asc("year_built")),
                },
                Summary::of("years", SummaryFunction::CountDistinct, "year_built"),
                Summary::of("area", SummaryFunction::Sum, "area"),
                Summary::of("known", SummaryFunction::Count, "sold"),
            ],
        ));
        let page = read(&fx, &def, None).await?;
        assert_eq!(
            names(&page),
            [
                "neighbourhood",
                "n",
                "mean",
                "median",
                "oldest",
                "dearest",
                "sd",
                "first",
                "last",
                "years",
                "area",
                "known"
            ]
        );
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![
                    json!(1),
                    json!(3),
                    json!(470000.0 / 3.0),
                    json!(150000.0),
                    json!(1990),
                    json!(200000.0),
                    json!(40_414.518_843_273_8),
                    json!(200000.0),
                    json!(150000.0),
                    json!(2),
                    json!(190.0),
                    json!(2),
                ],
                vec![
                    json!(2),
                    json!(2),
                    json!(195000.0),
                    json!(195000.0),
                    json!(1985),
                    json!(300000.0),
                    json!(148_492.424_049_175),
                    json!(90000.0),
                    json!(300000.0),
                    json!(2),
                    json!(180.0),
                    json!(2),
                ],
            ],
        );

        // With no summaries it is `distinct`; with no keys, one row.
        let distinct = DatasetDef::over_table("distinct", "houses")
            .then(aggregate(&["neighbourhood", "year_built"], Vec::new()));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &distinct).await?,
            &[
                vec![json!(1), json!(1990)],
                vec![json!(1), json!(2000)],
                vec![json!(2), json!(1985)],
                vec![json!(2), json!(2010)],
            ],
        );
        let all = DatasetDef::over_table("all", "houses").then(aggregate(
            &[],
            vec![Summary::of("median", SummaryFunction::Median, "area")],
        ));
        assert_rows(fx.backend, &rows_of(&fx, &all).await?, &[vec![json!(60.0)]]);

        // A mean of text is refused before any SQL is written.
        let schema = Schema::of_catalog(&fx.cat)?;
        let bad = DatasetDef::over_table("bad", "neighbourhoods").then(aggregate(
            &[],
            vec![Summary::of("m", SummaryFunction::Mean, "name")],
        ));
        let compiled = compile(&schema, &Library::default(), &bad, Options::default());
        let error = compiled.operations[0].error.clone().unwrap_or_default();
        assert!(error.contains("`name` is text"), "{error}");
    }
    Ok(())
}

#[tokio::test]
async fn a_group_key_with_a_literal_in_it_groups() -> Result<()> {
    // `price > 100000` would be written once in the select list and once in
    // the GROUP BY, each with a placeholder of its own, which Postgres does
    // not take for one expression; the key is computed a level down instead.
    for fx in both().await? {
        let def = DatasetDef::over_table("dear", "houses").then(Op::Aggregate(AggregateOp {
            group_by: vec![GroupKey {
                name: "dear".into(),
                formula: "price > 100000".into(),
            }],
            summaries: vec![Summary::count("n")],
        }));
        let rows = rows_of(&fx, &def).await?;
        let backend = fx.backend;
        assert_rows(
            backend,
            &rows,
            &[vec![json!(false), json!(1)], vec![json!(true), json!(4)]],
        );
    }
    Ok(())
}

#[tokio::test]
async fn limits_take_the_first_a_sample_or_the_top_of_each_group() -> Result<()> {
    let mut samples = Vec::new();
    for fx in both().await? {
        let first = DatasetDef::over_table("first", "houses")
            .then(sort("price", true))
            .then(Op::Limit(LimitOp {
                mode: LimitMode::First,
                n: 2,
                seed: 0,
                group_by: Vec::new(),
                order: Vec::new(),
            }))
            .then(select(&["id"]));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &first).await?,
            &[vec![json!(4)], vec![json!(1)]],
        );

        let top = DatasetDef::over_table("top", "houses")
            .then(Op::Limit(LimitOp {
                mode: LimitMode::Top,
                n: 1,
                seed: 0,
                group_by: vec!["neighbourhood".into()],
                order: vec![OrderKey::desc("price")],
            }))
            .then(select(&["id"]));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &top).await?,
            &[vec![json!(1)], vec![json!(4)]],
        );

        let sample = DatasetDef::over_table("sample", "houses")
            .then(Op::Limit(LimitOp {
                mode: LimitMode::Sample,
                n: 3,
                seed: 42,
                group_by: Vec::new(),
                order: Vec::new(),
            }))
            .then(select(&["id"]));
        let once = rows_of(&fx, &sample).await?;
        assert_eq!(once.len(), 3);
        assert_eq!(
            once,
            rows_of(&fx, &sample).await?,
            "the same seed, the same sample"
        );
        samples.push(once);
    }
    // The sample is arithmetic, not a backend's `random()`: the same on both.
    assert_eq!(samples[0], samples[1]);
    Ok(())
}

#[tokio::test]
async fn stack_and_split_turn_columns_into_rows_and_back() -> Result<()> {
    for fx in both().await? {
        let stacked = DatasetDef::over_table("stacked", "houses")
            .then(Op::filter("id <= 2"))
            .then(select(&["id", "price", "area"]))
            .then(Op::Stack(StackOp {
                columns: vec!["price".into(), "area".into()],
                names_to: "measure".into(),
                values_to: "value".into(),
            }));
        let page = read(&fx, &stacked, None).await?;
        assert_eq!(names(&page), ["id", "measure", "value"]);
        assert_eq!(page.grain, Grain::Derived);
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!(1), json!("price"), json!(200000.0)],
                vec![json!(1), json!("area"), json!(100.0)],
                vec![json!(2), json!("price"), json!(150000.0)],
                vec![json!(2), json!("area"), json!(50.0)],
            ],
        );

        // Stacking a number with text is refused.
        let schema = Schema::of_catalog(&fx.cat)?;
        let bad = DatasetDef::over_table("bad", "neighbourhoods").then(Op::Stack(StackOp {
            columns: vec!["id".into(), "name".into()],
            names_to: "k".into(),
            values_to: "v".into(),
        }));
        let compiled = compile(&schema, &Library::default(), &bad, Options::default());
        assert!(
            compiled.operations[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("must be of one kind"),
            "{:?}",
            compiled.operations[0]
        );

        // The split's columns are read from the data once, then fixed.
        let base = DatasetDef::over_table("sales", "sales");
        let compiled = compile(&schema, &Library::default(), &base, Options::default());
        let values = column_values(&fx.cat, compiled.last().expect("reads"), "quarter", 10).await?;
        let values: Vec<String> = values
            .iter()
            .filter_map(|v| v.as_text().map(str::to_owned))
            .collect();
        assert_eq!(values, ["q1", "q2"], "most frequent first");
        let split = base.then(Op::Split(SplitOp {
            names_from: "quarter".into(),
            values_from: "amount".into(),
            id_columns: vec!["region".into()],
            values,
            summary: SplitSummary::Sum,
        }));
        let page = read(&fx, &split, None).await?;
        assert_eq!(names(&page), ["region", "q1", "q2"]);
        assert_eq!(
            page.grain,
            Grain::Group {
                keys: vec!["region".into()]
            }
        );
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!("north"), json!(11), json!(20)],
                vec![json!("south"), json!(5), json!(null)],
            ],
        );
    }
    Ok(())
}

#[tokio::test]
async fn complete_adds_the_missing_combinations() -> Result<()> {
    for fx in both().await? {
        // From the data.
        let def = DatasetDef::over_table("by quarter", "sales")
            .then(aggregate(
                &["region", "quarter"],
                vec![Summary::of("total", SummaryFunction::Sum, "amount")],
            ))
            .then(Op::Complete(CompleteOp {
                columns: vec![
                    CompleteColumn {
                        column: "region".into(),
                        values: CompleteValues::Data,
                    },
                    CompleteColumn {
                        column: "quarter".into(),
                        values: CompleteValues::Data,
                    },
                ],
                fill: vec![FillValue {
                    column: "total".into(),
                    value: json!(0),
                }],
            }));
        let page = read(&fx, &def, None).await?;
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!("north"), json!("q1"), json!(11)],
                vec![json!("north"), json!("q2"), json!(20)],
                vec![json!("south"), json!("q1"), json!(5)],
                vec![json!("south"), json!("q2"), json!(0)],
            ],
        );
        assert!(matches!(page.grain, Grain::Group { .. }));

        // From the table a key refers to: East has no houses, and appears.
        let def = DatasetDef::over_table("per hood", "houses")
            .then(aggregate(&["neighbourhood"], vec![Summary::count("n")]))
            .then(Op::Complete(CompleteOp {
                columns: vec![CompleteColumn {
                    column: "neighbourhood".into(),
                    values: CompleteValues::Table,
                }],
                fill: vec![FillValue {
                    column: "n".into(),
                    value: json!(0),
                }],
            }));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![json!(1), json!(3)],
                vec![json!(2), json!(2)],
                vec![json!(3), json!(0)],
            ],
        );

        // From a number range, and from a date range.
        let def = DatasetDef::over_table("per year", "houses")
            .then(aggregate(&["year_built"], vec![Summary::count("n")]))
            .then(Op::Complete(CompleteOp {
                columns: vec![CompleteColumn {
                    column: "year_built".into(),
                    values: CompleteValues::Range {
                        from: json!(1985),
                        to: json!(2010),
                        step: Some(json!(5)),
                    },
                }],
                fill: vec![FillValue {
                    column: "n".into(),
                    value: json!(0),
                }],
            }));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![json!(1985), json!(1)],
                vec![json!(1990), json!(2)],
                vec![json!(1995), json!(0)],
                vec![json!(2000), json!(1)],
                vec![json!(2005), json!(0)],
                vec![json!(2010), json!(1)],
            ],
        );
        let def = DatasetDef::over_table("per day", "viewings")
            .then(aggregate(&["viewed_on"], vec![Summary::count("n")]))
            .then(Op::Complete(CompleteOp {
                columns: vec![CompleteColumn {
                    column: "viewed_on".into(),
                    values: CompleteValues::Range {
                        from: json!("2024-01-05"),
                        to: json!("2024-01-07"),
                        step: Some(json!("day")),
                    },
                }],
                fill: vec![FillValue {
                    column: "n".into(),
                    value: json!(0),
                }],
            }));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![json!("2024-01-05"), json!(1)],
                vec![json!("2024-01-06"), json!(0)],
                vec![json!("2024-01-07"), json!(0)],
                vec![json!("2024-01-20"), json!(1)],
                vec![json!("2024-02-10"), json!(1)],
                vec![json!("2024-03-01"), json!(1)],
            ],
        );
    }
    Ok(())
}

// --- A1.5: the operations that combine ---------------------------------------

#[tokio::test]
async fn joins_inner_left_full_and_nearest_earlier() -> Result<()> {
    for fx in both().await? {
        // Inner, on the other's primary key: the grain is kept.
        let inner = DatasetDef::over_table("inner", "viewings").then(Op::Join(JoinOp {
            with: Other::Table {
                table: "houses".into(),
            },
            kind: JoinKind::Inner,
            on: vec![JoinKey::new("house", "id")],
            asof: None,
            columns: Some(vec!["price".into()]),
            suffix: "_right".into(),
        }));
        let page = read(&fx, &inner, None).await?;
        assert_eq!(
            names(&page),
            ["id", "house", "viewed_on", "attended", "price"]
        );
        assert!(matches!(page.grain, Grain::Table { .. }));
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![
                    json!(1),
                    json!(1),
                    json!("2024-01-05"),
                    json!(true),
                    json!(200000.0),
                ],
                vec![
                    json!(2),
                    json!(1),
                    json!("2024-02-10"),
                    json!(false),
                    json!(200000.0),
                ],
                vec![
                    json!(3),
                    json!(3),
                    json!("2024-01-20"),
                    json!(true),
                    json!(90000.0),
                ],
                vec![
                    json!(4),
                    json!(4),
                    json!("2024-03-01"),
                    json!(true),
                    json!(300000.0),
                ],
            ],
        );

        // Left, on a dataset's group key: still one row per house, and still
        // rows of `houses` (Ↄ works after it).
        let counts = DatasetDef::over_table("viewing counts", "viewings")
            .then(aggregate(&["house"], vec![Summary::count("viewings")]));
        save_dataset(&fx.cat, &counts).await?;
        let left = DatasetDef::over_table("left", "houses")
            .then(Op::Join(JoinOp {
                with: Other::Dataset { dataset: counts.id },
                kind: JoinKind::Left,
                on: vec![JoinKey::new("id", "house")],
                asof: None,
                columns: None,
                suffix: "_right".into(),
            }))
            .then(Op::calculated("again", "viewingsↃhouse.length"))
            .then(select(&["id", "viewings", "again"]));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &left).await?,
            &[
                vec![json!(1), json!(2), json!(2)],
                vec![json!(2), json!(null), json!(0)],
                vec![json!(3), json!(1), json!(1)],
                vec![json!(4), json!(1), json!(1)],
                vec![json!(5), json!(null), json!(0)],
            ],
        );

        // Full: the keys are merged, and unmatched rows of both come through.
        let per_hood = DatasetDef::over_table("per hood", "houses")
            .then(aggregate(&["neighbourhood"], vec![Summary::count("n")]));
        save_dataset(&fx.cat, &per_hood).await?;
        let full = DatasetDef::over_table("full", "neighbourhoods")
            .then(Op::filter("id >= 2"))
            .then(Op::Join(JoinOp {
                with: Other::Dataset {
                    dataset: per_hood.id,
                },
                kind: JoinKind::Full,
                on: vec![JoinKey::new("id", "neighbourhood")],
                asof: None,
                columns: None,
                suffix: "_right".into(),
            }));
        let page = read(&fx, &full, None).await?;
        assert_eq!(page.grain, Grain::Derived);
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!(1), json!(null), json!(3)],
                vec![json!(2), json!("South"), json!(2)],
                vec![json!(3), json!("East"), json!(null)],
            ],
        );

        // As of: each viewing takes the rate in force on its day.
        let asof = DatasetDef::over_table("asof", "viewings")
            .then(Op::Join(JoinOp {
                with: Other::Table {
                    table: "rates".into(),
                },
                kind: JoinKind::Left,
                on: Vec::new(),
                asof: Some(JoinKey::new("viewed_on", "valid_from")),
                columns: Some(vec!["rate".into()]),
                suffix: "_right".into(),
            }))
            .then(select(&["id", "viewed_on", "rate"]));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &asof).await?,
            &[
                vec![json!(1), json!("2024-01-05"), json!(0.03)],
                vec![json!(2), json!("2024-02-10"), json!(0.04)],
                vec![json!(3), json!("2024-01-20"), json!(0.03)],
                vec![json!(4), json!("2024-03-01"), json!(0.05)],
            ],
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_union_matches_columns_by_name_and_says_where_rows_came_from() -> Result<()> {
    for fx in both().await? {
        let hoods = DatasetDef::over_table("hoods", "neighbourhoods");
        save_dataset(&fx.cat, &hoods).await?;
        let def = DatasetDef::over_table("u", "houses")
            .then(Op::filter("id <= 2"))
            .then(select(&["id", "price"]))
            .then(Op::Union(UnionOp {
                with: Other::Dataset { dataset: hoods.id },
                source_column: Some("src".into()),
                source_labels: vec!["houses".into(), "hoods".into()],
            }));
        let page = read(&fx, &def, None).await?;
        assert_eq!(names(&page), ["id", "price", "name", "src"]);
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!(1), json!(200000.0), json!(null), json!("houses")],
                vec![json!(2), json!(150000.0), json!(null), json!("houses")],
                vec![json!(1), json!(null), json!("North"), json!("hoods")],
                vec![json!(2), json!(null), json!("South"), json!("hoods")],
                vec![json!(3), json!(null), json!("East"), json!("hoods")],
            ],
        );

        // A number and text under one name are refused.
        let schema = Schema::of_catalog(&fx.cat)?;
        let bad = DatasetDef::over_table("bad", "houses")
            .then(Op::Select(SelectOp {
                columns: vec![SelectColumn::renamed("price", "name")],
            }))
            .then(Op::Union(UnionOp {
                with: Other::Table {
                    table: "neighbourhoods".into(),
                },
                source_column: None,
                source_labels: Vec::new(),
            }));
        let compiled = compile(&schema, &Library::default(), &bad, Options::default());
        let error = compiled.operations[1].error.clone().unwrap_or_default();
        assert!(error.contains("`name` is number here and text"), "{error}");
    }
    Ok(())
}

// --- A1.6: datasets over datasets, and invalid operations -------------------

#[tokio::test]
async fn a_dataset_over_a_dataset_reads_its_operations_first_and_a_cycle_is_refused() -> Result<()>
{
    for fx in both().await? {
        let a = DatasetDef::over_table("a", "houses").then(Op::filter("price > 100000"));
        save_dataset(&fx.cat, &a).await?;
        let b = DatasetDef::new("b", Base::dataset(a.id))
            .then(Op::calculated("ppm", "price / area"))
            .then(select(&["id", "ppm"]));
        save_dataset(&fx.cat, &b).await?;
        assert_rows(
            fx.backend,
            &rows_of(&fx, &b).await?,
            &[
                vec![json!(1), json!(2000.0)],
                vec![json!(2), json!(3000.0)],
                vec![json!(4), json!(2500.0)],
                vec![json!(5), json!(3000.0)],
            ],
        );

        // `a` joining `b`, which is based on `a`: the join is marked, and the
        // stage before it still reads.
        let looped = a.clone().then(Op::Join(JoinOp {
            with: Other::Dataset { dataset: b.id },
            kind: JoinKind::Left,
            on: vec![JoinKey::new("id", "id")],
            asof: None,
            columns: None,
            suffix: "_b".into(),
        }));
        let schema = Schema::of_catalog(&fx.cat)?;
        let mut library = sc_dataset::load_library(&fx.cat).await?;
        library.insert(looped.clone());
        let compiled = compile(&schema, &library, &looped, Options::default());
        let (i, report) = compiled.first_error().expect("a cycle");
        assert_eq!(i, 1);
        assert!(
            report
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("leads back to itself"),
            "{report:?}"
        );
        assert!(compiled.stage(1).is_ok());
        assert!(compiled.stage(2).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn disabled_operations_are_skipped_and_an_invalid_one_stops_by_id() -> Result<()> {
    for fx in both().await? {
        let mut def = DatasetDef::over_table("d", "houses")
            .then(Op::calculated("ppm", "price / area"))
            .then(Op::filter("price > 100000"))
            .then(aggregate(
                &["neighbourhood"],
                vec![
                    Summary::of("mean_ppm", SummaryFunction::Mean, "ppm"),
                    Summary::count("n"),
                ],
            ));
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![json!(1), json!(8000.0 / 3.0), json!(3)],
                vec![json!(2), json!(2500.0), json!(1)],
            ],
        );
        // Disable the filter: the counts change.
        def.operations[1].enabled = false;
        assert_rows(
            fx.backend,
            &rows_of(&fx, &def).await?,
            &[
                vec![json!(1), json!(8000.0 / 3.0), json!(3)],
                vec![json!(2), json!(2000.0), json!(2)],
            ],
        );
        // Rename the column the aggregate reads: the aggregate is marked,
        // naming the column, and the stages before it still read.
        def.operations[0].op = Op::calculated("price_per_m2", "price / area");
        let schema = Schema::of_catalog(&fx.cat)?;
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        let (i, report) = compiled.first_error().expect("broken");
        assert_eq!((i, report.id.as_str()), (2, "op3"));
        let error = report.error.clone().unwrap_or_default();
        assert!(
            error.contains("reads `ppm`, and that is not a column")
                && error.contains("`price_per_m2`"),
            "{error}"
        );
        assert_eq!(compiled.operations[1].status, OpStatus::Disabled);
        assert!(compiled.stage(2).is_ok());
        assert!(read(&fx, &def, Some(3)).await.is_err());
        assert_eq!(read(&fx, &def, Some(1)).await?.total, 5);
    }
    Ok(())
}

// --- A1.7: reading a stage ---------------------------------------------------

#[tokio::test]
async fn paging_is_stable_under_the_order_and_the_count_matches() -> Result<()> {
    for fx in both().await? {
        // Sorted by a column with ties: the row key breaks them.
        let def = DatasetDef::over_table("d", "houses")
            .then(sort("year_built", false))
            .then(select(&["id", "year_built"]));
        let schema = Schema::of_catalog(&fx.cat)?;
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        let stage = compiled.last().expect("reads");
        let whole = read_page(&fx.cat, stage, Page::first(100)).await?;
        assert_eq!(whole.total, 5);
        let mut paged = Vec::new();
        for offset in [0, 2, 4] {
            let page = read_page(&fx.cat, stage, Page { offset, limit: 2 }).await?;
            assert_eq!(page.total, 5);
            paged.extend(page.rows);
        }
        assert_eq!(paged, whole.rows);
        assert_rows(
            fx.backend,
            &json_rows(&whole),
            &[
                vec![json!(3), json!(1985)],
                vec![json!(1), json!(1990)],
                vec![json!(5), json!(1990)],
                vec![json!(2), json!(2000)],
                vec![json!(4), json!(2010)],
            ],
        );
    }
    Ok(())
}
