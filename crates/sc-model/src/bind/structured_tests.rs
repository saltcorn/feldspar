//! The structured binding kinds against §12's tables (Stan TODO task 6.6):
//! one test per way time series and space sit in a database, over hand-built
//! frames, and one per refusal.

use serde_json::json;

use super::tests::{
    bind, config, day, frame, index, int, interface, ints, reals, refused, texts, vector, with,
};
use super::*;
use crate::frame::{Column, Frame};
use crate::interface::{Declaration, Element, SizeExpr};

fn dates(values: &[&str]) -> Column {
    Column::Date(values.iter().map(|v| Some(day(v))).collect())
}

/// `array[size] int name`.
fn int_array(name: &str, size: &str) -> Declaration {
    Declaration::new(
        name,
        Element::Int,
        vec![SizeExpr::var(size)],
        format!("array[{size}] int"),
    )
}

/// `array[rows, cols] int name`, or `matrix[rows, cols] name` when `real`.
fn two_axes(name: &str, rows: &str, cols: &str, real: bool) -> Declaration {
    let (element, text) = if real {
        (Element::Real, format!("matrix[{rows}, {cols}]"))
    } else {
        (Element::Int, format!("array[{rows}, {cols}] int"))
    };
    Declaration::new(
        name,
        element,
        vec![SizeExpr::var(rows), SizeExpr::var(cols)],
        text,
    )
}

fn real(name: &str) -> Declaration {
    Declaration::new(name, Element::Real, vec![], "real")
}

fn daily_grid(horizon: u32) -> Json {
    json!({"day": {"kind": "time_grid", "dataset": "main", "column": "day", "step": "day",
                   "horizon": horizon}})
}

// ---------------------------------------------------------------------------
// Time series.

/// `daily_sales(day, amount)` with 2024-03-03 missing.
fn sales_with_a_gap() -> Frame {
    frame(
        &[1, 2, 3, 4],
        vec![
            (
                "day",
                dates(&["2024-03-01", "2024-03-02", "2024-03-04", "2024-03-05"]),
            ),
            ("amount", reals(&[10.0, 12.0, 15.0, 11.0])),
        ],
    )
}

#[test]
fn gaps_on_a_daily_grid_bind_as_the_missing_data_idiom() {
    let program = interface(vec![
        int("T"),
        int("N_obs"),
        index("t_obs", "N_obs", "T"),
        vector("y_obs", "N_obs"),
    ]);
    let bindings = with(
        config(json!({
            "T": {"kind": "size", "dimension": "day"},
            "N_obs": {"kind": "count", "dataset": "main"},
            "t_obs": {"kind": "index", "dataset": "main", "column": "day", "dimension": "day"},
            "y_obs": {"kind": "column", "dataset": "main", "column": "amount"},
        })),
        DIMENSIONS_KEY,
        daily_grid(0),
    );
    let bound = bind(&program, &bindings, vec![("main", sales_with_a_gap())]).expect("bind");
    assert_eq!(
        bound.json,
        json!({"T": 5, "N_obs": 4, "t_obs": [1, 2, 4, 5], "y_obs": [10.0, 12.0, 15.0, 11.0]})
    );
}

#[test]
fn gaps_on_a_daily_grid_bind_as_a_filled_series_with_its_mask() {
    let program = interface(vec![int("T"), vector("y", "T"), int_array("seen", "T")]);
    let bindings = with(
        config(json!({
            "T": {"kind": "size", "dimension": "day"},
            "y": {"kind": "series", "dataset": "main", "column": "amount",
                  "over": {"dimension": "day"}, "fill": 0},
            "seen": {"kind": "series_present", "dataset": "main", "column": "amount",
                     "over": {"dimension": "day"}},
        })),
        DIMENSIONS_KEY,
        daily_grid(0),
    );
    let bound = bind(&program, &bindings, vec![("main", sales_with_a_gap())]).expect("bind");
    assert_eq!(bound.json["y"], json!([10.0, 12.0, 0.0, 15.0, 11.0]));
    assert_eq!(bound.json["seen"], json!([1, 1, 0, 1, 1]));
    assert_eq!(
        bound.report.variables[1].binding,
        "series(main.amount over day, fill 0)"
    );

    // With no fill the gap is refused by its date, with both ways out.
    let mut no_fill = bindings.clone();
    no_fill[BINDINGS_KEY]["y"]
        .as_object_mut()
        .expect("object")
        .remove("fill");
    let err = refused(bind(&program, &no_fill, vec![("main", sales_with_a_gap())]));
    assert!(
        err.contains(
            "`y`, declared `vector[T]` and bound to `series(main.amount over day)`: \
             `2024-03-03` of `day` has no value; give a `fill`"
        ),
        "{err}"
    );
    assert!(err.contains("missing-data idiom"), "{err}");
}

#[test]
fn events_are_counted_into_the_days_they_fall_in() {
    // `visits(at)`: one row per visit, none on the 2nd.
    let visits = frame(
        &[1, 2, 3, 4],
        vec![(
            "day",
            dates(&[
                "2024-03-01T08:00:00Z",
                "2024-03-01T17:30:00Z",
                "2024-03-03T12:00:00Z",
                "2024-03-01T23:59:59Z",
            ]),
        )],
    );
    let program = interface(vec![int("T"), int_array("y", "T")]);
    let bindings = with(
        config(json!({
            "T": {"kind": "size", "dimension": "day"},
            "y": {"kind": "series", "dataset": "main", "over": {"dimension": "day"}},
        })),
        DIMENSIONS_KEY,
        daily_grid(0),
    );
    let bound = bind(&program, &bindings, vec![("main", visits.clone())]).expect("bind");
    assert_eq!(bound.json["y"], json!([3, 0, 1]));
    assert_eq!(
        bound.report.variables[1].binding,
        "series(count of main over day)"
    );

    // A value column with no aggregate refuses two rows in one day, by key.
    let mut valued = visits;
    valued
        .columns
        .push(("minutes".to_owned(), ints(&[5, 10, 7, 1])));
    let mut refusing = bindings.clone();
    refusing[BINDINGS_KEY]["y"] = json!({"kind": "series", "dataset": "main",
        "column": "minutes", "over": {"dimension": "day"}, "fill": 0});
    let err = refused(bind(&program, &refusing, vec![("main", valued.clone())]));
    assert!(
        err.contains("rows `1` and `2` of `main` both fall in `2024-03-01` of `day`"),
        "{err}"
    );
    refusing[BINDINGS_KEY]["y"]["aggregate"] = json!("sum");
    let bound = bind(&program, &refusing, vec![("main", valued)]).expect("bind");
    assert_eq!(bound.json["y"], json!([16, 0, 7]));

    // A sum needs something to sum.
    let mut no_column = bindings;
    no_column[BINDINGS_KEY]["y"]["aggregate"] = json!("sum");
    let one = frame(&[1], vec![("day", dates(&["2024-03-01"]))]);
    let err = refused(bind(&program, &no_column, vec![("main", one)]));
    assert!(err.contains("`sum` needs a `column` to aggregate"), "{err}");
}

#[test]
fn a_monthly_grid_steps_across_a_year_boundary_by_the_calendar() {
    let sales = frame(
        &[1, 2, 3, 4, 5],
        vec![
            (
                "at",
                dates(&[
                    "2023-11-15",
                    "2023-12-01",
                    "2023-12-31T23:00:00Z",
                    "2024-01-10",
                    "2024-02-29",
                ]),
            ),
            ("amount", ints(&[5, 7, 8, 2, 4])),
        ],
    );
    let program = interface(vec![int("M"), int_array("y", "M")]);
    let bindings = with(
        config(json!({
            "M": {"kind": "size", "dimension": "month"},
            "y": {"kind": "series", "dataset": "main", "column": "amount",
                  "over": {"dimension": "month"}, "aggregate": "sum"},
        })),
        DIMENSIONS_KEY,
        json!({"month": {"kind": "time_grid", "dataset": "main", "column": "at",
                         "step": "month"}}),
    );
    let bound = bind(&program, &bindings, vec![("main", sales)]).expect("bind");
    assert_eq!(bound.json["y"], json!([5, 15, 2, 4]));
    assert_eq!(
        bound.coordinates.dimension("month").expect("month").labels,
        ["2023-11-01", "2023-12-01", "2024-01-01", "2024-02-01"]
    );
}

#[test]
fn a_horizon_is_labelled_with_its_future_dates() {
    let sales = frame(
        &[1, 2, 3, 4],
        vec![
            (
                "day",
                dates(&["2024-03-01", "2024-03-02", "2024-03-03", "2024-03-04"]),
            ),
            ("amount", reals(&[10.0, 12.0, 15.0, 11.0])),
        ],
    );
    let mut program = interface(vec![
        int("N"),
        int("T"),
        int("H"),
        vector("y", "N"),
        vector("y_all", "T"),
        int_array("observed", "T"),
    ]);
    let y_future = vector("y_future", "H");
    program.generated.push(y_future.clone());
    let bindings = with(
        config(json!({
            "N": {"kind": "count", "dataset": "main"},
            "T": {"kind": "size", "dimension": "day"},
            "H": {"kind": "size", "dimension": "day.future"},
            "y": {"kind": "column", "dataset": "main", "column": "amount"},
            "y_all": {"kind": "series", "dataset": "main", "column": "amount",
                      "over": {"dimension": "day"}, "fill": 0},
            "observed": {"kind": "series_present", "dataset": "main", "column": "amount",
                         "over": {"dimension": "day"}},
        })),
        DIMENSIONS_KEY,
        daily_grid(3),
    );
    let bound = bind(&program, &bindings, vec![("main", sales)]).expect("bind");
    assert_eq!(bound.json["T"], json!(7));
    assert_eq!(bound.json["H"], json!(3));
    // The horizon has no observations: filled, and masked out.
    assert_eq!(
        bound.json["y_all"],
        json!([10.0, 12.0, 15.0, 11.0, 0.0, 0.0, 0.0])
    );
    assert_eq!(bound.json["observed"], json!([1, 1, 1, 1, 0, 0, 0]));

    // `vector[H] y_future` comes back labelled with the three future dates.
    let labeller = Labeller::new(&bindings, &bound.coordinates).expect("labeller");
    let (axes, problems) = labeller.axes("y_future", Some(&y_future), &[3]);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(axes[0].dimension.as_deref(), Some("day.future"));
    assert_eq!(axes[0].labels, ["2024-03-05", "2024-03-06", "2024-03-07"]);
    assert_eq!(
        element_label("y_future", &[2], &axes),
        "y_future[2024-03-06]"
    );
}

/// `weather(day, temp)`, a related dataset with one day before the grid.
fn weather() -> Frame {
    frame(
        &[1, 2, 3, 4],
        vec![
            (
                "day",
                dates(&["2024-03-02", "2024-03-01", "2024-02-28", "2024-03-03"]),
            ),
            ("temp", reals(&[6.5, 5.0, 3.0, 4.0])),
        ],
    )
}

#[test]
fn a_weather_table_is_aligned_to_the_grid_on_the_time_bucket() {
    let sales = frame(
        &[1, 2, 3],
        vec![
            (
                "day",
                dates(&["2024-03-01", "2024-03-02", "2024-03-03T18:00:00Z"]),
            ),
            ("amount", reals(&[10.0, 12.0, 15.0])),
        ],
    );
    let program = interface(vec![int("T"), vector("y", "T"), vector("temp", "T")]);
    let bindings = with(
        config(json!({
            "T": {"kind": "size", "dimension": "day"},
            "y": {"kind": "series", "dataset": "main", "column": "amount",
                  "over": {"dimension": "day"}},
            "temp": {"kind": "series", "dataset": "weather", "column": "temp",
                     "over": {"dimension": "day", "column": "day"}},
        })),
        DIMENSIONS_KEY,
        daily_grid(0),
    );
    let datasets = || vec![("main", sales.clone()), ("weather", weather())];

    // The day before the grid is an unknown key, refused by default …
    let err = refused(bind(&program, &bindings, datasets()));
    assert!(
        err.contains(
            "1 row of `weather` has a `day` that is not a position of `day` (the first is \
             `2024-02-28T00:00:00Z`, on row `3`)"
        ),
        "{err}"
    );
    // … and dropped when the policy says so.
    let dropping = with(
        bindings.clone(),
        POLICIES_KEY,
        json!({"weather": {"unknown": "drop"}}),
    );
    let bound = bind(&program, &dropping, datasets()).expect("bind");
    assert_eq!(bound.json["y"], json!([10.0, 12.0, 15.0]));
    assert_eq!(bound.json["temp"], json!([5.0, 6.5, 4.0]));
    assert_eq!(bound.report.dropped("weather"), 1);

    // An axis with no column reads the grid's own, which is `main`'s.
    let mut implicit = dropping;
    implicit[BINDINGS_KEY]["temp"]["over"] = json!({"dimension": "day"});
    let err = refused(bind(&program, &implicit, datasets()));
    assert!(
        err.contains(
            "`day` is over `main`, not `weather`, so give the `column` of `weather` that places \
             its rows in it"
        ),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Many series: a panel.

fn sensors() -> Frame {
    frame(&[1, 2], vec![(LABEL_COLUMN, texts(&["roof", "cellar"]))])
}

/// `readings(sensor → sensors, at, value)`, hourly, the cellar silent at 01:00.
fn readings() -> Frame {
    frame(
        &[1, 2, 3, 4, 5],
        vec![
            ("sensor", ints(&[1, 2, 1, 1, 2])),
            (
                "at",
                dates(&[
                    "2024-03-01T00:10:00Z",
                    "2024-03-01T00:20:00Z",
                    "2024-03-01T01:05:00Z",
                    "2024-03-01T02:00:00Z",
                    "2024-03-01T02:30:00Z",
                ]),
            ),
            ("value", reals(&[20.5, 12.0, 21.0, 22.5, 12.5])),
        ],
    )
}

fn hourly() -> Json {
    json!({"hour": {"kind": "time_grid", "dataset": "main", "column": "at", "step": "hour"}})
}

#[test]
fn a_panel_in_long_form_is_two_indexes_and_a_column() {
    let program = interface(vec![
        int("N"),
        int("S"),
        int("T"),
        index("sensor", "N", "S"),
        index("t", "N", "T"),
        vector("y", "N"),
    ]);
    let bindings = with(
        config(json!({
            "N": {"kind": "count", "dataset": "main"},
            "S": {"kind": "size", "dimension": "sensors"},
            "T": {"kind": "size", "dimension": "hour"},
            "sensor": {"kind": "index", "dataset": "main", "column": "sensor",
                       "dimension": "sensors"},
            "t": {"kind": "index", "dataset": "main", "column": "at", "dimension": "hour"},
            "y": {"kind": "column", "dataset": "main", "column": "value"},
        })),
        DIMENSIONS_KEY,
        hourly(),
    );
    let bound = bind(
        &program,
        &bindings,
        vec![("main", readings()), ("sensors", sensors())],
    )
    .expect("bind");
    assert_eq!(bound.json["sensor"], json!([1, 2, 1, 1, 2]));
    assert_eq!(bound.json["t"], json!([1, 1, 2, 3, 3]));
    assert_eq!(bound.json["T"], json!(3));
}

#[test]
fn a_panel_in_wide_form_is_cells_with_their_mask() {
    let program = interface(vec![
        int("S"),
        int("T"),
        two_axes("Y", "S", "T", true),
        two_axes("seen", "S", "T", false),
    ]);
    let bindings = with(
        config(json!({
            "S": {"kind": "size", "dimension": "sensors"},
            "T": {"kind": "size", "dimension": "hour"},
            "Y": {"kind": "cells", "dataset": "main", "column": "value",
                  "rows": {"dimension": "sensors", "column": "sensor"},
                  "cols": {"dimension": "hour"}, "fill": 0},
            "seen": {"kind": "cells_present", "dataset": "main", "column": "value",
                     "rows": {"dimension": "sensors", "column": "sensor"},
                     "cols": {"dimension": "hour"}},
        })),
        DIMENSIONS_KEY,
        hourly(),
    );
    let datasets = || vec![("main", readings()), ("sensors", sensors())];
    let bound = bind(&program, &bindings, datasets()).expect("bind");
    assert_eq!(
        bound.json["Y"],
        json!([[20.5, 21.0, 22.5], [12.0, 0.0, 12.5]])
    );
    assert_eq!(bound.json["seen"], json!([[1, 1, 1], [1, 0, 1]]));

    // Both axes are labelled: by the sensors' names and by the hours.
    let labeller = Labeller::new(&bindings, &bound.coordinates).expect("labeller");
    let decl = two_axes("Y", "S", "T", true);
    let (axes, _) = labeller.axes("Y", Some(&decl), &[2, 3]);
    assert_eq!(
        element_label("Y", &[2, 3], &axes),
        "Y[cellar,2024-03-01T02:00:00Z]"
    );

    // Without a fill, the silent cell is named by both its coordinates.
    let mut no_fill = bindings;
    no_fill[BINDINGS_KEY]["Y"]
        .as_object_mut()
        .expect("object")
        .remove("fill");
    let err = refused(bind(&program, &no_fill, datasets()));
    assert!(
        err.contains("(`cellar`, `2024-03-01T01:00:00Z`) of `sensors` × `hour` has no value"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Areal space, and space with time.

/// Five regions: A–B–D–C–A a square, and E an island.
fn regions() -> Frame {
    frame(
        &[1, 2, 3, 4, 5],
        vec![
            ("population", ints(&[1000, 2500, 800, 1200, 300])),
            (LABEL_COLUMN, texts(&["A", "B", "C", "D", "E"])),
        ],
    )
}

/// `region_adjacency(a, b)`: each pair once, except A–B stored both ways.
fn adjacency(pairs: &[(i64, i64)]) -> Frame {
    let keys: Vec<i64> = (1..=pairs.len() as i64).collect();
    frame(
        &keys,
        vec![
            ("a", ints(&pairs.iter().map(|p| p.0).collect::<Vec<_>>())),
            ("b", ints(&pairs.iter().map(|p| p.1).collect::<Vec<_>>())),
        ],
    )
}

fn square_and_island() -> Frame {
    adjacency(&[(1, 2), (1, 3), (2, 4), (3, 4), (2, 1)])
}

/// `cases(region → regions, week, count)`.
fn cases() -> Frame {
    frame(
        &[1, 2, 3, 4, 5],
        vec![
            ("region", ints(&[1, 1, 2, 5, 3])),
            (
                "week",
                dates(&[
                    "2024-01-01",
                    "2024-01-03",
                    "2024-01-09",
                    "2024-01-15",
                    "2024-01-02",
                ]),
            ),
            ("count", ints(&[3, 2, 4, 1, 0])),
        ],
    )
}

fn edges(kind: &str) -> Json {
    json!({"kind": kind, "dataset": "adjacency", "from": "a", "to": "b", "dimension": "regions"})
}

fn spatiotemporal() -> (Interface, Attrs) {
    let program = interface(vec![
        int("R"),
        int("T"),
        int("N_edges"),
        index("node1", "N_edges", "R"),
        index("node2", "N_edges", "R"),
        Declaration::new("scaling_factor", Element::Real, vec![], "real<lower=0>")
            .bounded(Some("0"), None),
        int("C"),
        index("component", "R", "C"),
        two_axes("W", "R", "R", true),
        vector("pop", "R"),
        two_axes("y", "R", "T", false),
    ]);
    let bindings = with(
        config(json!({
            "R": {"kind": "size", "dimension": "regions"},
            "T": {"kind": "size", "dimension": "week"},
            "N_edges": edges("edge_count"),
            "node1": edges("edge_from"),
            "node2": edges("edge_to"),
            "scaling_factor": edges("icar_scale"),
            "C": edges("components"),
            "component": edges("component"),
            "W": edges("adjacency"),
            "pop": {"kind": "column", "dataset": "regions", "column": "population"},
            "y": {"kind": "cells", "dataset": "main", "column": "count",
                  "rows": {"dimension": "regions", "column": "region"},
                  "cols": {"dimension": "week"}, "aggregate": "sum", "fill": 0},
        })),
        DIMENSIONS_KEY,
        json!({"week": {"kind": "time_grid", "dataset": "main", "column": "week", "step": "week"}}),
    );
    (program, bindings)
}

#[test]
fn a_region_by_week_model_binds_its_graph_and_its_counts() {
    let (program, bindings) = spatiotemporal();
    let bound = bind(
        &program,
        &bindings,
        vec![
            ("main", cases()),
            ("regions", regions()),
            ("adjacency", square_and_island()),
        ],
    )
    .expect("bind");
    // Each unordered pair once, the lesser first, sorted: B–A is A–B again.
    assert_eq!(bound.json["N_edges"], json!(4));
    assert_eq!(bound.json["node1"], json!([1, 1, 2, 3]));
    assert_eq!(bound.json["node2"], json!([2, 3, 4, 4]));
    assert_eq!(
        bound.json["W"],
        json!([
            [0.0, 1.0, 1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 1.0, 0.0],
            [1.0, 0.0, 0.0, 1.0, 0.0],
            [0.0, 1.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 0.0, 0.0]
        ])
    );
    assert_eq!(bound.json["C"], json!(2));
    assert_eq!(bound.json["component"], json!([1, 1, 1, 1, 2]));
    // The square is the cycle C_4, each variance (16 − 1) / 48; the island
    // contributes 1.
    let scale = bound.json["scaling_factor"].as_f64().expect("real");
    assert!((scale - 0.3125f64.powf(0.8)).abs() < 1e-12, "{scale}");
    // Weeks start on Monday 2024-01-01; A has two cases rows in its first.
    assert_eq!(bound.json["T"], json!(3));
    assert_eq!(
        bound.json["y"],
        json!([[5, 0, 0], [0, 4, 0], [0, 0, 0], [0, 0, 0], [0, 0, 1]])
    );
    // The island is warned about, by name, once.
    assert_eq!(
        bound.report.warnings.len(),
        1,
        "{:?}",
        bound.report.warnings
    );
    assert!(
        bound.report.warnings[0].starts_with("`regions` `E` has no neighbour in `adjacency`"),
        "{}",
        bound.report.warnings[0]
    );
    assert_eq!(
        bound.report.variables[2].binding,
        "edge_count(adjacency: a — b → regions)"
    );

    // `keep` gives every row as stored, so the edge list says so.
    let mut keep = bindings.clone();
    for var in ["N_edges", "node1", "node2"] {
        keep[BINDINGS_KEY][var]["symmetric"] = json!("keep");
    }
    let bound = bind(
        &program,
        &keep,
        vec![
            ("main", cases()),
            ("regions", regions()),
            ("adjacency", square_and_island()),
        ],
    )
    .expect("bind");
    assert_eq!(bound.json["node1"], json!([1, 1, 2, 3, 2]));
    assert_eq!(bound.json["node2"], json!([2, 3, 4, 4, 1]));
}

#[test]
fn a_graph_refuses_self_loops_and_null_ends_and_polices_unknown_regions() {
    // The regions are the model's own dataset here, since a graph needs no
    // observations: the dimension is `main`.
    let program = interface(vec![int("N_edges"), real("s")]);
    let into_main = |kind: &str| {
        let mut e = edges(kind);
        e["dimension"] = json!("main");
        e
    };
    let bindings_main =
        config(json!({"N_edges": into_main("edge_count"), "s": into_main("icar_scale")}));
    let run = |bindings: &Attrs, pairs: Frame| {
        bind(
            &program,
            bindings,
            vec![("main", regions()), ("adjacency", pairs)],
        )
    };

    let err = refused(run(&bindings_main, adjacency(&[(1, 2), (3, 3)])));
    assert!(
        err.contains("row `2` of `adjacency` joins `C` to itself"),
        "{err}"
    );
    let err = refused(run(&bindings_main, {
        let mut f = adjacency(&[(1, 2), (2, 3)]);
        f.columns[1].1 = Column::Int(vec![Some(2), None]);
        f
    }));
    assert!(
        err.contains("row `2` of `adjacency` has a null `b`, and an edge has two ends"),
        "{err}"
    );
    // An edge to a region not in the dimension follows `unknown`.
    let err = refused(run(&bindings_main, adjacency(&[(1, 2), (2, 9)])));
    assert!(
        err.contains("1 row of `adjacency` has a `b` that is not a position of `main`"),
        "{err}"
    );
    let dropping = with(
        bindings_main.clone(),
        POLICIES_KEY,
        json!({"adjacency": {"unknown": "drop"}}),
    );
    let bound = run(&dropping, adjacency(&[(1, 2), (2, 9)])).expect("bind");
    assert_eq!(bound.json["N_edges"], json!(1));
    assert_eq!(bound.report.dropped("adjacency"), 1);

    // `symmetric` belongs to the edge list; a scale is always undirected.
    let mut directed = bindings_main;
    directed[BINDINGS_KEY]["s"]["symmetric"] = json!("keep");
    let err = refused(run(&directed, adjacency(&[(1, 2)])));
    assert!(err.contains("`symmetric` is for an edge list"), "{err}");
}

// ---------------------------------------------------------------------------
// Point-referenced space.

/// London, Paris, Edinburgh.
fn sites() -> Frame {
    frame(
        &[1, 2, 3],
        vec![
            ("lat", reals(&[51.5074, 48.8566, 55.9533])),
            ("lon", reals(&[-0.1278, 2.3522, -3.1883])),
        ],
    )
}

#[test]
fn points_are_projected_about_their_centroid_and_distances_are_great_circles() {
    let program = interface(vec![
        int("N"),
        Declaration::new(
            "xy",
            Element::Real,
            vec![SizeExpr::var("N"), SizeExpr::literal(2)],
            "array[N] vector[2]",
        ),
        two_axes("D", "N", "N", true),
    ]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "xy": {"kind": "points", "dataset": "main", "lat": "lat", "lon": "lon", "project": true},
        "D": {"kind": "distances", "dataset": "main", "lat": "lat", "lon": "lon"},
    }));
    let bound = bind(&program, &bindings, vec![("main", sites())]).expect("bind");
    let number = |v: &Json| v.as_f64().expect("number");
    let d = &bound.json["D"];
    assert_eq!(number(&d[0][0]), 0.0);
    assert!((number(&d[0][1]) - 343.56).abs() < 0.1, "{d}");
    assert!((number(&d[0][2]) - 534.0).abs() < 1.0, "{d}");
    assert_eq!(d[1][2], d[2][1]);
    // Projected: centred on the centroid, Edinburgh north of London by about
    // its 4.45° of latitude.
    let xy = &bound.json["xy"];
    let north: f64 = (0..3).map(|i| number(&xy[i][1])).sum();
    assert!(north.abs() < 1e-9, "{xy}");
    assert!(
        (number(&xy[2][1]) - number(&xy[0][1]) - 4.4459 * 111.195).abs() < 0.1,
        "{xy}"
    );

    // Unprojected, a point is `[lon, lat]` in degrees.
    let mut degrees = bindings.clone();
    degrees[BINDINGS_KEY]["xy"]["project"] = json!(false);
    let bound = bind(&program, &degrees, vec![("main", sites())]).expect("bind");
    assert_eq!(bound.json["xy"][0], json!([-0.1278, 51.5074]));

    // A latitude off the globe is refused by row.
    let mut off = sites();
    off.columns[0].1 = reals(&[51.5, 91.0, 55.9]);
    let err = refused(bind(&program, &bindings, vec![("main", off)]));
    assert!(
        err.contains("`lat` of `main` is 91 on row `2`, outside ±90 degrees"),
        "{err}"
    );
}

#[test]
fn distances_are_capped_by_their_number_of_sites() {
    let n = MAX_DISTANCE_SITES + 1;
    let keys: Vec<i64> = (1..=n as i64).collect();
    let many = frame(
        &keys,
        vec![("lat", reals(&vec![0.0; n])), ("lon", reals(&vec![0.0; n]))],
    );
    let program = interface(vec![int("N"), two_axes("D", "N", "N", true)]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "D": {"kind": "distances", "dataset": "main", "lat": "lat", "lon": "lon"},
    }));
    let err = refused(bind(&program, &bindings, vec![("main", many)]));
    assert!(
        err.contains("`main` has 3001 rows, and distances are computed between at most 3000"),
        "{err}"
    );
}
