//! The Analytics UI's endpoints (analytics TODO A1.13, A2.6, A2.8, A3.3, A4.2, A5.5–A5.12):
//! datasets, plots, panels, map layers, the Map workspace's attribute table, selection and
//! toolbox, a model's outputs and workspaces.
//!
//! Admin-only in this milestone, like everything else under `/api`: A9 is
//! where a restricted application's users reach a subset of them under their
//! own authority.
//!
//! **A dataset is sent whole to be read.** The editor previews an operation
//! before it is saved — the spreadsheet shows the stage after the operation
//! being edited — so the read endpoints (`readDatasetStage`, `datasetShapes`,
//! `validateDatasetOperation`, `datasetColumnValues`) take the definition in
//! the body rather than an id, and it may be one that is not stored at all.
//! What is stored changes only through `createDataset`, `updateDataset`,
//! `cloneDataset` and `deleteDataset`.

use crate::endpoint::{AuthRequirement, Endpoint, EndpointSet, Method, PathSpec, QueryParam};
use crate::schema::{StructField, TypeSchema, ValueType};

fn api() -> PathSpec {
    PathSpec::root().lit(crate::admin::ADMIN_API_PREFIX)
}

/// Register the Analytics UI's endpoints on `set`.
pub(crate) fn register(set: &mut EndpointSet) {
    // --- datasets ------------------------------------------------------------

    // Every stored dataset, with whether it reads: the Analytics UI's front page,
    // the model form's picker and the explorer's drop-down.
    set.register(
        Endpoint::new("listDatasets", Method::Get, api().lit("datasets"))
            .output(TypeSchema::array(dataset_summary_schema()))
            .auth(AuthRequirement::admin()),
    );

    // One dataset, compiled: the report on its base and each operation.
    set.register(
        Endpoint::new(
            "getDataset",
            Method::Get,
            api().lit("datasets").param("id", ValueType::Uuid),
        )
        .output(dataset_detail_schema())
        .auth(AuthRequirement::admin()),
    );

    // A new dataset: a name, a base, and (usually none yet) operations.
    set.register(
        Endpoint::new("createDataset", Method::Post, api().lit("datasets"))
            .input(dataset_input_schema())
            .output(dataset_detail_schema())
            .auth(AuthRequirement::admin()),
    );

    // Replace a dataset's name, description and operations. The base is sent
    // too and must be the one it has: a base is never changed.
    set.register(
        Endpoint::new(
            "updateDataset",
            Method::Put,
            api().lit("datasets").param("id", ValueType::Uuid),
        )
        .input(dataset_input_schema())
        .output(dataset_detail_schema())
        .auth(AuthRequirement::admin()),
    );

    // Refused while other datasets read it; a model that uses it is listed by
    // `datasetUsage` for the warning, and is left listed with its error.
    set.register(
        Endpoint::new(
            "deleteDataset",
            Method::Delete,
            api().lit("datasets").param("id", ValueType::Uuid),
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "cloneDataset",
            Method::Post,
            api()
                .lit("datasets")
                .param("id", ValueType::Uuid)
                .lit("clone"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "name",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(dataset_detail_schema())
        .auth(AuthRequirement::admin()),
    );

    // What reads a dataset: what the delete warning lists. `workspaces` is
    // the usage index's answer (A4.2): the workspaces whose panels read it,
    // with how many, and the explorers that have it chosen (`panels` 0).
    set.register(
        Endpoint::new(
            "datasetUsage",
            Method::Get,
            api()
                .lit("datasets")
                .param("id", ValueType::Uuid)
                .lit("usage"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("datasets", TypeSchema::array(named_schema())),
            StructField::new("models", TypeSchema::array(named_schema())),
            StructField::new("workspaces", TypeSchema::array(workspace_use_schema())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The shapes of every stage of a definition — stored or being edited —
    // and what its formulas may name: the columns, the fields of the tables
    // each foreign key reaches (`neighbourhoodⱵname`), and the child tables
    // while rows are rows of a table (`viewingsↃhouse`).
    set.register(
        Endpoint::new(
            "datasetShapes",
            Method::Post,
            api().lit("datasets").lit("shapes"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "dataset",
            TypeSchema::json(),
        )]))
        .output(dataset_report_schema())
        .auth(AuthRequirement::admin()),
    );

    // Whether one operation compiles at a position of a definition, and the
    // columns after it — the operation form's check as the admin types.
    set.register(
        Endpoint::new(
            "validateDatasetOperation",
            Method::Post,
            api().lit("datasets").lit("validate"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("dataset", TypeSchema::json()),
            // Where it goes: 0 before the first operation; the number of
            // operations to append it.
            StructField::new("position", TypeSchema::int()),
            // Whether it replaces the operation at `position` rather than
            // being inserted there.
            StructField::new("replace", TypeSchema::optional(TypeSchema::bool())),
            StructField::new("operation", TypeSchema::json()),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            StructField::new("shape", TypeSchema::optional(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // A page of the stage after the first `upto` operations (all of them when
    // absent), with the column types and the total.
    set.register(
        Endpoint::new(
            "readDatasetStage",
            Method::Post,
            api().lit("datasets").lit("stage"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("dataset", TypeSchema::json()),
            StructField::new("upto", TypeSchema::optional(TypeSchema::int())),
            StructField::new("offset", TypeSchema::optional(TypeSchema::int())),
            StructField::new("limit", TypeSchema::optional(TypeSchema::int())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("columns", TypeSchema::array(TypeSchema::json())),
            StructField::new("grain", TypeSchema::json()),
            StructField::new("rows", TypeSchema::array(TypeSchema::json())),
            StructField::new("total", TypeSchema::int()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The distinct values of a column at a stage, most frequent first: what a
    // Split's new columns are pre-filled from.
    set.register(
        Endpoint::new(
            "datasetColumnValues",
            Method::Post,
            api().lit("datasets").lit("values"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("dataset", TypeSchema::json()),
            StructField::new("upto", TypeSchema::optional(TypeSchema::int())),
            StructField::new("column", TypeSchema::text()),
            StructField::new("limit", TypeSchema::optional(TypeSchema::int())),
        ]))
        .output(TypeSchema::array(TypeSchema::json()))
        .auth(AuthRequirement::admin()),
    );

    // The tables a dataset can be based on or combine with, and their
    // columns: the base picker's list.
    set.register(
        Endpoint::new(
            "listDatasetTables",
            Method::Get,
            api().lit("datasets").lit("tables"),
        )
        .output(TypeSchema::array(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("columns", TypeSchema::array(TypeSchema::json())),
            StructField::new("primary_key", TypeSchema::optional(TypeSchema::text())),
        ])))
        .auth(AuthRequirement::admin()),
    );

    // --- plots ---------------------------------------------------------------

    // A plot's data (A2.6): each layer's rows after its stat — bins, boxes,
    // curves, or a sample of the rows — and the domains each channel spans.
    // A spec that cannot be drawn is not an error of the request: it answers
    // `error` (the first reason, as a sentence) and `problems` (all of them),
    // which the explorer shows in place of the plot.
    set.register(
        Endpoint::new("renderPlot", Method::Post, api().lit("plots").lit("render"))
            .input(TypeSchema::struct_of([StructField::new(
                "spec",
                TypeSchema::json(),
            )]))
            .output(TypeSchema::struct_of([
                StructField::new("error", TypeSchema::optional(TypeSchema::text())),
                StructField::new(
                    "problems",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new(
                    "layers",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
                ),
                StructField::new("domains", TypeSchema::optional(TypeSchema::json())),
                StructField::new("facets", TypeSchema::optional(TypeSchema::json())),
                StructField::new("bins", TypeSchema::optional(TypeSchema::json())),
                StructField::new(
                    "warnings",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // A summary table's data (A2.8): the cells for each combination of the
    // row and column dimensions' values, and the totals. Answers `error` and
    // `problems`, as `renderPlot` does, for a spec that cannot be made.
    set.register(
        Endpoint::new("renderTable", Method::Post, api().lit("plots").lit("table"))
            .input(TypeSchema::struct_of([StructField::new(
                "spec",
                TypeSchema::json(),
            )]))
            .output(TypeSchema::struct_of([
                StructField::new("error", TypeSchema::optional(TypeSchema::text())),
                StructField::new(
                    "problems",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new(
                    "rows",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new(
                    "columns",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new(
                    "cells",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new("body", TypeSchema::optional(TypeSchema::json())),
                StructField::new("row_totals", TypeSchema::optional(TypeSchema::json())),
                StructField::new("column_totals", TypeSchema::optional(TypeSchema::json())),
                StructField::new("grand_total", TypeSchema::optional(TypeSchema::json())),
                StructField::new("bins", TypeSchema::optional(TypeSchema::json())),
                StructField::new("total", TypeSchema::optional(TypeSchema::int())),
                StructField::new("truncated", TypeSchema::optional(TypeSchema::bool())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // The gallery's items, the map shown disabled until A5. `reshapes` marks
    // the presets that build their spec from what is dropped every time
    // (scatterplot matrix, parallel coordinates, correlation heatmap, mosaic).
    set.register(
        Endpoint::new(
            "plotGallery",
            Method::Get,
            api().lit("plots").lit("gallery"),
        )
        .output(TypeSchema::array(TypeSchema::struct_of([
            StructField::new("preset", TypeSchema::text()),
            StructField::new("label", TypeSchema::text()),
            StructField::new("available", TypeSchema::bool()),
            StructField::new("reshapes", TypeSchema::bool()),
            StructField::new("arrives_in", TypeSchema::optional(TypeSchema::text())),
        ])))
        .auth(AuthRequirement::admin()),
    );

    // The hypothesis tests for the Y, X and Wrap drop zones (A2.12–A2.14):
    // chosen from the columns' types, computed over the dataset (sufficient
    // statistics in SQL, the rank tests on a sample), repeated for each value
    // of Wrap. Answers `design` and one section per Wrap value, each with its
    // tests, assumption checks and the test its sentence reports; or `error`
    // (and `problems`), as `renderPlot` does, when the roles have no test.
    set.register(
        Endpoint::new("runTests", Method::Post, api().lit("plots").lit("tests"))
            .input(TypeSchema::struct_of([StructField::new(
                "spec",
                TypeSchema::json(),
            )]))
            .output(TypeSchema::struct_of([
                StructField::new("error", TypeSchema::optional(TypeSchema::text())),
                StructField::new(
                    "problems",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new("design", TypeSchema::optional(TypeSchema::text())),
                StructField::new(
                    "y",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new("x", TypeSchema::optional(TypeSchema::text())),
                StructField::new("by", TypeSchema::optional(TypeSchema::text())),
                StructField::new(
                    "mu",
                    TypeSchema::optional(TypeSchema::Value(ValueType::Float)),
                ),
                StructField::new(
                    "level",
                    TypeSchema::optional(TypeSchema::Value(ValueType::Float)),
                ),
                StructField::new(
                    "sections",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
                ),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // The spec for what is on the drop zones (A2.2): by a gallery preset, which
    // fills the zones it needs, or by the column types, drawn as `mark` when
    // the mark palette chose one. Answers the spec and the drop zones as the
    // preset left them, or `error` when nothing can be drawn yet.
    set.register(
        Endpoint::new(
            "suggestPlot",
            Method::Post,
            api().lit("plots").lit("suggest"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("dataset", TypeSchema::uuid()),
            StructField::new("assignment", TypeSchema::optional(TypeSchema::json())),
            StructField::new("preset", TypeSchema::optional(TypeSchema::text())),
            StructField::new("mark", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("spec", TypeSchema::optional(TypeSchema::json())),
            StructField::new("assignment", TypeSchema::optional(TypeSchema::json())),
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- map layers (A5.5) -----------------------------------------------------

    // A map layer's data: a stored dataset's rows as features, with their
    // geometry from a column, from longitude and latitude columns or along a
    // foreign key (`layer` is `{ dataset, geometry, properties?, filter? }`).
    // A small layer answers `delivery: "geojson"` and the FeatureCollection in
    // `data`; a large one `delivery: "tiles"` and the URL template of its
    // vector tiles in `tiles` (MapLibre fills in `{z}`, `{x}` and `{y}`), with
    // the layer inside each tile in `source_layer`. Both say how many features
    // there are and their `bounds` (`[west, south, east, north]`). A layer that
    // cannot be drawn answers `delivery: "none"` and the sentence in `error`.
    set.register(
        Endpoint::new("layerData", Method::Post, api().lit("layers"))
            .input(TypeSchema::struct_of([StructField::new(
                "layer",
                TypeSchema::json(),
            )]))
            .output(TypeSchema::struct_of([
                StructField::new("delivery", TypeSchema::text()),
                StructField::new("error", TypeSchema::optional(TypeSchema::text())),
                StructField::new("count", TypeSchema::optional(TypeSchema::int())),
                StructField::new("vertices", TypeSchema::optional(TypeSchema::int())),
                StructField::new(
                    "bounds",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::Value(ValueType::Float))),
                ),
                StructField::new(
                    "geometry",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new(
                    "properties",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
                ),
                StructField::new("data", TypeSchema::optional(TypeSchema::json())),
                StructField::new("tiles", TypeSchema::optional(TypeSchema::text())),
                StructField::new("source_layer", TypeSchema::optional(TypeSchema::text())),
                StructField::new("keyed", TypeSchema::optional(TypeSchema::bool())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // One Mapbox vector tile of a layer (`application/vnd.mapbox-vector-tile`),
    // the layer given as `layerData`'s `layer`, JSON in the query string — the
    // URL `layerData`'s `tiles` is the template of. Empty where the layer has
    // nothing; a layer that cannot be drawn, or a tile outside the grid, is
    // refused with the sentence.
    set.register(
        Endpoint::new(
            "layerTile",
            Method::Get,
            api()
                .lit("layers")
                .lit("tiles")
                .param("z", ValueType::Int)
                .param("x", ValueType::Int)
                .param("y", ValueType::Int),
        )
        .query([QueryParam::new("layer", ValueType::Text).required()])
        .binary_output()
        .auth(AuthRequirement::admin()),
    );

    // A layer's rows for the Map workspace's attribute table (A5.10): the
    // first `limit` (at most 5,000), the geometry left out, each with its
    // feature's id in `ids` — the row's key while rows are a table's, else its
    // place in the dataset's order — so a selection is shared by the table and
    // the map. Sorted by `sort` (`{ formula, descending }`) while rows are a
    // table's (`sorted`); otherwise in the dataset's order, for the table to
    // sort. A layer that cannot be read answers `error`.
    set.register(
        Endpoint::new("layerRows", Method::Post, api().lit("layers").lit("rows"))
            .input(TypeSchema::struct_of([
                StructField::new("layer", TypeSchema::json()),
                StructField::new("sort", TypeSchema::optional(TypeSchema::json())),
                StructField::new("limit", TypeSchema::optional(TypeSchema::int())),
            ]))
            .output(TypeSchema::struct_of([
                StructField::new("error", TypeSchema::optional(TypeSchema::text())),
                StructField::new(
                    "columns",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
                ),
                StructField::new(
                    "rows",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
                ),
                StructField::new(
                    "ids",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
                ),
                StructField::new("total", TypeSchema::optional(TypeSchema::int())),
                StructField::new("keyed", TypeSchema::optional(TypeSchema::bool())),
                StructField::new("sorted", TypeSchema::optional(TypeSchema::bool())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // The features of a layer a selection finds (A5.10), `by` one of
    // `{ by: "condition", formula }`, `{ by: "shape", geometry }` (a GeoJSON
    // polygon: a lasso), `{ by: "near_point", longitude, latitude, distance }`
    // and `{ by: "near_features", layer, ids, distance }` (metres). Answers
    // their ids, how many match, and the condition they were found by — what
    // `saveSelection` filters by — or `error`, the sentence.
    set.register(
        Endpoint::new(
            "selectFeatures",
            Method::Post,
            api().lit("layers").lit("select"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("layer", TypeSchema::json()),
            StructField::new("by", TypeSchema::json()),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            StructField::new(
                "ids",
                TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
            ),
            StructField::new("count", TypeSchema::optional(TypeSchema::int())),
            StructField::new("truncated", TypeSchema::optional(TypeSchema::bool())),
            StructField::new("condition", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Save selection as dataset** (A5.10): a new dataset `name` whose base
    // is the layer's dataset, followed by the layer's filter and a Filter —
    // `condition` when the selection was made by one, else the clicked
    // features `ids` by their rows' keys or group keys. Answers the dataset as
    // `getDataset` does.
    set.register(
        Endpoint::new(
            "saveSelection",
            Method::Post,
            api().lit("layers").lit("selection"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("layer", TypeSchema::json()),
            StructField::new("name", TypeSchema::text()),
            StructField::new(
                "ids",
                TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
            ),
            StructField::new("condition", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(dataset_detail_schema())
        .auth(AuthRequirement::admin()),
    );

    // --- maps (A5.6, A5.7, A5.11, A5.12) ------------------------------------------

    // The base maps a map is drawn over (Settings → Maps): the MapLibre style
    // for a light page and for a dark one, each absent for no base map; and
    // `hosts`, every origin the Analytics UI's policy lets a map load from —
    // what a reference layer's service must be on (A5.11).
    set.register(
        Endpoint::new(
            "mapSettings",
            Method::Get,
            api().lit("maps").lit("settings"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("style", TypeSchema::optional(TypeSchema::text())),
            StructField::new("style_dark", TypeSchema::optional(TypeSchema::text())),
            StructField::new("hosts", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Let a map load from one more origin (A5.11): adds it to Settings →
    // Maps' further hosts, so the Analytics UI's policy names it from the
    // next page load on. The origin of `url` is taken, and anything that is
    // not one refused by name. Answers `mapSettings`' answer.
    set.register(
        Endpoint::new("allowMapHost", Method::Post, api().lit("maps").lit("hosts"))
            .input(TypeSchema::struct_of([StructField::new(
                "url",
                TypeSchema::text(),
            )]))
            .output(TypeSchema::struct_of([
                StructField::new("style", TypeSchema::optional(TypeSchema::text())),
                StructField::new("style_dark", TypeSchema::optional(TypeSchema::text())),
                StructField::new("hosts", TypeSchema::array(TypeSchema::text())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // The Map workspace's toolbox (A5.12): every tool, built in or a
    // plugin's, with its group and the form it asks (`params`, each
    // `{ name, label, kind: "layer" | "column" | "number" | "choice" | "text",
    // … }`).
    set.register(
        Endpoint::new("listMapTools", Method::Get, api().lit("maps").lit("tools"))
            .output(TypeSchema::array(TypeSchema::struct_of([
                StructField::new("id", TypeSchema::text()),
                StructField::new("group", TypeSchema::text()),
                StructField::new("label", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
                StructField::new("params", TypeSchema::array(TypeSchema::json())),
                StructField::new("module", TypeSchema::optional(TypeSchema::text())),
            ])))
            .auth(AuthRequirement::admin()),
    );

    // Run a tool (A5.12) on its form's answers (`params`, a layer's answer a
    // map layer): its dataset is checked, named (`name`, or the tool's own
    // name made unique), stored, and answered as `getDataset` does, with the
    // layer that shows it. A tool whose answers make no dataset that reads is
    // refused with the sentence, and nothing is stored.
    set.register(
        Endpoint::new(
            "runMapTool",
            Method::Post,
            api().lit("maps").lit("tools").lit("run"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("tool", TypeSchema::text()),
            StructField::new("params", TypeSchema::json()),
            StructField::new("name", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("dataset", TypeSchema::json()),
            StructField::new("report", dataset_report_schema()),
            StructField::new("layer", TypeSchema::json()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The map the explorer draws for a dataset (A5.7): its rows over a base
    // map, the geometry from `geometry` when it is one of the dataset's
    // sources and from the first when not, and Color, Size, Shape and Label
    // from the drop zones in `assignment`. Answers the map spec, every way the
    // dataset's rows can be put on a map (`sources`, each `{ source, label }`:
    // a geometry column, longitude and latitude columns, a key to a table
    // with a geometry column), or `error` when nothing can be drawn.
    set.register(
        Endpoint::new("suggestMap", Method::Post, api().lit("maps").lit("suggest"))
            .input(TypeSchema::struct_of([
                StructField::new("dataset", TypeSchema::uuid()),
                StructField::new("assignment", TypeSchema::optional(TypeSchema::json())),
                StructField::new("geometry", TypeSchema::optional(TypeSchema::json())),
            ]))
            .output(TypeSchema::struct_of([
                StructField::new("spec", TypeSchema::optional(TypeSchema::json())),
                StructField::new("sources", TypeSchema::array(TypeSchema::json())),
                StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // A map spec drawn (A5.6): for each layer, its features as `layerData`
    // answers them (GeoJSON, or the URL template of its tiles, or `delivery:
    // "none"` and the sentence) in `data`, the request they were read by in
    // `layer`, what each encoded column spans in `domains` (by channel:
    // `color`, `size`, `shape`), over every feature, and graduated colours'
    // breaks in `classes` (A5.9).
    set.register(
        Endpoint::new("renderMap", Method::Post, api().lit("maps").lit("render"))
            .input(TypeSchema::struct_of([StructField::new(
                "spec",
                TypeSchema::json(),
            )]))
            .output(TypeSchema::struct_of([StructField::new(
                "layers",
                TypeSchema::array(TypeSchema::json()),
            )]))
            .auth(AuthRequirement::admin()),
    );

    // --- panels (A4.2) ---------------------------------------------------------

    // A panel drawn from what it is stored as, now: a plot's data, a summary
    // table's, a test result's tests and plot, a fit's table. A panel whose
    // dataset or fit has been deleted answers `error`, a sentence saying so,
    // rather than failing; a plot that cannot be drawn answers its refusal in
    // `plot`, as `renderPlot` does. A text panel answers nothing to draw: the
    // browser renders its Markdown.
    set.register(
        Endpoint::new(
            "renderPanel",
            Method::Post,
            api().lit("panels").lit("render"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "panel",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::struct_of([
            StructField::new("kind", TypeSchema::text()),
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            StructField::new("plot", TypeSchema::optional(TypeSchema::json())),
            StructField::new("table", TypeSchema::optional(TypeSchema::json())),
            StructField::new("tests", TypeSchema::optional(TypeSchema::json())),
            StructField::new("output", TypeSchema::optional(TypeSchema::json())),
            // A map panel's layers, as `renderMap` answers them (A5.13).
            StructField::new("map", TypeSchema::optional(TypeSchema::json())),
            // A plot's foreign key columns: categories, though numbers.
            StructField::new(
                "categorical",
                TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
            ),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- the model editor -----------------------------------------------------

    // What a fit shows (A3.1–A3.3): its outputs as its provider declared them,
    // in order — tables filled from the fit, plots as specs over the fit's
    // output data, each drawn as `renderPlot` draws it. `fit` is the fit to
    // show; without it, the model's active fit, else its newest fitted one.
    // Optional plots ("More plots") come with their spec but are drawn only
    // when named in `include` (comma-separated), so opening a model draws what
    // is on the screen; `renderPlot` draws one later from its spec. `fit` is
    // null, with no outputs, for a model that has never been fitted.
    //
    // The rest of the model editor's needs are the model endpoints:
    // `listModelInstances` (each fit with `dataset_changed`), `fitModel`,
    // `cancelModelFit` (any fit, stopped between its stages unless its provider
    // can kill it sooner), `patchModelViewState` and `cloneModel`. A fit's
    // progress is **pushed**: `GET /api/model-instances/{id}/progress` is a
    // WebSocket, beside this set for the reason a stream's Observe socket is —
    // a socket has no shape in an `EndpointSet` (§13.1). It sends
    // `{"type":"progress","status","progress"}` whenever the fit's row
    // changes, then `{"type":"finished","status","error"}`, and closes.
    set.register(
        Endpoint::new(
            "getModelOutputs",
            Method::Get,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("outputs"),
        )
        .query([
            QueryParam::new("fit", ValueType::Uuid),
            QueryParam::new("include", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("model", TypeSchema::uuid()),
            // The fit shown: its id, name, status, when, whether active, and
            // `dataset_changed`.
            StructField::new("fit", TypeSchema::optional(TypeSchema::json())),
            // `{ name, label, optional, kind: "table" | "plot", table?, spec?,
            // plot?, error? }`, `plot` being `renderPlot`'s answer.
            StructField::new("outputs", TypeSchema::array(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // --- workspaces ----------------------------------------------------------

    // Every kind, with whether it is here yet and, when not, the milestone
    // that brings it — what the create dialog lists, disabled or not.
    set.register(
        Endpoint::new(
            "listWorkspaceKinds",
            Method::Get,
            api().lit("workspace-kinds"),
        )
        .output(TypeSchema::array(TypeSchema::struct_of([
            StructField::new("kind", TypeSchema::text()),
            StructField::new("label", TypeSchema::text()),
            StructField::new("available", TypeSchema::bool()),
            StructField::new("arrives_in", TypeSchema::optional(TypeSchema::text())),
        ])))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("listWorkspaces", Method::Get, api().lit("workspaces"))
            .output(TypeSchema::array(workspace_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getWorkspace",
            Method::Get,
            api().lit("workspaces").param("id", ValueType::Uuid),
        )
        .output(workspace_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createWorkspace", Method::Post, api().lit("workspaces"))
            .input(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("kind", TypeSchema::text()),
            ]))
            .output(workspace_schema())
            .auth(AuthRequirement::admin()),
    );

    // Rename. The state has its own endpoint, because it is saved as it
    // changes and a rename must not race a stale copy of it.
    set.register(
        Endpoint::new(
            "updateWorkspace",
            Method::Put,
            api().lit("workspaces").param("id", ValueType::Uuid),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "name",
            TypeSchema::text(),
        )]))
        .output(workspace_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "saveWorkspaceState",
            Method::Put,
            api()
                .lit("workspaces")
                .param("id", ValueType::Uuid)
                .lit("state"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "state",
            TypeSchema::json(),
        )]))
        .output(workspace_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteWorkspace",
            Method::Delete,
            api().lit("workspaces").param("id", ValueType::Uuid),
        )
        .auth(AuthRequirement::admin()),
    );
}

/// A stored dataset as a list shows it.
fn dataset_summary_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("base", TypeSchema::json()),
        // The table its rows start from, following datasets based on datasets.
        StructField::new("table", TypeSchema::text()),
        StructField::new("operations", TypeSchema::int()),
        // The columns of its last stage, when it reads.
        StructField::new("columns", TypeSchema::array(TypeSchema::json())),
        // The first thing that stops it reading, as a sentence.
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("grain", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// A dataset's definition and its compile report.
fn dataset_detail_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("dataset", TypeSchema::json()),
        StructField::new("report", dataset_report_schema()),
    ])
}

/// What a create or an update sends.
fn dataset_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("base", TypeSchema::json()),
        StructField::new("operations", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// A compiled definition: the base, each operation, and what formulas may
/// name at each stage.
fn dataset_report_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("base", TypeSchema::json()),
        StructField::new("operations", TypeSchema::array(TypeSchema::json())),
        // For each table a foreign key of some stage reaches, its columns.
        StructField::new("tables", TypeSchema::json()),
        // For each table, the child tables whose keys point at it.
        StructField::new("children", TypeSchema::json()),
    ])
}

fn named_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
    ])
}

/// A workspace that uses a dataset or a model, as the usage index (A4.2)
/// answers it.
pub(crate) fn workspace_use_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("kind", TypeSchema::text()),
        StructField::new("panels", TypeSchema::int()),
    ])
}

fn workspace_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("kind", TypeSchema::text()),
        StructField::new("state", TypeSchema::json()),
        StructField::new("created_by", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("updated_at", TypeSchema::timestamp()),
    ])
}
