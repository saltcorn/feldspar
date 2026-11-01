//! The admin API's endpoints: triggers, the actions they run, workflows and what an admin does to a
//! workflow run.
//!
//! `sc-server`'s `handlers::triggers` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(set: &mut EndpointSet) {
    // A trigger is one event bound to one configured action (§10.2), stored in
    // `_fd_triggers`. These endpoints are the row ⇄ live-set path the SPA
    // drives: every save is validated and the live set is reloaded, so what the
    // list shows is what will fire.

    set.register(
        Endpoint::new("listTriggers", Method::Get, api().lit("triggers"))
            .output(TypeSchema::array(trigger_schema()))
            // `describe_triggers` is the composite tool over the same rows and
            // says more about each; this is tagged as well because it carries
            // the trigger **ids** the workflow tools address a trigger by, and
            // that composite deliberately speaks in names.
            .mcp(
                McpTag::new(
                    "List the triggers with their ids: the event each fires on, the \
                 table where there is one, the action or workflow it runs, and \
                 whether it is enabled. The id is what the workflow tools \
                 address a workflow by, since a workflow *is* a trigger's \
                 body.",
                )
                .in_area(Area::Triggers),
            )
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createTrigger", Method::Post, api().lit("triggers"))
            .input(trigger_input_schema())
            .output(trigger_schema())
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateTrigger",
            Method::Put,
            api().lit("triggers").param("id", ValueType::Uuid),
        )
        .input(trigger_input_schema())
        .output(trigger_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteTrigger",
            Method::Delete,
            api().lit("triggers").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Run one trigger now — the admin's "test this now". The posted body is the
    // event's payload and the action's result comes back; an action that fails
    // comes back as an **error**, not a 200 carrying a failure nobody reads.
    set.register(
        Endpoint::new(
            "runTrigger",
            Method::Post,
            api()
                .lit("triggers")
                .param("id", ValueType::Uuid)
                .lit("run"),
        )
        .input(TypeSchema::json())
        .output(TypeSchema::struct_of([StructField::new(
            "result",
            TypeSchema::json(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Run one trigger as a **test**, from the admin's list. Everything about it
    // that differs from `runTrigger` is there because a person is watching:
    //
    //   - a failing action is a **200 carrying the failure**, not an error
    //     status. A test run that failed did its job; the message is the
    //     answer, and it arrives beside everything else the run produced
    //     rather than in place of it.
    //   - what the body **printed** comes back with it. An admin debugging a
    //     `run_js_code` adds a `console.log` and expects to see it — in v1 they
    //     read it off the server's terminal, which a hosted deployment does not
    //     have.
    //   - a trigger on `insert`/`update`/`delete` is run against a **row picked
    //     at random** from its table, because a table trigger has no occurrence
    //     of its own when a person presses a button, and a body that reads
    //     `row.title` against nothing fails for a reason about the test rather
    //     than about the trigger. Which row it was comes back, so a surprising
    //     result is traceable to the row that produced it.
    set.register(
        Endpoint::new(
            "testRunTrigger",
            Method::Post,
            api()
                .lit("triggers")
                .param("id", ValueType::Uuid)
                .lit("test-run"),
        )
        .input(TypeSchema::json())
        .output(TypeSchema::struct_of([
            StructField::new("ok", TypeSchema::bool()),
            StructField::new("result", TypeSchema::json()),
            // The failure, when there was one: the action's own message, which
            // is the whole point of a test run.
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
            StructField::new(
                "console",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("level", TypeSchema::text()),
                    StructField::new("text", TypeSchema::text()),
                ])),
            ),
            // The row this was run against, for a table event, and `null` for
            // every other kind — including a table trigger whose table is empty,
            // which the screen says rather than pretending a row was chosen.
            StructField::new("row", TypeSchema::optional(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The registered actions with the settings each declares, so the trigger
    // form renders a configuration form for an action it knows nothing about
    // (§13.3) — the same move the framework and file-store pickers make.
    //
    // `?table=` is optional and names the table the trigger being edited fires
    // on, because an action's declaration may **depend on it**: `send_email`
    // offers one attachment checkbox per File field of that table
    // (`Action::config_spec_for`). Without it the answer is the table-independent
    // spec, which is what every other action has and what a non-table trigger
    // gets. The screen still knows nothing about any particular setting — it asks
    // for the declaration for the table it is showing and renders what comes
    // back.
    set.register(
        Endpoint::new("listActions", Method::Get, api().lit("actions"))
            .query([QueryParam::new("table", ValueType::Text)])
            .output(TypeSchema::array(action_info_schema()))
            // The list; `describe_action` is the one action's settings in full,
            // which is progressive disclosure and stays the way to configure
            // one. This answers "what is there?" in one call rather than N.
            .mcp(
                McpTag::new(
                    "List the actions a trigger can run, each with a summary of what \
                 it does. Pass `table` to see the actions as they are declared \
                 for a trigger on that table, since an action's settings may \
                 depend on it. Then call `describe_action` for the one you want, \
                 which is where the full settings are.",
                )
                .in_area(Area::Triggers),
            )
            .auth(AuthRequirement::admin()),
    );

    // A workflow is a **trigger body** (§10.3, decision 1), so it is addressed by
    // the trigger's id and has no create or delete of its own: creating the
    // trigger creates version 1, and deleting the trigger takes its versions with
    // it. What is left is what a workflow has that an action body does not — a
    // program, and a history of the programs it used to be.
    //
    // **Versions are rows and the table is append-only** (decision 2). So there
    // is no `updateWorkflow`: `saveWorkflow` mints the next version, and
    // `revertWorkflow` mints a new version whose steps are an old one's, because
    // rewriting history is exactly what append-only says no to — and because a
    // run suspended on version 1 has to still be able to load version 1
    // tomorrow.

    set.register(
        Endpoint::new(
            "getWorkflow",
            Method::Get,
            api().lit("workflows").param("id", ValueType::Uuid),
        )
        // Which version to read, defaulting to the current one. A run is
        // **pinned** to the version it started on (decision 2), so the screen
        // that draws a run on the canvas it ran on has to be able to ask for
        // that version rather than for today's — otherwise "the path taken,
        // highlighted on the same graph the admin drew" would be the path of one
        // program drawn on the picture of another.
        .query([QueryParam::new("version", ValueType::Int)])
        .output(workflow_schema())
        .mcp(
            McpTag::new(
                "Read a workflow — the program a trigger's body runs. Addressed by \
             the **trigger's** id (from `listTriggers`), because a workflow is a \
             trigger body rather than an object of its own. Pass `version` to \
                 read an older one; the default is the current version.",
            )
            .in_area(Area::Triggers),
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "saveWorkflow",
            Method::Post,
            api().lit("workflows").param("id", ValueType::Uuid),
        )
        .input(TypeSchema::struct_of([
            // The whole program — the steps, the start, the workflow-level error
            // policy, the trace flag and the step budget — in the **stored**
            // shape, which is also the editor's. Passed through as JSON rather
            // than described field by field for the reason a run's `context` is:
            // the document's shape belongs to the engine, and a second
            // declaration of it here would be a second spelling to keep in step
            // (decision: the stored JSON is the API shape, and there is no
            // third).
            StructField::new("workflow", TypeSchema::json()),
            // The commit message. A version history without one is a list of
            // numbers.
            StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(workflow_schema())
        .mcp(
            McpTag::new(
                "Save a workflow's steps as a **new version** — the table is \
             append-only, so nothing is overwritten and a run already suspended \
             on an earlier version still loads that one. Send the whole program \
             in the shape `getWorkflow` returns it, with a description saying \
                 what changed. A `run_js_code` step's code is JavaScript against \
                 this server's own `db` API — call `describe_code_api` before \
                 writing one.",
            )
            .in_area(Area::Triggers)
            // A workflow is an existing trigger's body — version 1 is created
            // with the trigger — so writing one is an edit rather than a create.
            .needs(Grant::Edit),
        )
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "revertWorkflow",
            Method::Post,
            api()
                .lit("workflows")
                .param("id", ValueType::Uuid)
                .lit("revert"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("version", TypeSchema::int()),
            StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(workflow_schema())
        .mcp(
            McpTag::new(
                "Go back to an earlier version of a workflow by minting a new \
             version whose steps are that one's. History is not rewritten, so \
                 the version you reverted from is still readable.",
            )
            .in_area(Area::Triggers)
            // Reverting mints a version rather than removing one, so it is an
            // edit and not a drop: nothing stops being readable.
            .needs(Grant::Edit),
        )
        .auth(AuthRequirement::admin()),
    );

    // The runs of one workflow, newest first. Filterable by state and paged,
    // because a workflow that fires on every insert has as many runs as the table
    // has rows and the screen wants the ones that are stuck.
    set.register(
        Endpoint::new(
            "listWorkflowRuns",
            Method::Get,
            api()
                .lit("workflows")
                .param("id", ValueType::Uuid)
                .lit("runs"),
        )
        .query([
            QueryParam::new("state", ValueType::Text),
            QueryParam::new("limit", ValueType::Int),
            QueryParam::new("offset", ValueType::Int),
        ])
        .output(TypeSchema::array(run_summary_schema()))
        .mcp(
            McpTag::new(
                "List a workflow's runs, newest first, addressed by the trigger's \
             id. Filter by `state` — `waiting` is the one that wants attention — \
             and page with `limit` and `offset`, because a workflow that fires \
                 on every insert has as many runs as the table has rows. \
                 `getRun` reads one in full.",
            )
            .in_area(Area::Triggers),
        )
        .auth(AuthRequirement::admin()),
    );

    // On `runs/{id}` rather than under a workflow, because a run is addressed by
    // its own id everywhere else (`getRun`, `deleteRun`) and knowing which
    // workflow it is of is the server's job, not the caller's.

    set.register(
        Endpoint::new(
            "resumeRun",
            Method::Post,
            api().lit("runs").param("id", ValueType::Uuid).lit("resume"),
        )
        // The answers to the step's own form, keyed by its declared field names —
        // checked against that declaration, which is the one the person was shown
        // rather than the workflow's current text.
        .input(TypeSchema::json())
        .output(run_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "cancelRun",
            Method::Post,
            api().lit("runs").param("id", ValueType::Uuid).lit("cancel"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "reason",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(run_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "retryRun",
            Method::Post,
            api().lit("runs").param("id", ValueType::Uuid).lit("retry"),
        )
        .output(run_schema())
        .auth(AuthRequirement::admin()),
    );
}

/// One version of a workflow, with the program itself and the checks against it
/// (§10.3, phase 5.1).
fn workflow_schema() -> TypeSchema {
    TypeSchema::struct_of([
        // The trigger's id: a workflow is a trigger body, not an entity of its
        // own, so this is the trigger's identity and not a second one.
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        // The table the trigger fires on, which is what decides a step's scope
        // and an action's declaration — the editor needs it to ask `listActions`
        // the right question.
        StructField::new("channel", TypeSchema::optional(TypeSchema::text())),
        StructField::new("version", TypeSchema::int()),
        // The program, in the stored shape (see `saveWorkflow`).
        StructField::new("workflow", TypeSchema::json()),
        // Everything wrong with it, each naming the step it is about so the
        // canvas can mark the node. **Empty is usable**; a workflow with issues
        // is still stored, still listed and still editable — that is what makes
        // it fixable — and only refuses to start a run.
        StructField::new("issues", TypeSchema::array(workflow_issue_schema())),
        StructField::new("versions", TypeSchema::array(workflow_version_schema())),
    ])
}

/// One thing wrong with a workflow, and where it is.
fn workflow_issue_schema() -> TypeSchema {
    TypeSchema::struct_of([
        // Null for a problem about the workflow as a whole rather than any one
        // step (a start step that does not exist, an error policy naming a
        // stranger).
        StructField::new("step", TypeSchema::optional(TypeSchema::text())),
        StructField::new("problem", TypeSchema::text()),
    ])
}

/// One entry in a workflow's history: which version, when, and who saved it.
///
/// The steps are deliberately absent. A history is a list, and carrying every
/// version's whole program would send the same document a dozen times to draw a
/// dropdown; `revertWorkflow` is how an old one is brought back.
fn workflow_version_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("version", TypeSchema::int()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("created_at", TypeSchema::timestamp()),
        StructField::new("created_by", TypeSchema::optional(TypeSchema::uuid())),
    ])
}

fn trigger_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(trigger_fields());
    fields.push(StructField::new(
        "error",
        TypeSchema::optional(TypeSchema::text()),
    ));
    // Read-only, and absent from the input shape below: when a periodic trigger
    // last fired is the scheduler's record of what happened, not a field an
    // admin sets. The list shows it; nothing posts it back.
    fields.push(StructField::new(
        "last_run_at",
        TypeSchema::optional(TypeSchema::timestamp()),
    ));
    // The workflow half, and read-only for the same reason `last_run_at` is: a
    // workflow's steps are versions of their own, written through the workflow
    // endpoints. Null on an action body. The list shows them because "a
    // workflow" is not a useful description of a trigger — how many steps it has
    // and which version is live is what tells one apart from another, and the
    // alternative was one `getWorkflow` per row.
    fields.push(StructField::new(
        "workflow_version",
        TypeSchema::optional(TypeSchema::int()),
    ));
    fields.push(StructField::new(
        "workflow_steps",
        TypeSchema::optional(TypeSchema::int()),
    ));
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating a trigger: the same fields minus
/// the id (server-assigned on create, taken from the path on update) and minus
/// `error` (which is the server's answer, not the admin's input).
fn trigger_input_schema() -> TypeSchema {
    TypeSchema::Struct(trigger_fields())
}

/// The editable half of a trigger, shared by the read and write shapes.
fn trigger_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // The event kind, as the lowercase word it is stored under
        // (`insert`, `login`, …).
        StructField::new("when", TypeSchema::text()),
        // The table, for a table event; null for every other kind.
        StructField::new("channel", TypeSchema::optional(TypeSchema::text())),
        StructField::new("only_if", TypeSchema::optional(TypeSchema::text())),
        // Which engine runs it: `action` or `workflow` (§10.3). Absent on input
        // means `action`, which is what every trigger was before workflows and
        // what a client that has not heard of them sends.
        StructField::new("body", TypeSchema::optional(TypeSchema::text())),
        // Both are the **action** body's, and both are null for a workflow —
        // whose steps are a version of their own, read and written through the
        // workflow endpoints rather than as a field of the trigger.
        StructField::new("action", TypeSchema::optional(TypeSchema::text())),
        StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
        StructField::new("enabled", TypeSchema::bool()),
        // The periodic timing (§10.2), null on the kinds that have none. Three
        // flat fields rather than a nested object: each is one number, each is
        // one input in the form, and a kind that does not use one refuses it —
        // so a nested shape would only add a level to say the same thing.
        StructField::new("minute", TypeSchema::optional(TypeSchema::int())),
        StructField::new("hour", TypeSchema::optional(TypeSchema::int())),
        StructField::new("day_of_week", TypeSchema::optional(TypeSchema::int())),
    ]
}

/// A registered action and the settings it declares — name, one-line
/// description, and its `config_spec` in the same [`form_field_schema`]
/// vocabulary a framework's and a file-store backend's settings use.
fn action_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        // Whether this action can be offered as a **workflow step** on the table
        // asked about (§10.3, phase 5.4): false when its declaration for that
        // channel has a required setting with nothing to pick, which is what an
        // action that cannot serve this channel looks like from the outside. The
        // step palette hides those rather than offering a step whose settings
        // form cannot be completed.
        //
        // Decided from the declaration, not from a list of action names, so a
        // plugin's action is judged by the same rule as a built-in.
        StructField::new("workflow_step", TypeSchema::bool()),
    ])
}
