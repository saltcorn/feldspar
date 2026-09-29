# Saltcorn v2 — Models without actions of their own (milestone 31)

Ordered, checkable task list for the thirty-first milestone after the MVP. Earlier lists are
archived in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP) and
`docs/TODO-post-mvp-1.md` … [docs/TODO-post-mvp-30.md](./TODO-post-mvp-30.md) (most
recently: Bayesian models with Stan). The predictive-models milestone is
[docs/TODO-post-mvp-22.md](./TODO-post-mvp-22.md); both are now
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md) §14.2.

The last two milestones gave models three actions: `predict_row`, `write_posterior` and
`fit_model`. Two of them are too specific. Every trigger form lists them, including for an admin
who will never build a model, and GOALS asks for a **minimal** action set, because control flow
belongs to workflows rather than to more and more actions. `write_posterior` only means something
for one kind of provider. `predict_row` is what `update_rows` does, plus one value it cannot
compute. This milestone keeps the one action that is generic, removes the other two, and moves
their capabilities to where values are already computed: **methods on a model object in code
bodies**, and a **`predict()` function in formulas**, including non-stored calculated fields.

**Milestone definition of done:** the models tutorial's `houses` table has the `House prices`
linear regression (with `neighbourhoodⱵaverage_income` and `viewingsↃhouse.length` in its
dataset). The admin adds a non-stored calculated field `estimated_price` whose expression is
`predict("House prices")`. Listing `houses` over the REST API returns a number in
`estimated_price` for every row, with **one** provider call for the page, including for an
unsold house that the dataset's filter excludes. Filtering on `estimated_price` is refused with a
sentence naming the field. A nightly trigger runs `fit_model` on `House prices` with
`activate: if_clean`. The new fit becomes active, and the next read of `estimated_price` follows
it without anything being edited. A stub provider that reports a warning leaves its new fit
inactive. For **Radon**, a workflow runs `fit_model`, then a `run_js_code` step that does
`const m = await models.get("Radon"); await m.writePosterior({ variable: "alpha", statistics:
{ mean: "alpha_mean", sd: "alpha_sd" } })`. That fills `counties.alpha_mean` and fires the
`counties` update trigger. The same body calls `m.predict(...)` on a posterior and gets a
sentence pointing at `m.draws`; `models.get("House prices").draws` is absent, and calling it
gets a sentence too. The Python body does the same through `m.write_posterior(...)`.
`listActions` has `fit_model` and has neither `predict_row` nor `write_posterior`.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The specification

### 1. The rule this milestone applies

An action belongs in the action set only if it is **generic**: it makes sense for every table,
every provider, every application. A capability that exists for one kind of model is a
**method** of that model, reached from code. A computed value belongs in a **formula**, which
already has a place to live in every action that writes rows (`insert_row` values, `update_rows`
assignments, `only_if`, `{{ }}` templates) and in calculated fields.

| Capability | Before | After |
|---|---|---|
| Refit a model | `fit_model` | `fit_model`, generic across providers (§2) |
| Predict a row | `predict_row` action | `predict("Model")` in any formula (§4); `m.predict(row)` in code (§3) |
| Write a posterior back | `write_posterior` action | `m.writePosterior({...})` in code (§3), only on a posterior |
| Read draws / summary / fit | `models.draws(name, var)` etc. | `m.draws(var)`, `m.summary(var)`, `m.fit` (§3) |

**The admin API does not change.** `predictRows`, `writePosterior` and the posterior screen's
**Write back** button stay, because they are the admin's own tools and not the action namespace.
The one `write_posterior()` function they share moves from `sc-core-actions` into `sc-api`
(`sc_api::models`), which already owns the row layer and depends on `sc-model`, so the admin
handler and the code host (§3) call the same function.

**No migration.** A stored trigger that names `predict_row` or `write_posterior` loads as a
broken trigger ("unknown action"), listed with its reason and editable, which is how any trigger
whose action went away already behaves. Nothing goes in `TABLES_RENAME.sql`: no table or column
changes.

### 2. `fit_model`, for every provider

The action's steps are already provider-neutral: `validate_model`, then
`FitStarter::start_fit`, then optionally wait. What is not neutral is **"activate when clean"**.
"Clean" means "fitted with no warnings", and today only a posterior produces warnings. For a
random forest the option really means "if it didn't fail". So the fix is in the provider
interface, not the action:

- **`FitResult` gets `warnings: Vec<String>`** (`FitResult::warning("…")`), as sentences that
  say what to do, like a posterior's. A Python provider's `fit` may return
  `{"state": …, "parameters": […], "warnings": ["…"]}`. `@sc.model_provider`'s docstring and
  `tutorial-python.md` say so, and a `warnings.warn` from inside `fit` (sklearn's
  `ConvergenceWarning`) is caught and added. The fit job merges them with the posterior
  diagnostics into `ATTR_WARNINGS`, so `fitted_cleanly` means the same thing for every provider.
- **Configuration:** `model` (the picker), `activate`: `never` | `if_clean` | `always`
  (default `never`), `wait` (default true) and `name`.
- **The answer:** `{ instance, status, active, error, warnings, metrics }`. With `metrics`, a
  later workflow step can branch on `metrics.test.r2` without a new activation rule.
- The description loses its posterior wording: "Fit a model again, and optionally make the new
  fit active".

### 3. The model handle in code bodies

`models` stays a `const` built over the run's `db` handle (so it is on the run's call budget,
and a body with no `db` has no `models`). Its one function returns a **handle**:

```js
const m = await models.get("House prices");              // the model's active fit
const m = await models.get("House prices", { fit: id }); // a specific fit
m.name; m.provider; m.table; m.outcome;                   // outcome as recorded on the fit
m.fit;   // { id, name, status, active, created, error, warnings, metrics, parameters }

// every fit whose outcome predicts (regression, classification, clustering, embedding)
await m.predict(row);                    // → 312000 | "spam" | 3 | [0.1, …]
await m.predict([r1, r2, r3]);           // one provider call, answers in row order
await m.predict(row, { detail: true });  // → { value, probability } (probability for a class)

// a Posterior outcome only
await m.draws("alpha", { keys: [27001], chains: [1, 2], thin: 10 });
await m.summary("alpha", { keys: [27001] });
m.variables;                             // what the fit drew, `__` internals left out
await m.writePosterior({ variable: "alpha", statistics: { mean: "alpha_mean", sd: "alpha_sd" } });
await m.writePosterior({ variable: "y_future", mode: "insert", table: "forecasts",
                         statistics: { mean: "mean", q5: "lower", q95: "upper" },
                         coordinates: [{ axis: "day.future", field: "day" }] });
```

Python has the same handle, synchronously and in snake case: `m = models.get("Radon")`,
`m.predict(row)`, `m.predict(rows, detail=True)`, `m.draws("alpha", keys=[27001])`,
`m.summary("alpha")`, `m.write_posterior(variable="alpha", statistics={...})`.

The rules:

- **What a row is.** A row that carries the model table's primary key is read **through the
  model's dataset**, by key and unfiltered (design §14.2's `predict_subject` rule), so a join
  path and an aggregation are computed as they were at fit time. A row without one (not inserted
  yet, or made up) is taken as the dataset's columns as given
  (`Subject::Rows`) and must supply every feature. A missing one is refused by name. In a batch,
  keyed rows are read in one query and literal rows go in one frame.
- **The fit's recorded outcome decides which methods exist, not the provider's name.**
  `draws`, `summary`, `variables` and `writePosterior` exist on a handle whose outcome is
  `Posterior`. Today only Stan produces one, and a Bayesian provider from a module would get them
  unchanged. On any other handle they are **absent**, and calling one throws a sentence ("`House
  prices` is a linear_regression regression; draws are for posterior models"). JavaScript uses a
  getter that throws; Python uses `__getattr__`. `predict` exists on every handle; on an outcome
  that does not predict it throws `no_per_row_prediction`'s sentence. There is no per-provider
  method registry. One can be added when a provider needs a method nobody else has.
- **Authority.** A prediction and a draws read read the admin's own fit, as today. **A
  `writePosterior` writes under the handle's authority and the run's trigger chain.** That is
  the `db` handle's, the same as `db.counties.update(…)`: ownership is checked, the target's
  triggers fire, and the chain bounds recursion. The removed action wrote as admin. A trigger
  body that needs admin writes gets them the way any code body does.
- **The wire.** `op: "models"` requests on the `db` host, with `what`: `get`, `predict`,
  `draws`, `summary`, `write_posterior`. `get` answers everything the handle needs to be built
  without another call (the fit, the outcome, the variables). The later calls name the **fit
  id** the `get` resolved, so a handle does not change fit halfway through a body when someone
  activates another. The flat `models.draws(name, var)`, `models.summary` and `models.instance`
  are removed (prototype: no compatibility layer).
- **The seam.** The code host has a `Catalog`, and a prediction needs the provider registry and
  the `DatasetSource`. Both are reached through the `ModelHost` seam of §4, which is installed
  on the `Catalog`. The code host adds nothing of its own.

### 4. `predict()` in formulas

```js
predict("House prices")      // this row, through the model's active fit
```

**One argument, a string literal: the model's name.** Pinning a fit is what `active` is for,
and a formula that named a fit id would break the day that fit was deleted. It returns the
plain value, the one `predict_row` wrote: a number, a class name, a cluster index or a vector.
`predict` is a global of the formula language. A **column** called `predict` shadows it, which
is the scope rule's one rule. The built-in wins over a module function called `predict`, which
code bodies can still reach as `modfn("…").predict`.

**It is hoisted, exactly as a module function call is** (design §4b):

- `sc-expr`'s `analyze` recognises `predict(<string literal>)` and collects it into a new
  `Analysis::model_calls: BTreeSet<ModelCall { key, model }>`, keyed by `hoisted_call_key`'s text
  (`predict("House prices")`). A non-literal argument, a second argument, and a call inside `=>`
  are refused on save, with sentences in the module-call style.
- `translate` answers `Untranslatable` for it, so no SQL path ever tries to compute one.
- `sc_catalog::prefetch_bindings` resolves each model call before the formula runs, after the
  join paths, and binds the value under the key. The formula isolate stays op-less and does no
  I/O. The **row** is the row being evaluated: by its key when it has one, otherwise its values
  as literal dataset columns (a proposed row on the write path), under §3's rule.

**The seam: `ModelHost` on the `Catalog`.** `sc-catalog` sits below `sc-model`, so it cannot
call `predict_subject`. It declares a trait that speaks JSON and installs it the way
`module_functions()` is installed:

```rust
#[async_trait]
pub trait ModelHost: Send + Sync {                        // sc-catalog
    /// Predict `rows` of `table` with `model`'s active fit (or `fit`), in row order.
    async fn predict(&self, model: &str, fit: Option<&str>, table: &str,
                     rows: PredictRows<'_>, detail: bool) -> Result<Vec<Json>>;
    /// What a formula's save check needs: the model's table and whether its outcome
    /// predicts, and into which basic types.
    async fn describe(&self, model: &str) -> Result<ModelSummary>;
}
pub enum PredictRows<'a> { Keys(&'a [Json]), Values(&'a [Json]) }
```

`sc-server`'s `ModelServices` implements it over the provider registry and the `DatasetSource`,
and sets it on the `Catalog` at startup and again whenever the module set is rebuilt (the same
act that swaps the action registry today). A server with no model support has none, and a
formula that calls `predict` fails **naming the call**, never with a null, which is the module
functions' rule.

**Save-time checks** (async, because models are rows). They run in `schema_edit` for a
calculated field and on trigger save for an action's formulas and `only_if`:

- the model exists;
- its table is the formula's table ("`House prices` is a model of `houses`, and this formula is
  on `orders`");
- its outcome predicts (a t-test, or a posterior with no prediction quantity, is refused with
  `no_per_row_prediction`'s sentence);
- for a calculated field, the field's declared type is among the outcome's
  `possible_prediction_types`. This is the check `predict_row` made against its target field.

An **ownership formula refuses `predict`** outright, for the module-function reason: `Err` is
deny, and a rule that waits on a provider makes every read wait on it.

**Where it works:**

- **Action formulas**: `insert_row` values, `update_rows` assignments, `only_if`, `{{ }}`
  templates in `send_email`, `fetch` and the rest. These already go through
  `prefetch_bindings`, so they need nothing beyond the hoist. This is the direct replacement for
  `predict_row`: an insert trigger on `houses` whose `update_rows` sets
  `estimate = predict("House prices")`. A workflow that wanted the prediction in its context
  uses a formula step.
- **Non-stored calculated fields.** These need the read path to change. Today `rows.rs`'s
  `calc_projections` computes calculated fields only in SQL and **silently skips** one that does
  not translate (`crates/sc-api/src/rows.rs:1014`). This milestone adds the missing fallback:
  - After the `SELECT`, every calculated field that did not translate is evaluated by the
    reified evaluator over the fetched page, in the calculated fields' dependency order (a field
    that reads a predicting field sees its value).
  - **Predictions are batched per page**: for each model named on the page, one
    `ModelHost::predict` with `PredictRows::Keys` of every row's key, so a 50-row page is one
    dataset read and one provider call, not fifty. The other hoisted values (join paths, module
    calls) are resolved by `prefetch_bindings` per row, as they are on the write path.
  - The fallback is **general**. Any untranslatable calculated field is computed rather than
    skipped, which also covers a module function call in a calculated field.
  - Such a field **cannot be filtered or sorted on**. A `where` or `orderBy` that names it is
    refused with a sentence naming the field and saying why ("`estimated_price` is computed after
    the rows are read, because it calls `predict`"). Everywhere a query lowers a filter
    (`filter.rs`, GraphQL, the CSV export) refuses the same way.
  - **Errors fail the read, naming the field, the model and the row** (an unseen category, a
    model with no active fit). This is the system's position on silent failure and the rule the
    module functions already follow. A model with no active fit makes its table's reads fail
    until one is activated, and the save-time check warns about that when the field is added.
  - Every read path that projects calculated fields goes through the one fallback: the four
    `calc_projections` call sites in `rows.rs`, plus GraphQL and CSV if they project calculated
    fields separately. There must be one implementation, because two would drift.
- **Stored calculated fields** do not exist yet. When they do, `predict` in one is refused. A
  prediction depends on rows the model reads through its dataset, and recomputing it on every
  write to every one of them is the objection design §14.2 already records.

### 5. Where the code goes

| Crate | What changes |
|---|---|
| `sc-model` | `FitResult::warnings`; the fit job merges them into `ATTR_WARNINGS` |
| `sc-python` | the fit payload's `warnings`, `warnings.warn` caught in `fit`; the `Models` handle class in `saltcorn.py` |
| `sc-expr` | `ModelCall`, `Analysis::model_calls`, the hoist in `analyze`, `Untranslatable` in `translate`; the JavaScript `models` prelude becomes the handle |
| `sc-catalog` | the `ModelHost` trait, `Catalog::set_model_host`/`model_host`, model calls in `prefetch_bindings` |
| `sc-api` | `models::write_posterior` (moved); the code host's `get`/`predict`/`write_posterior`; the calculated-field fallback and batching in `rows.rs`; the save checks in `schema_edit.rs`; filter/sort refusals |
| `sc-core-actions` | `predict_row.rs` deleted; `posterior.rs` keeps only `FitModel` (renamed `fit_model.rs`); `register_model_actions` takes the registry and the `FitStarter` only |
| `sc-server` | `ModelServices` implements `ModelHost` and installs it; handlers call `sc_api::models::write_posterior`; the trigger save path runs the formula checks |
| `ui/admin` | `ModelForm.tsx` and `PosteriorInstance.tsx` wording; `codeTypes.ts` declares the handle |

---

## Phase 1 — One model action

- [x] 1.1 `FitResult::warnings` and `FitResult::warning(…)`; the fit job writes provider
      warnings into `ATTR_WARNINGS` beside the posterior diagnostics; the Python
      `@sc.model_provider` payload's `warnings` and `warnings.warn` captured during `fit`.
      Tests: a stub provider with a warning gives an instance with that sentence; a clean one
      gives none.
- [x] 1.2 `fit_model` generic (§2): `activate: never | if_clean | always`, `metrics` in the
      answer, the neutral description. Tests: fired from a trigger over a
      `linear_regression` model; `if_clean` with the warning stub stays inactive; `always`
      activates it anyway; `wait: false` with `if_clean` is activated by the job.
- [x] 1.3 Remove `predict_row` and `write_posterior`: delete `predict_row.rs`, move
      `write_posterior()` and its helpers (target checks, count rounding, dates as days) to
      `sc_api::models`, point `handlers.rs`'s `writePosterior` at it, and shrink
      `register_model_actions` and its callers (`sc-cli/src/main.rs`, `sc-server`'s
      `triggers.rs`, `modules.rs`, `apps.rs`). Rewrite the tests that used them
      (`posterior_api.rs`, `trigger_admin_api.rs`, `model_admin_api.rs`, `stan_models.rs`) to
      keep what they covered through the admin API, until Phases 2 and 3 give them their new
      home. The `sc-core-actions` test asserts the model action set is exactly `fit_model`.
- [x] 1.4 The admin UI's wording: `ModelForm.tsx`'s "what a `predict_row` action names this
      model by" and `PosteriorInstance.tsx`'s "the `write_posterior` action does the same"
      now point at `predict()` and at `m.writePosterior`. The strings go in the `admin` i18n
      domain.

## Phase 2 — The model handle in code

- [x] 2.1 The `ModelHost` trait in `sc-catalog` (§4), with `Catalog::set_model_host` and
      `model_host()`; `ModelServices` implements it over `predict_subject` (keys become a
      `Subject::Dataset` restricted to those keys; values become `Subject::Rows`) and is
      installed at startup and on every module rebuild. Tests: keyed rows come back in the
      order asked, including a row the dataset filter excludes; a literal row missing a
      feature is refused by name; `detail` carries a class's probability.
- [x] 2.2 The code host's `op: "models"` (§3): `get` (resolving a name to its active fit, or
      `fit` to that fit, answering fit, outcome, table and variables), `predict`, `draws`,
      `summary` and `write_posterior` by fit id; `write_posterior` through the handle's
      authority, chain and executor. Tests in `code_host/tests.rs`.
- [x] 2.3 The JavaScript prelude's `models.get` and the handle: the posterior-only methods as
      throwing getters on any other outcome; `predict` accepting a row or an array. Tests in
      `sc-expr`'s code tests over a fake host.
- [x] 2.4 Python's `Models.get` and the handle class, with `__getattr__` for the absent
      methods. Tests in `python_models.rs` (needs `--features python-host`; say so in the
      CHANGELOG if it cannot be run here).
- [x] 2.5 `codeTypes.ts` declares `models.get` and the handle (the posterior methods as
      optional members); `codeTypes.test.ts`. The MCP page `code_api_js.md` documents the
      handle.
- [x] 2.6 Integration tests in `sc-server`: a `run_js_code` trigger that predicts the event's
      row and writes it with `db`; a workflow `fit_model` → `run_js_code` `writePosterior`
      over the stub posterior provider, firing the target table's trigger; a non-admin
      body's `writePosterior` into a table it may not update is refused.

## Phase 3 — `predict()` in formulas and calculated fields

- [x] 3.1 `sc-expr`: `ModelCall`, `Analysis::model_calls`, the hoist in `analyze` (literal
      string only, one argument, not inside `=>`, shadowed by a column), `Untranslatable` in
      `translate`. Unit tests beside the module-call ones.
- [x] 3.2 `prefetch_bindings` resolves model calls through `model_host()`: by key when the row
      has one, otherwise from its values; no host is an error naming the call. Tests in
      `sc-catalog` over a fake `ModelHost`.
- [x] 3.3 Save-time checks (§4): in `schema_edit` for a calculated field (existence, table,
      outcome predicts, declared type among `possible_prediction_types`, a notice when the
      model has no active fit) and on trigger save for action formulas and `only_if`;
      ownership formulas refuse `predict`. Tests for each refusal's sentence.
- [x] 3.4 Action formulas: an insert trigger on `houses` with `update_rows` setting
      `estimate = predict("House prices")` writes the number; a `{{ predict(…) }}` in a
      template renders it. (Nothing to build beyond 3.1–3.3; this task is the test.)
- [x] 3.5 The read-path fallback in `rows.rs` (§4): untranslatable calculated fields evaluated
      after the `SELECT` in dependency order, predictions batched per page and per model, the
      silent skip removed; every read path that projects calculated fields shares it. Tests:
      a page of 50 houses is one `ModelHost::predict` (count the calls on a fake); a field
      that reads the predicting field sees its value; a module-function calculated field that
      was skipped before is now computed; an unseen category fails the read naming the field,
      the model and the row.
- [x] 3.6 Filtering and sorting on a fallback-computed field are refused with §4's sentence in
      REST, GraphQL and the code host's query plans. Tests.

## Phase 4 — Documentation and the definition of done

- [x] 4.1 `docs/TECHNICAL_DESIGN.md`: §10's built-in action list; §14.2's table row "prediction |
      … | the `predict_row` action", "Prediction: the action, and the calculated field there is
      not" rewritten as "Prediction: a formula and a method", "Reading and writing back"
      (`write_posterior` action and `models.draws` replaced by the handle); §4b gains model
      calls beside module calls; §6.2's calculated fields gain the read-path fallback and its
      filter/sort rule; `ModelHost` in the seams table.
- [x] 4.2 Tutorials: `tutorial-models.md` (the trigger that applies the model becomes the
      `estimated_price` calculated field, and an `update_rows` for the stored variant),
      `tutorial-stan.md` (write back from a code step after `fit_model`),
      `tutorial-triggers.md`, `tutorial-python.md` (the handle, and `warnings` in a provider);
      `README.md`, `OPERATIONS.md` where they name the removed actions.
      `crates/sc-cli/tests/repo_hygiene.rs`'s fragments (`predict_row`, "There is no
      calculated field that predicts") follow the documents.
- [x] 4.3 The definition of done as one `sc-server` test (`models_without_actions.rs`), over
      `linear_regression` and the stub posterior provider (no CmdStan), with the Radon half
      also in `stan_models.rs` behind its `#[ignore]`.

---

## Explicitly OUT of scope for this milestone

- **An `if_better` activation rule** (comparing a new fit's test metric with the active fit's).
  Decided against; `fit_model`'s answer carries `metrics`, so a workflow can make that
  decision itself.
- **Stored calculated fields**, and so `predict` in one (§4).
- **Prediction for new rows from a posterior**: still carried (below). When it is picked up it
  arrives as `m.predict` and `predict()` over a posterior, not as an action.
- **Caching predicted values** between reads (carried from TODO-post-mvp-22).
- **A per-provider method registry** on the handle (§3).

## Carried past this milestone

- **Prediction for new rows from a posterior**: TODO-post-mvp-30's Phase 9, skipped (design
  in its §19). The `new` pseudo-dataset and `prediction` in the configuration, empty at fit
  time, resolving `Outcome::Posterior { prediction }`; standalone generated quantities
  re-binding `new` only against the instance's stored coordinates, with the compiled model
  from the cache or recompiled from the snapshot and the CSVs fetched from the runs store;
  `Prediction::Distribution { mean, sd, q5, q95 }`; `predict()`, `m.predict` and
  `predictRows` accepting a posterior fit, with an unknown county refused by name. Picking it
  up means undoing two things done in its absence: `OutcomeSpec::Posterior
  { prediction: None }` declares no prediction types (so `predict()` over a Stan model is
  refused on save), and `sc_model::no_per_row_prediction` sends a posterior to `m.draws`.
- **LOO/WAIC and an instance comparison view**: PSIS-LOO over `log_lik` in `sc-model`, and the
  side-by-side screen TODO-post-mvp-22 already wanted.
- **A formula front end generating Stan** (brms-style), which would make the binding
  automatic because the program would be ours.
- **Bayesian providers from modules**: `@sc.model_provider(binds_data=True)` receiving bound
  data and returning draws.
- **Geometry types and adjacency from `ST_Touches`.**
- From TODO-post-mvp-29: W.8. An agent's trait configuration is not redacted when the agent is
  read back (streams do this with `redact_attrs`/`merge_secrets`); `http`'s `headers` is
  declared `secret()` and will be covered when agents adopt it. Also the agent half of that
  milestone's definition of done, which needs an API key.
- From TODO-post-mvp-28: the rest of 3.6 (`de`, `es`, `zh-Hans` and `ar` for all three
  domains, and `fr` for `admin` and `builder`) and the non-JSX half of 3.4's sweep. The
  recipes are in [docs/TODO-post-mvp-28.md](./TODO-post-mvp-28.md).
- From TODO-post-mvp-27: the live-broker half of the streams definition of done (10.3).
- From TODO-post-mvp-26: running the agent eval against a real provider (11.4) and walking the
  agent milestone's definition of done by hand (12.3).
- From TODO-post-mvp-25: page groups, HTML-file pages, copilot layout generation, uploading from
  the builder, v1's help topics, formula-editor completions, replacing CKEditor 4, a menu editor,
  cloning pages and views, sharing library items, collaborative editing, and the builder in a
  plugin pattern's mode.
- From TODO-post-mvp-24: `room`/`workflow-room` and realtime, tags, file upload from an Edit
  view, themes as plugins, a v1 `db` module for plugins, and externalising inline handlers to
  drop `'unsafe-inline'` from Saltcorn UI's CSP.
- From TODO-post-mvp-22: statsmodels as a second bundled module, k-fold cross-validation,
  application-facing prediction, a fit as a durable workflow run, predicted-value caching.
