# Saltcorn v2 — Bayesian models with Stan

Ordered, checkable task list for the thirtieth milestone after the MVP. Earlier lists are
archived in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP) and
`docs/TODO-post-mvp-1.md` … [docs/TODO-post-mvp-29.md](./TODO-post-mvp-29.md) (most
recently: static directories, and the interjected agent work — web access, screenshots, calling
the app's API). The predictive-models milestone this one builds on is
[docs/TODO-post-mvp-22.md](./TODO-post-mvp-22.md), whose design is now
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md) §14.2. Scope and rationale are in
[docs/GOALS.md](./GOALS.md) ("Model providers", "Predictive models"), quoted in §1.

The models so far answer questions about **one table, one row at a time**: a regression over
`houses`, a k-means over `customers`. The frame is one rectangle and every provider gets it.
Bayesian modelling does not look like that, and the reason it does not is the reason anybody
reaches for it: the data is **structured**. Homes sit in counties and counties have a uranium
reading; pupils sit in classes, classes in schools; a sensor has a reading every hour except
when it didn't; a region's disease rate is like its neighbours'. A relational database already
holds exactly this structure — as foreign keys, as timestamps, as junction tables — and a Stan
program wants it as flat arrays of 1-based integers and a handful of sizes. The distance between
those two representations is where every Stan user loses an afternoon, and it is the thing this
milestone is for.

So the centre of this milestone is not "run CmdStan" (that is a subprocess) — it is **binding**:
tying each variable in a Stan program's `data` block to a table, a column, a foreign key or a
time axis, checked against the program's declared types and sizes before anything is compiled;
and then turning the posterior draws that come back into something **labelled by the database
again** — `alpha[Aitkin County]`, not `alpha.1` — that can be read chain by chain, summarised,
and written back into the rows it is about.

**Milestone definition of done:** an admin has `counties` (`name`, `log_uranium`) and `homes`
(`county` → `counties`, `floor`, `log_radon`) — Gelman & Hill's radon data, synthetic here with
known parameters, 85 counties of which some have one home and one has none. They write
`radon.stan`, a varying-intercept model with a county-level predictor, into a file store from
the IDE. On the Models tab they create **Radon**, pick the **Stan** provider, point it at the
program, take `homes` as the dataset and add `counties` as a related dataset. **Bind
automatically** fills in `N`, `J`, `county` and `y`; they bind `x` to `floor` and `u` to
`counties.log_uranium` by hand. **Preview data** says `N = 919`, `J = 85`. They press Fit, watch
four chains compile, warm up and sample, and the instance comes back with every R̂ ≤ 1.01, no
divergences, and a posterior summary in which `alpha` is **one row per county, labelled with the
county's name** — including the county with no homes, whose interval is visibly wider. The
`alpha` screen shows its per-chain trace and a forest plot of the 85 counties. `getModelDraws`
returns `alpha`'s 4 × 1 000 draws keyed by county id. **Write back** puts `alpha`'s posterior
mean and standard deviation into `counties.alpha_mean` and `counties.alpha_sd`. The model keeps its
raw run in a file store, so **Download run** gives a zip that `cmdstanpy.from_csv` reads. The same scenario passes in `cargo test` against a
real CmdStan (`#[ignore]`d without one), and every part of it that does not need CmdStan passes
without.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The specification

### 1. What GOALS says

> … and bayesian inference (e.g. using https://mc-stan.org/ - model configuration is the model
> code in a stan file where the data section needs to be linked to the dataset).

> Running a model creates a model instance. This has parameters that can be inspected, which
> may be the main point of the fit. Or it can be applied to a new row in a table.

Three commitments, in order of importance for this milestone:

1. **The configuration is a Stan file.** Not a form that generates Stan. The admin writes (or
   the coding agent writes, or they paste from the Stan User's Guide) a real program, and it
   lives where every other file this system edits lives — in a file store (§6).
2. **The data section is linked to the dataset.** The core of the milestone (§§8–12).
3. **The parameters are the main point.** For a Bayesian model they are *draws*: every chain,
   every iteration, every element of every parameter (§§14–16). Applying the model to a new
   row is real (§19) but explicitly secondary — and **carried past this milestone**: an admin
   who needs it takes the draws into code (§17) and does it there.

### 2. Why this does not fit the existing provider seam, and what changes

`ModelProvider` (design §14.2) was designed around one rectangle: `fit(frame, config, hyper)`.
A Stan model breaks that in five places, and each break is a decision below rather than a
special case:

| the existing seam assumes | a Stan model has | so |
|---|---|---|
| one dataset | several tables: observations, groups, edges, a time axis | a model gains **related datasets** (§7) |
| a row-major world with no order | time series need order; levels need a stable numbering | a dataset gains an **order** (§7) |
| the host encodes the frame (one-hot, standardise) | the *program* says what it wants, variable by variable | the host **binds** instead of encoding (§§8–12) |
| parameters are a few `ParameterBlock`s | draws: chains × iterations × thousands of elements | draws get **their own table**, one row per element per chain, summarised by the host (§§14–16) |
| a fit takes seconds, cannot be cancelled | a compile takes a minute, sampling can take an hour, and it is a subprocess | progress, cancel, a timeout, a process budget (§13) |

What does **not** change: a model is still a `_fd_models` row edited on the Models tab; a fit is
still a job whose row is the registry (design §14.2, "Fitting is a job"); an instance is still what you
inspect and what you apply; **metrics are still the host's** — for a posterior they are the
convergence diagnostics, computed by the host from the draws, so a second Bayesian provider
(PyMC in a module, one day) would be scored by the same code.

### 3. Nouns

The five of design §14.2 stand. Six are added, and like those five they are fixed here because
every one of them is overloaded somewhere else.

| noun | what it is | where it lives |
|---|---|---|
| **program** | a Stan file in a file store (plus the files it `#include`s) | the model's configuration names it; the instance snapshots it |
| **interface** | what the program declares: its `data` variables and the shapes of its parameters and generated quantities | parsed from the program on demand; a fit records the one it ran with |
| **related dataset** | a named `Dataset` over another table, beside the model's main one | `_fd_models.related` |
| **dimension** | an ordered set of labelled positions `1..n`: the rows of a dataset, the distinct values of a column, the steps of a time grid | derived at bind time; its **coordinates** are stored on the instance |
| **binding** | the rule that computes one `data` variable from the datasets and dimensions | the model's configuration |
| **draws** | the sampler's output, per variable, per chain, per iteration | `_fd_model_draws`; the raw CmdStan run optionally also in a file store |

### 4. Where the code goes

- **`sc-model` (layer 6)** gets everything that is not Stan-specific: related datasets and
  ordering, the `Posterior` outcome, the provider seam extensions, **the binder** (§§8–12), the
  `_fd_model_draws` table and its reader (§14), and the posterior summary and diagnostics (§15). All of it is
  pure Rust over frames and is unit-testable with no toolchain. Putting the binder here rather
  than in the Stan crate is the "metrics are the host's" argument again: a Bayesian provider
  in another language would declare an interface and receive bound data, and must not have to
  reimplement "a foreign key becomes a 1-based index".
- **`sc-stan` (new, layer 6, beside `sc-model` and depending on it)**: the Stan-specific half —
  the declaration parser (§5), CmdStan discovery (§20), the compile cache and the runner (§13),
  the CmdStan CSV reader (§14), and `StanProvider` implementing `ModelProvider`. It depends on
  `sc-files` for the program and the optional raw-run directory.
- **`sc-server`** registers `StanProvider` in `ModelServices` beside the built-ins, owns the
  process budget and the cancel path, and wires the new endpoints.
- **`sc-core-actions`** gets `write_posterior` (§16).

**Why Rust driving CmdStan and not a Python module over `cmdstanpy`:** the binder has to be in
the host anyway, because it reads *several* datasets through the `DatasetSource` seam and a
module cannot; CmdStan's interface is a command line, a JSON file and CSV files, which Rust
handles without an interpreter; a subprocess can be **killed**, which is what makes cancel and
a timeout possible (a Python call cannot be interrupted, design §15.2); and it keeps Stan
available on a build without Python. There is **no Cargo feature**: the provider has no
link-time dependency, so it is always compiled and its availability is a runtime fact
("CmdStan was not found — …", §20), shown on the provider picker rather than hidden.

### 5. The program's interface: parsed by us, checked by `stanc`

The binder needs, for every `data` variable, its **element type** (int or real), its
**container** and its **size expressions** — `array[N] int<lower=1, upper=J> county` is an
int array of one dimension whose size is `N` and whose values should lie in `1..J`. For
labelling (§15) it needs the same for every variable in `parameters`, `transformed parameters`
and `generated quantities`: `vector[J] alpha` tells us `alpha`'s one axis has size `J`, and
if `J` is bound to the size of the `counties` dimension, `alpha[j]` is about county `j`.

`stanc --info` gives names, base types and the *number* of dimensions — not the size
expressions, which are the part we need. So `sc-stan` has **its own declaration parser**, and
it is deliberately narrow:

- comments (`//`, `/* */`), string literals, and `#include "file.stan"` resolved **inside the
  same store**, relative to the including file (`..` out of the store, or an absolute path, is
  refused by name);
- the block structure by brace matching: `functions`, `data`, `transformed data`,
  `parameters`, `transformed parameters`, `model`, `generated quantities`;
- **top-level declarations** in `data`, `parameters`, `transformed parameters` and
  `generated quantities` — recognised by a leading type keyword; statements are skipped;
- the modern type grammar only (Stan ≥ 2.33 removed the old array syntax): `int`, `real`,
  `complex`, `vector`, `row_vector`, `matrix`, the constrained forms (`simplex`, `unit_vector`,
  `sum_to_zero_vector`, `ordered`, `positive_ordered`, `cov_matrix`, `corr_matrix`,
  `cholesky_factor_cov`, `cholesky_factor_corr`), `array[...]`, and `<lower=, upper=, offset=,
  multiplier=>` kept as text;
- size expressions kept as text **and** as a tiny integer expression tree (identifiers,
  literals, `+`, `-`, `*`, `%/%`, `%`, parentheses); anything else (a function call) is kept as text and is
  simply not evaluable — the check that needed it is skipped and Stan performs it at runtime.

What comes out is `sc_model::Interface { data, parameters, transformed, generated }`, each a
list of `Declaration { name, element: Int|Real|Complex, dims: Vec<SizeExpr>, stan_type: String,
bounds: Option<(String, String)> }`, where `dims` is the full shape outer-to-inner
(`array[N] vector[K]` and `matrix[N, K]` are both `[N, K]`, which is also how CmdStan's JSON
nests them). A `tuple` or a `complex` in the `data` block is refused by name ("bind a tuple's
parts as separate variables"); in the output blocks it is allowed and simply left unlabelled.

**`stanc` is the authority on whether the program is valid.** When CmdStan is available,
saving a model and the "Check program" button run `stanc` (no C++ compile — about a second)
and show its diagnostics verbatim with the path mapped back to the store path; `stanc --info`
is compared against our parse and a disagreement is a bug report in the sentence, not a
silent preference. When CmdStan is not available the model still saves on our parse alone,
with a notice that the program has not been checked.

### 6. The program lives in a file store; the instance snapshots it

The configuration names `program_store` (a file-store pick-list, the `files` trait's
server-query) and `program` (a path in it). That is the only place the program lives, so it is
edited in the IDE, versioned by a git store, and written by the coding agent with the tools it
already has. There is no inline-code alternative: two places a program can live is two places
to look.

A fit **snapshots** the program and every included file into the instance's `state` (they are
kilobytes; also into the raw-run directory when there is one, §14) and records their SHA-256.
Editing the file afterwards changes the model, never an existing instance, and the instance
screen can say "the program has changed since this fit".

### 7. Related datasets, and order

`Model` gains `related: Vec<NamedDataset>` (`NamedDataset { name, dataset: Dataset, label:
Option<String> }`), stored as a new **nullable** JSON column `_fd_models.related` — nullable, so
`bootstrap_table` adds it to an existing installation and `TABLES_RENAME.sql` needs nothing (a
test pins that an existing `_fd_models` gains the column on boot). The main dataset is
addressed as **`main`** in bindings; related names are identifiers, unique, and not `main`.
`validate_model` validates each exactly as it validates the main one, against its own table.

`Dataset` gains `order: Vec<DatasetOrder { expr, descending }>` — formulas, like everything
else in a dataset — which the `DatasetSource` puts in the `ORDER BY`, **always followed by the
primary key** so the order is total. Other providers ignore it (their split is a hash, design
§14.2) and it costs them nothing.

Order matters for a posterior for a reason beyond time series: MCMC with the same seed over the
same data in a **different row order** gives different draws. A total, deterministic order is
what makes "same data, same seed, same draws" true, and that is what makes a run reproducible
from its snapshot.

`label` is a formula whose value names a row on the screen (`name` for `counties`); it
defaults to the primary key.

### 8. Dimensions: where a 1-based index comes from

Every Stan index is a position in `1..n`. A database has keys. A **dimension** is the mapping
between the two, and it is the most important object in this milestone because every label,
every write-back and every hierarchical model runs through it.

| kind | positions are | coordinates recorded | typical use |
|---|---|---|---|
| **rows** of a dataset | the dataset's rows in its order | each row's key and label | groups with their own table: counties, schools, sensors, regions |
| **values** of a column | the distinct non-null values, sorted | the values | groups that are only a column: `region = 'north'` |
| **time grid** over a date column | the steps from start to end at a fixed step, plus a horizon | each step's start instant | regular time series, forecasting |

Every dataset **is** a rows dimension with the dataset's name — `main`, `counties` — so the
common case needs no configuration. The others are declared in `dimensions` in the
configuration.

Decisions worth stating:

- **A group's positions come from the group's table, not from the observations.** A county
  with no homes is a row of `counties`, so it gets a position and a parameter, and the
  hierarchical model gives it a prediction from the county-level predictor alone. That is the
  point of partial pooling, and a numbering built from "the distinct county ids in `homes`"
  would silently drop exactly the counties it is most informative about. (A **values**
  dimension cannot do this — it only knows the values present — and the docs say so.)
- **Positions are the instance's private business; everything that leaves the host speaks
  keys.** A county inserted between two fits may shift every position after it. Each instance
  stores its own coordinates, and the draws API, the summary, the write-back and the code API
  all answer by key and label. Nothing outside an instance ever sees `alpha.37`.
- **Sort orders are defined, not inherited**: values sort numerically for numbers, by
  Unicode code point for text (not by locale — the order must not change with the server's
  environment), `false < true`.
- **A time grid** has a `step` — `N minutes|hours|days|weeks|months|quarters|years` — a start
  (the first value floored to the step, or given), an end (the last value, or given) and a
  `horizon` of extra steps after the end. It exposes **two** dimensions: `day` (all steps,
  horizon included) and `day.future` (the horizon alone), so a forecast declared
  `vector[H] y_future` is labelled with the future dates. A timestamp maps to the step it falls
  in, in UTC; months and years step by the calendar, not by a fixed number of seconds.

### 9. Bindings: one data variable, one rule

The configuration's `bindings` maps each `data` variable to exactly one binding. One variable,
one binding, no binding that produces three variables — so the form is a table with one row
per declared variable, and every variable's provenance is one line.

**The core kinds** (Phase 3):

| kind | parameters | produces | binds to, e.g. |
|---|---|---|---|
| `value` | a JSON literal | the literal | `real prior_scale;`, `vector[3] w;` |
| `count` | a dataset | its row count | `int N;` |
| `size` | a dimension | its number of positions | `int J;` |
| `column` | a dataset, a column, and for a date `time: { unit, origin }` | one value per row | `vector[N] y;` `array[N] int k;` `array[N] real t;` |
| `columns` | a dataset, a list of columns | one row per dataset row, one column per listed column | `matrix[N, K] X;` `array[N] vector[2] xy;` |
| `design` | a dataset, a list of columns, `standardise` | a model matrix: categoricals one-hot (reference-coded), numbers as-is or standardised | `matrix[N, K] X;` |
| `width` | a `design`-bound variable | its number of columns | `int K;` |
| `index` | a dataset, a column, a dimension, and optionally `match` (which column of the target dataset the values are compared with — the primary key by default) | the 1-based position of each row's value in the dimension | `array[N] int<lower=1, upper=J> county;` |
| `present` / `absent` | a dataset, a column | the 1-based row positions where it is / is not null | `array[N_obs] int ii_obs;` |
| `count_present` / `count_absent` | a dataset, a column | how many | `int N_obs;` |
| `present_values` | a dataset, a column | the non-null values | `vector[N_obs] y_obs;` |
| `segment_start` / `segment_size` | a dataset, an `index`-bound variable | for each position of the dimension, where its rows start in the dataset and how many | `array[J] int start;` |

**The structured kinds** (Phase 6, for time series and space — §12): `series`, `series_present`, `cells`,
`cells_present`, `edge_count`, `edge_from`, `edge_to`, `adjacency`, `components`,
`component`, `icar_scale`, `points`, `distances`.

Everything uses **`sc-expr` formulas as columns**, because a dataset's columns already are
formulas: `countyⱵlog_uranium` on `main` is a join path; `homesↃcounty.length` on `counties`
is an aggregation. The binder never learns a second way to reach across a key.

`design` reuses `sc_model::encode` (design §14.2, "The encoding belongs to the instance") and
records the encoding, so the model matrix's columns have names — which become the labels of
whatever parameter the program sizes by `K`: `beta[floor=first]`, not `beta[2]`.

A `segment_*` binding requires its dataset to be sorted by the index; the binder **sorts** the
dataset's frame by that position (stable, so the declared order breaks ties) before anything
else of that dataset is bound, and every binding of that dataset sees the same order.

### 10. What the binder checks, and when

**At save** (no data read — structure only), each with a sentence naming the variable:

- every `data` variable has a binding, and every binding names a declared variable (a typo
  lists the declared ones);
- the binding kind can produce the declaration's element type and rank: an `index` into an
  `int` array of rank 1, a `columns` into rank 2, a `count` into an int scalar;
- the datasets, columns and dimensions it names exist; a `width` names a `design`; a
  `segment_*` names an `index`.

**At preview and at fit** (data read):

- **Sizes.** Every size expression that evaluates is compared with the bound value's actual
  shape — "`y` is declared `vector[N]` with `N` = 919 (the row count of `main`), but its
  binding `counties.log_uranium` has 85 values". This is the single most useful check in the
  milestone: it is the error CmdStan would give as "mismatch in dimension declared and found in
  context; processing stage=data initialization", after a minute of compiling.
- **Types.** A float column into an `int` variable is refused (write `round(x)` in the
  dataset); a boolean becomes `0`/`1`; a date must say its `time` unit and origin; text
  reaches Stan only through an `index` or a `design`.
- **Declared bounds that evaluate.** `int<lower=1, upper=J>` checked against the values, so a
  zero-based mistake is caught here rather than as a Stan exception.
- **Nulls.** A `column`, `columns`, `design` or `index` over a column holding nulls follows the
  dataset's **`nulls`** policy (both policies live in the configuration's `policies`, per
  dataset name: `{"main": {"nulls": "drop", "unknown": "refuse"}}`) — `refuse` (the default; names the column, the count, and the
  first row's key) or `drop` (the row leaves the dataset before anything of it is counted,
  indexed or bound, and the count is recorded). `present`/`absent`/`present_values` and the
  structured kinds handle nulls themselves and never trigger the policy.
- **Unknown keys.** An `index` value that is not a position of its dimension — a home whose
  county was filtered out of `counties` — follows the dataset's **`unknown`** policy, `refuse`
  or `drop`, with the same reporting.
- **Resolution order.** Datasets are resolved in dependency order — a dataset indexed *into* is
  resolved (and its drops applied) before the datasets that index it — so a county dropped for
  a null is an unknown key to `homes`, and the report says both. A cycle is refused by name.
- **Size.** The total number of values in the bound data is capped (`--stan-max-data-values`,
  default 20 million), refused by name before anything is written; `distances` is counted as
  the n² it is.

What comes out is `BoundData { json, coordinates, report }`: the CmdStan data file as a
`serde_json::Value` (ints as ints, reals as reals with `"NaN"`/`"Inf"` strings where the value
is non-finite, matrices as nested row-major arrays — CmdStan's JSON convention), the
coordinates of every dimension, and the report (sizes, drops, and per-variable one-line
summaries for the preview).

### 11. Representing hierarchical data, and binding it

A hierarchical model's data is groups within groups. How that sits in the database decides the
binding, and the binder has a recipe for each shape rather than one that fits none of them
well.

| in the database | example | binding recipe |
|---|---|---|
| **observations with a key to a group table** | `homes.county → counties` | dataset `counties`; `J = size(counties)`; `county = index(main.county → counties)`; group-level predictors are `column`s of `counties` |
| **nested levels, each a table** | `pupils.class → classes.school → schools` | `class = index(main.class → classes)`, `school_of_class = index(classes.school → schools)` — each level indexes the one above, as the Stan User's Guide writes it; or `index(main.classⱵschool → schools)` when the program wants the school of each pupil directly |
| **groups that are only a column** | `homes.region` is `'north'`… | a **values** dimension over `main.region`; `index(main.region → region)`. No row, no unobserved group — said on the form |
| **crossed factors** | `responses(person → people, item → items, correct)` (IRT) | two `index` bindings into two rows dimensions |
| **multiple membership** | `pupil_schools(pupil, school, weight)` | a related dataset over the junction table; `index` into `pupils` and into `schools`, the weights as a `column` |
| **group sizes the program slices by** | Stan's "ragged array" idiom | `segment_start` / `segment_size` over the `index` variable; the dataset is sorted for it |
| **a group-level summary of the observations** | how many homes a county has | a formula on the level dataset — `homesↃcounty.length` on `counties` — because the dataset language already aggregates, so the binder does not |
| **groups with no table and no FK — a code** | `homes.fips` matched to `counties.fips` | `index(main.fips → counties, match: fips)` |

The worked example, which is the definition of done:

```stan
data {
  int<lower=1> N;                              // count(main)
  int<lower=1> J;                              // size(counties)
  array[N] int<lower=1, upper=J> county;       // index(main.county → counties)
  vector[N] x;                                 // column(main.floor)
  vector[J] u;                                 // column(counties.log_uranium)
  vector[N] y;                                 // column(main.log_radon)
}
parameters {
  real gamma0;
  real gamma1;
  real beta;
  real<lower=0> sigma_a;
  real<lower=0> sigma_y;
  vector[J] alpha_raw;
}
transformed parameters {
  vector[J] alpha = gamma0 + gamma1 * u + sigma_a * alpha_raw;   // labelled by county
}
model {
  alpha_raw ~ std_normal();
  gamma0 ~ normal(0, 5);
  gamma1 ~ normal(0, 5);
  beta ~ normal(0, 5);
  sigma_a ~ normal(0, 2);
  sigma_y ~ normal(0, 2);
  y ~ normal(alpha[county] + beta * x, sigma_y);
}
generated quantities {
  vector[N] log_lik;                                               // labelled by home
  for (n in 1:N) log_lik[n] = normal_lpdf(y[n] | alpha[county[n]] + beta * x[n], sigma_y);
}
```

"Bind automatically" (§18) gets `N`, `J`, `county` and `y` right on its own from the names, the
foreign key and the size expressions; `x` and `u` are the admin's, because nothing in the
names says so.

### 12. Representing time series and space, and binding them

**Time series.**

| in the database | binding recipe |
|---|---|
| **one row per period, no gaps** (`daily_sales(day, amount)`) | order `main` by `day`; `T = count(main)`, `y = column(main.amount)`. The binder does not *assume* regularity: a `series` binding over a time grid checks it |
| **one row per period, with gaps** | a time grid `day` over `main.day`; `T = size(day)`; either `N_obs = count(main)`, `t_obs = index(main.day → day)`, `y_obs = column(main.amount)` (the Stan missing-data idiom), or `y = series(main.amount over day, fill 0)` with the mask `seen = series_present(main.amount over day)` |
| **events, not periods** (`visits(at)`, one row per visit) | `y = series(count over day)` — the `series` binding **aggregates** rows that fall in one step: `count`, `sum`, `mean`, `min`, `max`, `first`, `last`, or `refuse` (the default when a value column is given) |
| **irregular times** (`readings(at, value)` for a GP or a continuous-time model) | `t = column(main.at, time: { unit: hours, origin: min })` into `array[N] real t` |
| **many series** (`readings(sensor → sensors, at, value)`) | long form: `index` into `sensors`, `index` into the grid, `column` for the value — best when series are ragged. Or wide: `Y = cells(value, rows: sensor → sensors, cols: at → hour, fill 0)` into `matrix[S, T] Y`, with `cells_present` into `array[S, T] int seen` |
| **covariates per period in another table** (`holidays(day)`, `weather(day, temp)`) | a related dataset aligned to the **same grid**: `index(weather.day → day)`, or `series(weather.temp over day)`. The join is on the time bucket, not on a key — the one join the formula language cannot express, which is why the grid does it |
| **a forecast horizon** | the grid's `horizon: 30`; `T = size(day)`, `H = size(day.future)`. The program writes `vector[H] y_future` in `generated quantities`, and it comes back labelled with the thirty future dates, ready to write into a `forecasts` table (§16) |

Lags, differences, seasonality and autoregression are the **program's** business — they are
arithmetic on an ordered vector, which Stan is good at and the dataset language is not.

**Areal (lattice) space.** Regions are rows of a `regions` table. Adjacency is best stored as
what it is, a **junction table** over `regions` with two keys (`region_adjacency(a, b)`), with
either one row per unordered pair or both directions. The edge bindings, all over one dataset
of that table and the `regions` dimension:

- `edge_count`, `edge_from`, `edge_to` — the `N_edges`, `node1`, `node2` of the ICAR
  formulation (Morris et al., 2019). `symmetric: dedupe` (the default) keeps each unordered
  pair once with `node1 < node2`; a self-loop is refused; an edge to a region not in the
  dimension follows the `unknown` policy.
- `adjacency` — the dense `matrix[R, R]` 0/1 for a CAR written with a matrix.
- `components` and `component` — the number of connected components and each region's, for a
  program that constrains per component; a region with no neighbour is its own component, and
  the preview warns about it by name because a plain ICAR over a disconnected graph is
  improper.
- `icar_scale` — BYM2's **scaling factor**: the geometric mean of the marginal variances of
  the ICAR precision's generalised inverse (`Q = D − W`, per connected component; a singleton
  component contributes 1), computed with `nalgebra`'s symmetric eigendecomposition (already in
  the tree). This is the number every BYM2 user otherwise computes in R with INLA, and it is
  capped (`R ≤ 5 000`) because the decomposition is cubic.

Deriving adjacency from **geometry** (`ST_Touches`) needs a geometry type this system does not
have; until it does, the junction table is filled by whatever made it (an import, a custom
query on a PostGIS database). Named under *Carried past*.

**Point-referenced space.** Sites are rows with `lat`/`lon` (or projected `x`/`y`) columns.
`points` gives `array[N] vector[2]` or `matrix[N, 2]`, optionally **projected** to kilometres
(equirectangular about the centroid — adequate at city-to-country scale, and said so);
`distances` gives the `matrix[N, N]` great-circle distances in km, capped (`N ≤ 3 000`) and
counted as n² against the data cap.

**Space and time.** The composition of the two — nothing new: `index(main.region → regions)`,
`index(main.week → week)`, the edge bindings over `regions`, and a `column` of counts; or
`cells` for a `matrix[R, T]`. The tutorial's third model (a BYM2 spatial term plus a random-walk
weekly term over Poisson case counts with a population offset) is exactly this.

### 13. Compiling and running

**Compile cache.** A compiled model is keyed by the SHA-256 of the program and its includes,
the CmdStan version and the fixed compile options, and lives in `--stan-cache-dir` (default:
the platform data directory, beside the modules root). A cache hit costs nothing; a miss runs
`make` in the CmdStan directory on a copy of the program (the includes laid out beside it,
`--include-paths` pointing only there), **one compile at a time per node** — a Stan compile is
a C++ compile, 1–2 GB of memory and a minute of CPU, and two at once is how a small server runs
out of memory. Never `--allow-undefined`, never admin-supplied
`CXXFLAGS`, so a program cannot inject C++. A "Compile" button warms the cache without fitting.

**One process per chain.** `<exe> sample num_warmup=… num_samples=… thin=… adapt delta=…
algorithm=hmc engine=nuts max_depth=… id=<chain> random seed=<seed> init=<init>
data file=data.json output file=chain-<c>.csv sig_figs=<n> refresh=<r>` — one process per chain
rather than CmdStan's `num_chains`, because per-chain progress and per-chain failure are both
simpler to read from separate processes, and it needs no `STAN_THREADS` build. The chains of a
fit run in parallel up to `parallel_chains`, and every chain of every fit on the node draws from
one **process budget** (`--stan-max-processes`, default half the available CPUs, minimum 1); a
fit waiting for the budget says `queued`.

`sig_figs` defaults to 9, not CmdStan's 6: six significant figures is a visible error in a
posterior standard deviation computed from them.

**Methods.** `sample` (NUTS) is the milestone. `optimize` (the posterior mode, one "draw") and
`pathfinder` (approximate draws) share every line of the runner and the reader and are two
small tasks in Phase 4; ADVI is not offered (it is deprecated in spirit, and Pathfinder is its
replacement).

**Sampler settings** are the provider's configuration, not hyperparameters — there is no grid
search over a posterior, and a model with a hyperparameter list and the Stan provider is
refused, as a hypothesis test is: `method`, `chains` (4), `parallel_chains`, `iter_warmup`
(1 000), `iter_sampling` (1 000), `thin` (1), `adapt_delta` (0.8), `max_treedepth` (10),
`seed` (empty = a fresh seed per fit, **recorded** on the instance), `init` (2, meaning
uniform(−2, 2) on the unconstrained scale), `save_warmup` (off), `max_runtime_minutes` (60).

**The job.** The existing fit job (design §14.2, "Fitting is a job"), with three additions the
subprocess makes possible:

- **Progress.** The runner parses CmdStan's `Iteration: 400 / 2000 [ 20%] (Warmup)` lines and
  reports `stage` (`queued`, `compiling`, `sampling`, `summarising`) and per-chain
  `{iteration, total, phase}` through a `FitProgress` sink; the host writes them to the
  instance's `attributes.progress` at most once a second. The screen already polls.
- **Cancel.** `cancelModelFit` sets `attributes.cancel_requested` on the row. The job reads it
  back at each progress write and kills the chain processes (their process group). A cancel
  works from any node, because **the row is still the registry** — there is no in-memory map
  another node would not have. A fit whose provider cannot be cancelled (every existing one) is
  refused by name.
- **Timeout.** `max_runtime_minutes` kills the processes and fails the instance, saying how
  long it ran.

Children are spawned with `kill_on_drop` and, on Linux, `PR_SET_PDEATHSIG`, so a server that
dies takes its chains with it; boot's reap (design §14.2) is unchanged, and additionally clears
stale scratch directories. The child environment is scrubbed to `PATH`, `HOME`, `TMPDIR` and
CmdStan's own variables.

**Failures are sentences.** A `stanc` error is quoted with the store path; a data error that
the binder somehow missed is quoted with CmdStan's variable name; "Rejecting initial value"
after the last retry says so and suggests `init: 0` or tighter priors; any other non-zero exit
carries the last 40 lines of that chain's output.

### 14. Draws: where they are kept, and in what shape

A fit of the radon model produces 4 × 1 000 draws of ~1 100 elements (91 parameters, the 85
`alpha`s, the 919 `log_lik`s): 4.4 million numbers. That is too many for the instance's JSON
columns, but it is **not** too many for a table of their own, and the database is where they
belong: it is the one place every node of an installation already shares, it is written in the
same transaction that marks the instance `fitted` (so a fitted instance always has all of its
draws and a failed one has none — no half-published directory to reap), it is deleted with the
instance, and it is in every backup without a second mechanism.

**`_fd_model_draws`**: `id` (uuid pk), `instance` (uuid), `variable` (text — `alpha`),
`element` (JSON — the 1-based index array, `[]` for a scalar, `[2, 1]` for `Sigma[2, 1]`),
`chain` (int), `warmup` (bool), `draws` (JSON — the numbers, one per iteration in order). One
row per **element per chain**, indexed on `(instance, variable)`.

That granularity is the question this milestone was asked. "The chains for this parameter" is
one indexed read; one element's chain is one value that `serde_json` parses straight into a
`Vec<f64>`; and the row count is elements × chains — 4 400 for radon, 200 000 for a program
that saves a 50 000-element `y_rep`, which is what `exclude_variables` is for. The obvious
alternatives both lose: a row per *draw* is 4.4 million rows for one fit, and a row per
*variable* makes `y_rep` one 60 MB value that must be read whole to plot one element.

JSON and not `bytea` for the reason design §14.2 gives for `state`: a system table with a binary
column would be the only one, and the numbers would be unreadable to every tool that is not
ours. The cost is text: with CmdStan's `sig_figs` at 9 (§13) the shortest representation of a
draw is at most about a dozen characters, so radon is **about 50 MB per fit** in `jsonb`, and
that is the number the size bound is written in. Before sampling, the expected stored size is
computed from the interface and the bound sizes; a run over `--stan-max-draws-bytes` (default
1 GB) is refused by name, suggesting `thin`, fewer iterations, `exclude_variables`, or
**`keep_draws: false`**, which keeps the summary and the diagnostics (§15) and discards the draws
once they are computed. The rows are written in batched multi-row `INSERT`s inside the
transaction that saves the instance.

Element indices come from **CmdStan's column names** (`Sigma.2.1`), never from column position
— CmdStan writes matrices column-major, and a reader that assumed otherwise would transpose
every covariance matrix without a word. `lp__` and the sampler columns (`accept_stat__`,
`stepsize__`, `treedepth__`, `n_leapfrog__`, `divergent__`, `energy__`) are stored as variables
like any other, so the diagnostics of §15 can be recomputed from the table alone. Warmup draws
are stored only when `save_warmup` is on, as rows with `warmup = true`.

**The raw run, optionally, in a file store.** When the configuration names a `runs_store` (a
file store) and `runs_dir` (default `stan-runs`), a fit also publishes CmdStan's own output to
`<runs_dir>/<model name>/<instance id>/`:

```
program/…            the program and its includes, as fitted (§6)
data.json            the bound data, exactly as CmdStan read it (§10)
coordinates.json     every dimension's keys and labels (§8)
config.json          the method, every sampler argument, the seed, the CmdStan version
chain-1.csv.gz …     CmdStan's own output, gzipped
chain-1.log …        each chain's stdout/stderr
```

This is the interoperable artifact and not a second copy for its own sake. It is what
`downloadModelRun` zips for `cmdstanpy.from_csv`; it is what standalone generated quantities
(§19) needs, because CmdStan reads fitted parameters only from its own CSV format and the data
only from a file; and it is the exact reproduction of the run. CmdStan writes to a local scratch
directory while it runs (it needs a real path), and the directory is published to the store in
one pass after the draws are loaded. In a git-backed store a `.gitignore` of `*` is written into
`runs_dir` on first use, so megabytes of CSV never become a commit. Without a `runs_store` the
scratch directory is simply deleted: the draws, the summary and the program snapshot are all in
the database, and the download is a zip of per-chain draws CSVs built from the table. (Standalone
generated quantities, §19, would need a runs store; it is carried past this milestone.)

The instance's `state` stays small: the program snapshot and its hashes, the seed, the CmdStan
version, the interface, the coordinates, the compiled-model cache key, and the run directory's
location when there is one. Deleting an instance deletes its draws rows in the same transaction,
and its run directory through a new `ModelProvider::discard(state)` hook (default: nothing),
which the host calls on instance and model deletion.

### 15. What the host computes from the draws

**Labels.** For each output variable, each axis whose declared size expression is a bare
identifier bound by `size(d)` or `count(d)` (or `width(X)`) gets dimension `d`'s labels (or
the design's column names). Anything else stays numeric. The configuration's `labels` map can
override per variable (`"y_future": ["day.future"]`), validated against the actual lengths.

**The summary**, per element: mean, sd, MCSE of the mean, the 5 %, 50 % and 95 % quantiles,
rank-normalised split-R̂, bulk-ESS and tail-ESS (Vehtari, Gelman, Simpson, Carpenter & Bürkner,
2021 — the definitions `posterior` and ArviZ use, so our numbers agree with theirs). The
autocovariances go through a small in-crate radix-2 FFT rather than a new dependency; the normal
quantile function comes from `statrs`, already in the tree. Stored as one
`ParameterBlock::Table` per variable, with the label columns first, for every variable in
`parameters` and `transformed parameters` and for `generated quantities` variables of up to
`--stan-summary-max-elements` elements (default 1 000); larger ones are summarised on demand
(§16).

**Metrics are the host's**, as ever: a new `Metrics::Posterior` holding the sampler
diagnostics — divergent transitions (count and per chain), iterations that hit
`max_treedepth`, E-BFMI per chain, the worst R̂ and the smallest bulk and tail ESS across all
parameters, and the wall time per chain. **Warnings** are derived from them with the published
thresholds (R̂ > 1.01, ESS below 100 per chain, any divergence, E-BFMI below 0.3, any
tree-depth saturation) and stored on the instance as sentences — "12 divergent transitions
after warmup: the posterior has regions the sampler cannot explore; raise `adapt_delta` or
reparameterise" — because an admin who is not a statistician needs the diagnostic *and* what to
do about it. A fit with warnings is still `fitted`: a posterior is not wrong because it is
hard, and the warning is the honest output.

**`optimize`** has one draw: the summary is the point estimates alone and the metrics are the
optimiser's log density and iterations. **`pathfinder`** draws are summarised without R̂ (one
approximation, not chains), and the screen says why the column is empty.

### 16. Reading the posterior, and writing it back

The API (admin, like every model endpoint):

- `getModelDraws(instance, variable, elements?, chains?, warmup?, thin?)` — columnar:
  `{ variable, dims, labels: [[…]], keys: [[…]], chains: [ { chain, draws: [[…per element…]] } ] }`.
  `elements` selects by **key or label** (`{ "counties": ["27001"] }`) as well as by position.
  `thin` and a cap on the returned numbers (`--stan-max-draws-response`, default 2 million)
  keep a careless request from being a 400 MB response, refused by name with the arithmetic.
- `getPosteriorSummary(instance, variable, elements?)` — the §15 summary for any variable,
  including the ones too large to store.
- `downloadModelRun(instance)` — the raw run directory as a zip when there is one (§14),
  otherwise per-chain draws CSVs built from `_fd_model_draws` with `coordinates.json`. This is
  the escape hatch that makes "the admin can do the rest in
  code" true of *any* code: `cmdstanpy.from_csv`, ArviZ, R's `posterior`.

**Write-back** — `writePosterior` in the API and **`write_posterior`** as an action (so a
trigger or a workflow can refit and write back nightly, together with the `fit_model` action
of Phase 7):

- **update mode**: a variable whose axis is labelled by a **rows** dimension writes, per
  element, the chosen statistics (`mean`, `sd`, `q5`, `q50`, `q95`, `rhat`, …) into chosen
  fields of *that dimension's table*, matched by key. `alpha → counties.alpha_mean,
  counties.alpha_sd`.
- **insert mode**: one new row per element into any table — the element's coordinates into
  chosen fields (a key, a date, a label), the statistics into others, and optionally the
  instance id. A forecast into `forecasts(day, mean, lower, upper, instance)`.
- A two-axis variable (`matrix[R, T]`) writes one row per cell in insert mode; update mode
  needs a one-axis variable and says so.

Writes go **through the row layer** (validated, ownership-checked, and firing the table's own
triggers), as `update_rows` and `insert_row` do, and the target fields must be `Float` (or
`Int` for a count statistic) — checked on the action's form, like `predict_row`'s target.

### 17. The draws in code

"Making predictions about new cases is secondary and it is fine if the user does some of the
work in code" — so code must be able to reach the draws. JavaScript and Python code bodies gain
a `models` global beside `db`:

```js
const alpha = await models.draws("Radon", "alpha");          // the active instance's draws
// { dims: [85], labels: [["Aitkin", …]], keys: [[27001, …]], chains: [{ chain: 1, draws: [[…]] }, …] }
const s = await models.summary("Radon", "alpha", { keys: [27001] });
const inst = await models.instance("Radon");                  // id, status, warnings, metrics
```

The first argument is a model name (meaning its active instance) or an instance id, exactly as
`predict_row`'s. It is `code_api_js.md`'s page and the Python equivalent that document it,
because a method not on that page does not exist (that page's own rule). Posterior predictive
computations for new rows — "draw `alpha[county] + beta * x` for this home" — are then ten
lines of code over `models.draws`, which is the workaround §19 makes unnecessary for programs
written for it.

### 18. The admin UI

The Models tab gains no second screen for Stan; the existing model form and instance screen
grow the parts a posterior needs, and **none of them names Stan**. A provider declares the
capability (`ModelProviderKind::binds_data`), and the form renders the binding editor for any
provider that declares it — the rule `FileStoreForm` follows for git.

- **The model form**, for a binding provider: the program picker (store + path, "Open in IDE",
  "Check program" with `stanc`'s diagnostics); the main dataset builder as today, plus
  **related datasets** (add, name, build, order, label) reusing the same builder; the
  **dimensions** editor (values and time-grid kinds; rows dimensions are implicit and listed);
  the **binding table** — one row per declared `data` variable showing its Stan type, a kind
  picker filtered to the kinds that can produce that type, and that kind's fields; **Bind
  automatically** (`suggestBindings`: a column named like the variable, a size from the
  expressions of the variables it sizes, an `index` from a foreign key into a related
  dataset's table, `width` beside `design`) which fills only empty rows; **Preview data**
  (`previewModelData`) with each variable's resolved shape, first values and errors inline on
  its row; the sampler settings; and the runs store. The split and the hyperparameter grid are
  hidden for a posterior outcome.
- **The instance screen**, for a posterior: while fitting, the stage and a progress bar per
  chain, with Cancel; after, the warnings in plain language at the top, the metrics, and one
  section per variable — the summary table with its labels, and for a chosen element its
  **trace plot per chain** and a **histogram**; for a one-axis labelled variable a **forest
  plot** (interval per element, sortable by label or by mean) — the plot a hierarchical model
  is read by. Download run, Write back (a dialog over `writePosterior`), and "the program has
  changed since this fit" when it has.
- Plots follow the existing admin UI's charting and the `dataviz` guidance (chains are a
  categorical series of four; one palette; readable in both themes). Every new string goes
  through the `admin` i18n domain.

### 19. Prediction for new rows (secondary)

> **Carried past this milestone** (Phase 9 was skipped). What stands in the meantime: every
> Stan model's outcome is `Posterior { prediction: None }`, so `predicts()` is false;
> `predict_row` over one is refused when the trigger is saved (and `predictRows` when called)
> with a sentence that calls it a posterior and points at `models.draws`; the runs store is
> kept for **Download run**. Nothing in Phases 10 and 11 depends on it: a forecast is a time
> grid's horizon and a generated quantity of the fit itself (§12), written back with
> `writePosterior` (§16), not a prediction for new rows. The design below is kept for when it
> is picked up.

A Stan program can predict new cases **if it is written to**: extra `data` variables for the
new rows, and a `generated quantities` variable computing the prediction. CmdStan's
**standalone generated quantities** (`method=generate_quantities fitted_params=chain-1.csv`)
then runs *only* that block, for new data, over the existing draws, without refitting. That is
what makes `predict_row` possible for a posterior:

- bindings may name a pseudo-dataset **`new`** — "the rows being predicted", read through the
  main dataset's formulas and *unfiltered* (design §14.2's `predict_subject` rule). At fit time
  `new` has no rows (Stan accepts `array[0]`);
- `prediction` in the configuration names the generated-quantities variable whose one axis is
  `count(new)` (`vector[N_new] y_new`);
- it needs the model's **runs store** (§14), because CmdStan takes the fitted draws only as its
  own CSVs and the data only as a file; a model without one is refused `prediction` on save;
- at predict time the host takes the stored `data.json`, re-binds only the `new` variables
  against the caller's rows **using the instance's stored coordinates** (a county not in them
  is refused by name, as an unknown category is), and runs standalone GQ with the compiled
  model (recompiled from the snapshot if the cache was cleared);
- the answer is a new `Prediction::Distribution { mean, sd, q5, q95 }`, whose value in a row is
  the mean, and whose `Outcome` resolves to `Posterior { prediction: Some(var) }`, which
  `predicts()`.

It is a subprocess per call — a second or two — so `predict_row` on a busy trigger is the wrong
tool, and the docs say so; `predictRows` over a filter is one call for all of them. A program
not written this way has `prediction` unset, `predicts()` false, and is inspected, not applied,
exactly like a hypothesis test.

### 20. Operations

- **CmdStan** is found at `--cmdstan <dir>`, else `$CMDSTAN`, else the newest
  `~/.cmdstan/cmdstan-*` (cmdstanpy's convention, so an existing install is picked up). Version
  ≥ 2.33 is required (the array syntax the parser speaks, and Pathfinder); older is refused by
  name. `feldspar cmdstan status` prints what was found, its version, and whether `make` and a
  C++ compiler are on the path. `feldspar cmdstan install [--version V] [--dir D] [--jobs J]`
  downloads the release tarball from GitHub and builds it — a download and a build the admin
  runs on purpose from a shell, never something the server does on its own; `--jobs` defaults
  to 1 for the memory reason in §13. It is the **first** thing built (Phase 0), so that the
  machine this milestone is developed on has a CmdStan before any test needs one.
- New server flags, machine properties like `--model-max-rows` and for the same reason:
  `--cmdstan`, `--stan-cache-dir`, `--stan-max-processes`, `--stan-max-data-values`,
  `--stan-max-draws-bytes`, `--stan-max-draws-response`, `--stan-summary-max-elements`.
- **Security.** The program is admin-authored and compiles to native code, so writing to the
  program's store is a way to change what a fit computes — the same trust as writing a code
  body. The program cannot reach C++ (§13), includes cannot leave the store, and the process
  inherits no secrets. Fits remain admin-only.
- **Backups** include models, instances and their draws (all rows), which is right — an
  instance without its draws is half an instance — and is where the size of `_fd_model_draws`
  shows up; `keep_draws: false` and deleting old instances are the answers. Raw run directories
  are in a file store, and a file store's backup is the store's. An instance whose run
  directory is gone is listed with that sentence; its draws and summary still read.

---

## Phase 0 — CmdStan on the development machine, first

The integration tests of Phases 4 and 11 (and 9, now carried past) need a real CmdStan, and building one takes a while,
so the installer is the first thing written and this machine gets a CmdStan before anything
else.

- [x] 0.1 `crates/sc-stan`, layer 6 beside `sc-model`, in the workspace and the design's crate
      table with the layering comment (§4) — only its `cmdstan` module for now.
- [x] 0.2 CmdStan discovery (§20): `--cmdstan`, `$CMDSTAN`, the newest `~/.cmdstan/cmdstan-*`;
      the version read and ≥ 2.33 enforced; `make` and a C++ compiler looked for. Tests
      against a fake CmdStan directory — found, too old, absent, no compiler.
- [x] 0.3 `feldspar cmdstan status` and `feldspar cmdstan install [--version V] [--dir D]
      [--jobs J]` (§20): the release tarball from GitHub, unpacked into `~/.cmdstan` by
      default, `make build -jJ` with `J` = 1 by default, progress printed, a half-finished
      install removed on failure. Unit tests for the URL, the target directory and the
      version parsing; the download itself is exercised by 0.4.
- [x] 0.4 Run it here: install the latest CmdStan into `~/.cmdstan` with `--jobs 1` (this
      machine's `systemd-oomd` kills heavy parallel builds), confirm `feldspar cmdstan status`
      finds it and compiles and samples the `bernoulli` example that ships with CmdStan, and
      record in the project memory how the ignored tests find it (`CMDSTAN`, or the default
      directory).

## Phase 1 — `sc-model` groundwork: many datasets, an order, a posterior

- [x] 1.1 `Dataset::order` (`DatasetOrder { expr, descending }`), validated like a column,
      translated into the `Select`'s `ORDER BY` with the primary key appended; `Read` and
      `CatalogDatasetSource` carry it (§7). Unit test: the rendered SQL; a DB test: a frame
      comes back in the declared order with ties broken by key.
- [x] 1.2 `NamedDataset` and `Model::related`, the nullable `_fd_models.related` column, the
      strict row mapping, `validate_model` validating each related dataset against its own
      table, names unique and not `main` (§7). Test: an existing `_fd_models` without the
      column gains it on bootstrap and its rows read back with no related datasets.
- [x] 1.3 `OutcomeSpec::Posterior` and `Outcome::Posterior { prediction: Option<String> }`
      (`predicts()` only with a prediction; `prediction_type()` Float); `Metrics::Posterior`
      as a type with no computation yet; the grid refused for a posterior as for a test.
- [x] 1.4 The seam: `Interface`/`Declaration`/`SizeExpr` (§5); `ModelProvider::interface(cfg)`
      (async, default `None`), `fit_posterior(input, cfg, ctx)` (default: refused), `discard(state)`
      (default: nothing); `FitContext { progress: &dyn FitProgress, cancelled() }`;
      `ModelProviderKind::binds_data`. `run_fit` branches on `Posterior`: materialise main and
      related (each under the row cap), bind (Phase 3), call the provider, summarise (Phase 5).
      Tested with a stub posterior provider returning canned draws.
- [x] 1.5 `_fd_model_draws` (§14): bootstrapped beside `_fd_model_instances`; the rows written
      in batches inside the transaction that saves a fitted instance, and deleted in the one
      that deletes it; `DrawsReader` answering one variable, some elements, some chains, with or
      without warmup. DB tests on Postgres and SQLite: the round trip, the atomicity (a failed
      write leaves the instance `fitting`, not `fitted` with half its draws), and the delete.
- [x] 1.6 Instance deletion and model deletion call `discard`; `attributes.progress`,
      `attributes.cancel_requested`, `attributes.warnings` named as constants beside
      `ATTR_OUTCOME`.

## Phase 2 — The program: the parser and `stanc`

- [x] 2.1 The lexer and block splitter: comments, strings, braces, the seven block names;
      `#include` resolved through the store relative to the including file, cycles and
      escapes refused (§5).
- [x] 2.2 The declaration parser: every type of §5, constraints kept as text, the full shape
      outer-to-inner, `SizeExpr` with its evaluator; tuples and complex in `data` refused.
- [x] 2.3 Unit tests over real programs: the radon model, eight schools, an AR(1), the ICAR/BYM2
      program of Morris et al., one with `#include`, and one of every constrained type — each
      asserting the `Interface` it should produce; and each refusal with its sentence.
- [x] 2.4 `stanc` against the discovered CmdStan (Phase 0): its diagnostics mapped back to store
      paths, and `--info` compared against our parse. Tests with a fake `stanc` script, and
      one against the real CmdStan (`#[ignore]`d without one).
- [x] 2.5 `StanProvider`: `kind()` (the configuration fields of §§6, 13, 14 — program store and
      path, datasets are the model's, `dimensions`, `bindings`, `labels`, sampler settings,
      `runs_store`, `runs_dir`, `exclude_variables`, `keep_draws`), `interface()`, `validate()` running the
      save-time checks of §10; registered in `ModelServices`; listed with "CmdStan was not
      found" when it was not.

## Phase 3 — Binding: the data block tied to the tables

- [x] 3.1 `sc_model::bind`: dimensions — rows (implicit per dataset, key and label), values
      (sorted as §8 says), time grid with its calendar steps, start/end/horizon and the
      `.future` slice (§8) — and `Coordinates`, serialisable for the instance.
- [x] 3.2 The core binding kinds of §9, each producing a typed value with a shape; `value`,
      `count`, `size`, `column` (with `time` for dates), `columns`, `design` (via
      `sc_model::encode`, encoding recorded, column names kept), `width`, `index` (with
      `match`), `present`, `absent`, `count_present`, `count_absent`, `present_values`,
      `segment_start`, `segment_size` (with the stable sort).
- [x] 3.3 The save-time checks and the data-time checks of §10: sizes against evaluated
      expressions, element types, evaluable bounds, the `nulls` and `unknown` policies,
      resolution order and cycles, the data-values cap — every failure a sentence naming the
      variable, its declaration and its binding.
- [x] 3.4 `BoundData`: the CmdStan JSON (ints, reals, `"NaN"`/`"Inf"`, row-major nesting,
      empty arrays), the coordinates and the report.
- [x] 3.5 Unit tests, one per row of §11's table, over hand-built frames: two-level radon with
      an empty county, three-level nesting both ways, a values dimension, crossed IRT indices,
      a weighted junction, segments; plus every refusal: a size mismatch, a zero-based index
      caught by `lower=1`, a null under `refuse` and the same row dropped under `drop`, an
      orphan key, a dependency cycle, the cap.

## Phase 4 — Compiling and running

- [x] 4.1 The compile cache (§13): the key, the cache layout, `make` with the includes laid out
      and nothing admin-supplied on the command line, one compile at a time per node, `stanc`
      errors mapped back to store paths.
- [x] 4.2 The runner: one process per chain with the arguments of §13, `sig_figs` 9, the
      process budget and `queued`, the scrubbed environment, `kill_on_drop` and
      `PR_SET_PDEATHSIG`, progress parsed and sent to `FitProgress`.
- [x] 4.3 In `sc-server`: the budget, progress written to the instance at most once a second,
      `cancel_requested` read back and honoured, `max_runtime_minutes`, and boot's scratch
      cleanup.
- [x] 4.4 Failure sentences (§13): compile error, data error, initialisation failure, any other
      exit with the chain's last 40 lines.
- [x] 4.5 The raw run (§14): scratch while running; published to `runs_store` after the draws
      are loaded when there is one, deleted when there is not; `.gitignore` in a git store;
      `discard` deleting it.
- [x] 4.6 `optimize` and `pathfinder` through the same runner.
- [x] 4.7 Tests against a **fake model executable** (a script that reads its arguments and
      writes a canned CmdStan CSV with progress lines): the arguments it is given, progress
      reaching the instance, two fits sharing a budget of one, cancel killing a sleeping chain,
      the timeout, a non-zero exit's sentence, and the raw run directory's contents.

## Phase 5 — Draws, summary and diagnostics

- [x] 5.1 The CmdStan CSV reader: comment lines (adaptation, timing), the header, element
      indices from the **names** (a column-major matrix read back correctly), sampler columns,
      warmup rows when saved. *(Read a line at a time, skipping the columns of variables the
      host will not read; the step size and CmdStan's timing are recorded per chain in the
      state, and the optimiser's iterations are read from its log.)*
- [x] 5.2 Loading the draws (§14): the CSVs read chain by chain into `_fd_model_draws` rows
      (Phase 1.5), `exclude_variables`, `keep_draws: false`, and the size estimate with its
      `--stan-max-draws-bytes` refusal before sampling. *(Both keys are the host's now; the
      limits ride on `FitContext` as `PosteriorLimits`, whose server flags are Phase 10. A
      size the plan cannot evaluate is measured after sampling, and draws over the limit are
      then dropped with a warning rather than failing the fit.)*
- [x] 5.3 The summary (§15): mean, sd, MCSE, quantiles, rank-normalised split-R̂, bulk- and
      tail-ESS, with the FFT in-crate. Tested against reference numbers from `posterior`
      (R) or ArviZ, pasted as constants with the command that made them in a comment — on a
      well-mixed normal, on an AR(1) chain with high autocorrelation, and on four chains
      where one is stuck (R̂ must be large). *(Neither R nor ArviZ is on this machine; the
      reference is CmdStan 2.40's `stansummary`, which implements the same definitions. It
      agrees to 1e-7 on the first two; on the stuck chains R̂ agrees and the ESS is not
      compared, because there the implementations truncate Geyer's sequence differently.)*
- [x] 5.4 `Metrics::Posterior` and the warnings with their sentences (§15); the `optimize` and
      `pathfinder` variants. *(`Metrics::PosteriorMode` and
      `Metrics::PosteriorApproximation`.)*
- [x] 5.5 Labels (§15): axes matched to dimensions through the size expressions, the `labels`
      override checked against lengths, and the per-variable `ParameterBlock::Table`s with
      the label columns first.

## Phase 6 — Time series and space

- [x] 6.1 `series` and `series_present`: one value per step of a grid (or position of any
      dimension), `fill`, and the aggregations `count sum mean min max first last` or `refuse`.
      *(The axis is `over: { dimension, column?, match? }`; with no column it is the declared
      dimension's own, which must be over the same dataset. A null value leaves its row out of
      its position; an empty position needs `fill` except under `count`. Every lookup — an
      `index`, a series axis, an edge end — shares the `unknown` policy and the resolution
      order.)*
- [x] 6.2 `cells` and `cells_present`: the two-axis version into `matrix[R, C]` /
      `array[R, C] int`. *(`rows` and `cols`, each shaped like `over`.)*
- [x] 6.3 The edge bindings: `edge_count`, `edge_from`, `edge_to` (with `symmetric: dedupe`),
      `adjacency`, `components`, `component`; self-loops refused, isolated regions warned.
      *(`symmetric: keep` gives the rows as stored; a null end is refused by row; the warning
      is `BindReport::warnings`, new, for the preview.)*
- [x] 6.4 `icar_scale` with `nalgebra` per connected component, singleton components as 1,
      the `R ≤ 5 000` cap; tested against the scaling factors published for a small graph
      (and a 4 × 4 lattice computed independently, pasted as a constant). *(Against closed
      forms — K₄, C₆, P₃ — rather than a published table, and the lattice by exact rational
      arithmetic with no eigendecomposition. `nalgebra` was only in `Cargo.lock` as an
      optional dependency of statrs; it is now compiled, for this.)*
- [x] 6.5 `points` (with the optional projection) and `distances` (great-circle km), with
      their caps. *(Both over `lat` and `lon`; planar x/y is a `columns` binding. Only
      `distances` has a cap of its own, `N ≤ 3 000`.)*
- [x] 6.6 Unit tests, one per row of §12's tables: gaps on a daily grid both ways, events
      counted into days, a monthly grid across a year boundary, a horizon labelled with future
      dates, a weather table aligned to the grid, a panel long and wide, and a
      region/week spatiotemporal binding.

## Phase 7 — The API, the actions and the code API

- [x] 7.1 `listModelProviders` carries `binds_data` and CmdStan's availability;
      `getProgramInterface(store, path)` (the parse and `stanc`'s diagnostics);
      `previewModelData(model)`; `suggestBindings(model)` (§18); `compileModel(model)`.
      `saveModel` of a Stan model runs `StanProvider::check_program` (§5: `stanc` when
      CmdStan is available, and its warnings or the "not checked" notice in the answer) —
      the check exists since 2.4/2.5 but nothing on the save path calls it yet, because
      `validate_model` also runs at load and must not start a subprocess per model.
      *(Providers also carry `cancellable` and `unavailable`. A program `stanc` refuses is
      `getProgramInterface`'s `error` field, not a failed request, and refuses the save.
      The preview binds what binds and puts each variable's error on its row
      (`sc_model::preview_data`); `compileModel` waits for the compile.)*
- [x] 7.2 `cancelModelFit(instance)` (§13), refused for a provider that cannot cancel.
      *(`ModelProvider::cancellable`, true for Stan; a finished fit is refused too.)*
- [x] 7.3 `getModelDraws`, `getPosteriorSummary` and `downloadModelRun` (§16) with the response
      cap and selection by key or label. *(A fit now records each output variable's shape
      and axis dimensions in `attributes.axes`, so an instance is read by the labels it was
      fitted with. The cap's flag is Phase 10; `ModelServices::set_max_draws_response` holds
      it. The download gunzips a raw run's CSVs; without one, or when it can no longer be
      read, it is per-chain CSVs from the table with `coordinates.json`, `variables.json`
      and a `README.txt` saying why.)*
- [x] 7.4 `writePosterior` and the `write_posterior` action (§16): update and insert modes,
      through the row layer, target types checked on the form. *(An effective sample size
      may go into an integer field, rounded down; every other statistic needs a float.)*
- [x] 7.5 The **`fit_model` action** (carried from TODO-post-mvp-22): start a fit of a named
      model from a trigger or a workflow, optionally activating the result when it has no
      warnings — what makes "refit and write back every night" two steps. *(It waits for the
      fit by default, so the next step sees it; `wait: false` starts it and the job activates
      it. The seam is `sc_model::FitStarter`.)*
- [x] 7.6 `models.draws`, `models.summary`, `models.instance` in JavaScript and Python code
      bodies (§17), documented in `code_api_js.md` and its Python counterpart. *(Not a sixth
      host surface: an `op: "models"` request on the `db` host, with a few lines of prelude in
      each language. There is no Python counterpart page; `tutorial-python.md` step 5 documents
      it, and the editor's `codeTypes.ts` declares it.)*
- [x] 7.7 API tests over a stub posterior provider (no CmdStan): the lifecycle, preview with an
      inline error, suggest, cancel, draws by key with thinning and the cap, a summary on
      demand, write-back in both modes firing the target table's trigger, the zip's contents,
      `fit_model` from a trigger, and the code API from both languages. *(`sc-server`'s
      `posterior_api.rs`; Python's is `sc-python`'s `python_models.rs`, which needs
      `--features python-host` and so `python3-dev` — not installed on this machine, so it
      has not been run here; the lowering was checked against the package directly.)*

## Phase 8 — The admin UI

- [x] 8.1 The model form for a `binds_data` provider (§18): program picker with Check and
      Open in IDE, related datasets, dimensions, the binding table with kinds filtered by
      declared type, Bind automatically, Preview data with per-row errors, sampler settings,
      runs store; split and grid hidden for a posterior. *(`ModelBindings.tsx`; the dataset
      builder became `DatasetBuilder.tsx` and edits the order. Compile is on the program card.
      Open in IDE opens the file: `ideUrl(store, path)` and the IDE's `?path=`.)*
- [x] 8.2 The instance screen for a posterior: stage and per-chain progress with Cancel;
      warnings, metrics, per-variable summary tables with labels; trace per chain,
      histogram, and the forest plot; Download run; Write back; "the program has changed".
      *(`PosteriorInstance.tsx`, `PosteriorPlots.tsx`. "The program has changed" is
      `getModelInstance`'s new `program_changed`, from `ModelProvider::program_changed`.
      Walked by hand against real CmdStan; what it found is in the CHANGELOG — two Phase 4
      gaps around a cancel that lands after sampling are not fixed here.)*
- [x] 8.3 `models.ts` helpers and their tests: which binding kinds fit a declaration, the
      binding editor's parse and print, the warning ordering, the forest plot's sort, and
      the element selection by key/label; strings in the `admin` i18n domain.
      *(`posterior.test.ts`. `feldspar i18n lint` is clean; `i18n check` still fails on three
      pre-existing non-literal `t()` calls in `SourceControl.tsx`.)*

## Phase 9 — Prediction for new rows (secondary): skipped

Carried past this milestone — see the last section, and the note at the head of §19.

## Phase 10 — Operations

- [x] 10.1 The server flags of §20 in `ServerConfig`, the CLI and the config file, with their
      defaults and their tests. *(`ServerConfig::stan`, an `sc_server::StanSettings`, reaches
      the provider and the services through `install_models_with`; the preview and every fit
      now use the configured `PosteriorLimits` rather than the constants. The file's keys are
      `cmdstan` and `stan_*`, turned into the flags they mirror before the command line's so
      a flag wins and a `0` is refused in one place. `serve` says at startup which CmdStan it
      found, the budget and the cache. Tests: `config.rs`, `sc-config-file`, `run.rs`, and
      `posterior_api.rs`'s `the_stan_flags_reach_the_provider_the_preview_and_the_fit`.)*
- [x] 10.2 `docs/OPERATIONS.md`: installing CmdStan (and its disk and memory), the flags, the
      compile cache, the size of `_fd_model_draws` and `keep_draws`, the raw run directories
      and their backup, the security paragraph of §20. *(§9 there, seven subsections; the
      config-file key table, the environment variables and the common failures gained the
      Stan rows. Under the systemd unit CmdStan is installed as the service account, because
      `~` is `/var/lib/feldspar` and `ProtectHome` hides yours. README's flag table too.)*

## Phase 11 — Real CmdStan, documentation and the definition of done

- [x] 11.1 `crates/sc-server/tests/stan_models.rs`, `#[ignore]`d unless `CMDSTAN` is set: the
      radon definition of done end to end on synthetic data with known parameters (the
      recovered `gamma0`, `gamma1`, `beta` inside their 90 % intervals, the empty county's
      interval wider than the median county's, labels by name, draws by key, write-back);
      an AR(1) with gaps and a 14-day forecast labelled with dates; a BYM2 over a small
      lattice with a weekly random walk. Fixed seeds, small iteration counts.
      *(`#[ignore]`d always, like `sc-stan`'s: `--ignored` runs them, and CmdStan is found the
      way the server finds it, so no variable is needed here. Radon samples 1 000 draws per
      chain: at 500, a few of the 85 R̂s landed just over 1.01. The BYM2 needs 1 000 + 1 000,
      because `sigma` and `rho` mix slowly. Radon also runs the tutorial's
      prediction-in-code trigger. All three in parallel from a cold cache take 90 s here.
      Found a bug: a time grid's key is an instant, and writing it into a `date` field was
      refused. Fixed in `sc-core-actions`: a midnight instant goes into a date as its day.)*
- [x] 11.2 `docs/TECHNICAL_DESIGN.md`: §14.2 gains "Bayesian models" (the nouns, the binder,
      dimensions and coordinates, `_fd_model_draws` and the optional raw run, the host's summary), the crate table and
      layer diagram gain `sc-stan`. *(The crate table and the dependency graph had it since
      0.1; the ASCII layer diagram and the tree's comment now do too. §14.2 also says where
      a posterior differs from what it said before: `Dataset::order`, `Outcome::Posterior`,
      cancel, `_fd_models.related`.)*
- [x] 11.3 `docs/tutorial-stan.md`: radon (the definition of done), a daily time series with a
      forecast written into a `forecasts` table, and the spatiotemporal BYM2 — each with its
      tables, its program, its bindings and what to read on the instance screen. A section
      "Doing prediction in code" over `models.draws` — the only route while §19 is carried
      past, so it says `predict_row` refuses a posterior. *(Its programs are the test's, and
      its SQL data scripts were run against Postgres. The JavaScript prediction body is run
      by the radon test.)*
- [x] 11.4 README §3, `CHANGELOG`. *(§3 had CmdStan since Phase 0; it gains the tutorial and
      how to run the ignored tests, and §1 a bullet.)*
- [x] 11.5 The definition of done by hand on a real server with a real CmdStan, and what it
      found written down here.
      *(A debug `feldspar serve` on a scratch Postgres database, with CmdStan 2.40 and an
      empty compile cache. The tutorial's SQL made the tables and data. The walk was driven
      over HTTP by a script doing what the form does, and the instance screen was read in
      headless Chromium. The form itself was walked by hand in 8.2.*
      - *Bind automatically proposed `N`, `J`, `county` and `y`, with its reasons; `x` and
        `u` were bound by hand. Preview said `main 919`, `counties 85`.*
      - *The fit showed `compiling` (8 s), four chains `sampling`, then `summarising`. It
        came back with no warnings: largest R̂ 1.008, no divergences, smallest bulk ESS
        1 710.*
      - *All five population parameters were inside their 90 % intervals (`gamma0` 1.515
        [1.423, 1.605]).*
      - *`alpha` was 85 rows labelled `County 01`…`County 85`. County 85, with no homes, had
        a 90 % width of 1.06 against a median of 0.65. The screen showed its forest plot and
        per-chain traces.*
      - *`getModelDraws` gave 4 × 1 000 draws keyed 27001…27169, and one county by key.
        Write back filled 85 rows.*
      - *Download run was CmdStan's CSVs and logs, `data.json`, `coordinates.json`,
        `config.json` and the program. `cmdstanpy.from_csv` (1.3.0) read it as 4 × 1 000
        draws, and its summary agrees with ours (`sigma_a`'s bulk ESS is 1706.98 in both).*
      - *Found:*
        - *the date write-back bug of 11.1, fixed;*
        - *the tutorial said "Models" where the sidebar says "Predictive models", fixed;*
        - *not fixed, outside this milestone: `serve` logs "database configured from the
          `production` environment of …/feldspar.toml" when `--database-url` has overridden
          that environment. It connected to the flag's database, as documented, but the
          one line meant to say which database it is names another.)*

---

## Explicitly OUT of scope for this milestone

- **A form that writes Stan.** GOALS says the configuration *is* a Stan file. A `brms`-style
  formula front end (`y ~ x + (1 | county)`) generating a program is a good idea and a
  different milestone; this one makes the program's data binding good enough that writing the
  program is the only hard part left.
- **PyMC, NumPyro, JAGS, or a Bayesian provider in a module.** The seam is built for them —
  `interface`, bound data, draws the host summarises — but crossing the module boundary with
  it (a module declaring an interface, returning draws) is its own piece of work.
- **LOO, WAIC and model comparison.** `log_lik` in the radon example is there so the draws are
  ready for it; PSIS-LOO needs a generalised Pareto fit and a comparison screen. Carried past.
- **Prior and posterior predictive check plots.** The draws of `y_rep` are available; a plot
  that overlays them on `y` is a screen, not a mechanism.
- **Simulation-based calibration, ADVI, and `laplace`.** Pathfinder replaces ADVI; the rest are
  a statistician's tools, not an admin's.
- **Within-chain parallelism** (`reduce_sum`, `STAN_THREADS`, MPI, OpenCL). A program that
  uses `reduce_sum` still compiles and runs, on one thread per chain.
- **Geometry and adjacency from shapes.** §12.
- **Time zones on a time grid.** UTC, said so; a `tz` setting on the grid is small and nobody
  has asked yet.
- **Durable fits.** A fit still does not survive a restart (design §14.2) — the chains die
  with the server, by construction.

## Carried past this milestone

- **Prediction for new rows from a posterior** — this milestone's Phase 9, skipped (design
  in §19): the `new` pseudo-dataset and `prediction` in the configuration, empty at fit time,
  resolving `Outcome::Posterior { prediction }`; standalone generated quantities re-binding
  `new` only against the instance's stored coordinates, with the compiled model from the
  cache or recompiled from the snapshot and the CSVs fetched from the runs store;
  `Prediction::Distribution { mean, sd, q5, q95 }`; `predict_row` and `predictRows`
  accepting a posterior instance, an unknown county refused by name. Picking it up means
  undoing two things this milestone did in its absence: `OutcomeSpec::Posterior
  { prediction: None }` declares no prediction types (so `predict_row` over a Stan model is
  refused on save), and `sc_model::no_per_row_prediction` sends a posterior to
  `models.draws`.
- **LOO/WAIC and an instance comparison view** — PSIS-LOO over `log_lik` in `sc-model`, and the
  side-by-side screen TODO-post-mvp-22 already wanted.
- **A formula front end generating Stan** (brms-style), which would make the binding
  automatic because the program would be ours.
- **Bayesian providers from modules**: `@sc.model_provider(binds_data=True)` receiving bound
  data and returning draws.
- **Geometry types and adjacency from `ST_Touches`.**
- From TODO-post-mvp-29: W.8 — an agent's trait configuration is not redacted when the agent is
  read back (streams do this with `redact_attrs`/`merge_secrets`); `http`'s `headers` is
  declared `secret()` and will be covered when agents adopt it. And the agent half of that
  milestone's definition of done, which needs an API key.
- From TODO-post-mvp-28: the rest of 3.6 — `de`, `es`, `zh-Hans` and `ar` for all three
  domains, and `fr` for `admin` and `builder` — and the non-JSX half of 3.4's sweep. The
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
