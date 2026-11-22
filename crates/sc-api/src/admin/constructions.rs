//! The admin API's endpoints: the App Constructor (TODO-app-constructor AC1.3,
//! AC2.2, AC2.3, AC3.4, AC7, AC8, AC9).
//!
//! An application has one construction at most, so a construction is
//! addressed by its application's id. Their handlers are registered by
//! `sc-server`'s `constructions` module.

use super::*;

/// The path of an application's construction.
fn construction_path() -> PathSpec {
    api()
        .lit("applications")
        .param("id", ValueType::Uuid)
        .lit("construction")
}

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // One line per construction, for the Home screen.
    set.register(
        Endpoint::new(
            ids::LIST_CONSTRUCTIONS,
            Method::Get,
            api().lit("constructions"),
        )
        .output(TypeSchema::array(construction_summary_schema())),
    );
    set.register(
        Endpoint::new(ids::GET_CONSTRUCTION, Method::Get, construction_path())
            .output(construction_schema()),
    );
    // Starts a construction for a React application, with the agent that plans
    // and builds its data model on `provider`. Without it: the provider the
    // application's settings choose, else the builder agent's, else the
    // default.
    set.register(
        Endpoint::new(ids::CREATE_CONSTRUCTION, Method::Post, construction_path())
            .input(TypeSchema::struct_of([StructField::new(
                "provider",
                TypeSchema::optional(TypeSchema::text()),
            )]))
            .output(construction_schema()),
    );
    set.register(
        Endpoint::new(
            ids::SAVE_CONSTRUCTION_SPECIFICATION,
            Method::Put,
            construction_path().lit("specification"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "specification",
            TypeSchema::text(),
        )]))
        .output(construction_schema()),
    );
    // Starts the run that plans the phases from the specification, and answers
    // at once (202) with `plan_run` set and `planning` true; the run saves the
    // phases as it goes. Poll `getConstruction` until `planning` is false.
    set.register(
        Endpoint::new(
            ids::PLAN_CONSTRUCTION,
            Method::Post,
            construction_path().lit("plan"),
        )
        .output(construction_schema()),
    );
    register_questions(set);
    register_mockups(set);
    register_feedback(set);
    // Replaces the phases, in order; the validation the agent's `save_phases`
    // goes through. A phase named by `id` is updated in place, and one with
    // tasks that the list leaves out is kept.
    set.register(
        Endpoint::new(
            ids::SAVE_PHASES,
            Method::Put,
            construction_path().lit("phases"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "phases",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("id", TypeSchema::optional(TypeSchema::uuid())),
                StructField::new("title", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
                StructField::new("requirements", TypeSchema::array(requirement_schema())),
            ])),
        )]))
        .output(construction_schema()),
    );
    // Starts the runner (202): it plans each phase's tasks when it reaches the
    // phase and runs them, until every phase is done, or `until_phase` is. Poll
    // `getConstruction` while `status` is `running`.
    set.register(
        Endpoint::new(
            ids::RUN_CONSTRUCTION,
            Method::Post,
            construction_path().lit("run"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "until_phase",
            TypeSchema::optional(TypeSchema::uuid()),
        )]))
        .output(construction_schema()),
    );
    // Plans the tasks of one phase and stops before they run, so the admin
    // reviews them first: the phase the runner reaches next, every phase
    // before it done. A phase planned already whose tasks have not started is
    // planned again, the planning run told the old plan and `note`. Answers
    // at once (202), `running` with `plan_only`; it ends `planned`.
    set.register(
        Endpoint::new(
            ids::PLAN_PHASE_TASKS,
            Method::Post,
            construction_path()
                .lit("phases")
                .param("phase", ValueType::Uuid)
                .lit("plan"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "note",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(construction_schema()),
    );
    // Stops the runner: the task being run is aborted and goes back to `todo`,
    // and the construction becomes `stopped` once the runner has stopped.
    set.register(
        Endpoint::new(
            ids::STOP_CONSTRUCTION,
            Method::Post,
            construction_path().lit("stop"),
        )
        .output(construction_schema()),
    );
    let task = || {
        construction_path()
            .lit("tasks")
            .param("task", ValueType::Uuid)
    };
    // A failed task back to `todo`; a failed construction carries on.
    set.register(
        Endpoint::new(ids::RETRY_TASK, Method::Post, task().lit("retry"))
            .output(construction_schema()),
    );
    // A waiting or failed task set aside: the tasks after it run as if it were
    // done. A failed construction carries on.
    set.register(
        Endpoint::new(ids::SKIP_TASK, Method::Post, task().lit("skip"))
            .output(construction_schema()),
    );
    // Deletes the construction, its phases and tasks, and the agent made for
    // it. What it built stays. Refused while it runs.
    set.register(
        Endpoint::new(
            ids::DELETE_CONSTRUCTION,
            Method::Delete,
            construction_path(),
        )
        .output(TypeSchema::struct_of([
            StructField::new("deleted", TypeSchema::bool()),
            StructField::new("agent", TypeSchema::optional(TypeSchema::text())),
        ])),
    );
}

/// The clarifying questions' endpoints (AC8).
fn register_questions(set: &mut EndpointSet) {
    // Starts the run that asks the admin what the specification leaves open,
    // answering at once (202) with `asking` true. The open questions are
    // replaced by what it asks; the answered ones stay, and the run is told
    // them. The questions come before the mockups: once they are asked, the
    // mockups there were are discarded, the chosen one too. Poll
    // `getConstruction` until `asking` is false.
    set.register(
        Endpoint::new(
            ids::ASK_CONSTRUCTION_QUESTIONS,
            Method::Post,
            construction_path().lit("questions"),
        )
        .output(construction_schema()),
    );
    // Saves the admin's answers, by question; blank text leaves one open. The
    // answers go with the specification to every run from then on.
    set.register(
        Endpoint::new(
            ids::ANSWER_CONSTRUCTION_QUESTIONS,
            Method::Put,
            construction_path().lit("questions"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "answers",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("question", TypeSchema::uuid()),
                StructField::new("text", TypeSchema::text()),
            ])),
        )]))
        .output(construction_schema()),
    );
}

/// The feedback's endpoints (AC9).
fn register_feedback(set: &mut EndpointSet) {
    // Gives feedback on a phase that has built something (201). It is only
    // recorded, `given`, until approved with `planConstructionFeedback`; so it
    // can be given while the construction runs.
    set.register(
        Endpoint::new(
            ids::GIVE_CONSTRUCTION_FEEDBACK,
            Method::Post,
            construction_path().lit("feedback"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("phase", TypeSchema::uuid()),
            StructField::new("title", TypeSchema::text()),
            StructField::new("description", TypeSchema::text()),
        ]))
        .output(construction_schema()),
    );
    // Approves a feedback: starts the run that plans it, answering at once
    // (202) with the feedback `planning`. The run adds its requirements and
    // follow-up tasks after the phase's own, which wait as `todo` for the
    // admin to run. Also plans again a feedback whose planning ended without
    // tasks, e.g. failed. Refused while the construction runs. Poll
    // `getConstruction` until it is planned.
    set.register(
        Endpoint::new(
            ids::PLAN_CONSTRUCTION_FEEDBACK,
            Method::Post,
            construction_path()
                .lit("feedback")
                .param("feedback", ValueType::Uuid)
                .lit("plan"),
        )
        .output(construction_schema()),
    );
    // Deletes (or declines) a feedback with its follow-up tasks; refused once
    // one of them has started, and for approved feedback while the
    // construction runs.
    set.register(
        Endpoint::new(
            ids::DELETE_CONSTRUCTION_FEEDBACK,
            Method::Delete,
            construction_path()
                .lit("feedback")
                .param("feedback", ValueType::Uuid),
        )
        .output(construction_schema()),
    );
}

/// The mockups' endpoints (AC7).
fn register_mockups(set: &mut EndpointSet) {
    // Starts the run that proposes mockups (AC7), answering at once (202) with
    // `designing` true; the run saves them as a new round. `count` is how many
    // (2 when absent). `feedback` on the last round's mockups, or a `note`,
    // makes it a revision: the run is told them and the mockups they are
    // about. Poll `getConstruction` until `designing` is false.
    set.register(
        Endpoint::new(
            ids::PROPOSE_MOCKUPS,
            Method::Post,
            construction_path().lit("mockups"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("count", TypeSchema::optional(TypeSchema::int())),
            StructField::new(
                "feedback",
                TypeSchema::optional(TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("mockup", TypeSchema::uuid()),
                    StructField::new("text", TypeSchema::text()),
                ]))),
            ),
            StructField::new("note", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(construction_schema()),
    );
    // A mockup's screens with their HTML. The construction lists the screens
    // without it, so a poll does not carry every page: the screen fetches
    // them per mockup, once, as a mockup never changes after it is saved.
    set.register(
        Endpoint::new(
            ids::GET_MOCKUP_SCREENS,
            Method::Get,
            construction_path()
                .lit("mockups")
                .param("mockup", ValueType::Uuid)
                .lit("screens"),
        )
        .output(TypeSchema::array(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("file", TypeSchema::text()),
            StructField::new("html", TypeSchema::text()),
        ]))),
    );
    // Chooses one mockup, unchoosing any other, and writes its screens into
    // the application's file store as `mockups/<screen>.html`, where the
    // builder agent reads them. The phases are then planned to follow it.
    set.register(
        Endpoint::new(
            ids::CHOOSE_MOCKUP,
            Method::Post,
            construction_path()
                .lit("mockups")
                .param("mockup", ValueType::Uuid)
                .lit("choose"),
        )
        .output(construction_schema()),
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
        // The run only plans `until_phase`'s tasks (`planPhaseTasks`).
        StructField::new("plan_only", TypeSchema::bool()),
        // Whether a run proposing mockups is being driven now.
        StructField::new("designing", TypeSchema::bool()),
        // Whether a run asking clarifying questions is being driven now.
        StructField::new("asking", TypeSchema::bool()),
        // The clarifying questions, in order; `answer` absent while open.
        StructField::new(
            "questions",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("id", TypeSchema::uuid()),
                StructField::new("text", TypeSchema::text()),
                StructField::new("answer", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
        // Every round's mockups, oldest first.
        StructField::new("mockups", TypeSchema::array(mockup_schema())),
        // The admin's feedback on the phases, in the order it was given.
        StructField::new("feedback", TypeSchema::array(feedback_schema())),
        // The errors it met, newest first, at most 100 (AC10). `error` is the
        // current one, marked `current` here; they stay after a new run.
        StructField::new("errors", TypeSchema::array(error_schema())),
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

fn mockup_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("round", TypeSchema::int()),
        StructField::new("title", TypeSchema::text()),
        StructField::new("rationale", TypeSchema::text()),
        StructField::new(
            "screens",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                // Its file under `mockups/` once the mockup is chosen.
                StructField::new("file", TypeSchema::text()),
            ])),
        ),
        // proposed | chosen
        StructField::new("status", TypeSchema::text()),
        StructField::new("feedback", TypeSchema::optional(TypeSchema::text())),
    ])
}

fn error_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("construction", TypeSchema::uuid()),
        StructField::new("at", TypeSchema::timestamp()),
        // phases | tasks | task | mockups | questions | feedback | runner:
        // planning the phases, planning a phase's tasks, running a task,
        // proposing mockups, asking questions, planning a feedback, or the
        // runner itself.
        StructField::new("source", TypeSchema::text()),
        StructField::new("phase", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("task", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("feedback", TypeSchema::optional(TypeSchema::uuid())),
        // The run that failed, if there was one.
        StructField::new("run", TypeSchema::optional(TypeSchema::uuid())),
        // The construction run it happened in (AC10.5); absent on errors
        // logged before those were recorded.
        StructField::new("construction_run", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("message", TypeSchema::text()),
        // Whether it is the construction's `error`: the newest error of its
        // latest run. A new run has none until it fails.
        StructField::new("current", TypeSchema::bool()),
    ])
}

fn feedback_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("phase", TypeSchema::uuid()),
        StructField::new("title", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // What it adds to the phase; empty until planned.
        StructField::new("requirements", TypeSchema::array(requirement_schema())),
        // The run that planned it last; absent until approved.
        StructField::new("run", TypeSchema::optional(TypeSchema::uuid())),
        // Whether that run is being driven now.
        StructField::new("planning", TypeSchema::bool()),
        // given (not approved yet) | open (approved, no tasks yet) | planned |
        // running | failed | done: from its tasks once approved, which are its
        // phase's with `feedback` set to its id.
        StructField::new("status", TypeSchema::text()),
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
        // The feedback it was planned for; absent for the phase's own tasks.
        StructField::new("feedback", TypeSchema::optional(TypeSchema::uuid())),
    ])
}
