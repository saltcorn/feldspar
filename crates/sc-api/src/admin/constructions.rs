//! The admin API's endpoints: the App Constructor (TODO-app-constructor AC1.3,
//! AC2.2, AC2.3, AC3.4).
//!
//! An application has one construction at most, so a construction is
//! addressed by its application's id. Their handlers are registered by
//! `sc-server`'s `constructions` module.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    let construction = || {
        api()
            .lit("applications")
            .param("id", ValueType::Uuid)
            .lit("construction")
    };
    // One line per construction, for the Home screen.
    set.register(
        Endpoint::new("listConstructions", Method::Get, api().lit("constructions"))
            .output(TypeSchema::array(construction_summary_schema()))
            .auth(AuthRequirement::admin()),
    );
    set.register(
        Endpoint::new("getConstruction", Method::Get, construction())
            .output(construction_schema())
            .auth(AuthRequirement::admin()),
    );
    // Starts a construction for a React application, with the agent that plans
    // and builds its data model on `provider`. Without it: the provider the
    // application's settings choose, else the builder agent's, else the
    // default.
    set.register(
        Endpoint::new("createConstruction", Method::Post, construction())
            .input(TypeSchema::struct_of([StructField::new(
                "provider",
                TypeSchema::optional(TypeSchema::text()),
            )]))
            .output(construction_schema())
            .auth(AuthRequirement::admin()),
    );
    set.register(
        Endpoint::new(
            "saveConstructionSpecification",
            Method::Put,
            construction().lit("specification"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "specification",
            TypeSchema::text(),
        )]))
        .output(construction_schema())
        .auth(AuthRequirement::admin()),
    );
    // Starts the run that plans the phases from the specification, and answers
    // at once (202) with `plan_run` set and `planning` true; the run saves the
    // phases as it goes. Poll `getConstruction` until `planning` is false.
    set.register(
        Endpoint::new("planConstruction", Method::Post, construction().lit("plan"))
            .output(construction_schema())
            .auth(AuthRequirement::admin()),
    );
    // Replaces the phases, in order; the validation the agent's `save_phases`
    // goes through. A phase named by `id` is updated in place, and one with
    // tasks that the list leaves out is kept.
    set.register(
        Endpoint::new("savePhases", Method::Put, construction().lit("phases"))
            .input(TypeSchema::struct_of([StructField::new(
                "phases",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("id", TypeSchema::optional(TypeSchema::uuid())),
                    StructField::new("title", TypeSchema::text()),
                    StructField::new("description", TypeSchema::text()),
                    StructField::new("requirements", TypeSchema::array(requirement_schema())),
                ])),
            )]))
            .output(construction_schema())
            .auth(AuthRequirement::admin()),
    );
    // Starts the runner (202): it plans each phase's tasks when it reaches the
    // phase and runs them, until every phase is done, or `until_phase` is. Poll
    // `getConstruction` while `status` is `running`.
    set.register(
        Endpoint::new("runConstruction", Method::Post, construction().lit("run"))
            .input(TypeSchema::struct_of([StructField::new(
                "until_phase",
                TypeSchema::optional(TypeSchema::uuid()),
            )]))
            .output(construction_schema())
            .auth(AuthRequirement::admin()),
    );
    // Stops the runner: the task being run is aborted and goes back to `todo`,
    // and the construction becomes `stopped` once the runner has stopped.
    set.register(
        Endpoint::new("stopConstruction", Method::Post, construction().lit("stop"))
            .output(construction_schema())
            .auth(AuthRequirement::admin()),
    );
    let task = || construction().lit("tasks").param("task", ValueType::Uuid);
    // A failed task back to `todo`; a failed construction carries on.
    set.register(
        Endpoint::new("retryTask", Method::Post, task().lit("retry"))
            .output(construction_schema())
            .auth(AuthRequirement::admin()),
    );
    // A waiting or failed task set aside: the tasks after it run as if it were
    // done. A failed construction carries on.
    set.register(
        Endpoint::new("skipTask", Method::Post, task().lit("skip"))
            .output(construction_schema())
            .auth(AuthRequirement::admin()),
    );
    // Deletes the construction, its phases and tasks, and the agent made for
    // it. What it built stays. Refused while it runs.
    set.register(
        Endpoint::new("deleteConstruction", Method::Delete, construction())
            .output(TypeSchema::struct_of([
                StructField::new("deleted", TypeSchema::bool()),
                StructField::new("agent", TypeSchema::optional(TypeSchema::text())),
            ]))
            .auth(AuthRequirement::admin()),
    );
}

fn status_fields() -> [StructField; 4] {
    [
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("application", TypeSchema::uuid()),
        // draft | planned | running | stopped | done | failed
        StructField::new("status", TypeSchema::text()),
        StructField::new("agent", TypeSchema::optional(TypeSchema::text())),
    ]
}

fn construction_summary_schema() -> TypeSchema {
    TypeSchema::struct_of(status_fields().into_iter().chain([
        StructField::new("phases", TypeSchema::int()),
        StructField::new("phases_done", TypeSchema::int()),
    ]))
}

fn construction_schema() -> TypeSchema {
    TypeSchema::struct_of(status_fields().into_iter().chain([
        StructField::new("specification", TypeSchema::text()),
        StructField::new("started_by", TypeSchema::optional(TypeSchema::uuid())),
        // The run that planned the phases last, in the agent's run history.
        StructField::new("plan_run", TypeSchema::optional(TypeSchema::uuid())),
        // Whether that run is being driven now.
        StructField::new("planning", TypeSchema::bool()),
        // Why it stopped or failed last.
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        // The phase the run ends after; all of them when absent.
        StructField::new("until_phase", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("updated_at", TypeSchema::optional(TypeSchema::timestamp())),
        StructField::new("phases", TypeSchema::array(phase_schema())),
    ]))
}

fn phase_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("title", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("requirements", TypeSchema::array(requirement_schema())),
        // unplanned | planned | running | failed | done: from the tasks.
        StructField::new("status", TypeSchema::text()),
        StructField::new("tasks", TypeSchema::array(task_schema())),
    ])
}

fn requirement_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("text", TypeSchema::text()),
        // must | should | could
        StructField::new("priority", TypeSchema::text()),
    ])
}

fn task_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        // data_model | feature
        StructField::new("kind", TypeSchema::text()),
        StructField::new("title", TypeSchema::text()),
        StructField::new("brief", TypeSchema::text()),
        StructField::new("depends_on", TypeSchema::array(TypeSchema::uuid())),
        // todo | running | done | failed | skipped
        StructField::new("status", TypeSchema::text()),
        StructField::new("runs", TypeSchema::array(TypeSchema::uuid())),
        StructField::new("summary", TypeSchema::optional(TypeSchema::text())),
        StructField::new("created", TypeSchema::array(TypeSchema::json())),
    ])
}
