//! The binder against §11's table (Stan TODO task 3.5): one test per way
//! hierarchical data sits in a database, over hand-built frames, and one per
//! refusal.

use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::*;
use crate::frame::{Column, Frame};
use crate::interface::{Declaration, Element, SizeExpr};

pub(super) fn ints(v: &[i64]) -> Column {
    Column::Int(v.iter().map(|x| Some(*x)).collect())
}

pub(super) fn reals(v: &[f64]) -> Column {
    Column::Float(v.iter().map(|x| Some(*x)).collect())
}

pub(super) fn texts(v: &[&str]) -> Column {
    Column::Str(v.iter().map(|x| Some((*x).to_owned())).collect())
}

/// A frame whose rows have the integer primary keys `keys`.
pub(super) fn frame(keys: &[i64], columns: Vec<(&str, Column)>) -> Frame {
    Frame::new(
        columns
            .into_iter()
            .map(|(n, c)| (n.to_owned(), c))
            .collect(),
        keys.iter().map(|k| format!("int:{k}")).collect(),
    )
    .expect("frame")
}

pub(super) fn int(name: &str) -> Declaration {
    Declaration::new(name, Element::Int, vec![], "int")
}

/// `array[size] int<lower=1, upper=upper> name`.
pub(super) fn index(name: &str, size: &str, upper: &str) -> Declaration {
    Declaration::new(
        name,
        Element::Int,
        vec![SizeExpr::var(size)],
        format!("array[{size}] int<lower=1, upper={upper}>"),
    )
    .bounded(Some("1"), Some(upper))
}

/// `vector[size] name`.
pub(super) fn vector(name: &str, size: &str) -> Declaration {
    Declaration::new(
        name,
        Element::Real,
        vec![SizeExpr::var(size)],
        format!("vector[{size}]"),
    )
}

pub(super) fn interface(data: Vec<Declaration>) -> Interface {
    Interface {
        data,
        ..Interface::default()
    }
}

pub(super) fn config(bindings: Json) -> Attrs {
    let mut attrs = Attrs::new();
    attrs.insert(BINDINGS_KEY.to_owned(), bindings);
    attrs
}

pub(super) fn with(mut attrs: Attrs, key: &str, value: Json) -> Attrs {
    attrs.insert(key.to_owned(), value);
    attrs
}

pub(super) fn bind(
    interface: &Interface,
    config: &Attrs,
    datasets: Vec<(&str, Frame)>,
) -> Result<BoundData> {
    let datasets: Vec<(String, Frame)> = datasets
        .into_iter()
        .map(|(n, f)| (n.to_owned(), f))
        .collect();
    bind_data(interface, config, &datasets, DEFAULT_MAX_DATA_VALUES)
}

pub(super) fn refused(result: Result<BoundData>) -> String {
    result.expect_err("refused").to_string()
}

// ---------------------------------------------------------------------------
// The radon example (§11, row 1): observations with a key to a group table.

/// 85 counties is the real thing; four does the job. Anoka (27003) has no
/// homes at all, and must still get a position.
fn counties() -> Frame {
    frame(
        &[27001, 27003, 27005, 27007],
        vec![
            ("log_uranium", reals(&[-0.5, 0.25, 0.75, 1.0])),
            (
                LABEL_COLUMN,
                texts(&["Aitkin", "Anoka", "Becker", "Beltrami"]),
            ),
        ],
    )
}

fn homes() -> Frame {
    frame(
        &[1, 2, 3, 4, 5],
        vec![
            ("county", ints(&[27001, 27005, 27001, 27007, 27005])),
            ("floor", ints(&[0, 1, 0, 0, 1])),
            ("log_radon", reals(&[0.8, 1.1, 0.4, 2.0, -0.1])),
        ],
    )
}

fn radon_program() -> Interface {
    interface(vec![
        Declaration::new("N", Element::Int, vec![], "int<lower=1>").bounded(Some("1"), None),
        Declaration::new("J", Element::Int, vec![], "int<lower=1>").bounded(Some("1"), None),
        index("county", "N", "J"),
        vector("x", "N"),
        vector("u", "J"),
        vector("y", "N"),
    ])
}

fn radon_bindings() -> Json {
    json!({
        "N": {"kind": "count", "dataset": "main"},
        "J": {"kind": "size", "dimension": "counties"},
        "county": {"kind": "index", "dataset": "main", "column": "county", "dimension": "counties"},
        "x": {"kind": "column", "dataset": "main", "column": "floor"},
        "u": {"kind": "column", "dataset": "counties", "column": "log_uranium"},
        "y": {"kind": "column", "dataset": "main", "column": "log_radon"},
    })
}

#[test]
fn radon_binds_every_county_including_the_one_with_no_homes() {
    let bound = bind(
        &radon_program(),
        &config(radon_bindings()),
        vec![("main", homes()), ("counties", counties())],
    )
    .expect("bind");
    assert_eq!(
        bound.json,
        json!({
            "N": 5,
            "J": 4,
            "county": [1, 3, 1, 4, 3],
            "x": [0.0, 1.0, 0.0, 0.0, 1.0],
            "u": [-0.5, 0.25, 0.75, 1.0],
            "y": [0.8, 1.1, 0.4, 2.0, -0.1],
        })
    );
    // The report lists the variables in declaration order.
    let order: Vec<&str> = bound
        .report
        .variables
        .iter()
        .map(|v| v.name.as_str())
        .collect();
    assert_eq!(order, ["N", "J", "county", "x", "u", "y"]);

    // The counties are labelled by the label formula and keyed by their key.
    let counties = bound.coordinates.dimension("counties").expect("counties");
    assert_eq!(counties.kind, DimensionKind::Rows);
    assert_eq!(counties.labels, ["Aitkin", "Anoka", "Becker", "Beltrami"]);
    assert_eq!(
        counties.keys,
        [json!(27001), json!(27003), json!(27005), json!(27007)]
    );
    // The homes are labelled by their key, having no label.
    let homes = bound.coordinates.dimension("main").expect("main");
    assert_eq!(homes.labels, ["1", "2", "3", "4", "5"]);

    assert_eq!(bound.report.dimensions["counties"], 4);
    assert!(bound.report.drops.is_empty());
    assert_eq!(bound.report.values, 2 + 5 * 3 + 4);
    let county = &bound.report.variables[2];
    assert_eq!(county.binding, "index(main.county → counties)");
    assert_eq!(county.shape, [5]);
    assert_eq!(
        county.first,
        [json!(1), json!(3), json!(1), json!(4), json!(3)]
    );
}

#[test]
fn the_save_time_check_passes_the_radon_model_and_names_what_is_wrong_with_others() {
    use crate::dataset::Dataset;
    use crate::model::{Model, NamedDataset};
    let model = |bindings: Json| {
        Model::new(
            "radon",
            "stan",
            Dataset::new("homes")
                .column("county", "county")
                .column("floor", "floor")
                .column("log_radon", "log_radon"),
        )
        .related(
            NamedDataset::new(
                "counties",
                Dataset::new("counties").column("log_uranium", "log_uranium"),
            )
            .labelled("name"),
        )
        .config(BINDINGS_KEY, bindings)
    };
    check_bindings(&radon_program(), &model(radon_bindings())).expect("radon");

    let mut typo = radon_bindings();
    typo["y"]["column"] = json!("log_radn");
    let err = check_bindings(&radon_program(), &model(typo)).unwrap_err();
    assert!(
        err.to_string().contains(
            "`y`, declared `vector[N]` and bound to `main.log_radn`: `main` has no column \
             `log_radn` (its columns are `county`, `floor`, `log_radon`)"
        ),
        "{err}"
    );

    let mut matrix = radon_bindings();
    matrix["x"] = json!({"kind": "columns", "dataset": "main", "columns": ["floor"]});
    let err = check_bindings(&radon_program(), &model(matrix)).unwrap_err();
    assert!(
        err.to_string()
            .contains("it has 1 axis but a `columns` binding produces 2 axes"),
        "{err}"
    );

    let mut nowhere = radon_bindings();
    nowhere["J"] = json!({"kind": "size", "dimension": "county"});
    let err = check_bindings(&radon_program(), &model(nowhere)).unwrap_err();
    assert!(
        err.to_string().contains("there is no dimension `county`"),
        "{err}"
    );

    let mut designed = radon_bindings();
    designed["N"] = json!({"kind": "width", "of": "x"});
    let err = check_bindings(&radon_program(), &model(designed)).unwrap_err();
    assert!(
        err.to_string()
            .contains("`width` is the width of a `design`-bound variable, and `x` is not one"),
        "{err}"
    );

    let mut real = radon_bindings();
    real["county"] = json!({"kind": "design", "dataset": "main", "columns": ["floor"]});
    let err = check_bindings(&radon_program(), &model(real)).unwrap_err();
    assert!(
        err.to_string().contains("has 1 axis but a `design`"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Nested levels, each a table (§11, row 2), both ways.

fn schools() -> Frame {
    frame(&[10, 20], vec![("funding", reals(&[1.5, 2.5]))])
}

fn classes() -> Frame {
    frame(&[1, 2, 3], vec![("school", ints(&[20, 10, 20]))])
}

fn pupils() -> Frame {
    frame(
        &[100, 101, 102, 103],
        vec![
            ("class", ints(&[3, 1, 2, 3])),
            // The formula `classⱵschool`, as the dataset would compute it.
            ("school", ints(&[20, 20, 10, 20])),
            ("score", reals(&[0.1, 0.2, 0.3, 0.4])),
        ],
    )
}

#[test]
fn three_levels_each_indexing_the_one_above() {
    let program = interface(vec![
        int("N"),
        int("C"),
        int("S"),
        index("class", "N", "C"),
        index("school_of_class", "C", "S"),
    ]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "C": {"kind": "size", "dimension": "classes"},
        "S": {"kind": "size", "dimension": "schools"},
        "class": {"kind": "index", "dataset": "main", "column": "class", "dimension": "classes"},
        "school_of_class": {"kind": "index", "dataset": "classes", "column": "school", "dimension": "schools"},
    }));
    // Given in an order that is not the resolution order: `classes` indexes
    // `schools`, so `schools` resolves first whatever the model's order.
    let bound = bind(
        &program,
        &bindings,
        vec![
            ("main", pupils()),
            ("classes", classes()),
            ("schools", schools()),
        ],
    )
    .expect("bind");
    assert_eq!(
        bound.json,
        json!({"N": 4, "C": 3, "S": 2, "class": [3, 1, 2, 3], "school_of_class": [2, 1, 2]})
    );
}

#[test]
fn three_levels_with_the_school_of_each_pupil_directly() {
    let program = interface(vec![int("N"), int("S"), index("school", "N", "S")]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "S": {"kind": "size", "dimension": "schools"},
        "school": {"kind": "index", "dataset": "main", "column": "school", "dimension": "schools"},
    }));
    let bound = bind(
        &program,
        &bindings,
        vec![("main", pupils()), ("schools", schools())],
    )
    .expect("bind");
    assert_eq!(bound.json["school"], json!([2, 2, 1, 2]));
}

// ---------------------------------------------------------------------------
// Groups that are only a column (§11, row 3): a values dimension.

#[test]
fn a_values_dimension_numbers_the_sorted_distinct_values() {
    let main = frame(
        &[1, 2, 3, 4, 5],
        vec![(
            "region",
            texts(&["south", "north", "south", "east", "north"]),
        )],
    );
    let program = interface(vec![int("N"), int("R"), index("region", "N", "R")]);
    let bindings = with(
        config(json!({
            "N": {"kind": "count", "dataset": "main"},
            "R": {"kind": "size", "dimension": "region"},
            "region": {"kind": "index", "dataset": "main", "column": "region", "dimension": "region"},
        })),
        DIMENSIONS_KEY,
        json!({"region": {"kind": "values", "dataset": "main", "column": "region"}}),
    );
    let bound = bind(&program, &bindings, vec![("main", main)]).expect("bind");
    assert_eq!(bound.json["R"], json!(3));
    assert_eq!(bound.json["region"], json!([3, 2, 3, 1, 2]));
    let region = bound.coordinates.dimension("region").expect("region");
    assert_eq!(region.kind, DimensionKind::Values);
    assert_eq!(region.labels, ["east", "north", "south"]);
    assert_eq!(region.column.as_deref(), Some("region"));
}

// ---------------------------------------------------------------------------
// Crossed factors (§11, row 4): IRT.

#[test]
fn crossed_indices_into_two_rows_dimensions() {
    let responses = frame(
        &[1, 2, 3, 4, 5, 6],
        vec![
            ("person", ints(&[7, 7, 7, 8, 8, 8])),
            ("item", ints(&[1, 2, 3, 1, 2, 3])),
            (
                "correct",
                Column::Bool(vec![
                    Some(true),
                    Some(false),
                    Some(true),
                    Some(true),
                    Some(true),
                    Some(false),
                ]),
            ),
        ],
    );
    let people = frame(&[7, 8], vec![("name", texts(&["Ada", "Bo"]))]);
    let items = frame(&[1, 2, 3], vec![("text", texts(&["a", "b", "c"]))]);
    let program = interface(vec![
        int("N"),
        int("P"),
        int("I"),
        index("person", "N", "P"),
        index("item", "N", "I"),
        Declaration::new(
            "y",
            Element::Int,
            vec![SizeExpr::var("N")],
            "array[N] int<lower=0, upper=1>",
        )
        .bounded(Some("0"), Some("1")),
    ]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "P": {"kind": "size", "dimension": "people"},
        "I": {"kind": "size", "dimension": "items"},
        "person": {"kind": "index", "dataset": "main", "column": "person", "dimension": "people"},
        "item": {"kind": "index", "dataset": "main", "column": "item", "dimension": "items"},
        "y": {"kind": "column", "dataset": "main", "column": "correct"},
    }));
    let bound = bind(
        &program,
        &bindings,
        vec![("main", responses), ("people", people), ("items", items)],
    )
    .expect("bind");
    assert_eq!(bound.json["person"], json!([1, 1, 1, 2, 2, 2]));
    assert_eq!(bound.json["item"], json!([1, 2, 3, 1, 2, 3]));
    // A boolean is 0 or 1.
    assert_eq!(bound.json["y"], json!([1, 0, 1, 1, 1, 0]));
}

// ---------------------------------------------------------------------------
// Multiple membership (§11, row 5): a weighted junction table.

#[test]
fn a_weighted_junction_indexes_both_sides() {
    let memberships = frame(
        &[1, 2, 3, 4],
        vec![
            ("pupil", ints(&[101, 101, 100, 102])),
            ("school", ints(&[10, 20, 20, 10])),
            ("weight", reals(&[0.5, 0.5, 1.0, 1.0])),
        ],
    );
    let pupils = frame(&[100, 101, 102], vec![("score", reals(&[1.0, 2.0, 3.0]))]);
    let program = interface(vec![
        int("N"),
        int("M"),
        int("S"),
        index("member_pupil", "M", "N"),
        index("member_school", "M", "S"),
        vector("weight", "M"),
    ]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "M": {"kind": "count", "dataset": "memberships"},
        "S": {"kind": "size", "dimension": "schools"},
        "member_pupil": {"kind": "index", "dataset": "memberships", "column": "pupil", "dimension": "main"},
        "member_school": {"kind": "index", "dataset": "memberships", "column": "school", "dimension": "schools"},
        "weight": {"kind": "column", "dataset": "memberships", "column": "weight"},
    }));
    let bound = bind(
        &program,
        &bindings,
        vec![
            ("main", pupils),
            ("memberships", memberships),
            ("schools", schools()),
        ],
    )
    .expect("bind");
    assert_eq!(bound.json["member_pupil"], json!([2, 2, 1, 3]));
    assert_eq!(bound.json["member_school"], json!([1, 2, 2, 1]));
    assert_eq!(bound.json["weight"], json!([0.5, 0.5, 1.0, 1.0]));
}

// ---------------------------------------------------------------------------
// Group sizes the program slices by (§11, row 6): segments.

#[test]
fn segments_sort_the_dataset_stably_by_its_index() {
    let program = interface(vec![
        int("N"),
        int("J"),
        index("county", "N", "J"),
        vector("y", "N"),
        Declaration::new(
            "start",
            Element::Int,
            vec![SizeExpr::var("J")],
            "array[J] int",
        ),
        Declaration::new(
            "size",
            Element::Int,
            vec![SizeExpr::var("J")],
            "array[J] int",
        ),
    ]);
    let mut bindings = radon_bindings();
    let bindings = bindings.as_object_mut().expect("object");
    bindings.remove("x");
    bindings.remove("u");
    bindings.insert(
        "start".into(),
        json!({"kind": "segment_start", "dataset": "main", "index": "county"}),
    );
    bindings.insert(
        "size".into(),
        json!({"kind": "segment_size", "dataset": "main", "index": "county"}),
    );
    let bound = bind(
        &program,
        &config(Json::Object(bindings.clone())),
        vec![("main", homes()), ("counties", counties())],
    )
    .expect("bind");
    // Homes 1 and 3 (Aitkin), then 2 and 5 (Becker), then 4 (Beltrami): the
    // declared order breaks the ties.
    assert_eq!(bound.json["county"], json!([1, 1, 3, 3, 4]));
    assert_eq!(bound.json["y"], json!([0.8, 0.4, 1.1, -0.1, 2.0]));
    // Anoka has no homes: it starts where the next county does, with none.
    assert_eq!(bound.json["start"], json!([1, 3, 3, 5]));
    assert_eq!(bound.json["size"], json!([2, 0, 2, 1]));
    // And the homes' own coordinates follow the sort.
    assert_eq!(
        bound.coordinates.dimension("main").expect("main").labels,
        ["1", "3", "2", "5", "4"]
    );
}

// ---------------------------------------------------------------------------
// Groups with no foreign key — a code (§11, row 8).

#[test]
fn an_index_can_match_on_a_code_rather_than_the_key() {
    let counties = frame(
        &[1, 2],
        vec![
            ("fips", texts(&["27001", "27003"])),
            ("u", reals(&[0.1, 0.2])),
        ],
    );
    let main = frame(
        &[1, 2, 3],
        vec![("fips", texts(&["27003", "27003", "27001"]))],
    );
    let program = interface(vec![int("N"), int("J"), index("county", "N", "J")]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "J": {"kind": "size", "dimension": "counties"},
        "county": {"kind": "index", "dataset": "main", "column": "fips", "dimension": "counties", "match": "fips"},
    }));
    let bound = bind(
        &program,
        &bindings,
        vec![("main", main), ("counties", counties)],
    )
    .expect("bind");
    assert_eq!(bound.json["county"], json!([2, 2, 1]));

    let twice = frame(&[1, 2], vec![("fips", texts(&["27001", "27001"]))]);
    let main = frame(&[1], vec![("fips", texts(&["27001"]))]);
    let err = refused(bind(
        &program,
        &bindings,
        vec![("main", main), ("counties", twice)],
    ));
    assert!(
        err.contains("`27001` is on two of its rows (1 and 2)"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// A model matrix, and its width.

#[test]
fn a_design_one_hots_text_records_its_columns_and_sizes_its_width() {
    let main = frame(
        &[1, 2, 3],
        vec![
            ("floor", ints(&[0, 1, 1])),
            ("region", texts(&["north", "south", "east"])),
        ],
    );
    let program = interface(vec![
        int("N"),
        int("K"),
        Declaration::new(
            "X",
            Element::Real,
            vec![SizeExpr::var("N"), SizeExpr::var("K")],
            "matrix[N, K]",
        ),
    ]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "K": {"kind": "width", "of": "X"},
        "X": {"kind": "design", "dataset": "main", "columns": ["floor", "region"]},
    }));
    let bound = bind(&program, &bindings, vec![("main", main)]).expect("bind");
    assert_eq!(bound.json["K"], json!(3));
    // `east` is the baseline: the row of zeros.
    assert_eq!(
        bound.json["X"],
        json!([[0.0, 1.0, 0.0], [1.0, 0.0, 1.0], [1.0, 0.0, 0.0]])
    );
    assert_eq!(
        bound.coordinates.designs["X"].columns,
        ["floor", "region=north", "region=south"]
    );
}

#[test]
fn columns_bind_a_matrix_and_keep_ints_as_ints() {
    let main = frame(&[1, 2], vec![("a", ints(&[1, 2])), ("b", ints(&[3, 4]))]);
    let program = interface(vec![Declaration::new(
        "M",
        Element::Int,
        vec![SizeExpr::literal(2), SizeExpr::literal(2)],
        "array[2, 2] int",
    )]);
    let bindings =
        config(json!({"M": {"kind": "columns", "dataset": "main", "columns": ["a", "b"]}}));
    let bound = bind(&program, &bindings, vec![("main", main)]).expect("bind");
    assert_eq!(bound.json["M"], json!([[1, 3], [2, 4]]));
}

// ---------------------------------------------------------------------------
// Missing data, values, and time.

#[test]
fn the_missing_data_idiom_binds_positions_and_present_values() {
    let main = frame(
        &[1, 2, 3, 4],
        vec![("y", Column::Float(vec![Some(1.5), None, Some(2.5), None]))],
    );
    let program = interface(vec![
        int("N_obs"),
        int("N_mis"),
        Declaration::new(
            "ii_obs",
            Element::Int,
            vec![SizeExpr::var("N_obs")],
            "array[N_obs] int",
        ),
        Declaration::new(
            "ii_mis",
            Element::Int,
            vec![SizeExpr::var("N_mis")],
            "array[N_mis] int",
        ),
        vector("y_obs", "N_obs"),
    ]);
    let bindings = config(json!({
        "N_obs": {"kind": "count_present", "dataset": "main", "column": "y"},
        "N_mis": {"kind": "count_absent", "dataset": "main", "column": "y"},
        "ii_obs": {"kind": "present", "dataset": "main", "column": "y"},
        "ii_mis": {"kind": "absent", "dataset": "main", "column": "y"},
        "y_obs": {"kind": "present_values", "dataset": "main", "column": "y"},
    }));
    // None of these trigger the `nulls` policy: they are how nulls are bound.
    let bound = bind(&program, &bindings, vec![("main", main)]).expect("bind");
    assert_eq!(
        bound.json,
        json!({"N_obs": 2, "N_mis": 2, "ii_obs": [1, 3], "ii_mis": [2, 4], "y_obs": [1.5, 2.5]})
    );
}

#[test]
fn a_value_is_bound_as_written() {
    let program = interface(vec![
        Declaration::new("scale", Element::Real, vec![], "real<lower=0>").bounded(Some("0"), None),
        Declaration::new("w", Element::Real, vec![SizeExpr::literal(3)], "vector[3]"),
        Declaration::new("K", Element::Int, vec![], "int"),
    ]);
    let bindings = config(json!({
        "scale": {"kind": "value", "value": 2},
        "w": {"kind": "value", "value": [1, "Inf", 0.5]},
        "K": {"kind": "value", "value": 3},
    }));
    let bound = bind(
        &program,
        &bindings,
        vec![("main", frame(&[1], vec![("a", ints(&[1]))]))],
    )
    .expect("bind");
    assert_eq!(
        bound.json,
        json!({"scale": 2.0, "w": [1.0, "Inf", 0.5], "K": 3})
    );

    let bindings = config(json!({
        "scale": {"kind": "value", "value": -1},
        "w": {"kind": "value", "value": [1, 2, 3]},
        "K": {"kind": "value", "value": 3},
    }));
    let err = refused(bind(
        &program,
        &bindings,
        vec![("main", frame(&[1], vec![("a", ints(&[1]))]))],
    ));
    assert!(
        err.contains("`scale`, declared `real<lower=0>` and bound to `value(-1)`: its value is -1.0, below its lower bound `0`"),
        "{err}"
    );
}

pub(super) fn day(text: &str) -> i64 {
    spec::parse_instant(text).expect(text)
}

#[test]
fn a_time_grid_indexes_dates_and_exposes_its_future() {
    let main = frame(
        &[1, 2, 3],
        vec![
            (
                "day",
                Column::Date(vec![
                    Some(day("2024-03-01T09:00:00Z")),
                    Some(day("2024-03-04")),
                    Some(day("2024-03-02")),
                ]),
            ),
            ("amount", reals(&[3.0, 4.0, 5.0])),
        ],
    );
    let program = interface(vec![
        int("N"),
        int("T"),
        int("H"),
        index("t_obs", "N", "T"),
        Declaration::new(
            "t",
            Element::Real,
            vec![SizeExpr::var("N")],
            "array[N] real",
        ),
    ]);
    let bindings = with(
        config(json!({
            "N": {"kind": "count", "dataset": "main"},
            "T": {"kind": "size", "dimension": "day"},
            "H": {"kind": "size", "dimension": "day.future"},
            "t_obs": {"kind": "index", "dataset": "main", "column": "day", "dimension": "day"},
            "t": {"kind": "column", "dataset": "main", "column": "day", "time": {"unit": "days", "origin": "min"}},
        })),
        DIMENSIONS_KEY,
        json!({"day": {"kind": "time_grid", "dataset": "main", "column": "day", "step": "day", "horizon": 2}}),
    );
    let bound = bind(&program, &bindings, vec![("main", main)]).expect("bind");
    assert_eq!(bound.json["T"], json!(6));
    assert_eq!(bound.json["H"], json!(2));
    assert_eq!(bound.json["t_obs"], json!([1, 4, 2]));
    assert_eq!(bound.json["t"], json!([0.0, 2.625, 0.625]));
    assert_eq!(
        bound
            .coordinates
            .dimension("day.future")
            .expect("future")
            .labels,
        ["2024-03-05", "2024-03-06"]
    );
    assert_eq!(
        bound.coordinates.dimension("day").expect("day").keys[0],
        json!("2024-03-01T00:00:00Z")
    );

    // A date with no scale is refused, naming what it needs.
    let mut no_scale = bindings.clone();
    no_scale[BINDINGS_KEY]["t"] = json!({"kind": "column", "dataset": "main", "column": "day"});
    let main = frame(
        &[1],
        vec![
            ("day", Column::Date(vec![Some(0)])),
            ("amount", reals(&[1.0])),
        ],
    );
    let err = refused(bind(&program, &no_scale, vec![("main", main)]));
    assert!(
        err.contains("a date reaches Stan as a number only with a `time` scale"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// The refusals of §10.

#[test]
fn a_size_mismatch_names_the_size_what_it_is_and_what_was_bound() {
    let mut bindings = radon_bindings();
    bindings["y"] = json!({"kind": "column", "dataset": "counties", "column": "log_uranium"});
    let err = refused(bind(
        &radon_program(),
        &config(bindings),
        vec![("main", homes()), ("counties", counties())],
    ));
    assert!(
        err.contains(
            "`y` is declared `vector[N]` with `N` = 5 (the row count of `main`), but its binding \
             `counties.log_uranium` has 4 values"
        ),
        "{err}"
    );
}

#[test]
fn a_zero_based_index_is_caught_by_its_lower_bound() {
    let main = frame(
        &[1, 2, 3],
        vec![
            // Positions a programmer computed by hand, from 0.
            ("county0", ints(&[0, 2, 1])),
            ("county", ints(&[27001, 27005, 27003])),
            ("floor", ints(&[0, 0, 1])),
            ("log_radon", reals(&[0.1, 0.2, 0.3])),
        ],
    );
    let mut bindings = radon_bindings();
    bindings["county"] = json!({"kind": "column", "dataset": "main", "column": "county0"});
    let err = refused(bind(
        &radon_program(),
        &config(bindings),
        vec![("main", main), ("counties", counties())],
    ));
    assert!(
        err.contains(
            "`county`, declared `array[N] int<lower=1, upper=J>` and bound to `main.county0`: \
             element [1] is 0 (row `1` of `main`), below its lower bound `1`"
        ),
        "{err}"
    );
}

fn homes_with_a_null_floor() -> Frame {
    frame(
        &[1, 2, 3],
        vec![
            ("county", ints(&[27001, 27005, 27001])),
            ("floor", Column::Int(vec![Some(0), None, Some(1)])),
            ("log_radon", reals(&[0.8, 1.1, 0.4])),
        ],
    )
}

#[test]
fn a_null_is_refused_by_default_and_dropped_when_the_policy_says_so() {
    let err = refused(bind(
        &radon_program(),
        &config(radon_bindings()),
        vec![
            ("main", homes_with_a_null_floor()),
            ("counties", counties()),
        ],
    ));
    assert!(
        err.contains(
            "`x`, declared `vector[N]` and bound to `main.floor`: `floor` of `main` is null in 1 \
             row (the first is row `2`); fill it in, filter it out of the dataset, or set \
             `main`'s `nulls` policy to `drop`"
        ),
        "{err}"
    );

    let dropping = with(
        config(radon_bindings()),
        POLICIES_KEY,
        json!({"main": {"nulls": "drop"}}),
    );
    let bound = bind(
        &radon_program(),
        &dropping,
        vec![
            ("main", homes_with_a_null_floor()),
            ("counties", counties()),
        ],
    )
    .expect("bind");
    // The row left before anything of `main` was counted, indexed or bound.
    assert_eq!(bound.json["N"], json!(2));
    assert_eq!(bound.json["county"], json!([1, 1]));
    assert_eq!(bound.json["y"], json!([0.8, 0.4]));
    assert_eq!(bound.report.dropped("main"), 1);
    assert_eq!(bound.report.drops[0].column, "floor");
    assert_eq!(bound.report.drops[0].first, "2");
    assert_eq!(
        bound.coordinates.dimension("main").expect("main").keys,
        [json!(1), json!(3)]
    );
}

#[test]
fn an_orphan_key_is_refused_by_default_and_dropped_when_the_policy_says_so() {
    let orphan = frame(
        &[1, 2],
        vec![
            ("county", ints(&[27001, 99999])),
            ("floor", ints(&[0, 1])),
            ("log_radon", reals(&[0.8, 1.1])),
        ],
    );
    let err = refused(bind(
        &radon_program(),
        &config(radon_bindings()),
        vec![("main", orphan.clone()), ("counties", counties())],
    ));
    assert!(
        err.contains(
            "1 row of `main` has a `county` that is not a position of `counties` (the first is \
             `99999`, on row `2`); fix it, filter it out, or set `main`'s `unknown` policy to \
             `drop`"
        ),
        "{err}"
    );
    let dropping = with(
        config(radon_bindings()),
        POLICIES_KEY,
        json!({"main": {"unknown": "drop"}}),
    );
    let bound = bind(
        &radon_program(),
        &dropping,
        vec![("main", orphan), ("counties", counties())],
    )
    .expect("bind");
    assert_eq!(bound.json["N"], json!(1));
    assert_eq!(bound.report.drops[0].dimension.as_deref(), Some("counties"));
}

#[test]
fn a_county_dropped_for_a_null_is_an_unknown_key_to_its_homes_and_both_are_said() {
    let counties = frame(
        &[27001, 27005, 27007],
        vec![(
            "log_uranium",
            Column::Float(vec![Some(0.1), None, Some(0.3)]),
        )],
    );
    let policies = json!({"counties": {"nulls": "drop"}});
    let err = refused(bind(
        &radon_program(),
        &with(config(radon_bindings()), POLICIES_KEY, policies.clone()),
        vec![("main", homes()), ("counties", counties.clone())],
    ));
    assert!(
        err.contains(
            "(the first is `27005`, on row `2`); `counties` itself dropped 1 row of `counties` \
             where `log_uranium` is null (the first is row `27005`)"
        ),
        "{err}"
    );
    let mut both = policies;
    both["main"] = json!({"unknown": "drop"});
    let bound = bind(
        &radon_program(),
        &with(config(radon_bindings()), POLICIES_KEY, both),
        vec![("main", homes()), ("counties", counties)],
    )
    .expect("bind");
    assert_eq!(bound.json["J"], json!(2));
    assert_eq!(bound.json["county"], json!([1, 1, 2]));
    assert_eq!(bound.report.drops.len(), 2);
}

#[test]
fn datasets_that_index_into_each_other_are_a_cycle() {
    let a = frame(&[1], vec![("b", ints(&[1]))]);
    let b = frame(&[1], vec![("a", ints(&[1]))]);
    let program = interface(vec![int("N"), index("ab", "N", "N"), index("ba", "N", "N")]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "ab": {"kind": "index", "dataset": "main", "column": "b", "dimension": "other"},
        "ba": {"kind": "index", "dataset": "other", "column": "a", "dimension": "main"},
    }));
    let err = refused(bind(&program, &bindings, vec![("main", a), ("other", b)]));
    assert!(
        err.contains(
            "the datasets `main`, `other` index into each other (`ab` indexes `main` into \
             `other`; `ba` indexes `other` into `main`)"
        ),
        "{err}"
    );
}

#[test]
fn the_data_values_cap_is_refused_by_name_before_anything_is_written() {
    let datasets = vec![
        ("main".to_owned(), homes()),
        ("counties".to_owned(), counties()),
    ];
    let err = bind_data(&radon_program(), &config(radon_bindings()), &datasets, 10)
        .expect_err("over the cap")
        .to_string();
    assert!(
        err.contains(
            "the bound data has 21 values, more than the 10 allowed (`--stan-max-data-values`); \
             the largest are `county` (5 values), `x` (5 values), `y` (5 values)"
        ),
        "{err}"
    );
}

#[test]
fn a_float_into_an_int_and_text_into_a_number_are_refused() {
    let mut bindings = radon_bindings();
    bindings["county"] = json!({"kind": "column", "dataset": "main", "column": "log_radon"});
    let err = refused(bind(
        &radon_program(),
        &config(bindings),
        vec![("main", homes()), ("counties", counties())],
    ));
    assert!(
        err.contains("it is an int, and its binding has real values"),
        "{err}"
    );

    let main = frame(&[1], vec![("region", texts(&["north"]))]);
    let program = interface(vec![vector("r", "N"), int("N")]);
    let bindings = config(json!({
        "N": {"kind": "count", "dataset": "main"},
        "r": {"kind": "column", "dataset": "main", "column": "region"},
    }));
    let err = refused(bind(&program, &bindings, vec![("main", main)]));
    assert!(
        err.contains("text reaches Stan only through an `index` or a `design`"),
        "{err}"
    );
}
