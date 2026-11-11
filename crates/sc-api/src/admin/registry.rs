//! The admin API's endpoints: the registry editor (APP_SYNC_PROPOSAL D7,
//! §7.6) — every configuration value of the installation, one JSON file of the
//! instance tree at a time.
//!
//! `sc-server`'s `handlers::registry` holds their handlers, over `sc_sync`'s
//! registry. None is offered to the MCP projection (§7.2): an agent has its
//! own tools, and a save can drop data.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // Every file of the instance tree, by path, with the kind and name the
    // editor's tree groups it by, what of it is this instance's alone, and
    // the version a save names.
    set.register(
        Endpoint::new(ids::LIST_REGISTRY, Method::Get, api().lit("registry")).output(
            TypeSchema::struct_of([StructField::new(
                "entries",
                TypeSchema::array(registry_entry_schema()),
            )]),
        ),
    );

    // One file, with its JSON (secrets as the sentinel), and the JSON Schema
    // the editor validates it against as it is typed, where the API's types
    // describe the kind: a settings section's declared keys exactly, and for
    // the kinds an admin endpoint takes whole (a view, a trigger, an
    // application…) that endpoint's types, open to keys it does not name.
    set.register(
        Endpoint::new(
            ids::GET_REGISTRY_ENTITY,
            Method::Get,
            api().lit("registry").lit("entity"),
        )
        .query([QueryParam::new("path", ValueType::Text).required()])
        .output(TypeSchema::struct_of(registry_entity_fields())),
    );

    // The review of a save: the instance tree with this one file replaced
    // (`value`), added (no `version`) or removed (`value` null), planned as a
    // deploy is. `value` in the answer is what to confirm: what was sent,
    // with an id given to a new entity that had none.
    set.register(
        Endpoint::new(
            ids::PLAN_REGISTRY_SAVE,
            Method::Post,
            api().lit("registry").lit("plan"),
        )
        .input(TypeSchema::struct_of(registry_edit_fields()))
        .output(TypeSchema::struct_of([
            StructField::new("operations", TypeSchema::array(planned_op_schema())),
            StructField::new("blockers", TypeSchema::array(blocker_schema())),
            StructField::new("needs_secrets", TypeSchema::array(TypeSchema::text())),
            StructField::new("needs_credentials", TypeSchema::array(TypeSchema::text())),
            StructField::new("digest", TypeSchema::text()),
            StructField::new("before", TypeSchema::optional(TypeSchema::json())),
            StructField::new("value", TypeSchema::optional(TypeSchema::json())),
        ])),
    );

    // Apply the reviewed save: refused as stale when the file or anything
    // the plan was made from changed since, and refused while it has a
    // blocker. `ticked` is every operation to apply; the rest are deferred.
    // The answer is the receipt, and what is stored now with where it
    // differs from what was sent.
    set.register(
        Endpoint::new(
            ids::REGISTRY_SAVE,
            Method::Post,
            api().lit("registry").lit("save"),
        )
        .input(TypeSchema::struct_of(
            registry_edit_fields().into_iter().chain([
                StructField::new("plan_digest", TypeSchema::text()),
                StructField::new("ticked", TypeSchema::array(TypeSchema::text())),
            ]),
        ))
        .output(registry_receipt_schema()),
    );
}

/// What a save is of: the file, its new JSON (null deletes it), and the
/// version the editor opened (null for a new file).
fn registry_edit_fields() -> Vec<StructField> {
    vec![
        StructField::new("path", TypeSchema::text()),
        StructField::new("value", TypeSchema::optional(TypeSchema::json())),
        StructField::new("version", TypeSchema::optional(TypeSchema::text())),
    ]
}

fn registry_entry_fields() -> Vec<StructField> {
    vec![
        StructField::new("path", TypeSchema::text()),
        // `table`, `view`, `setting`, `build_host`, …
        StructField::new("kind", TypeSchema::text()),
        StructField::new("name", TypeSchema::text()),
        // The application a view, page, library item or translation is in.
        StructField::new("application", TypeSchema::optional(TypeSchema::text())),
        // `{ scope: "whole" }`, or `{ scope: "keys", keys: [JSON pointers] }`;
        // null when all of it is portable.
        StructField::new(
            "instance_only",
            TypeSchema::optional(TypeSchema::struct_of([
                StructField::new("scope", TypeSchema::text()),
                StructField::new(
                    "keys",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
            ])),
        ),
        StructField::new("version", TypeSchema::text()),
    ]
}

fn registry_entry_schema() -> TypeSchema {
    TypeSchema::struct_of(registry_entry_fields())
}

fn registry_entity_fields() -> Vec<StructField> {
    registry_entry_fields()
        .into_iter()
        .chain([
            StructField::new("value", TypeSchema::json()),
            StructField::new("schema", TypeSchema::optional(TypeSchema::json())),
            // With a sync link, what the base and the branch tip hold for it,
            // for the diff tabs and, in a conflict, the three-way merge;
            // null when the installation is not linked.
            StructField::new(
                "sync",
                TypeSchema::optional(TypeSchema::struct_of([
                    StructField::new("class", TypeSchema::optional(TypeSchema::text())),
                    StructField::new("key", TypeSchema::optional(TypeSchema::text())),
                    StructField::new("base", TypeSchema::optional(TypeSchema::json())),
                    StructField::new("tip", TypeSchema::optional(TypeSchema::json())),
                ])),
            ),
        ])
        .collect()
}

/// One planned operation (`sc_sync::PlannedOp`).
pub(super) fn planned_op_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::text()),
        StructField::new("step", TypeSchema::text()),
        // `safe`, `metadata_delete`, `can_fail`, `data_destructive`,
        // `unsupported`.
        StructField::new("class", TypeSchema::text()),
        StructField::new("ticked", TypeSchema::bool()),
        StructField::new("kind", TypeSchema::text()),
        StructField::new("entity", TypeSchema::text()),
        StructField::new("action", TypeSchema::text()),
        StructField::new("summary", TypeSchema::text()),
    ])
}

pub(super) fn blocker_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("op", TypeSchema::optional(TypeSchema::text())),
        StructField::new("entity", TypeSchema::optional(TypeSchema::text())),
        StructField::new("message", TypeSchema::text()),
    ])
}

pub(super) fn failure_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("op", TypeSchema::text()),
        StructField::new("summary", TypeSchema::text()),
        StructField::new("message", TypeSchema::text()),
    ])
}

/// What a deploy, a discard or a save applied (`sc_sync::Receipt`).
pub(super) fn receipt_fields() -> Vec<StructField> {
    let texts = || TypeSchema::array(TypeSchema::text());
    vec![
        StructField::new("id", TypeSchema::uuid()),
        // `converged`, `drifted` or `failed`.
        StructField::new("result", TypeSchema::text()),
        StructField::new("applied", texts()),
        StructField::new("deferred", texts()),
        StructField::new("not_reached", texts()),
        StructField::new("failed", TypeSchema::optional(failure_schema())),
        StructField::new("needs_secret", TypeSchema::array(failure_schema())),
        StructField::new("notes", texts()),
        StructField::new("drift", texts()),
        // The safety backup's file, when the plan dropped data.
        StructField::new("backups", texts()),
        StructField::new("needs_secrets", texts()),
        StructField::new("needs_credentials", texts()),
    ]
}

/// A save's receipt (`sc_sync::EditReceipt`).
fn registry_receipt_schema() -> TypeSchema {
    TypeSchema::struct_of(receipt_fields().into_iter().chain([
        // Where the entity is now (a rename moves it); null once deleted.
        StructField::new("path", TypeSchema::optional(TypeSchema::text())),
        StructField::new("stored", TypeSchema::optional(TypeSchema::json())),
        StructField::new("stored_version", TypeSchema::optional(TypeSchema::text())),
        // Where what is stored differs from what was sent.
        StructField::new(
            "changes",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("path", TypeSchema::text()),
                StructField::new("before", TypeSchema::optional(TypeSchema::json())),
                StructField::new("after", TypeSchema::optional(TypeSchema::json())),
            ])),
        ),
    ]))
}
