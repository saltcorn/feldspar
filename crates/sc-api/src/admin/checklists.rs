//! The admin API's endpoints: checklists.
//!
//! Their handlers are registered by `sc-server`'s `checklists` module.

use super::*;

/// Declare this domain's endpoints on `set`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(set: &mut EndpointSet) {
    // What "working" means for an application (or for the data, with no
    // application): checklists, their steps, their runs, and the agents'
    // proposals waiting for a person. A person may do anything here; the agent
    // tools go through the same rules with the agent's grant (§4). Steps are
    // never updated in place, so an edit answers the new version and a delete
    // writes a tombstone.
    let checklist_id = || api().lit("checklists").param("id", ValueType::Uuid);
    let step_id = || api().lit("checklist-steps").param("id", ValueType::Uuid);
    let proposal_id = || {
        api()
            .lit("checklist-proposals")
            .param("id", ValueType::Uuid)
    };
    // `application` is an application's id, or `none` for the checklists with
    // no application; absent lists them all.
    set.register(
        Endpoint::new(ids::LIST_CHECKLISTS, Method::Get, api().lit("checklists"))
            .query([QueryParam::new("application", ValueType::Text)])
            .output(TypeSchema::array(checklist_summary_schema())),
    );
    set.register(
        Endpoint::new(ids::CREATE_CHECKLIST, Method::Post, api().lit("checklists"))
            .input(checklist_input_schema(true))
            .output(checklist_detail_schema()),
    );
    set.register(
        Endpoint::new(ids::GET_CHECKLIST, Method::Get, checklist_id())
            .output(checklist_detail_schema()),
    );
    set.register(
        Endpoint::new(ids::UPDATE_CHECKLIST, Method::Put, checklist_id())
            .input(checklist_input_schema(false))
            .output(checklist_detail_schema()),
    );
    set.register(
        Endpoint::new(ids::DELETE_CHECKLIST, Method::Delete, checklist_id()).output(
            TypeSchema::struct_of([StructField::new("deleted", TypeSchema::bool())]),
        ),
    );
    // A new step at the end, or before the step `before` names.
    set.register(
        Endpoint::new(
            ids::ADD_CHECKLIST_STEP,
            Method::Post,
            checklist_id().lit("steps"),
        )
        .input(TypeSchema::struct_of(
            checklist_step_input_fields()
                .into_iter()
                .chain([StructField::new(
                    "before",
                    TypeSchema::optional(TypeSchema::uuid()),
                )]),
        ))
        .output(checklist_step_schema()),
    );
    set.register(
        Endpoint::new(
            ids::REORDER_CHECKLIST_STEPS,
            Method::Put,
            checklist_id().lit("order"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "order",
            TypeSchema::array(TypeSchema::uuid()),
        )]))
        .output(checklist_detail_schema()),
    );
    set.register(
        Endpoint::new(ids::RUN_CHECKLIST, Method::Post, checklist_id().lit("run"))
            .output(checklist_run_schema()),
    );
    set.register(
        Endpoint::new(
            ids::LIST_CHECKLIST_RUNS,
            Method::Get,
            checklist_id().lit("runs"),
        )
        .query([QueryParam::new("limit", ValueType::Int)])
        .output(TypeSchema::array(checklist_run_schema())),
    );
    // Every enabled checklist of an application (`application`: its id), the
    // ones with none (`none`), or all of them (absent).
    set.register(
        Endpoint::new(
            ids::RUN_CHECKLISTS,
            Method::Post,
            api().lit("checklist-runs"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "application",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(TypeSchema::array(checklist_run_schema())),
    );
    set.register(
        Endpoint::new(
            ids::GET_CHECKLIST_RUN,
            Method::Get,
            api().lit("checklist-runs").param("id", ValueType::Uuid),
        )
        .output(checklist_run_schema()),
    );
    // A screenshot a run kept (C2.5), by the path its step result names, as
    // base64 for the run detail to show inline.
    set.register(
        Endpoint::new(
            ids::GET_CHECKLIST_SCREENSHOT,
            Method::Get,
            api().lit("checklist-screenshots"),
        )
        .query([QueryParam::new("path", ValueType::Text)])
        .output(TypeSchema::struct_of([
            StructField::new("content_type", TypeSchema::text()),
            StructField::new("data", TypeSchema::text()),
        ])),
    );
    set.register(
        Endpoint::new(ids::UPDATE_CHECKLIST_STEP, Method::Put, step_id())
            .input(TypeSchema::struct_of(checklist_step_input_fields()))
            .output(checklist_step_schema()),
    );
    // Rejecting an agent's unaccepted step is deleting it: a tombstone.
    set.register(
        Endpoint::new(ids::DELETE_CHECKLIST_STEP, Method::Delete, step_id()).output(
            TypeSchema::struct_of([StructField::new("deleted", TypeSchema::bool())]),
        ),
    );
    set.register(
        Endpoint::new(
            ids::ACCEPT_CHECKLIST_STEP,
            Method::Post,
            step_id().lit("accept"),
        )
        .output(checklist_step_schema()),
    );
    set.register(
        Endpoint::new(
            ids::ACCEPT_CHECKLIST_PROPOSAL,
            Method::Post,
            proposal_id().lit("accept"),
        )
        .output(checklist_detail_schema()),
    );
    set.register(
        Endpoint::new(
            ids::REJECT_CHECKLIST_PROPOSAL,
            Method::Post,
            proposal_id().lit("reject"),
        )
        .output(checklist_detail_schema()),
    );
    // The step types a checklist may use; with `application` (an id) the ones
    // an application's checklist may, else the ones a data-only one may.
    set.register(
        Endpoint::new(
            ids::LIST_CHECKLIST_STEP_TYPES,
            Method::Get,
            api().lit("checklist-step-types"),
        )
        .query([QueryParam::new("application", ValueType::Text)])
        .output(TypeSchema::array(checklist_step_type_schema())),
    );
    // The starter checklists an application's framework offers it (C3.6) and
    // it does not have yet; creating one is a click, never automatic.
    set.register(
        Endpoint::new(
            ids::LIST_CHECKLIST_STARTERS,
            Method::Get,
            api().lit("checklist-starters"),
        )
        .query([QueryParam::new("application", ValueType::Uuid)])
        .output(TypeSchema::array(checklist_starter_schema())),
    );
    set.register(
        Endpoint::new(
            ids::CREATE_CHECKLIST_STARTER,
            Method::Post,
            api().lit("checklist-starters"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("application", TypeSchema::uuid()),
            StructField::new("name", TypeSchema::text()),
        ]))
        .output(checklist_detail_schema()),
    );
    // Make a user a check user — one a checklist may run as, signed in by the
    // runner without a password (C1.5) — or stop it being one. An admin's
    // alone; no agent tool reaches it.
    set.register(
        Endpoint::new(
            ids::SET_CHECK_USER,
            Method::Put,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("check-user"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "check_user",
            TypeSchema::bool(),
        )]))
        .output(TypeSchema::struct_of([
            StructField::new("id", TypeSchema::uuid()),
            StructField::new("check_user", TypeSchema::bool()),
        ])),
    );
    // Every user with whether it is a check user, for the `run_as` picker: a
    // person may run a checklist as anybody, an agent only as a check user.
    set.register(
        Endpoint::new(
            ids::LIST_CHECKLIST_USERS,
            Method::Get,
            api().lit("checklist-users"),
        )
        .output(TypeSchema::array(TypeSchema::struct_of([
            StructField::new("id", TypeSchema::uuid()),
            StructField::new("email", TypeSchema::text()),
            StructField::new("role", TypeSchema::int()),
            StructField::new("check_user", TypeSchema::bool()),
        ]))),
    );
}

/// A checklist's own fields (TODO-checklists C1.1).
fn checklist_fields() -> Vec<StructField> {
    vec![
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("application", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("run_as", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("enabled", TypeSchema::bool()),
        StructField::new("mutates", TypeSchema::bool()),
        StructField::new("attributes", TypeSchema::json()),
    ]
}

/// A checklist as the list shows it: its last run, and how much waits for a
/// person (proposals and unaccepted agent steps).
fn checklist_summary_schema() -> TypeSchema {
    TypeSchema::struct_of(checklist_fields().into_iter().chain([
        StructField::new("steps", TypeSchema::int()),
        StructField::new("pending", TypeSchema::int()),
        StructField::new(
            "last_run",
            TypeSchema::optional(TypeSchema::struct_of([
                StructField::new("id", TypeSchema::uuid()),
                StructField::new("status", TypeSchema::text()),
                StructField::new("finished_at", TypeSchema::text()),
            ])),
        ),
    ]))
}

/// A checklist with its current steps, its pending proposals and its recent
/// runs, newest first.
fn checklist_detail_schema() -> TypeSchema {
    TypeSchema::struct_of(checklist_fields().into_iter().chain([
        StructField::new("steps", TypeSchema::array(checklist_step_schema())),
        StructField::new("proposals", TypeSchema::array(checklist_proposal_schema())),
        StructField::new("runs", TypeSchema::array(checklist_run_schema())),
    ]))
}

/// What a create or an update submits. A create may carry its first steps.
fn checklist_input_schema(create: bool) -> TypeSchema {
    let mut fields = vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("application", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("run_as", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("enabled", TypeSchema::optional(TypeSchema::bool())),
        StructField::new("attributes", TypeSchema::optional(TypeSchema::json())),
    ];
    if create {
        fields.push(StructField::new(
            "steps",
            TypeSchema::optional(TypeSchema::array(TypeSchema::struct_of(
                checklist_step_input_fields(),
            ))),
        ));
    }
    TypeSchema::Struct(fields)
}

/// One step as a person writes it.
fn checklist_step_input_fields() -> Vec<StructField> {
    vec![
        StructField::new("type", TypeSchema::text()),
        StructField::new("params", TypeSchema::optional(TypeSchema::json())),
        StructField::new("label", TypeSchema::optional(TypeSchema::text())),
        StructField::new("as", TypeSchema::optional(TypeSchema::text())),
    ]
}

/// One version of one step.
fn checklist_step_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("checklist", TypeSchema::uuid()),
        StructField::new("position", TypeSchema::Value(ValueType::Float)),
        StructField::new("type", TypeSchema::text()),
        StructField::new("params", TypeSchema::json()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("as", TypeSchema::optional(TypeSchema::text())),
        StructField::new("author", TypeSchema::text()),
        StructField::new("accepted", TypeSchema::bool()),
        StructField::new("superseded_by", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("deleted", TypeSchema::bool()),
        StructField::new("created_at", TypeSchema::text()),
    ])
}

/// An agent's change waiting for a person.
fn checklist_proposal_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("checklist", TypeSchema::uuid()),
        StructField::new("kind", TypeSchema::text()),
        StructField::new("step", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("change", TypeSchema::json()),
        StructField::new("reason", TypeSchema::text()),
        StructField::new("author", TypeSchema::text()),
        StructField::new("created_at", TypeSchema::text()),
    ])
}

/// One run of one checklist, with each step's result.
fn checklist_run_schema() -> TypeSchema {
    let step = TypeSchema::struct_of([
        StructField::new("step", TypeSchema::uuid()),
        StructField::new("type", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("outcome", TypeSchema::text()),
        StructField::new("skip", TypeSchema::optional(TypeSchema::text())),
        StructField::new("message", TypeSchema::text()),
        StructField::new("duration_ms", TypeSchema::int()),
        StructField::new("captured", TypeSchema::optional(TypeSchema::json())),
        StructField::new("console_errors", TypeSchema::array(TypeSchema::text())),
        StructField::new("failed_requests", TypeSchema::array(TypeSchema::text())),
        // A path for `getChecklistScreenshot`: the page when a browser step
        // failed, or what `browser.screenshot` kept.
        StructField::new("screenshot", TypeSchema::optional(TypeSchema::text())),
    ]);
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("checklist", TypeSchema::uuid()),
        StructField::new("checklist_name", TypeSchema::text()),
        StructField::new("trigger", TypeSchema::text()),
        StructField::new("target", TypeSchema::text()),
        StructField::new("status", TypeSchema::text()),
        StructField::new("run_as", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("started_at", TypeSchema::timestamp()),
        StructField::new("finished_at", TypeSchema::text()),
        StructField::new("steps", TypeSchema::array(step)),
        StructField::new("step_versions", TypeSchema::array(TypeSchema::uuid())),
        StructField::new("cleanup_errors", TypeSchema::array(TypeSchema::text())),
        StructField::new("message", TypeSchema::optional(TypeSchema::text())),
        // What the run would have sent and kept instead (C4.1): `kind` is
        // `email`, `webhook` or `module`; `to` the addresses, the URL, or
        // `module/action`; `subject` an e-mail's subject or a request's method.
        StructField::new(
            "outbox",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("kind", TypeSchema::text()),
                StructField::new("to", TypeSchema::array(TypeSchema::text())),
                StructField::new("subject", TypeSchema::text()),
                StructField::new("body", TypeSchema::json()),
                StructField::new("at", TypeSchema::text()),
            ])),
        ),
    ])
}

/// A step type: what the step form is rendered from.
fn checklist_step_type_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("needs", TypeSchema::array(TypeSchema::text())),
        StructField::new("mutates", TypeSchema::bool()),
        // The framework whose applications alone it checks (`react`,
        // `server-rendered`), or none for a core type.
        StructField::new("framework", TypeSchema::optional(TypeSchema::text())),
        StructField::new(
            "params",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("label", TypeSchema::text()),
                StructField::new("type", TypeSchema::text()),
                StructField::new("required", TypeSchema::bool()),
                StructField::new("default", TypeSchema::optional(TypeSchema::json())),
                StructField::new("options", TypeSchema::array(TypeSchema::json())),
                StructField::new("multiline", TypeSchema::bool()),
                StructField::new("sublabel", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
    ])
}

/// A starter checklist a framework offers (C3.6), as the admin UI lists it.
fn checklist_starter_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("run_as", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new(
            "steps",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("type", TypeSchema::text()),
                StructField::new("label", TypeSchema::text()),
                StructField::new("params", TypeSchema::json()),
            ])),
        ),
    ])
}
