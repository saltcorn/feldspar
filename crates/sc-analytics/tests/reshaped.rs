//! Summary tables (analytics TODO A2.8) and the presets that reshape their
//! data (A2.11) on both backends, over the ten houses of `plots.rs`:
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

use sc_analytics::plot::{
    AggregateFn, Assignment, Bin, Cell, Channel, Coord, DataRef, FacetScales, FieldDef, Fold,
    Layer, Mark, PlotSpec, Preset, RenderedTable, Stat, TableData, TableSpec, preset, render_table,
    validate,
};
use sc_dataset::{Options, Schema, StageShape, compile};
use sc_error::Result;
use serde_json::{Value as Json, json};

use super::plots::{Fixture, assert_rows, both, draw, refused, spec};

async fn shape(fx: &Fixture) -> StageShape {
    let def = sc_dataset::load_dataset(&fx.cat, fx.houses)
        .await
        .unwrap()
        .unwrap();
    let schema = Schema::of_catalog(&fx.cat).unwrap();
    let library = sc_dataset::load_library(&fx.cat).await.unwrap();
    compile(&schema, &library, &def, Options::default())
        .last()
        .expect("reads")
        .shape()
}

fn table(fx: &Fixture, rows: Vec<FieldDef>, columns: Vec<FieldDef>, cells: Vec<Cell>) -> TableSpec {
    TableSpec {
        data: DataRef::Dataset { dataset: fx.houses },
        fold: None,
        rows,
        columns,
        cells,
        totals: true,
    }
}

async fn tabulate(fx: &Fixture, spec: &TableSpec) -> TableData {
    match render_table(&fx.cat, spec).await.expect("renders") {
        RenderedTable::Table(data) => *data,
        RenderedTable::Refused { problems, .. } => {
            panic!("on {}: refused: {problems:?}", fx.backend)
        }
    }
}

async fn table_refused(fx: &Fixture, spec: &TableSpec) -> String {
    match render_table(&fx.cat, spec).await.expect("answers") {
        RenderedTable::Refused { error, .. } => error,
        RenderedTable::Table(_) => panic!("on {}: made {spec:?}", fx.backend),
    }
}

#[tokio::test]
async fn a_summary_table_has_cells_by_rows_and_columns_and_totals() -> Result<()> {
    for fx in both().await? {
        let spec = table(
            &fx,
            vec![FieldDef::of("neighbourhood")],
            vec![FieldDef::of("sold")],
            vec![Cell::of(AggregateFn::Mean, "price"), Cell::count()],
        );
        let data = tabulate(&fx, &spec).await;
        assert_eq!(data.cells, vec!["mean of price", "rows"]);
        assert_eq!(data.body.columns, vec!["r0", "c0", "n", "v0", "v1"]);
        let third = 800.0 / 3.0;
        assert_rows(
            fx.backend,
            &data.body.rows,
            &[
                vec![json!(1), json!(false), json!(1), json!(200), json!(1)],
                vec![json!(1), json!(true), json!(3), json!(third), json!(3)],
                vec![json!(2), json!(false), json!(2), json!(250), json!(2)],
                vec![json!(2), json!(true), json!(2), json!(625), json!(2)],
                vec![json!(3), json!(true), json!(1), json!(220), json!(1)],
                vec![json!(3), Json::Null, json!(1), json!(120), json!(1)],
            ],
        );
        // The Total column (one per neighbourhood), the Total row (one per
        // value of `sold`) and the corner: means of the rows, not of means.
        let totals = data.row_totals.as_ref().expect("row totals");
        assert_eq!(totals.columns, vec!["r0", "n", "v0", "v1"]);
        assert_rows(
            fx.backend,
            &totals.rows,
            &[
                vec![json!(1), json!(4), json!(250), json!(4)],
                vec![json!(2), json!(4), json!(437.5), json!(4)],
                vec![json!(3), json!(2), json!(170), json!(2)],
            ],
        );
        assert_rows(
            fx.backend,
            &data.column_totals.as_ref().expect("column totals").rows,
            &[
                vec![json!(false), json!(3), json!(700.0 / 3.0), json!(3)],
                vec![json!(true), json!(6), json!(2270.0 / 6.0), json!(6)],
                vec![Json::Null, json!(1), json!(120), json!(1)],
            ],
        );
        assert_rows(
            fx.backend,
            &data.grand_total.as_ref().expect("grand total").rows,
            &[vec![json!(10), json!(309), json!(10)]],
        );
        assert_eq!(data.total, 10);

        // Medians by binned rows, with no column dimension: no Total column
        // or row, only the corner.
        let binned = table(
            &fx,
            vec![FieldDef {
                field: "area".into(),
                bin: Some(Bin {
                    width: Some(20.0),
                    bins: None,
                }),
            }],
            Vec::new(),
            vec![Cell::of(AggregateFn::Median, "price")],
        );
        let data = tabulate(&fx, &binned).await;
        assert_eq!(data.body.columns, vec!["r0", "r0_end", "n", "v0"]);
        // 40–60: 100, 120, 150, 220; 60–80: 200, 250, 300, 350; 80–100: 400, 1000.
        assert_rows(
            fx.backend,
            &data.body.rows,
            &[
                vec![json!(40), json!(60), json!(4), json!(135)],
                vec![json!(60), json!(80), json!(4), json!(275)],
                vec![json!(80), json!(100), json!(2), json!(700)],
            ],
        );
        assert!(data.row_totals.is_none() && data.column_totals.is_none());
        assert_rows(
            fx.backend,
            &data.grand_total.unwrap().rows,
            &[vec![json!(10), json!(235)]],
        );

        // No dimensions at all: the body is the whole table, and the cells
        // default to a count.
        let data = tabulate(&fx, &table(&fx, Vec::new(), Vec::new(), Vec::new())).await;
        assert_rows(fx.backend, &data.body.rows, &[vec![json!(10), json!(10)]]);
        assert!(data.grand_total.is_none());

        // What cannot be a table is said in a sentence.
        let err = table_refused(
            &fx,
            &table(&fx, vec![FieldDef::of("area")], Vec::new(), Vec::new()),
        )
        .await;
        assert!(
            err.contains("`area`, a number with many values; bin it"),
            "{err}"
        );
        let err = table_refused(
            &fx,
            &table(
                &fx,
                vec![FieldDef::of("sold")],
                Vec::new(),
                vec![Cell::of(AggregateFn::Mean, "sold")],
            ),
        )
        .await;
        assert_eq!(err, "the mean needs numbers, and `sold` is a boolean");
    }
    Ok(())
}

fn on_y(columns: &[&str]) -> Assignment {
    Assignment {
        y: columns.iter().map(|c| FieldDef::of(*c)).collect(),
        ..Assignment::default()
    }
}

#[tokio::test]
async fn a_correlation_heatmap_correlates_every_pair() -> Result<()> {
    for fx in both().await? {
        let shape = shape(&fx).await;
        let data_ref = DataRef::Dataset { dataset: fx.houses };
        let (spec, a) = preset(
            Preset::Correlation,
            data_ref,
            &shape,
            &on_y(&["price", "area"]),
        )
        .expect("a preset");
        assert_eq!(spec.fold, Some(Fold::pairs(["price", "area"], true)));
        assert_eq!(a.y.len(), 2);
        assert!(
            validate(&spec, &shape).is_empty(),
            "{:?}",
            validate(&spec, &shape)
        );
        let data = draw(&fx, &spec).await;
        let r = 0.816_304_376_521_976_2;
        let cells = &data.layers[0];
        assert_eq!(cells.columns, vec!["x", "y", "color", "n"]);
        assert_rows(
            fx.backend,
            &cells.rows,
            &[
                vec![json!("area"), json!("area"), json!(1), json!(10)],
                vec![json!("area"), json!("price"), json!(r), json!(10)],
                vec![json!("price"), json!("area"), json!(r), json!(10)],
                vec![json!("price"), json!("price"), json!(1), json!(10)],
            ],
        );
        // The second layer writes the same numbers as labels.
        assert_eq!(data.layers[1].columns, vec!["x", "y", "label", "n"]);
        assert_eq!(data.layers[1].rows.len(), 4);
    }
    Ok(())
}

#[tokio::test]
async fn a_scatterplot_matrix_and_parallel_coordinates() -> Result<()> {
    for fx in both().await? {
        let shape = shape(&fx).await;
        let data_ref = DataRef::Dataset { dataset: fx.houses };
        // Nothing dropped: the dataset's first numbers, never the row key or
        // a foreign key.
        let (splom, a) = preset(
            Preset::Splom,
            data_ref.clone(),
            &shape,
            &Assignment::default(),
        )
        .expect("a preset");
        let picked: Vec<&str> = a.y.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(picked, vec!["price", "area", "year_built"]);
        assert_eq!(splom.facet.scales, FacetScales::Free);
        let data = draw(&fx, &splom).await;
        let layer = &data.layers[0];
        assert_eq!(layer.columns, vec!["x", "y", "row", "column"]);
        // Six plots of ten houses each: every pair but a column with itself.
        assert_eq!(layer.rows.len(), 60, "on {}", fx.backend);
        assert_eq!(
            data.facets["column"],
            vec![json!("area"), json!("price"), json!("year_built")]
        );
        assert!(
            layer
                .rows
                .iter()
                .any(|r| r == &vec![json!(50.0), json!(100.0), json!("price"), json!("area")]),
            "on {}: house 1's area against its price",
            fx.backend
        );

        let mut colored = on_y(&["price", "area"]);
        colored.color = Some(FieldDef::of("sold"));
        let (parallel, _) = preset(Preset::Parallel, data_ref, &shape, &colored).expect("a preset");
        assert_eq!(parallel.coord, Coord::Parallel);
        let data = draw(&fx, &parallel).await;
        let layer = &data.layers[0];
        assert_eq!(layer.columns, vec!["color", "y_0", "y_1"]);
        assert_eq!(layer.info["axes"], json!(["price", "area"]));
        assert_eq!(layer.rows.len(), 10);
        assert!(
            layer
                .rows
                .iter()
                .any(|r| r == &vec![json!(true), json!(1000.0), json!(90.0)]),
            "on {}",
            fx.backend
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_mosaic_counts_by_two_categories() -> Result<()> {
    for fx in both().await? {
        let shape = shape(&fx).await;
        let a = Assignment {
            x: Some(FieldDef::of("neighbourhood")),
            y: vec![FieldDef::of("sold")],
            color: Some(FieldDef::of("year_built")),
            ..Assignment::default()
        };
        let (mosaic, a) = preset(
            Preset::Mosaic,
            DataRef::Dataset { dataset: fx.houses },
            &shape,
            &a,
        )
        .expect("a preset");
        assert!(a.color.is_none(), "a mosaic colours by Y");
        let data = draw(&fx, &mosaic).await;
        assert_eq!(data.layers[0].columns, vec!["x", "y", "size"]);
        assert_rows(
            fx.backend,
            &data.layers[0].rows,
            &[
                vec![json!(1), json!(false), json!(1)],
                vec![json!(1), json!(true), json!(3)],
                vec![json!(2), json!(false), json!(2)],
                vec![json!(2), json!(true), json!(2)],
                vec![json!(3), json!(true), json!(1)],
                vec![json!(3), Json::Null, json!(1)],
            ],
        );
    }
    Ok(())
}

#[tokio::test]
async fn reshaped_specs_that_cannot_be_drawn_say_why() -> Result<()> {
    for fx in both().await? {
        let mut lines = spec(
            &fx,
            Layer::new(Mark::Line, Stat::Identity)
                .with(Channel::X, FieldDef::of("area"))
                .with(Channel::Y, FieldDef::of("price")),
        );
        lines.coord = Coord::Parallel;
        assert!(
            refused(&fx, &lines)
                .await
                .starts_with("parallel coordinates draw several columns compared as one variable")
        );

        let mosaic = spec(
            &fx,
            Layer::new(Mark::Mosaic, Stat::Count)
                .with(Channel::X, FieldDef::of("neighbourhood"))
                .with(Channel::Y, FieldDef::of("area")),
        );
        assert_eq!(
            refused(&fx, &mosaic).await,
            "a mosaic's tiles are the values of `area`, a number with many values; bin it"
        );

        let mut correlation: PlotSpec = spec(
            &fx,
            Layer::new(
                Mark::Rect,
                Stat::Correlation {
                    x: "price".into(),
                    y: "sold".into(),
                },
            )
            .with(Channel::X, FieldDef::of("neighbourhood")),
        );
        assert_eq!(
            refused(&fx, &correlation).await,
            "a correlation needs numbers, and `sold` is a boolean"
        );
        correlation.layers[0].mark = Mark::Bar;
        correlation.layers[0].stat = Stat::Correlation {
            x: "price".into(),
            y: "area".into(),
        };
        assert_eq!(
            refused(&fx, &correlation).await,
            "a correlation cannot be drawn as bars"
        );
    }
    Ok(())
}
