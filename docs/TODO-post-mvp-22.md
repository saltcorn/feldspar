# Saltcorn v2 — Predictive models

Ordered, checkable task list for the twenty-second milestone after the MVP. Earlier lists are
archived in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP),
[docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md) (file stores + the React framework),
[docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md) (the `_sc_tables`/`_sc_fields` overlays,
rich types and File fields), [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md) (ownership
formulae, calculated fields and row-level security),
[docs/TODO-post-mvp-4.md](./TODO-post-mvp-4.md) (actions and triggers),
[docs/TODO-post-mvp-5.md](./TODO-post-mvp-5.md) (the file-store IDE),
[docs/TODO-post-mvp-6.md](./TODO-post-mvp-6.md) (agents),
[docs/TODO-post-mvp-7.md](./TODO-post-mvp-7.md) (the GraphQL provider),
[docs/TODO-post-mvp-8.md](./TODO-post-mvp-8.md) (REST queries, custom SQL and the
generated client), [docs/TODO-post-mvp-9.md](./TODO-post-mvp-9.md) (table constraints
and indexes), [docs/TODO-post-mvp-10.md](./TODO-post-mvp-10.md) (email),
[docs/TODO-post-mvp-11.md](./TODO-post-mvp-11.md) (tables in code),
[docs/TODO-post-mvp-12.md](./TODO-post-mvp-12.md) (concurrent code bodies),
[docs/TODO-post-mvp-13.md](./TODO-post-mvp-13.md) (modules),
[docs/TODO-post-mvp-14.md](./TODO-post-mvp-14.md) (SQLite),
[docs/TODO-post-mvp-15.md](./TODO-post-mvp-15.md) (modules in-process),
[docs/TODO-post-mvp-16.md](./TODO-post-mvp-16.md) (table providers),
[docs/TODO-post-mvp-17.md](./TODO-post-mvp-17.md) (writable table providers),
[docs/TODO-post-mvp-18.md](./TODO-post-mvp-18.md) (workflows),
[docs/TODO-post-mvp-19.md](./TODO-post-mvp-19.md) (the Python code adapter),
[docs/TODO-post-mvp-20.md](./TODO-post-mvp-20.md) (the administration MCP server) and
[docs/TODO-post-mvp-21.md](./TODO-post-mvp-21.md) (bundled modules).
Scope and rationale remain in [docs/GOALS.md](./GOALS.md) ("Model providers",
"Datasets", "Predictive models") and [docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md)
(**§14.2**, which this milestone replaces).

Everything in this system so far *retrieves*: a query answers what is in the tables, an
expression computes what follows from a row, an agent asks a model about text. Nothing yet
answers **"what does this data imply about a row I have not seen"** — or the question that is
often the real one, **"what does this data imply, full stop"**: is the coefficient on price
negative, do the two groups differ, how many clusters are there.

This milestone is that. A **model** is a saved question about a table: which rows and which
derived values make up the data (the **dataset**), which provider answers it, and with what
settings. Fitting one produces a **model instance** — the fitted parameters, the metrics, and
enough state to apply it to a new row. Both halves of the point are first class: an instance
you inspect (the coefficients, the test statistic, the explained variance) and an instance you
apply (a predicted price on a row a trigger just inserted).

**Milestone definition of done:** an admin opens the Models tab on a server with a `houses`
table, creates *House prices*, and builds its dataset by picking columns — `price`,
`bedrooms`, `neighbourhoodⱵaverage_income`, `viewingsↃcount` — and a filter, `sold`. They
choose **Linear regression**, name `price` as the label, and press Fit. Seconds later the
instance shows a coefficient table with standard errors, *t* and *p* for each feature, R² and
RMSE on the training and test rows, and the number of rows each was fitted on. They mark it
active. A trigger on `houses` with a `predict_row` action now writes `estimated_price` on
every insert. On the same screen, the same dataset, they fit a scikit-learn gradient-boosting
model from a bundled module and compare its RMSE against the regression's — and nothing on the
screen knows that one of the two answers came from Python.

**Not in this milestone:** Bayesian inference (mc-stan), which needs cmdstan on the host and a
model *file* rather than a form; statsmodels as a second bundled module; k-fold
cross-validation; and any application-facing (REST/GraphQL) prediction endpoint. Each is named
under *Carried past this milestone* with what it would take.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The specification

### 1. The shape of the thing, end to end

Five nouns, and it is worth fixing them before anything else because the words are overloaded
everywhere else in the industry.

| noun | what it is | where it lives |
| --- | --- | --- |
| **model provider** | code that can fit something — `linear_regression`, `kmeans`, a module's `sklearn` | a registry, like actions |
| **dataset** | which table, which derived columns, which rows | a JSON column *on the model* |
| **model** | a dataset + a provider + its configuration + its hyperparameter space | `_sc_models` |
| **model instance** | one fit: parameters, metrics, encoding, serialised state | `_sc_model_instances` |
| **prediction** | applying an instance to rows | the `predict_row` action, and an endpoint |

A model is edited and refitted; each fit leaves an instance behind, so the instances of a model
are its history and are comparable — same dataset, same split, different settings. One instance
per model may be **active**, which is what lets a trigger name a model rather than a fit.

### 2. A dataset is a list of formulas, and that is the whole of it

GOALS asks for "table fields and derived fields such as calculations, joinfields and
aggregations, and any inclusion/exclusion criteria on the rows". This system already has one
language that is exactly those four things: the calc-field/ownership expression language
(`sc-expr`), with `Ⱶ` for a join path and `Ↄ` for an aggregation over an incoming key, already
translated to SQL by `translate_value`, already falling back to the reified evaluator when a
construct has no SQL counterpart, and already proven at parity between the two.

So a dataset is:

```rust
pub struct Dataset {
    pub table: String,
    pub columns: Vec<DatasetColumn>,   // { name, expr }
    pub filter: Option<String>,        // one boolean formula, or none
}
```

and `expr` is a formula: `price`, `price / area`, `neighbourhoodⱵaverage_income`,
`viewings.filter(v => v.attended).length` — whatever the calc-field editor already accepts,
validated against the same `SchemaShape` with the same errors. There is **no second vocabulary**
of "field / joinfield / aggregation" with three shapes in the JSON and three code paths behind
it. The admin UI still offers a picker — click a field, click a join path, click an aggregation
— but what the picker *writes* is a formula, and a user who wants `log(price)` types it.

`user` and the operation flags are out of scope in a dataset formula for the same reason they
are out of scope in a calc field: a dataset has no caller, and a fit that meant something
different depending on who pressed the button would be indefensible.

### 3. The dataset lives on the model, and has no tab

GOALS says so ("Datasets do not have their own tab in the admin UI"), and the reason is worth
recording. A shared, named dataset would need a lifecycle — what happens to the four models
fitted against it when somebody adds a column, whether an instance fitted against version 1 is
still readable, whether deleting it is allowed. That is a versioning problem bought for a
saving (retyping a column list) that a **Duplicate model** button answers instead. So
`_sc_models.dataset` is a JSON column, the definition belongs to that model, and `Dataset` is a
plain Rust value with no store, no id and no name.

### 4. `sc-model` at layer 6, and the two seams that put it there

The crate goes **beside `sc-action`, before `sc-module`** — not above the row layer where its
data comes from — for one reason: a module supplies model providers the way it supplies actions
and table providers, and `sc-module` (layer 6) can only implement a trait declared below it.
That is the same placement argument `sc-action` already carries, and `TableProviderHost`
(declared in `sc-catalog` at layer 4, implemented in `sc-module` at layer 6) is the same shape.

The cost is that `sc-model` cannot read a row: `sc-api::rows` is layer 8, and going around it
would mean a dataset that ignored non-stored calc fields, ownership and RLS, and could not read
a provided table at all. So the crate declares the seam and somebody above the row layer fills
it in — exactly as `sc-agent` declares `ProviderConnector` and `sc-server` supplies it:

```rust
/// How a dataset becomes rows. Implemented in `sc-server` over `sc_api::rows`.
#[async_trait]
pub trait DatasetSource: Send + Sync {
    async fn materialise(&self, ds: &Dataset, cap: u64) -> Result<Frame>;
}

/// The model providers a module supplies. Implemented in `sc-module` and `sc-python`.
#[async_trait]
pub trait ModelProviderHost: Send + Sync {
    fn providers(&self) -> Vec<ModelProviderKind>;
    async fn fit(&self, p: &str, frame: &Frame, cfg: &Attrs, hp: &Attrs) -> Result<FitResult>;
    async fn predict(&self, p: &str, state: &Json, frame: &Frame) -> Result<Vec<Prediction>>;
}
```

`ModelServices` in `sc-server/src/models.rs` assembles the pieces the way `AgentServices` and
the trigger dispatcher already are: the registry (built-ins plus one entry per module-supplied
provider, rebuilt on every module change), the `DatasetSource`, and the fit job runner.

### 5. The split is a hash of the primary key, not a shuffle

A fit divides its rows into **train**, **validation** and **test**. The obvious implementation
shuffles a vector with a seeded RNG. This one instead assigns each row by hashing its primary
key with the fit's seed and taking the fraction — which costs the same and buys three things:

- **It does not depend on row order**, so a dataset materialised with a different `ORDER BY`, a
  different `LIMIT`, or off a table provider that answers in feed order splits identically.
- **A refit after new rows arrive keeps every old row on the side it was on.** The test metric
  of instance 7 is therefore comparable with the test metric of instance 3, which is the entire
  reason anybody looks at two instances of one model.
- **It is reproducible from the row, not from the run** — an instance records its seed and
  fractions, so the question "was this row in the training set" is answerable afterwards
  without storing a list of ids.

The price is that the fractions are approximate on small datasets (200 rows at 20% test is
whatever the hash gives, not exactly 40). The instance records the counts it actually got, so
nobody has to guess.

A dataset whose table has **no single primary key** cannot be split this way and is refused by
name, saying so: there is nothing stable to hash. (Reads are unaffected — that restriction is
the fit's, not the dataset's, and an unsupervised fit with no split is still allowed.)

### 6. The encoding belongs to the instance

A model provider wants numbers. A dataset column is a string, a boolean, a date or a float. The
translation — one-hot for a categorical feature, a label mapping for a classification target,
an epoch-seconds cast for a date, standardisation where a provider asks for it — happens once,
in `sc-model`, and the **result is stored on the instance**:

```rust
pub struct Encoding { pub columns: Vec<ColumnEncoding>, pub target: Option<TargetEncoding> }
```

This is the single most load-bearing decision in the milestone, because the failure it prevents
is silent. If prediction re-derived the one-hot column order from whatever categories happen to
be in the rows being predicted, a model fitted when `region` had four values and applied to a
batch containing three would put every coefficient against the wrong column and return
confident nonsense. Fitting the encoding once and carrying it means a prediction is encoded
**the way the fit was**, or it fails.

And it fails loudly. A category at predict time that was not present at fit time is an error
naming the column and the value — not a row of zeros, which is the industry's usual answer and
is a prediction from a model that was never shown this input. A null in a feature is a dropped
row at fit time (counted, and reported on the instance) and an error at predict time, for the
same reason: at fit time dropping is a defensible sample restriction that we report; at predict
time it would mean answering a question about a row we cannot represent.

### 7. Metrics are the host's, parameters are the provider's

A provider returns `FitResult { state, parameters }` and **no metrics**. `sc-model` computes
them, by running the fitted state back over each split and scoring the predictions:

- **regression** — R², RMSE, MAE per split
- **classification** — accuracy, per-class precision/recall/F1, and the confusion matrix
- **clustering** — cluster sizes and within-cluster sum of squares
- **dimensionality reduction** — explained variance per component
- **hypothesis test** — nothing; the parameters *are* the answer

Two reasons. It makes providers **comparable** — the smartcore regression and the scikit-learn
one are scored by the same code on the same rows, so the number on the screen means one thing —
and it means a provider in another language does not have to reimplement R² to be a citizen
here. What a provider *does* own is its parameters, which is where the providers genuinely
differ, and those are structured for display rather than free JSON:

```rust
pub enum ParameterBlock {
    Scalar { name: String, value: f64 },
    Table  { name: String, columns: Vec<String>, rows: Vec<ParameterRow> },
    Text   { name: String, body: String },
}
```

`Table` is a coefficient table (estimate, std. error, *t*, *p*); `Text` is for a provider whose
own output is a summary nobody should reformat — statsmodels' `summary()` is the case this
variant exists for, and it can be added later without touching a schema.

### 8. Fitting is a job, not a request

A fit reads every row of a dataset and runs an optimiser over it. That is seconds at best and
minutes at worst, and it must not be an HTTP request that a proxy times out halfway through
while the work carries on invisibly.

So `fitModel` **creates the instance row first**, with `status = "fitting"`, returns its id, and
runs the fit on a spawned task that writes `fitted` (with parameters and metrics) or `failed`
(with the sentence) when it finishes. The screen polls. There is no in-memory job registry,
because the row is the registry.

Two consequences, both stated rather than discovered:

- **A fit does not survive a restart.** A process that dies mid-fit leaves an instance saying
  `fitting` forever, so boot reaps them: any instance still `fitting` at startup becomes
  `failed` with "the server restarted while this fit was running". Making a fit durable is the
  workflow engine's job and would mean expressing a fit as a workflow, which is a bigger claim
  than this milestone makes.
- **There is no cancel.** Stopping a fit means stopping a `smartcore` call or a Python call
  mid-flight, and §15.2 has already said what CPython can and cannot be interrupted at. The
  bound that exists is the row cap (§9), and it is the honest one.

### 9. The frame is columnar, and it is bounded

```rust
pub enum Column { Float(Vec<Option<f64>>), Int(…), Bool(…), Str(…), Null }
pub struct Frame { pub columns: Vec<(String, Column)>, pub rows: usize }
```

Columnar because every consumer wants a column: the encoder standardises one, the splitter
indexes rows across all of them, and a numeric matrix is built column-major anyway.

**Bounded** because a dataset is a `SELECT` an admin wrote and the server has to hold the answer
in memory. `--model-max-rows` (default 200 000) is the ceiling; a materialisation that would
exceed it is refused by name — "the dataset selects more than 200 000 rows; add a filter or
raise `--model-max-rows`" — rather than by the OOM killer. The count is asked for before the
rows, so the refusal costs one `COUNT(*)` and not a partial read.

### 10. What a provider is, and what its outcome is

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// The form, given the dataset's columns — a provider naming a label needs
    /// to offer *these* columns as its options.
    fn config_spec(&self, shape: &DatasetShape) -> Vec<FormField>;
    fn hyperparameters(&self) -> Vec<FormField>;
    /// What a fit of this configuration will produce. Not a constant: a random
    /// forest is a regressor or a classifier depending on its label's type.
    fn outcome(&self, shape: &DatasetShape, cfg: &Attrs) -> Result<Outcome>;
    fn validate(&self, shape: &DatasetShape, cfg: &Attrs) -> Result<()> { Ok(()) }
    async fn fit(&self, frame: &Frame, cfg: &Attrs, hp: &Attrs) -> Result<FitResult>;
    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>>;
}
```

`config_spec` takes the dataset's shape for the reason `Action::config_spec_for` takes the
catalog and the channel: a label picker with a free-text field would push the checking to fit
time and the guessing to the admin. `outcome` is a *function of the configuration* because
GOALS says it is — "the model provider defines what the outcome is, depending on the
configuration parameters" — and because the alternative is four providers where there is one
algorithm:

```rust
pub enum Outcome {
    Regression { label: String },
    Classification { label: String, classes: Option<Vec<String>> },
    Cluster,                        // a cluster number per row
    Embedding { dimensions: usize },// a vector per row
    Test,                           // no per-row output; the parameters are the result
}
```

`Outcome` is what the UI renders against, what the metrics are chosen by, and what `predict_row`
checks before it writes a number into a text column.

**Prediction takes a frame, not a row.** A single row is a frame of one. Batching is what makes
a Python provider usable at all (the call is the cost, not the arithmetic) and it is what lets
the metric pass score 50 000 rows in one call rather than 50 000.

### 11. Hyperparameters, and the search over them

A provider declares its hyperparameters as form fields. A **model** stores, per hyperparameter,
either a value or a **list** of values; a fit runs the grid of the lists, scores each point on
the **validation** split by the outcome's primary metric (R² for a regression, accuracy for a
classification), fits the winner, and reports the **test** metrics for it. The instance records
the chosen point and the score of every point tried, so the search is inspectable and not a
number that appeared.

A grid and a fixed three-way split, and not k-fold cross-validation, is the deliberate stopping
point: k-fold is *k* times the fits for a variance estimate that matters at hundreds of rows and
not at hundreds of thousands, and it changes nothing about the seam. It is named under
*Carried past*.

With no lists declared there is no search, the validation split is empty, and a fit is a fit —
which is the common case and must not pay for the uncommon one.

### 12. Prediction: the action, and the calc field there is not

`predict_row` is an ordinary action (`sc-action-builtins`, layer 9 with the others that write
rows): configure a model — or a named instance — and where the answer goes, either a field on
the row or a key in the workflow context. Its `config_spec_for` offers the models on *this*
table when the trigger has one, and it validates that the target field's type can hold what the
model's `Outcome` produces.

**There is no calculated field that predicts,** and the reason is not effort. A calc field is an
`sc-expr` formula with two evaluators that must agree, and a prediction is translatable to
neither SQL nor the reified evaluator; a *stored* one would have to be recomputed on every write
to every row the model reads, which for a model with an aggregation in its dataset is every row
of two tables. An action, fired by a trigger the admin wrote, puts the recomputation where
somebody chose it.

### 13. The built-ins, and the `smartcore` feature

GOALS: "The core rust code supplies model providers for regression, classification, clustering
and dimensionality reduction based on smartcore. this needs to be a cargo feature flag so it can
be disabled."

`sc-model`'s `smartcore` feature (**default on**, so `--no-default-features` is the opt-out) adds
five: `linear_regression`, `logistic_regression`, `random_forest` (regressor or classifier by
its label's type — the case `Outcome` exists for), `kmeans` and `pca`.

Two more are **not** behind it, because they are arithmetic and not machine learning:
`t_test` (one-sample, two-sample, paired, Welch) and `anova` (one-way). They are GOALS'
"statistical hypothesis testing" category, they need a distribution function and nothing else
(`statrs`), and a build with no smartcore should still be able to answer whether two groups
differ.

The regression provider computes **standard errors, *t* and *p* for every coefficient** — from
the residual variance and `(XᵀX)⁻¹`, about forty lines on top of the fit. smartcore does not
give them, and without them "a regression model where we are more interested in the slope
coefficients" (GOALS again) is a number with no way to tell whether it means anything.

A build without the feature lists the providers it has and says on the screen that the built-in
model providers were compiled out, rather than showing an empty list that reads like a bug.

### 14. Providers from modules, in both languages

The third source, and the one that makes this an extension point rather than a fixed menu. A
JavaScript module exports `modelproviders` beside its `actions` and `table_providers`; a Python
plugin decorates with `@sc.model_provider`. Both flatten to the same `ModelProviderKind` on the
manifest and route to the worker or interpreter that loaded them, exactly as a table provider
does — the machinery is built, and what this milestone adds is one more key at each end.

`plugins/sklearn` is the proof and the useful thing: a bundled Python module wrapping a curated
set of scikit-learn estimators, installed with one click from the Modules tab, appearing on the
model form beside the built-ins. It is the third bundled module and needs nothing new from the
mechanism.

The frame crosses the seam as columns, not as rows of objects: a 50 000 × 12 dataset is 12 JSON
arrays and not 50 000 JSON objects with the same twelve keys repeated, and on the Python side it
lands as something `numpy.asarray` takes directly.

### 15. Storage (§9's rule applied)

`_sc_models`: `id` (uuid pk), `name` (unique), `description`, `table_name`, `provider`,
`dataset` (JSON), `configuration` (JSON), `hyperparameters` (JSON — values or lists), `split`
(JSON — fractions and seed), `attributes` (JSON).

`_sc_model_instances`: `id` (uuid pk), `model` (uuid), `name`, `description`, `status`
(`fitting` | `fitted` | `failed`), `created`, `active` (bool), `state` (JSON — the provider's
serialised fit), `parameters` (JSON), `metrics` (JSON), `encoding` (JSON), `hyperparameters`
(JSON — the chosen point), `attributes` (JSON).

The judgements §9 asks for, made out loud: `status` is a column (every row has one, and it is
what the list filters on) while the **failure sentence** is in `attributes`, because it is
present only on the rows that failed. `active` is a column because at most one row per model
carries it and the uniqueness is enforced on save — a nullable column would be a second way to
say the same thing. `state` is a column and it is the big one; a provider that wants to store
bytes stores base64, because a system table with a `bytea` column would be the only one.

---

## Phase 1 — The dataset

- [x] 1.1 `crates/sc-model`, layer 6, in the workspace between `sc-action` and `sc-module`,
      with the layering comment saying why it is below the row layer it reads through (§4).
- [x] 1.2 `sc_model::dataset`: `Dataset`, `DatasetColumn`, `DatasetShape` (the column names and
      their inferred types, which is what a provider's `config_spec` is handed), and
      `validate_dataset` against a `SchemaShape` — each column's formula parsed and validated
      like a calc field, `user` and the operation flags refused by name, duplicate and empty
      column names refused, the filter validated in boolean position.
- [x] 1.3 `Dataset::select`: the `Select` a dataset becomes — one `Projection::expr_as` per
      column from `translate_value`, the filter from `translate`, against the dataset's table.
      Columns that will not translate are **not** an error here; they are the ones the row layer
      falls back to the reified evaluator for, which is the arrangement calc fields already have.
- [x] 1.4 `Frame` and `Column` (§9), `Frame::column`, `Frame::take_rows`, and the JSON encoding
      that crosses a module seam — columnar, one array per column.
- [x] 1.5 The `DatasetSource` seam, and `sc_server::models::CatalogDatasetSource` implementing
      it over `sc_api::rows::list_rows_query` with the projections from 1.3 — the row cap asked
      as a `COUNT(*)` first, and refused by name over `--model-max-rows`.
- [x] 1.6 `sc_model::split`: `Split { train, validation, test, seed }`, `assign(pk, seed)` by
      hash (§5), `Frame::split` returning three frames, the actual counts recorded, and the
      refusal when the table has no single primary key.
- [x] 1.7 Unit tests: a formula per column translating to the projection it should, a filter
      folding into the `WHERE`, `user` refused, the split stable across a reordered frame and
      across an appended one, and the primary-key refusal.

## Phase 2 — The seam, the registry and the store

- [x] 2.1 `ModelProvider`, `Outcome`, `FitResult`, `ParameterBlock`, `Prediction` (§7, §10) —
      the trait and the vocabulary, with no implementation behind them yet.
- [x] 2.2 `ModelRegistry`: the built-ins plus `ModelProviderHost`'s, assembled the way
      `ActionRegistry` is, rebuilt on every module change, a duplicate name refused naming both
      sources, and `kinds()` for the picker.
- [x] 2.3 `_sc_models`: the fields, `bootstrap_models`, the `Model` ⇄ row mapping read strictly
      (a missing or misshapen column is an error naming the model and the column, never a
      default), and `save_model` / `delete_model` / `models`.
- [x] 2.4 `_sc_model_instances`: the same, plus `active` enforced at most one per model on save,
      and `reap_fitting_instances` marking every `fitting` row failed at boot (§8).
- [x] 2.5 `validate_model`, run on save **and** on load: the dataset validates, the provider
      exists, the configuration validates against `config_spec(shape)` and the provider's own
      `validate`, the hyperparameters are known names, the split fractions sum to 1. A model
      that fails on load is listed with its reason and stays editable — the agent rule.
- [x] 2.6 Unit tests: the round-trip through both tables, the strict read refusing each way a
      row can be wrong, `active` uniqueness, and the boot reap.

## Phase 3 — Encoding, fitting and prediction

- [x] 3.1 `sc_model::encode`: `Encoding`, `ColumnEncoding` (passthrough, standardised, one-hot
      with its fitted category list, date-to-epoch), `TargetEncoding` (label map), `fit_encoding`
      and `apply_encoding` → a `Matrix` (row-major `Vec<f64>` plus width, which is what every
      provider wants). Unknown category and null refused by name at apply time (§6).
- [x] 3.2 `sc_model::metrics`: the five metric sets of §7, computed from predictions and truth,
      as a `Metrics` value that serialises to the instance's column.
- [x] 3.3 `sc_model::fit`: the orchestration — materialise, split, fit the encoding on train,
      run the hyperparameter grid scoring on validation (§11), fit the winner, score every
      split, write the instance. One function, taking the `DatasetSource` and the registry.
- [x] 3.4 `sc_model::predict`: load an instance, apply its encoding to a frame, call the
      provider, and map the raw output back through the target encoding into `Prediction`s —
      a class *name* and not a class index, because the index is an implementation detail of
      the encoding and nobody's row wants to hold a 2.
- [x] 3.5 Unit tests against a stub provider (a deterministic "predict the mean"): the grid
      picking the point it should, the encoding fitted on train only and applied to test, the
      unknown-category refusal, and the class round-trip.

## Phase 4 — The built-in providers

- [x] 4.1 The `smartcore` feature (default on) and the `statrs` dependency; the two
      distribution-only providers built either way (§13).
- [x] 4.2 `linear_regression`: OLS through smartcore, plus standard errors, *t* and *p* from
      the residual variance and `(XᵀX)⁻¹`, as a `ParameterBlock::Table`. Intercept optional.
- [x] 4.3 `logistic_regression`: coefficients, odds ratios, and predicted class with the
      predicted probability as the `Prediction`'s uncertainty.
- [x] 4.4 `random_forest`: regressor or classifier by the label's type — the `Outcome`
      demonstration — with `n_trees`, `max_depth` and `min_samples_leaf` as hyperparameters and
      feature importances as parameters.
- [x] 4.5 `kmeans`: `k` as a hyperparameter, cluster centres and sizes as parameters, the
      cluster number as the per-row prediction.
- [x] 4.6 `pca`: components, explained variance ratio, and the projected vector per row.
- [x] 4.7 `t_test` and `anova`: configuration is which column is the value and which the group
      (or the two columns, or the constant, per test type); parameters are the statistic, the
      degrees of freedom, the p-value, the group means and the confidence interval. `Outcome`
      is `Test`, so nothing asks them to predict.
- [x] 4.8 Unit tests with hand-checked numbers: a regression whose coefficients, standard errors
      and p-values are asserted against values computed independently (R/`statsmodels` output
      pasted into the test as constants), a two-class logistic separation, k-means on three
      obvious blobs, PCA on a rotated line, and each test statistic against a textbook example.
- [x] 4.9 A build with `--no-default-features` compiles, lists two providers, and says on the
      screen that the rest were compiled out.

## Phase 5 — The API and the action

- [x] 5.1 `sc-api::admin` endpoints: `listModelProviders` (name, description, hyperparameter
      spec, and — given a dataset in the query — the config spec and the outcome),
      `listModels`, `getModel`, `saveModel`, `deleteModel`.
- [x] 5.2 `previewDataset`: validate a dataset and return its column types and the first rows —
      what makes the dataset builder a thing you can see the answer of before you fit it.
- [x] 5.3 `fitModel` (creates the instance, returns its id, spawns the job — §8),
      `listModelInstances`, `getModelInstance`, `deleteModelInstance`, `activateModelInstance`.
- [x] 5.4 `predictRows`: an instance (or a model, meaning its active instance) plus either
      literal rows or a filter over the model's table; answers predictions in row order. Admin
      only, like everything else on this API.
- [x] 5.5 `predict_row` in `sc-action-builtins` (§12): `config_spec_for` offering this table's
      models, the target checked against the outcome's type, writing to a field or to the
      workflow context.
- [x] 5.6 `--model-max-rows` in `ServerConfig` and the CLI.
- [x] 5.7 API tests: the full model lifecycle over HTTP, a fit polled to completion, a fit that
      fails leaving the sentence on the instance, and the action writing a prediction onto a row
      through a trigger.

## Phase 6 — The admin UI

- [x] 6.1 The **Models** tab: the models with their table, provider, outcome and last fit; the
      compiled-out notice when there are no built-ins; a model that failed validation listed
      with its reason.
- [x] 6.2 The **dataset builder**: a column list where each row is a name and a formula, with
      the field / join-path / aggregation picker writing formulas into it (§2), the filter
      formula beside it, and a live preview from `previewDataset` — types and the first rows.
- [x] 6.3 The **model form**: provider picker, the provider's config form rendered from
      `config_spec` against the dataset's shape, the hyperparameter grid (a value or a list per
      hyperparameter), and the split.
- [x] 6.4 The **fit** button and the instance list: status, the poll while `fitting`, the
      failure sentence, Activate, and Delete.
- [x] 6.5 The **instance screen**: the parameter blocks rendered per variant (scalar, table,
      text), the metrics per split, the search results when there was a grid, the row counts and
      what was dropped, and a "try a row" box that calls `predictRows`.
- [x] 6.6 `models.ts` helpers and their tests: the hyperparameter grid's parse and print, the
      outcome-to-metric-set mapping, the parameter-table formatting (significance stars and
      p-values that do not print as `1.2e-16` in a table), and the instance ordering.

## Phase 7 — Providers from modules

- [x] 7.1 `ModelProviderHost` implemented in `sc-module`: the `modelproviders` export read into
      the manifest as `ModelProviderKind`, `fit`/`predict` routed to the module's worker, the
      frame crossing columnar (§14).
- [x] 7.2 The same in `sc-python`: `@sc.model_provider` in `plugin.py`, the class's
      `fit`/`predict` called with the frame as columns, and the entry on the module's manifest.
- [x] 7.3 `plugins/sklearn` — a bundled Python module over scikit-learn, with its
      `feldspar-module.json` (card, `installs`, no permissions), a curated estimator list
      (ridge, gradient boosting, SVM, DBSCAN, t-SNE) and its parameters as blocks.
- [x] 7.4 The registry composing all three sources, a module change rebuilding it, and a model
      whose provider has gone away listed with the sentence rather than dropped.
- [x] 7.5 Tests: `sc-module`'s provider seam against a fixture module (no network); `sc-python`'s
      `bundled_sklearn` (ignored; pip) fitting and predicting through the real package; and the
      catalog test extended to the third bundled module.

## Phase 8 — Documentation and the definition of done

- [x] 8.1 `docs/TECHNICAL_DESIGN.md` §14.2 rewritten from the sketch it is now: the five nouns,
      the two seams, the split, the encoding, the job, and the storage tables. §2's crate table
      and the layer diagram gain `sc-model`.
- [x] 8.2 `docs/tutorial-models.md`: the house-prices walk-through of the definition of done,
      end to end, including the trigger that writes the prediction and the second provider from
      a bundled module.
- [x] 8.3 README §3, `docs/OPERATIONS.md` (the `--model-max-rows` bound, the `smartcore` feature
      in the build-time table, and what a `fitting` instance means after a restart), and the
      CHANGELOG.
- [x] 8.4 The definition of done, run by hand on a real server, and what it found written down.

### What running it by hand found

Run on 2026-09-06 against a real PostgreSQL, on a debug build serving `houses` (60 rows, 50 of
them sold), `neighbourhoods` (3) and `viewings` (154), with a dataset of `price`, `area`,
`bedrooms`, `neighbourhoodⱵaverage_income` and `viewingsↃhouse.length` under a `sold` filter.
Everything the definition of done names happened — a fit in under a second, a coefficient table
recovering the generating coefficients (1769 ± 60 against a true 1800; 7472 ± 1355 against 9000;
1.07 ± 0.18 against 0.9; `viewings_count` correctly *p* = 0.89), R² 0.969 on the held-out rows,
**Activate**, a trigger writing `estimated_price`, and the same dataset fitted by
`sklearn_gradient_boosting` from `feldspar-sklearn` with a three-point grid over `n_estimators`,
both fits holding out the **same twelve rows** so their test RMSEs (15 195 against 26 254) are a
real comparison. Four things it found:

1. **The filter blocked every prediction, which was a real bug and is fixed.** `predict_row`
   read the row *through the dataset including its filter*, so a model fitted on `sold === true`
   answered "the dataset of model `House prices` does not select this row" for the unsold house
   the trigger had just inserted — the exact row the definition of done asks it about. The
   filter says which rows a model is fitted **from**, not which rows it may be asked about.
   `Read::unfiltered` now carries that distinction and `predict_subject` uses it; the API test
   that covers the trigger gained a filter, because a dataset without one could never have
   caught this.
2. **A bare boolean is not a filter.** The definition of done writes the filter as `sold`;
   `sc-expr` refuses a bare value in boolean position and asks for `sold === true`. The message
   says exactly that and names the fix, and the rule is the one every other boolean formula in
   the system already follows, so this is a documentation correction (the tutorial says it) and
   not a change.
3. **`predictRows` takes a model *id* and `predict_row` takes a model *name*.** Both are right
   for their caller — the screen has an id in hand, an admin writing a trigger has a name — but
   the asymmetry is worth knowing before reaching for the endpoint from a script.
4. **A *p* of 4×10⁻²⁰ prints as `0` in the raw API.** The admin UI's formatter handles it (6.6),
   but a caller reading `parameters` off the endpoint sees the underflow. Left alone: the value
   is a `f64` and rounding it in the API would be the wrong place.

---

## Explicitly OUT of scope for this milestone

- **Bayesian inference / mc-stan.** GOALS names it, and it is genuinely different in kind: the
  configuration is a *Stan file* whose `data` block has to be matched against the dataset's
  columns, the toolchain is a cmdstan installation on the host, and the output is posterior
  draws rather than parameters and metrics. Every one of those is a design question this
  milestone would have to answer badly to answer at all. The seam it needs is the one being
  built — a provider whose config is a file reference and whose parameters are `Table` blocks.
- **statsmodels as a second bundled module.** Nearly free once `plugins/sklearn` works (same
  decorator, a `Text` parameter block for `summary()`), and therefore not the thing that proves
  anything. Carried past.
- **k-fold cross-validation.** §11.
- **Application-facing prediction.** A REST or GraphQL endpoint that scores a row is an
  application API question — which application, which permission, what shape — and this
  milestone's API is the admin's.
- **A calculated field that predicts.** §12.
- **Online / incremental fitting, and a scheduled refit.** A refit is a fit; scheduling one is
  a trigger firing an action, and the action that fits does not exist yet. It is two lines when
  somebody wants it.
- **Feature selection, imputation and outlier removal as configuration.** A dataset is a
  formula list, so a transformation the admin wants is a formula they write. Automatic ones are
  a provider's business, not the host's.

## Carried past this milestone

- **`fit_model` as an action**, so a trigger can refit nightly. Wanted the moment somebody has a
  model in production; needs the job (§8) to be startable from outside a request, which it
  already is.
- **A fit as a durable workflow run.** What would make a fit survive a restart instead of being
  reaped. The engine is there; expressing a fit as steps is the work.
- **Predicted-value caching.** An instance applied to a table's every row, stored, and
  invalidated on write — the thing a "stored calculated prediction" would actually be, done
  where the recomputation is visible.
- **Probability calibration and prediction intervals.** The `Prediction`'s `uncertainty` is
  carried from the start and is a class probability today; a regression's interval needs the
  residual variance kept on the instance, which the regression provider already computes.
- **Comparing instances side by side.** The instances of a model are comparable by construction
  (§5) and the screen lists them; a comparison view is the obvious next screen and needs no new
  data.
