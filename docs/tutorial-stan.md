# Tutorial: Bayesian models with Stan

[tutorial-models.md](tutorial-models.md) fitted models that answer about **one table, one row
at a time**: a regression over `houses`, a k-means over `customers`. A Bayesian model is usually
worth writing because the data is *not* like that. Homes sit in counties, and counties have a
uranium reading. A shop's sales have a value every day except the days they don't. A region's
case rate is like its neighbours'. Your database already holds that structure as foreign keys,
dates and junction tables. A Stan program wants it as flat arrays of 1-based integers and a
handful of sizes, and closing that gap is most of an afternoon for anyone who has done it by
hand.

The **Stan** model provider closes it for you. You write a real Stan program into a file store.
Then you tie each variable in its `data` block to a table, a column, a foreign key or a time
axis, and the server checks every tie before anything is compiled. The posterior comes back
**labelled by your data** again: `alpha[Aitkin County]`, not `alpha.1`. You read it chain by
chain and write it back into the rows it is about.

This tutorial builds three models. Each one shows a different way the database's structure
becomes a Stan program's data:

1. **Radon** (Gelman & Hill): a varying-intercept model of homes within counties. Here the
   structure is a **foreign key**.
2. **Daily sales**: an AR(1) with missing days and a 14-day forecast written into a `forecasts`
   table. Here it is a **time axis**.
3. **Cases by region and week**: a BYM2 spatial model with a weekly random walk. Here it is an
   **adjacency table**, crossed with a time axis.

Then there is a section on **doing prediction in code**, which is how a posterior answers a
question about a new row for now.

This continues from [tutorial-models.md](tutorial-models.md): you know the **Predictive models** screen, a dataset
and a fit. You don't need to know much Stan to follow along, since the programs are given in
full. You do need to be comfortable reading one.

---

## Step 0 — CmdStan

Stan programs are compiled to native code and sampled by **CmdStan**, which the server runs as
a subprocess. It isn't part of the server, and the server never downloads it by itself. Check
from a shell on the server:

```bash
feldspar cmdstan status
```

If it says none was found, install one. This downloads the release from GitHub and builds it
into `~/.cmdstan`, which takes a few minutes and about 1.2 GB:

```bash
feldspar cmdstan install --jobs 1
```

Restart the server, and its startup log says which CmdStan it will use. Without one, the
**stan** provider is still on the model form's provider picker, with the sentence saying why it can't
fit. [OPERATIONS.md](OPERATIONS.md) §9 has the details: memory, the compile cache, where
it looks, and the service account under systemd.

---

## Part 1 — Radon: counties, and the homes in them

The EPA measured radon in homes across Minnesota. A home's radon depends on its county (the
geology) and on whether it was measured in the basement (`floor = 0`) or on the first floor
(`floor = 1`). Each county has a uranium reading. Some counties have dozens of homes, some one,
and some none. How do you estimate a county's radon level when it has one home, or none?

The answer is partial pooling. Each county's intercept `alpha[j]` is drawn from a distribution
centred on what its uranium predicts. A county with lots of homes is estimated mostly from its
homes. A county with few is pulled towards the prediction. A county with none is the
prediction, with honest uncertainty. That is Gelman & Hill's radon model, chapter 12.

### Step 1 — Two tables

Under **Data → Tables**, make `counties`:

| Field | Type | Notes |
|---|---|---|
| `name` | String | |
| `log_uranium` | Float | the county-level predictor |
| `alpha_mean` | Float | the model will write this |
| `alpha_sd` | Float | and this |

Then `homes`:

| Field | Type | Notes |
|---|---|---|
| `county` | Key to `counties` | |
| `floor` | Float | 0 for the basement, 1 for the first floor |
| `log_radon` | Float | |

The real data is on the website of Gelman & Hill's book (`srrs2.dat` and `cty.dat`). For this
tutorial, synthetic data drawn from **known parameters** is better: you can check the fit
recovers them. It has 85 counties with FIPS codes as keys. Five counties have one home each,
one county has none, and there are 919 homes in all. In `psql` against the application's
database:

```sql
SELECT setseed(0.42);
CREATE TEMP TABLE truth AS
  SELECT k, 27001 + 2 * k AS id,
         -0.2 + 0.35 * sqrt(-2 * ln(1 - random())) * cos(2 * pi() * random()) AS u,
         0.3 * sqrt(-2 * ln(1 - random())) * cos(2 * pi() * random()) AS eta
  FROM generate_series(0, 84) AS k;
INSERT INTO counties (id, name, log_uranium)
  SELECT id, 'County ' || lpad((k + 1)::text, 2, '0'), u FROM truth;
-- log_radon = 1.5 + 0.7·u + county effect (sd 0.3) − 0.7·floor + noise (sd 0.75)
INSERT INTO homes (county, floor, log_radon)
  SELECT id, floor,
         1.5 + 0.7 * u + eta - 0.7 * floor
           + 0.75 * sqrt(-2 * ln(1 - random())) * cos(2 * pi() * random())
  FROM (SELECT t.*, CASE WHEN random() < 0.2 THEN 1.0 ELSE 0.0 END AS floor
        FROM truth t,
             generate_series(1, CASE WHEN k < 5 THEN 1 WHEN k = 5 THEN 137 WHEN k = 84 THEN 0
                                     ELSE 2 + (k * 37) % 17 END)) AS h;
```

So the truth is `gamma0 = 1.5`, `gamma1 = 0.7`, `beta = −0.7`, `sigma_a = 0.3` and
`sigma_y = 0.75`. **County 85** has no homes.

### Step 2 — The program, in a file store

**The configuration of a Stan model is a Stan file.** There is no form that writes Stan, and no
text box on the model form to paste one into. The program lives where every other file this
system edits lives, in a **file store**. So you edit it in the IDE, version it in a git store,
and the coding agent can write it with the tools it already has.

Under **File stores → New file store**, make a store called `stan`: backend `local`, a
directory such as `/srv/stan`, and tick **Create the directory**. Open it in the IDE (**(edit
code)** on its row) and create `models/radon.stan`:

```stan
// Varying intercepts by county, with a county-level predictor
// (Gelman & Hill, ch. 12).
data {
  int<lower=1> N;                              // count(main)
  int<lower=1> J;                              // size(counties)
  array[N] int<lower=1, upper=J> county;       // index(main.county -> counties)
  vector[N] x;                                 // column(main.floor)
  vector[J] u;                                 // column(counties.log_uranium)
  vector[N] y;                                 // column(main.y)
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

The comments in the `data` block are for you, not the server. They say what each variable is
going to be bound to, and Step 4 does exactly that. `alpha` is written *non-centred*
(`alpha_raw` times a scale): with small counties, the centred form makes the sampler struggle,
and Stan's User's Guide explains why.

`#include "lib/priors.stan"` works too. It resolves inside the same store, relative to the
including file, and a path that leaves the store is refused.

### Step 3 — The model: a dataset, a related dataset, a program

**Predictive models → New model**, named `Radon`. Pick **stan** as the **Model provider**. The form grows
the parts a posterior needs; none of them says "Stan", because any provider that binds data
gets them.

**Dataset.** Table `homes`, with three columns:

| Name | Formula |
|---|---|
| `county` | `county` |
| `floor` | `floor` |
| `y` | `log_radon` |

The dataset's column names are the ones the bindings will use, so naming this one `y` means
**Bind automatically** can match it to the program's `y`.

**Related datasets → Add a related dataset.** Name it `counties`, over table `counties`, with
one column `log_uranium` (formula `log_uranium`). Set its **label formula** to `name`. The
model's own dataset is called `main` from now on.

Every dataset is also a **dimension**: an ordered set of positions `1..n`, one per row, each
with its key and its label. That is where a Stan index comes from. **A county's position comes
from the `counties` table, not from the homes.** County 85 has no homes, but it is a row of
`counties`, so it gets a position and an `alpha`. A numbering built from "the distinct counties
in `homes`" would silently drop exactly the county partial pooling says the most about.

**Program.** File store `stan`, path `models/radon.stan`. Press **Check program**. With CmdStan
available this runs `stanc`, the Stan compiler's front end: about a second and no C++. Its
warnings (or its error, with the store path it is about) appear under the card. **Open in IDE**
takes you back to the file.

**Settings.** The sampler's settings are ordinary provider settings, with the defaults CmdStan
users know: 4 chains, 1 000 warmup and 1 000 sampling iterations, `adapt_delta` 0.8, tree
depth 10. Set **Random seed** to any number: the same data, order and seed give the same draws,
which makes a fit reproducible. Leave it empty for a fresh seed each fit; the one used is
recorded on the fit either way. Set **File store for the raw CmdStan run** to `stan`, so the
fit keeps CmdStan's own output for **Download run** (Step 8).

There is no split and no hyperparameter grid for a posterior: the form hides them. Save.

### Step 4 — Bind the data block

The binding table has one row per `data` variable, with its declared Stan type. Each row gets
**one binding**, and a binding produces exactly one variable, so every variable's provenance
is one line.

Press **Bind automatically**. It fills the empty rows it can and says why for each, from three
sources: the names, the foreign keys, and the size expressions.

| Variable | Binding | Because |
|---|---|---|
| `county` | `index(main.county → counties)` | `main` has a column `county`, a key into `counties`, which is the dataset `counties` |
| `J` | `size(counties)` | `county` indexes into `counties` and is declared `upper=J` |
| `N` | `count(main)` | `county` has one value per row of `main` and is declared `array[N]` |
| `y` | `column(main.y)` | `main` has a column `y` |

`x` and `u` are yours, because nothing in their names says what they are. Pick **column** for
each: `x` is dataset `main`, column `floor`; `u` is dataset `counties`, column `log_uranium`.
The kind picker only offers the kinds that can produce the declared type: a `vector[N]` is
never offered `count`.

An `index` is the heart of it. For each home, it gives the **1-based position** of its
`county` value among the rows of the `counties` dimension. Keys become positions here, going
in, and positions become keys again on the way out.

### Step 5 — Preview data

Press **Preview data**. Each row gains its bound shape and first values: `N` is `919`, `J` is
`85`, `county` is 919 positions between 1 and 85, and `y` shows its first five numbers. Below the table,
the datasets say `main: 919 of 919 rows` and `counties: 85 rows`.

This is where mistakes surface, as sentences on the row they are about, before a minute of C++
compiling. Try one. Bind `u` to `main`'s `floor` instead, and preview:

> `u` is declared `vector[J]` with `J` = 85 (the size of `counties`), but its binding
> `main.floor` has 919 values

CmdStan would have said "mismatch in dimension declared and found in context; processing
stage=data initialization", after the compile. Other checks work the same way. An index that
would be zero is caught by `int<lower=1>`. A float column into an `int` is refused, with the
advice to write `round(x)` in the dataset. A null follows the dataset's **nulls** policy
(**refuse the fit**, naming the column and the first row's key, or **drop the row**, counted). A
key that isn't in `counties` follows its **unknown** policy the same way. Put `u` back.

### Step 6 — Fit

Press **Fit**. The fit screen shows the stage and a progress bar per chain:

- **compiling**: the first fit of a program is a C++ compile, a minute or so. It happens once
  per program, and the next fit starts sampling at once. **Compile** on the program card warms
  the cache without fitting.
- **sampling**: four bars, warmup then sampling, each chain its own process. Radon takes a
  second or two.
- **summarising**: the host reads the draws, computes the summary, and stores them with the
  fit.

**Cancel** stops the chains; it works from any server of an installation, because the request
goes through the fit's row. A fit waiting for another fit's chains says
`Waiting for the server's process budget`.

### Step 7 — Read the posterior

**Warnings come first**, in plain words and with what to do. This fit should have none. If a
fit of yours says *"The chains disagree"* (an R̂ above 1.01) or *"Too few effective draws"*, run
more iterations. If it says there were divergent transitions, raise `adapt_delta` or
reparameterise. A fit with warnings is still a fit: a posterior isn't wrong for being hard, and
the warning is the honest output.

**Diagnostics**: divergent transitions per chain (0), iterations that hit the maximum tree
depth (0), E-BFMI per chain, the worst R̂ and the smallest bulk and tail effective sample sizes
across every parameter, and each chain's wall time.

**Then one section per variable.** The scalars `gamma0`, `gamma1`, `beta`, `sigma_a` and
`sigma_y` each have one row: mean, sd, MCSE, the 5 %, 50 % and 95 % quantiles, R̂, and bulk and
tail ESS. These are the same definitions the R `posterior` package and ArviZ use. Compare them
with the truth from Step 1: each true value should sit inside its 5–95 % interval.

**`alpha` is one row per county, and its first column is the county's name.** The server worked
that out itself: `alpha` is declared `vector[J]`, and `J` is bound to `size(counties)`, so
`alpha`'s axis is the `counties` dimension. Find **County 85**. Its interval is visibly wider
than a county with a dozen homes, because nothing but its uranium reading informs it. Find the
five one-home counties too: they sit between the two, pulled towards what their uranium
predicts. `log_lik` is labelled by home in the same way, through `N = count(main)`.

Pick an element (type its key, its label, part of the label, or `#12`) and its **Trace per
chain** and **Histogram** appear. Four chains that mixed overlap in one band with no trend.
**Forest plot** draws all 85 intervals, sortable **by label** or **by mean**; it's the plot a
hierarchical model is read by.

### Step 8 — Write back, download, and the draws by key

**Write back** puts a statistic into the rows a variable is about. Choose `alpha`, **Update the
rows of counties, matched by key**, `mean` into `alpha_mean` and `sd` into `alpha_sd`, then
**Write**. It says it wrote 85 rows. The writes go through the row layer, so they are
validated, and any trigger on `counties` fires. The other mode, insert, is in Part 2.

**Download run** is a zip of CmdStan's own output: the chain CSVs, the program as fitted, the
`data.json` CmdStan read, the coordinates of every dimension and the arguments. It is the
escape hatch to any other tool:

```python
import cmdstanpy
fit = cmdstanpy.from_csv("radon-run/")          # the unzipped directory
fit.summary().loc["alpha[85]"]
```

(Without a runs store the download is rebuilt from the stored draws in the same layout.)

From the API, `getModelDraws` answers one variable's draws **by key or label**, never by
position:

```
GET /api/model-instances/{id}/draws?variable=alpha&elements={"counties":["27169"]}
```

The answer has `dims [85]`, the county labels and keys, and one array of 1 000 draws per chain.
`getPosteriorSummary` is the summary of any variable, including generated quantities too large
to have been summarised with the fit. Positions are private to the fit: a county inserted
tomorrow may renumber every county after it, and nothing outside a fit ever sees `alpha.37`.

---

## Part 2 — Daily sales: gaps, and a forecast

A shop records its takings each day, except the days it doesn't. You want to model the series
and forecast the next two weeks, and the forecast should land in a table, one row per day.

### The tables

`sales`: `day` (Date), `amount` (Float). `forecasts`: `day` (Date), `mean`, `lower`, `upper`
(Float), `fit` (String).

The data is 120 days from 1 January 2025, an AR(1) about 10 (`phi = 0.7`, `sigma = 1`), with
eleven days missing:

```sql
SELECT setseed(0.23);
INSERT INTO sales (day, amount)
WITH RECURSIVE walk(d, level) AS (
  SELECT 0, 10 + 1.4 * sqrt(-2 * ln(1 - random())) * cos(2 * pi() * random())
  UNION ALL
  SELECT d + 1, 10 + 0.7 * (level - 10) + sqrt(-2 * ln(1 - random())) * cos(2 * pi() * random())
  FROM walk WHERE d < 119
)
SELECT DATE '2025-01-01' + d, level FROM walk
WHERE d NOT IN (9, 23, 40, 41, 42, 57, 71, 88, 95, 104, 113);
```

### The program

`models/sales.stan`:

```stan
// A daily AR(1) with missing days and a forecast.
data {
  int<lower=2> T;                          // size(day): every day, the horizon included
  int<lower=0, upper=T - 2> H;             // size(day.future): the horizon
  vector[T] y;                             // series(main.amount over day, fill 0)
  array[T] int<lower=0, upper=1> seen;     // series_present(main.amount over day)
}
transformed data {
  int T0 = T - H;                          // the observed span
  int N_mis = T0 - sum(seen[1:T0]);
}
parameters {
  real mu;
  real<lower=-1, upper=1> phi;
  real<lower=0> sigma;
  vector[N_mis] y_mis;                     // the missing days
}
transformed parameters {
  vector[T0] level;
  {
    int m = 1;
    for (t in 1:T0) {
      if (seen[t]) {
        level[t] = y[t];
      } else {
        level[t] = y_mis[m];
        m += 1;
      }
    }
  }
}
model {
  mu ~ normal(0, 20);
  phi ~ normal(0, 0.5);
  sigma ~ normal(0, 5);
  level[1] ~ normal(mu, sigma / sqrt(1 - square(phi)));
  level[2:T0] ~ normal(mu + phi * (level[1:(T0 - 1)] - mu), sigma);
}
generated quantities {
  vector[H] y_future;                      // labelled with the horizon's dates
  {
    real previous = level[T0];
    for (h in 1:H) {
      y_future[h] = normal_rng(mu + phi * (previous - mu), sigma);
      previous = y_future[h];
    }
  }
}
```

Lags, differences and autoregression are the **program's** business: they're arithmetic on an
ordered vector, which Stan is good at. The server's job is getting the vector right.

### A time grid

New model `Sales`, provider **stan**, dataset `sales` with columns `day` and `amount`. Under
**Order by**, add `day`. A time series *is* an order. Also, MCMC with the same seed over the
same rows in a different order gives different draws, so an order is what makes a fit
reproducible. The primary key is always appended, so the order is total.

The rows are not a regular series: eleven days are missing, and a plain `column(main.amount)`
would silently close the gaps. So declare a **dimension**. Under **Dimensions → Declare a
dimension**, add one named `day`, kind **time grid**, over `main`'s column `day`, step
`1 day`, horizon `14`. Its positions are every day from the first to the last, gaps included,
then fourteen more. It is also a second dimension, `day.future`, which is those fourteen alone.

Bind:

| Variable | Kind | Settings |
|---|---|---|
| `T` | size | dimension `day` |
| `H` | size | dimension `day.future` |
| `y` | series | dataset `main`, column `amount`, over dimension `day`, fill `0` |
| `seen` | series_present | dataset `main`, column `amount`, over dimension `day` |

A `series` puts each row's value at its day's position. A day with no row gets the `fill`, and
`seen` says which days are real. Without a fill, the first gap is refused by its date, with both
ways out: a fill and a mask, or Stan's missing-data idiom (`index(main.day → day)` for the
observed days' positions, and `column(main.amount)` for their values). Several rows in one step
need an `aggregate` (`sum`, `mean`, `count`, …); `count` over an events table with no value
column turns "one row per visit" into visits per day.

**Preview data** says `day` has 134 positions: 120 days and the horizon. `seen` starts
`1, 1, 1, 1, 1`, and its tenth value is `0`.

### The forecast

Fit. `y_mis` is eleven rows, the missing days' estimates, numbered because nothing sizes them
by a dimension. **`y_future` is fourteen rows labelled `2025-05-01` to `2025-05-14`.** The
server labelled it from `vector[H]` and `H = size(day.future)`. Its intervals widen day by day,
and its mean drifts back to `mu`, as an AR(1)'s should.

**Write back**, `y_future`, **Insert one new row per element into a table**: table
`forecasts`, `mean` into `mean`, `q5` into `lower`, `q95` into `upper`. Then choose where the
**axis** goes: `day.future` into `day`, as **its key**. A time grid's key is an instant, and
into a Date field it is written as the day. Tick **this fit's id** into `fit`. Fourteen rows
arrive in `forecasts`.

### Every night

A nightly refit is a `daily` trigger whose action is a workflow of two steps. The first is
**`fit_model`**, the one model action, which fits a named model. With `activate: if_clean` it
makes the new fit the active one only if it has **no warnings**. The second is a
**`run_js_code`** step that does what the **Write back** button does, from code:

```json
{ "name": "refit", "kind": { "type": "action", "action": "fit_model",
    "configuration": { "model": "Sales", "activate": "if_clean" } },
  "next": { "type": "step", "step": "forecast" } }
{ "name": "forecast", "kind": { "type": "action", "action": "run_js_code",
    "configuration": { "code": "…the body below…" } },
  "next": { "type": "end" } }
```

```js
const m = await models.get("Sales");            // the active fit
return await m.writePosterior({
  variable: "y_future", mode: "insert", table: "forecasts",
  statistics: { mean: "mean", q5: "lower", q95: "upper" },
  coordinates: [{ axis: "day.future", field: "day" }],
  instance_field: "fit",
});
```

`fit_model` waits for the fit by default, so the next step sees it. Its answer (`{ instance,
status, active, error, warnings, metrics }`) is in the workflow's context as `context.refit`, so
a step can branch on it. A fit with warnings is kept but left inactive, so the code step writes
last night's forecast again from the fit that is still active. Use `models.get("Sales", { fit:
context.refit.instance })` if you would rather write the new fit's forecast whatever it says.

The write goes through the row layer **as the trigger**, the way a `db` write in the same body
does, and the `forecasts` table's triggers fire. `m.asUser().writePosterior(…)` writes as
whoever caused the event instead, and is refused if they may not write `forecasts`. The same
body can write `alpha` back for Radon after a refit, with
`m.writePosterior({ variable: "alpha", statistics: { mean: "alpha_mean", sd: "alpha_sd" } })`.
`update` is the default mode.

---

## Part 3 — Cases by region and week: BYM2 and a random walk

Weekly case counts per region, where neighbouring regions are alike and every region shares
one trend over the weeks. This is the standard areal model, BYM2 (Morris et al., 2019), plus a
random walk in time. The database side is the new part: **adjacency as a table**.

### The tables

- `regions`: `name` (String) and `expected` (Float, the expected count per week, from
  population).
- `region_adjacency`: `a` and `b`, both Key to `regions`. This is a junction table with one row
  per pair of neighbours. Storing each pair once or in both directions both work.
- `cases`: `region` (Key to `regions`), `week` (Date, a Monday) and `cases` (Integer).

A 5 × 5 lattice of regions with a north–south gradient and a gentle wave over twelve weeks
(the counts are a normal approximation to Poisson, which is fine for a tutorial):

```sql
SELECT setseed(0.37);
INSERT INTO regions (id, name, expected)
  SELECT 5 * r + c + 1, 'R' || (r + 1) || (c + 1), 5 + 15 * random()
  FROM generate_series(0, 4) AS r, generate_series(0, 4) AS c;
INSERT INTO region_adjacency (a, b)
  SELECT 5 * r + c + 1, 5 * r + c + 2 FROM generate_series(0, 4) AS r, generate_series(0, 3) AS c
  UNION ALL
  SELECT 5 * r + c + 1, 5 * r + c + 6 FROM generate_series(0, 3) AS r, generate_series(0, 4) AS c;
INSERT INTO cases (region, week, cases)
  SELECT id, DATE '2025-01-06' + 7 * w,
         greatest(0, round(m + sqrt(m) * sqrt(-2 * ln(1 - random())) * cos(2 * pi() * random())))
  FROM (SELECT g.id, w, g.expected * exp(0.5 + 0.25 * ((g.id - 1) / 5 - 2) + 0.2 * sin(w / 2.0)) AS m
        FROM regions AS g, generate_series(0, 11) AS w) AS x;
```

### The program

`models/outbreak.stan`:

```stan
// Weekly case counts per region: BYM2 in space, a random walk in time.
functions {
  real icar_normal_lpdf(vector phi, int N, array[] int node1, array[] int node2) {
    return -0.5 * dot_self(phi[node1] - phi[node2])
           + normal_lpdf(sum(phi) | 0, 0.001 * N);
  }
}
data {
  int<lower=1> R;                                // size(regions)
  int<lower=2> T;                                // size(week)
  int<lower=0> N_edges;                          // edge_count(adjacency)
  array[N_edges] int<lower=1, upper=R> node1;    // edge_from(adjacency)
  array[N_edges] int<lower=1, upper=R> node2;    // edge_to(adjacency)
  real<lower=0> scaling_factor;                  // icar_scale(adjacency)
  vector<lower=0>[R] E;                          // column(regions.expected)
  array[R, T] int<lower=0> y;                    // cells(main.cases, region x week)
}
transformed data {
  vector[R] log_E = log(E);
}
parameters {
  real beta0;
  real<lower=0> sigma;
  real<lower=0, upper=1> rho;
  vector[R] theta;
  vector[R] phi;
  real<lower=0> sigma_rw;
  vector[T - 1] rw_raw;
}
transformed parameters {
  vector[R] convolved_re = sqrt(1 - rho) * theta + sqrt(rho / scaling_factor) * phi;
  vector[T] rw;                                  // labelled by week
  {
    vector[T] walk = append_row(0, cumulative_sum(rw_raw * sigma_rw));
    rw = walk - mean(walk);
  }
}
model {
  for (t in 1:T) {
    y[:, t] ~ poisson_log(log_E + beta0 + convolved_re * sigma + rw[t]);
  }
  phi ~ icar_normal(R, node1, node2);
  theta ~ std_normal();
  beta0 ~ normal(0, 2);
  sigma ~ normal(0, 1);
  rho ~ beta(0.5, 0.5);
  sigma_rw ~ normal(0, 0.5);
  rw_raw ~ std_normal();
}
```

### The model

New model `Outbreak`, provider **stan**, dataset `cases` with columns `region`, `week` and
`cases`. Add two related datasets:

- `regions`, over `regions`, with column `expected` and label formula `name`;
- `adjacency`, over `region_adjacency`, with columns `a` and `b`.

Declare a time grid `week` over `main`'s `week`, step `1 week`. Weeks start on Monday, in UTC.

Bind:

| Variable | Kind | Settings |
|---|---|---|
| `R` | size | dimension `regions` |
| `T` | size | dimension `week` |
| `N_edges` | edge_count | dataset `adjacency`, from `a`, to `b`, dimension `regions` |
| `node1` | edge_from | the same |
| `node2` | edge_to | the same |
| `scaling_factor` | icar_scale | the same |
| `E` | column | dataset `regions`, column `expected` |
| `y` | cells | dataset `main`, column `cases`, rows: dimension `regions` by column `region`; cols: dimension `week` |

The **edge** kinds read the junction table as the ICAR formulation wants it: each unordered
pair once, with `node1 < node2`. That is `symmetric: dedupe`, the default; `keep` takes the rows
as stored. A self-loop is refused, and an edge to a region not in `regions` follows the
`adjacency` dataset's **unknown** policy. `adjacency` gives the dense 0/1 matrix instead, for a
CAR written with one. `components` and `component` give the connected components, for a program
that constrains per component.

**`icar_scale` is BYM2's scaling factor.** It is the geometric mean of the marginal variances of
the ICAR precision's generalised inverse, per connected component. It's the number every BYM2
user otherwise computes in R with INLA before they can start. The server computes it from the
same edges, up to 5 000 regions.

`cells` is `series` in two dimensions: `matrix[R, T]` or `array[R, T] int`, one row per region
and one column per week, with the same `fill` and `aggregate`. A region-week with no row needs a
fill; `cells_present` is its mask.

**Preview data**: `N_edges` is `40` (a 5 × 5 rook lattice), `scaling_factor` is about `0.516`,
`T` is `12`, and `y`'s shape is `[25, 12]`. Delete one region's adjacency rows and preview again.
The island is **warned about by name**, because a plain ICAR over a disconnected graph is
improper. Put them back.

### Fitting it

BYM2's `sigma` and `rho` share out one variance between two terms, and on 25 regions the data
says little about the split. At 500 draws per chain, expect *"The chains disagree"* about them
and *"Too few effective draws"*. At the default 1 000 warmup and 1 000 sampling iterations the
fit comes back clean. That is what the warnings are for: they tell you what to change, and why.

Read it:

- `beta0` should cover the true `0.5`.
- **`convolved_re` is one row per region, labelled `R11` … `R55`.** Sort the forest plot **by
  mean** and the north–south gradient is the order.
- **`rw` is one row per week, labelled `2025-01-06` … `2025-03-24`**, the shared wave.

Space and time compose with no new mechanism: two dimensions and a `cells`. The long form works
too: `index(main.region → regions)`, `index(main.week → week)`, and `column(main.cases)`.

---

## Doing prediction in code

A predictive model from [tutorial-models.md](tutorial-models.md) predicts in a formula: a
calculated field whose formula is `predict("House prices")` fills itself in. **A posterior
doesn't do that here.** Saving `predict("Radon")` in a formula is refused, because the model is

> a posterior: its draws are the answer, and a posterior does not predict rows here — read its
> draws in a code body (`m.draws(…)`, on `models.get(…)`) and compute the prediction there

and `m.predict(row)` on its handle throws the same sentence. (Stan can do it itself, with
CmdStan's standalone generated quantities over new data. That is designed and not yet built.)
Meanwhile it's about fifteen lines, because code bodies have **`models`** beside `db`.

Add `predicted_mean`, `predicted_q5` and `predicted_q95` (Float) to `homes`, mark the Radon fit
**active**, and give `homes` a trigger on **insert** with a `run_js_code` body:

```js
const m = await models.get("Radon");
const alpha = await m.draws("alpha", { keys: [row.county] });
const beta = await m.draws("beta");
const sigma = await m.draws("sigma_y");
const ys = [];
for (let c = 0; c < alpha.chains.length; c++) {
  const a = alpha.chains[c].draws[0], b = beta.chains[c].draws[0], s = sigma.chains[c].draws[0];
  for (let i = 0; i < a.length; i++) {
    const z = Math.sqrt(-2 * Math.log(1 - Math.random())) * Math.cos(2 * Math.PI * Math.random());
    ys.push(a[i] + b[i] * row.floor + s[i] * z);
  }
}
ys.sort((x, y) => x - y);
const at = (p) => ys[Math.floor(p * (ys.length - 1))];
await db.homes.where({ id: row.id }).update({
  predicted_mean: ys.reduce((t, y) => t + y, 0) / ys.length,
  predicted_q5: at(0.05),
  predicted_q95: at(0.95),
});
```

Insert a home in County 85. It gets that county's `alpha` mean as its prediction. Its interval
combines the county's uncertainty with a home's own spread `sigma_y`, which is the posterior
predictive distribution.

Three things make this correct rather than approximately right:

- **`keys: [row.county]`** selects `alpha` by the database's key. The body never learns a
  position, so a county inserted after the fit changes nothing.
- **Draw `i` of every variable comes from the same iteration of the same chain.** Combining them
  draw by draw keeps the correlation between `alpha` and `beta`. Averaging each first and then
  combining would not.
- `models.get("Radon")` means the model's **active** fit, so refitting and activating changes
  every later prediction with no edit to the trigger. `models.get("Radon", { fit: id })` pins
  one. Either way, the handle stays on the fit it got for the rest of the body, so the three
  `draws` calls cannot straddle two fits.

The handle also has `m.summary("alpha", { keys: [27001] })`, `m.variables` (what the fit drew),
`m.fit` (its id, status, warnings and metrics) and `m.writePosterior(…)` (above). These four are
there because the fit's outcome is a posterior. A handle on a regression doesn't have them, and
touching one throws a sentence saying so. In Python the handle is the same in snake case:
`m = models.get("Radon")`, then `m.draws("alpha", keys=[row["county"]])` and so on
([tutorial-python.md](tutorial-python.md) step 5). One `draws` answer is capped at 500 000
numbers, so use `thin` or `chains` to take less.

---

## When it goes wrong

Every refusal is a sentence naming the variable, its declaration and its binding. Most of them
come from **Preview data**, before anything compiles. The ones that come later:

| It says | What to do |
|---|---|
| `stanc refused the program` … `'models/radon.stan', line 12` | a Stan syntax or type error, in `stanc`'s own words, with the store path; fix the file and **Check program** |
| `CmdStan was not found: …` | Step 0 |
| `Rejecting initial value` … after the last retry | the sampler can't start: try **init** `0`, or tighter priors |
| a chain's last 40 lines | a runtime error from the program; the lines are CmdStan's |
| `the bound data has … values, more than the … allowed` | `--stan-max-data-values`; see OPERATIONS §9 |
| `… draws … over the limit of …` | `thin`, fewer iterations, **Variables whose draws are not kept** (`log_lik` is a good candidate), or turn off **Keep the draws** (the summary stays) |
| `the program has changed since this fit` | not an error: the fit keeps the copy it ran, and a new fit reads the new one |

---

## Where to go next

- [OPERATIONS.md](OPERATIONS.md) §9: installing CmdStan on a server, the process budget, the
  compile cache, how big `_fd_model_draws` gets and what to do about it, backups.
- [TECHNICAL_DESIGN.md](TECHNICAL_DESIGN.md) §14.2, "Bayesian models": why the binder is the
  host's, why the draws are a table, and what was left out.
- The Stan User's Guide, for the programs themselves. Its hierarchical, time-series and
  missing-data chapters are written with exactly the `index`, `series` and `present`/`absent`
  bindings in mind.
