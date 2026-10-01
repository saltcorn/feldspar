//! The Analytics UI's endpoints (analytics TODO A1.13): datasets and
//! workspaces.
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

use crate::endpoint::{AuthRequirement, Endpoint, EndpointSet, Method, PathSpec};
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

    // What reads a dataset: what the delete warning lists.
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
