# Tutorial: Bayesian regression from a formula

[tutorial-stan.md](tutorial-stan.md) wrote a Stan program by hand and tied its `data` block to
the tables. That is the right tool for a model nobody has a formula for: a spatial field, a
forecast with gaps. Most hierarchical models are not like that. They are a regression with
some of its coefficients varying by group, and statisticians have written those as **one line**
for twenty years:

```text
log_radon ~ floor + (1 | county)
```

The **bayesian_regression** model provider takes that line, in the syntax of R's lme4 and brms
packages. It generates the Stan program, binds its data from the columns the formula names,
sets the default priors, fits it with CmdStan and reports the result in the formula's own terms.
You never see Stan unless you ask to.

This tutorial fits the radon model three ways:

1. **Varying intercepts**: homes in counties, partially pooled.
2. **Varying slopes**: the basement effect varies by county too, correlated with the intercept.
3. **Distributional regression**: the residual SD differs between basement and first-floor
   readings.

It continues from [tutorial-models.md](tutorial-models.md): you know the Analytics UI's model
editor, a named dataset and a fit. You don't need to know Stan. If you know brms, you already
know the formula language, and [What brms users should know](#what-brms-users-should-know) at
the end lists the differences.

---

## Step 0 — CmdStan

The formula is compiled to a Stan program and sampled by CmdStan, exactly as a hand-written
program is. [tutorial-stan.md](tutorial-stan.md) Step 0 sets it up (`feldspar cmdstan status`,
`feldspar cmdstan install`). Without a CmdStan the provider is still on the picker, and you
can still write and check a model. It says why it cannot fit.

---

## Step 1 — The data

You need one table, `homes`, with a row per measurement:

| Field | Type | Notes |
|---|---|---|
| `county` | String (or a key to `counties`) | the grouping factor |
| `floor` | Float | 0 for the basement, 1 for the first floor |
| `log_radon` | Float | the response |

Make a named dataset over it (**Datasets → New**) with those three columns. A row with a null
in any column the formula uses is **dropped** before the fit, and the fit says how many it
dropped. A home with no reading costs nothing more than its row.

---

## Part 1 — Varying intercepts

### Step 2 — The model

In **Models → New**, give it a name (`Radon`), pick the dataset, and choose the provider
**bayesian_regression**. The form changes: in place of a program and a binding table there is a
**Formula** editor. Type:

```text
log_radon ~ floor + (1 | county)
```

As you type, the editor checks the formula against the dataset. Column names complete, and a
mistake is underlined where it is, with a sentence under the editor:

```text
line 1, column 13: `flor` is not a column of the dataset; did you mean `floor`?
```

The **Family** is `gaussian`, the default. The **Links** show each distributional parameter's
link (`mu` identity, `sigma` log), and **Centre the predictors** is on, as in brms.

What the formula says:

- `log_radon ~` is the response.
- `floor` is a population-level coefficient. A numeric column is one coefficient. A text or
  yes/no column is treatment-coded, with one coefficient per level after the first, named as R
  names them: `regionNorth`, `regionSouth`.
- `(1 | county)` is a varying intercept: one for each distinct value of `county`, drawn from a
  normal with an SD the model estimates. That is **partial pooling**. A county with many homes
  is estimated mostly from them, and a county with one home is pulled towards the average.

### Step 3 — The priors

The **Priors** card lists every parameter the formula creates, with its prior:

| class | coef | group | prior | source |
|---|---|---|---|---|
| b | (all) | | flat | default |
| b | floor | | flat | default |
| Intercept | | | *student_t(3, median, scale) of `log_radon`* | default, from the data |
| sd | (all) | county | *student_t(3, 0, scale) of `log_radon`* | default, from the data |
| sd | Intercept | county | | default, from the data |
| sigma | | | *student_t(3, 0, scale) of `log_radon`* | default, from the data |

These are brms's documented defaults. Three of them depend on the response, so until the data
has been read the table shows their **rule**. Press **Preview data** (in the **Data** card), and
the rules become the numbers a fit would use now, such as `student_t(3, 1.3, 2.5)`. The
location is the median of `log_radon`, and the scale its MAD, never below 2.5. Both are
computed over the rows the fit will keep.

Greyed rows are defaults or inherited. To change one, type in its **prior** cell. The row then
becomes explicit and is shown in full, with a ✕ that clears it back to the default. Typing in
an **(all)** row sets the whole class, and every coefficient without a row of its own follows
it. Try `normal(0, 1)` on `b` (all): `floor` now reads "from `b`".

The cell checks what you type against the prior menu as you type it. `normal(0, -1)` is
underlined "σ must be positive (it is -1)", and `gamma(2, 1)` on `b` is refused because `b`
can be negative. Only `b` rows have **lb**/**ub** cells. A bound changes the program, not just
its data, and the row says so with a *structural* badge.

**Pasting priors from R works too.** Add a line to the formula:

```text
log_radon ~ floor + (1 | county)
prior(normal(0, 1), class = b) + prior(exponential(1), class = sigma)
```

When you leave the editor, the two calls move out of the formula and into the table. The editor
says which rows they replaced.

### Step 4 — Preview data

**Preview data** reads the dataset, binds it the way a fit will, and lists each of the
generated program's data variables with its first values. `N` is the rows, `X_1` the
population-level model matrix, `J_1` each home's county as a position, `Z_1_1` the varying
effects' matrix, and the prior numbers. Under it are the dimension sizes (`county: 85`) and
the rows dropped.

The names are positional on purpose. Two collapsed panels show what the formula became:

- **How the formula's data is bound**: the bindings, read-only. `X_1` is a `terms` binding of
  `floor`, `J_1` an `index` into the `county` dimension.
- **Show Stan program**: the generated program, read-only.

Have a look at the program. It mentions neither `log_radon`, nor `floor`, nor `county`, and it
does not say how many coefficients or counties there are: all of that is data. So
`price ~ area + (1 | region)` on another table is **the same program**. The server compiles
each such structure once, and every model of that shape shares the executable
([OPERATIONS.md](OPERATIONS.md) §9.3). Your first fit of a new structure waits about a minute
in **Compiling**. Later ones, yours or anybody's, go straight to sampling.

### Step 5 — Fit

Press **Fit**. The fit's progress shows the stages a Stan fit has: reading, compiling (the
first time), the chains, summarising.

### Step 6 — Read the posterior

The fit's parameters start with three tables in the formula's terms, as `summary()` of a brms
fit lays them out:

**Population-level effects**

| Parameter | Estimate | Est.Error | l-95% CI | u-95% CI | Rhat | Bulk_ESS | Tail_ESS |
|---|---|---|---|---|---|---|---|
| Intercept | 1.46 | 0.05 | 1.36 | 1.56 | 1.00 | 2100 | 1700 |
| floor | −0.69 | 0.07 | −0.82 | −0.55 | 1.00 | 3900 | 2900 |

**Group-level effects ~county**: `sd(Intercept)`.

**Family-specific parameters**: `sigma`.

(The numbers are an illustration: yours are your data's.) The `Intercept` is on the
original scale. The model samples it on the centred predictors, as brms does, and recovers it.

Below them is the full summary of every variable the program has, with labels taken from your
data: `b_1[floor]`, and `r_1[Aitkin, Intercept]` for each county's deviation from the average.
The draws and the trace plots work as they do for any Stan
model ([tutorial-stan.md](tutorial-stan.md) Steps 7–8).

---

## Part 2 — Varying slopes

Does the basement effect differ between counties? Let it vary:

```text
log_radon ~ floor + (1 + floor | county)
```

Each county now has an intercept and a `floor` slope, correlated with each other. The priors
table gains `sd` rows for `Intercept` and `floor` in `county`, and a `cor` row for the
correlation, `lkj(1)` by default. **Group-level effects ~county** reports `sd(Intercept)`,
`sd(floor)` and `cor(Intercept,floor)`.

To keep the slope and the intercept independent, write `||`:

```text
log_radon ~ floor + (1 + floor || county)
```

The correlated version needs no compile: "one correlated block" is the same structure
whether the block has one effect or two, so it reuses Part 1's program. The `||` version is a
different structure, so its first fit compiles once.

---

## Part 3 — The residual SD varies too

A first-floor reading may be noisier than a basement one. In a distributional regression the
residual SD has a formula of its own, on its log scale:

```text
log_radon ~ floor + (1 | county)
sigma ~ floor
```

`sigma` is no longer one parameter. It is a linear predictor with an intercept and a `floor`
coefficient, and the population-level table reports them as `sigma_Intercept` and `sigma_floor`.
Its priors are rows with **dpar** `sigma`.

A varying effect can join both formulas into one correlated block by sharing an ID:

```text
log_radon ~ floor + (1 | p | county)
sigma ~ floor + (1 | p | county)
```

Each county then has a mean deviation and a noise deviation, and the model estimates how the
two are correlated.

---

## What brms users should know

The formula language is brms's, so a formula pasted from R mostly just works. `bf(...)`
wrappers are unwrapped, and their `family =` and `center =` override the form's, which the
editor tells you. What differs:

- **Columns, not R expressions.** A term is a column of the dataset, or one of `log()`,
  `log1p()`, `exp()`, `sqrt()`, `abs()`, `inv()`, `scale()`, `poly(x, k)` and `I(...)` over
  columns and numbers. Anything else belongs in a Calculated column of the dataset.
- **Grouping by a combination** (`(1 | g1:g2)`, `(1 | g1/g2)`) is not generated yet. Add a
  Calculated column holding the combination and group by it.
- **No raw Stan.** There is no `stanvars`. A model that needs Stan the formula cannot say will
  be converted into an ordinary Stan model in a later release.
- **The default priors** are brms's documented ones. Where brms does not document how a
  number is computed, the rule is this provider's own, and it is stated, in the table, and in
  the TODO's §3.7: the median of the response on the link scale, and its MAD, at least 2.5.
- **The same model, not the same numbers.** A fit should agree with brms's within Monte Carlo
  error, not to the last digit: the program and the seeds differ.
- **Families**: `gaussian`, `student`, `lognormal`, `skew_normal`, `bernoulli`, `binomial`
  (with `trials()`), `poisson`, `negbinomial`, `geometric`, `Gamma`, `weibull`, `exponential`
  and `Beta`. The response may take `weights()`, `trials()`, `cens()`, `trunc()`, `se()` and
  `rate()`. A censoring column holds −1, 0, 1 and 2 (left, none, right, interval), or yes/no
  for right censoring.
- **Not yet**: prediction for new rows (`predict("Radon")` in a formula), non-linear and
  multivariate formulas, and the zero-inflated, hurdle and ordinal families arrive in the next
  release. Smooths, Gaussian processes, autocorrelation terms and mixtures are not planned.

## Where to go next

- [tutorial-stan.md](tutorial-stan.md) covers the posterior's draws, the write-back and
  prediction in code, all of which apply here unchanged.
- [OPERATIONS.md](OPERATIONS.md) §9 covers CmdStan, the compile cache and what a fit costs.
