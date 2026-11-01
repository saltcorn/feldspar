//! The admin API's endpoints: predictive models, their instances and their posteriors.
//!
//! `sc-server`'s `handlers::models` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // Five nouns and three screens (TODO "Predictive models"): a **model
    // provider** is code that can fit something, a **dataset** is which rows and
    // which derived values, a **model** is a dataset plus a provider plus its
    // settings, a **model instance** is one fit of it, and a **prediction** is
    // that fit applied to rows.
    //
    // The dataset has no endpoints of its own, on purpose (§3): it is a JSON
    // column on the model, because a shared named dataset would need a lifecycle
    // — what happens to the four models fitted against it when somebody adds a
    // column — bought for a saving that a Duplicate button answers instead. What
    // it does have is `previewDataset`, which is not a store: it reads the first
    // rows and answers their types, so the builder is a thing you can see the
    // answer of before you fit against it.

    // The providers this build carries, each with the settings and
    // hyperparameters it declares — the same "settings as data" move the trigger
    // form, the file-store picker and the agent-trait form make, so the model
    // form renders a provider it has never heard of.
    //
    // `?dataset=` is the JSON of the dataset being built, and it is what turns a
    // declaration into a form: a provider naming a label has to offer *these*
    // columns as its options (§10), and it cannot know them when it is written.
    // With it, `config_spec` comes back resolved against the dataset's own
    // columns and `outcome` says what a fit would produce; without it the answer
    // is the unresolved declaration, which is what the picker shows before a
    // dataset exists. `?configuration=` is the settings so far, because the
    // outcome is a *function of the configuration* — a random forest is a
    // regressor or a classifier depending on the type of the column its
    // configuration names.
    //
    // An object rather than an array, because the empty list is a real state
    // with a sentence attached: a build made with `--no-default-features` has
    // the two hypothesis tests and nothing else (§13), and an empty picker reads
    // like a bug where "this build was made without them" reads like the
    // decision it is.
    set.register(
        Endpoint::new(
            "listModelProviders",
            Method::Get,
            api().lit("model-providers"),
        )
        // **Text**, holding JSON, rather than a `Json` parameter: a query string
        // carries text, and typing it as JSON would generate a client that
        // stringifies an object with `String()` and sends `[object Object]`.
        // The caller serialises, which is what it is actually doing.
        .query([
            QueryParam::new("dataset", ValueType::Text),
            QueryParam::new("configuration", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("providers", TypeSchema::array(model_provider_schema())),
            StructField::new("builtins_compiled_out", TypeSchema::bool()),
            StructField::new("notice", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Validate a dataset and show what it answers: the column types, and the
    // first rows. A `POST` because the dataset is a document rather than an
    // identifier, and it does not exist anywhere yet — this is the builder
    // asking "is this what I meant?" before there is a model to save.
    //
    // The read is **limited** rather than capped, which is the one place the row
    // bound does not apply: previewing is exactly what an admin does to a
    // dataset that turns out to be too big to fit, and refusing to show it would
    // refuse the answer they came for.
    set.register(
        Endpoint::new(
            "previewDataset",
            Method::Post,
            api().lit("model-datasets").lit("preview"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("dataset", TypeSchema::json()),
            StructField::new("limit", TypeSchema::optional(TypeSchema::int())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("columns", TypeSchema::array(dataset_column_schema())),
            StructField::new("rows", TypeSchema::array(TypeSchema::json())),
            StructField::new("primary_key", TypeSchema::optional(TypeSchema::text())),
            // Why this dataset cannot be split — a table with a composite or
            // absent primary key reads perfectly well and cannot be fitted (§5),
            // and the builder should say so while it is still being built.
            StructField::new("split_error", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("listModels", Method::Get, api().lit("models"))
            .query([QueryParam::new("table", ValueType::Text)])
            .output(TypeSchema::array(model_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getModel",
            Method::Get,
            api().lit("models").param("id", ValueType::Uuid),
        )
        .output(model_schema())
        .auth(AuthRequirement::admin()),
    );

    // One endpoint for create and replace, rather than the `POST`/`PUT` pair the
    // other records have. A model is edited and refitted continuously and the
    // form always sends the whole definition — there is no partial edit to
    // express — so two endpoints would be one behaviour under two names. The id
    // in the body is what says which: absent is a new model, present is that one
    // replaced.
    set.register(
        Endpoint::new("saveModel", Method::Post, api().lit("models"))
            .input(model_input_schema())
            .output(model_schema())
            .auth(AuthRequirement::admin()),
    );

    // A copy of a model under a new name — the given one, or "… (copy)" —
    // reading the same named datasets, with no fits, and with the original's
    // view state, so it opens laid out as the original was (analytics TODO
    // A3.4–A3.5: the model list's Clone).
    set.register(
        Endpoint::new(
            "cloneModel",
            Method::Post,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("clone"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "name",
            TypeSchema::optional(TypeSchema::text()),
        )]))
        .output(model_schema())
        .auth(AuthRequirement::admin()),
    );

    // What refers to a model by name (analytics TODO A3.5): the calculated
    // fields whose formula calls `predict("…")` on it, and the triggers and
    // workflows that fit it (`fit_model`) or name it in their configuration,
    // and the workspaces whose panels show one of its fits (A4.2).
    // The model list's delete warning lists them; deleting is not refused,
    // since each of them reports the missing model by name when it next runs.
    set.register(
        Endpoint::new(
            "modelUsage",
            Method::Get,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("usage"),
        )
        .output(TypeSchema::struct_of([
            StructField::new(
                "fields",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("table", TypeSchema::text()),
                    StructField::new("field", TypeSchema::text()),
                ])),
            ),
            StructField::new(
                "triggers",
                TypeSchema::array(TypeSchema::struct_of([
                    StructField::new("id", TypeSchema::text()),
                    StructField::new("name", TypeSchema::text()),
                    // `fits` (a `fit_model` step) or `names` (any other mention).
                    StructField::new("how", TypeSchema::text()),
                ])),
            ),
            // The workspaces whose panels show one of its fits (A4.2).
            StructField::new(
                "workspaces",
                TypeSchema::array(crate::analytics::workspace_use_schema()),
            ),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // A model's **view state** (analytics TODO A3.4): the dictionary the
    // screens showing a model keep their layout in — which outputs are open,
    // the optional plots chosen, the selected fit — so that it reopens as it
    // was left. Not part of the model: `saveModel` neither reads nor writes it,
    // a fit does not record it, and nothing about "changed since this fit"
    // looks at it. Patched key by key — a key set to `null` is removed, the
    // others are set, keys not named are left — so two screens keeping
    // different keys do not overwrite each other without a read first. Answers
    // the view state as it now is; `getModel` answers it too.
    set.register(
        Endpoint::new(
            "patchModelViewState",
            Method::Patch,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("view-state"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "patch",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::struct_of([StructField::new(
            "view_state",
            TypeSchema::json(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Deleting a model takes its instances with it, and that is the difference
    // from an agent (whose runs outlive it): an instance is not a record of what
    // happened, it is a fit *of this model* — its coefficients are meaningless
    // without the dataset they were fitted over.
    set.register(
        Endpoint::new(
            "deleteModel",
            Method::Delete,
            api().lit("models").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // **Start** a fit (§8). It answers the instance as soon as the row exists,
    // saying `fitting`, and the work runs on a spawned task — because a fit
    // reads every row of a dataset and runs an optimiser over it, which is
    // seconds at best and minutes at worst, and must not be a request a proxy
    // times out halfway through while the work carries on invisibly. The screen
    // polls `getModelInstance`.
    //
    // A fit of a provider that can stop one (a posterior's chains are
    // processes) is cancelled with `cancelModelFit`; for the others the row cap
    // is the bound that exists instead, since stopping a `smartcore` or a
    // Python call mid-flight is not something the host can do.
    set.register(
        Endpoint::new(
            "fitModel",
            Method::Post,
            api().lit("models").param("id", ValueType::Uuid).lit("fit"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::optional(TypeSchema::text())),
            StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(model_instance_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "listModelInstances",
            Method::Get,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("instances"),
        )
        .output(TypeSchema::array(model_instance_schema()))
        .auth(AuthRequirement::admin()),
    );

    // The instance in full: the parameter blocks, the metrics per split, the
    // grid's scores and the row counts. Separate from the list because the
    // parameters of forty fits are not something a list should carry.
    set.register(
        Endpoint::new(
            "getModelInstance",
            Method::Get,
            api().lit("model-instances").param("id", ValueType::Uuid),
        )
        .output(model_instance_detail_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteModelInstance",
            Method::Delete,
            api().lit("model-instances").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // At most one instance per model is **active**, and that is what lets a
    // formula name a model rather than a fit: the admin refits, activates the
    // new instance, and every `predict("…")` follows without being edited.
    // Activating one deactivates whichever was.
    set.register(
        Endpoint::new(
            "activateModelInstance",
            Method::Post,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("activate"),
        )
        .output(model_instance_schema())
        .auth(AuthRequirement::admin()),
    );

    // Apply a fit to rows, in row order. Either a named `instance` or a `model`
    // (meaning its active instance), and either `rows` typed by the caller — the
    // instance screen's "try a row" box, and a what-if about a row that is not
    // in the table at all — or the model's own dataset, optionally restricted by
    // a `filter` formula.
    //
    // The two are not variations on one thing. Literal rows are whatever the
    // caller typed, because there is nothing to derive them from; dataset rows
    // are read **through the dataset**, so a join path and an aggregation are
    // computed by the row layer exactly as they were at fit time.
    //
    // Admin only, like everything else on this API: an application-facing
    // prediction endpoint is named under *Carried past this milestone*.
    set.register(
        Endpoint::new("predictRows", Method::Post, api().lit("model-predictions"))
            .input(TypeSchema::struct_of([
                StructField::new("model", TypeSchema::optional(TypeSchema::uuid())),
                StructField::new("instance", TypeSchema::optional(TypeSchema::uuid())),
                StructField::new(
                    "rows",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::json())),
                ),
                StructField::new("filter", TypeSchema::optional(TypeSchema::text())),
            ]))
            .output(TypeSchema::struct_of([
                StructField::new("instance", TypeSchema::uuid()),
                StructField::new("outcome", TypeSchema::json()),
                StructField::new("predictions", TypeSchema::array(prediction_schema())),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // What a Bayesian model needs beyond the model screens' endpoints (Stan TODO
    // §§5, 13, 16, 18). None of them names Stan: a provider that binds data is
    // one that declares an interface, and a posterior is read, summarised and
    // written back by the host whichever provider sampled it. The two that are
    // about a *program* — checking one and compiling one — are the Stan
    // provider's, because a program is what only it has.

    // Check a program without saving anything: what it declares, and `stanc`'s
    // warnings — or the sentence saying it was not checked, when this server
    // has no CmdStan (§5). A program `stanc` refuses is answered with its
    // diagnostics in `error` rather than as a failed request, because the
    // "Check program" button exists to show them.
    set.register(
        Endpoint::new(
            "getProgramInterface",
            Method::Get,
            api().lit("model-programs"),
        )
        .query([
            QueryParam::new("store", ValueType::Text),
            QueryParam::new("path", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("interface", TypeSchema::optional(TypeSchema::json())),
            StructField::new("warnings", TypeSchema::optional(TypeSchema::text())),
            StructField::new("notice", TypeSchema::optional(TypeSchema::text())),
            StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Preview data** (§18): the model as the form holds it, bound as far as it
    // binds — each data variable's shape and first values, or its error, on its
    // own row. A `POST` of the whole model for `previewDataset`'s reason: it
    // need not be saved, and usually is not yet.
    set.register(
        Endpoint::new(
            "previewModelData",
            Method::Post,
            api().lit("model-data").lit("preview"),
        )
        .input(model_input_schema())
        .output(TypeSchema::struct_of([
            StructField::new("variables", TypeSchema::array(TypeSchema::json())),
            // The datasets read and bound, the dimensions' sizes, the drops and
            // the warnings of what did bind; null when a sentence about no one
            // variable stopped it, which is then in `errors`.
            StructField::new("report", TypeSchema::optional(TypeSchema::json())),
            StructField::new("errors", TypeSchema::array(TypeSchema::text())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // **Bind automatically** (§18): a binding for each data variable the model
    // leaves unbound that the names, the foreign keys and the size expressions
    // say something about, and why. Nothing bound is replaced; the form fills
    // only its empty rows.
    set.register(
        Endpoint::new(
            "suggestBindings",
            Method::Post,
            api().lit("model-bindings").lit("suggest"),
        )
        .input(model_input_schema())
        .output(TypeSchema::struct_of([
            StructField::new("bindings", TypeSchema::json()),
            StructField::new("reasons", TypeSchema::json()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Warm the compile cache without fitting (§13). It waits for the compile —
    // a minute the first time, nothing after — and answers the cache key, so a
    // compile error is shown where the button was pressed.
    set.register(
        Endpoint::new(
            "compileModel",
            Method::Post,
            api()
                .lit("models")
                .param("id", ValueType::Uuid)
                .lit("compile"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("key", TypeSchema::text()),
            StructField::new("cached", TypeSchema::bool()),
            StructField::new("cmdstan", TypeSchema::text()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Ask a running fit to stop (§13). It sets `cancel_requested` on the row,
    // which the job reads back within a second — so it works from any node,
    // because the row is the registry. Refused by name for a provider whose
    // fit cannot be stopped, and for a fit that has already finished.
    set.register(
        Endpoint::new(
            "cancelModelFit",
            Method::Post,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("cancel"),
        )
        .output(model_instance_schema())
        .auth(AuthRequirement::admin()),
    );

    // One variable's draws, columnar and labelled (§16): per chain, one array
    // per selected element. `elements` is JSON — index arrays, or per axis the
    // keys or labels wanted (`{"counties":["27001"]}`); `chains` is a
    // comma-separated list. An answer of more than `--stan-max-draws-response`
    // numbers is refused with the arithmetic, which `thin` is for.
    set.register(
        Endpoint::new(
            "getModelDraws",
            Method::Get,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("draws"),
        )
        .query([
            QueryParam::new("variable", ValueType::Text),
            QueryParam::new("elements", ValueType::Text),
            QueryParam::new("chains", ValueType::Text),
            QueryParam::new("warmup", ValueType::Bool),
            QueryParam::new("thin", ValueType::Int),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("variable", TypeSchema::text()),
            StructField::new("dims", TypeSchema::array(TypeSchema::int())),
            StructField::new("axes", TypeSchema::array(TypeSchema::text())),
            StructField::new("labels", TypeSchema::array(TypeSchema::json())),
            StructField::new("keys", TypeSchema::array(TypeSchema::json())),
            StructField::new("elements", TypeSchema::array(TypeSchema::json())),
            StructField::new("names", TypeSchema::array(TypeSchema::text())),
            StructField::new("thin", TypeSchema::int()),
            StructField::new("chains", TypeSchema::array(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The §15 summary of any variable, computed now from its stored draws —
    // including one too large for the fit to have stored a table of. When the
    // draws were not kept, the stored table answers (`source: "stored"`).
    set.register(
        Endpoint::new(
            "getPosteriorSummary",
            Method::Get,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("summary"),
        )
        .query([
            QueryParam::new("variable", ValueType::Text),
            QueryParam::new("elements", ValueType::Text),
        ])
        .output(TypeSchema::struct_of([
            StructField::new("variable", TypeSchema::text()),
            StructField::new("source", TypeSchema::text()),
            StructField::new("columns", TypeSchema::array(TypeSchema::text())),
            StructField::new("elements", TypeSchema::array(TypeSchema::json())),
            StructField::new("names", TypeSchema::array(TypeSchema::text())),
            StructField::new("keys", TypeSchema::array(TypeSchema::json())),
            StructField::new("rows", TypeSchema::array(TypeSchema::json())),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // The run as a zip (§16): the raw CmdStan run directory when the model
    // keeps one in a file store, else per-chain draws CSVs built from the table
    // with `coordinates.json`. The response **is** the file — the endpoint model
    // has no bytes shape, so the declared output is empty and the admin UI
    // links to the path rather than calling the client.
    set.register(
        Endpoint::new(
            "downloadModelRun",
            Method::Get,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("run"),
        )
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // Write one variable's summary into rows (§16), through the row layer:
    // `update` into the rows of the table its one axis is about, matched by
    // key; `insert`, one row per element into `table`. The same write-back a
    // code body's model handle makes (`sc_api::models::write_posterior`).
    set.register(
        Endpoint::new(
            "writePosterior",
            Method::Post,
            api()
                .lit("model-instances")
                .param("id", ValueType::Uuid)
                .lit("posterior-writes"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("variable", TypeSchema::text()),
            StructField::new("mode", TypeSchema::text()),
            StructField::new("statistics", TypeSchema::json()),
            StructField::new("table", TypeSchema::optional(TypeSchema::text())),
            StructField::new("coordinates", TypeSchema::optional(TypeSchema::json())),
            StructField::new("instance_field", TypeSchema::optional(TypeSchema::text())),
            StructField::new("elements", TypeSchema::optional(TypeSchema::json())),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("variable", TypeSchema::text()),
            StructField::new("mode", TypeSchema::text()),
            StructField::new("table", TypeSchema::text()),
            StructField::new("instance", TypeSchema::text()),
            StructField::new("written", TypeSchema::int()),
        ]))
        .auth(AuthRequirement::admin()),
    );
}

/// One column of a previewed dataset: its name and the type its values came
/// back as.
///
/// The type is the **data's**, not the schema's, and that is not a shortcut: a
/// `SchemaShape` carries no types at all, and the type of `price / area`, of a
/// join path or of an aggregation is not derivable from one.
fn dataset_column_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("type", TypeSchema::text()),
    ])
}

/// One stored model, as the list and the form see it (TODO §15).
///
/// `error` is the twin of an agent's and a trigger's: a model that stopped
/// validating — a dataset column whose formula no longer resolves, a provider
/// whose module was uninstalled — is **still listed and still editable**,
/// because editing it is the repair.
///
/// `last_fit` rides along because that is what the list is read for: which
/// models have been fitted, when, and whether the last one worked.
fn model_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("provider", TypeSchema::text()),
        StructField::new("table_name", TypeSchema::text()),
        StructField::new("dataset", TypeSchema::json()),
        // The datasets beside the main one, each under the name bindings
        // address it by (Stan TODO §7); `[]` for every model that has none.
        StructField::new("related", TypeSchema::json()),
        StructField::new("configuration", TypeSchema::json()),
        StructField::new("hyperparameters", TypeSchema::json()),
        StructField::new("split", TypeSchema::json()),
        StructField::new("attributes", TypeSchema::json()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("instances", TypeSchema::int()),
        StructField::new("last_fit", TypeSchema::optional(model_instance_schema())),
        StructField::new(
            "active_instance",
            TypeSchema::optional(model_instance_schema()),
        ),
        // On `saveModel`'s answer for a program provider: `stanc`'s warnings,
        // or the notice that the program was not checked (Stan TODO §5). Null
        // everywhere else.
        StructField::new("program_check", TypeSchema::optional(TypeSchema::json())),
        // The screens' layout (analytics TODO A3.4), written only by
        // `patchModelViewState`; `{}` for a new model.
        StructField::new("view_state", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// What a `saveModel` sends. The id is what says create or replace.
fn model_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("provider", TypeSchema::text()),
        StructField::new("dataset", TypeSchema::json()),
        StructField::new("related", TypeSchema::optional(TypeSchema::json())),
        StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
        // Per hyperparameter either a value or a **list** of values, and a fit
        // runs the grid of the lists (§11). One field rather than two, because a
        // list of one and a scalar are the same search.
        StructField::new("hyperparameters", TypeSchema::optional(TypeSchema::json())),
        StructField::new("split", TypeSchema::optional(TypeSchema::json())),
        StructField::new("attributes", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// One fit, as a list sees it: everything except the parameters, the metrics and
/// the encoding, which a list of forty fits must not carry.
fn model_instance_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("model", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // `fitting` | `fitted` | `failed`. A column rather than an attribute
        // because every row has one and it is what the list filters on; the
        // failure **sentence** is the other way round, present only on the rows
        // that failed, which is why it is `error` here and an attribute in the
        // row (§15).
        StructField::new("status", TypeSchema::text()),
        StructField::new("created", TypeSchema::timestamp()),
        StructField::new("active", TypeSchema::bool()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("hyperparameters", TypeSchema::json()),
        StructField::new("outcome", TypeSchema::optional(TypeSchema::json())),
        StructField::new("metrics", TypeSchema::json()),
        StructField::new("rows", TypeSchema::optional(TypeSchema::json())),
        // A running posterior fit's stage and per-chain iterations, written at
        // most once a second (Stan TODO §13); null otherwise.
        StructField::new("progress", TypeSchema::optional(TypeSchema::json())),
        // Whether somebody has asked this fit to stop.
        StructField::new("cancel_requested", TypeSchema::bool()),
        // A posterior's diagnostic warnings, as sentences that say what to do
        // (§15); empty for every other fit.
        StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
        // Whether the model's datasets differ now from the ones this fit read
        // (analytics TODO A1.10, A3.3): on every fit a list or a model shows,
        // so the model editor's list of fits can say which are out of date;
        // null when it cannot tell, or where it was not asked.
        StructField::new("dataset_changed", TypeSchema::optional(TypeSchema::bool())),
    ])
}

/// One fit in full, which is the instance screen: the list's fields plus the
/// parameter blocks, the encoding and every grid point that was tried.
///
/// The state is deliberately **not** here. It is the provider's serialised fit,
/// opaque to everything but the provider, and it is the big column: a random
/// forest's is every tree.
fn model_instance_detail_schema() -> TypeSchema {
    let TypeSchema::Struct(summary) = model_instance_schema() else {
        // `model_instance_schema` is a struct literal above; this arm cannot be
        // reached and returning the summary is the harmless reading if it were.
        return model_instance_schema();
    };
    TypeSchema::Struct(
        summary
            .into_iter()
            .chain([
                // Scalar, table or text — three renderings, and the admin UI
                // never has to know what a coefficient, a cluster centre or an
                // explained-variance ratio is (§7).
                StructField::new("parameters", TypeSchema::array(TypeSchema::json())),
                StructField::new("encoding", TypeSchema::json()),
                // Every hyperparameter point tried and what it scored, so the
                // search is inspectable and not a number that appeared (§11).
                StructField::new("search", TypeSchema::array(TypeSchema::json())),
                // A posterior's output variables: each one's shape and the
                // dimension labelling each axis (Stan TODO §16) — what the
                // draws and the summary endpoints are asked about.
                StructField::new("variables", TypeSchema::json()),
                // A posterior's binding report: rows read and bound, drops, a
                // line per data variable.
                StructField::new("binding", TypeSchema::optional(TypeSchema::json())),
                // Whether the model's program differs now from the one this
                // fit snapshotted (Stan TODO §§6, 18); null for a provider with
                // no program, or when it cannot tell.
                StructField::new("program_changed", TypeSchema::optional(TypeSchema::bool())),
            ])
            .collect(),
    )
}

/// One prediction, and which row it is for.
///
/// `value` is what the prediction **writes into a row** (§12) — the number, the
/// class name, the cluster index or the vector — beside the structured
/// `prediction` the screen renders, which also carries the probability where the
/// provider gave one.
fn prediction_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("prediction", TypeSchema::json()),
        StructField::new("value", TypeSchema::json()),
        // The row's primary key, for a prediction over the dataset; null for a
        // literal row, which has none.
        StructField::new("key", TypeSchema::optional(TypeSchema::text())),
    ])
}
