# Saltcorn v2 — Bayesian regression from a formula (milestones R1–R2)

A new model provider, **`bayesian_regression`**, that fits generalized linear, distributional,
non-linear and multivariate multilevel models written as a **one-line formula** in lme4/brms
syntax, typed straight into the model page:

```text
price | weights(w) ~ area + region + (1 + area | county)
sigma ~ region
```

The provider **generates** a Stan program from the formula and fits it through the existing Stan
machinery (`sc-stan`: compile cache, runner, CSV reader; `sc-model`: binder, draws, summary,
diagnostics — TECHNICAL_DESIGN §14.2 "Bayesian models"). The admin never sees Stan unless they
ask to. TECHNICAL_DESIGN §14.2 lists "a formula front end generating Stan" as later work; this is
that work.

The design comes first (Part I), then the two milestones (Part II). R1 is the core GLMM end to
end, and is the release that matters. R2 is breadth, plus prediction.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Part I — Design

## 1. What has to work

| | Use case | Formula |
|---|---|---|
| **A** | Hierarchical linear regression: radon in homes in counties, partial pooling by county | `log_radon ~ floor + (1 \| county)` |
| **B** | Varying slopes, correlated with the intercepts | `reaction ~ days + (1 + days \| subject)` |
| **C** | A GLM with a count or a proportion | `deaths \| trials(n) ~ dose`, family `binomial`; `claims ~ age + offset(log(exposure))`, family `poisson` |
| **D** | Distributional regression: the residual SD varies too | `y ~ x + (1 \| g)` and `sigma ~ x` |
| **E** | Censored survival or lab values below a detection limit | `time \| cens(censored) ~ treatment`, family `weibull` |
| **F** (R2) | Non-linear growth curve, parameters varying by group | `y ~ a * (1 - exp(-b * t))`, `a ~ 1 + (1 \| tree)`, `b ~ 1` |
| **G** (R2) | Two outcomes, correlated residuals | `mvbind(weight, height) ~ age + (1 \|p\| school)` |
| **H** (R2) | Use the fit in a formula: `predict("Radon")` in a calculated field | posterior mean of E[y \| row] |

A user who already knows brms or lme4 should be able to type the formula they would type in R and
get the model they expected. Exact numerical agreement with brms is **not** a goal (different
seeds, different code). The same model, the same default priors where they are documented, and
summaries that agree within Monte Carlo error **are** goals.

## 2. Decisions taken

1. **A clean-room implementation.** brms is GPL-2. Nothing here may be derived from its source:
   no reading of its R code or of its Stan templates (`inst/chunks/*.stan`), and no copying of
   `make_stancode()` output into the generator or its tests. The allowed sources are:
   - the brms **reference documentation** and vignettes (behaviour, not code): `brmsformula`,
     `brmsfamily`, `set_prior`, `mvbrmsformula`, `addition-terms`, `autocor-terms`, `gr`, `brm`;
   - Bürkner (2017) *JSS* 80(1) and (2018) *R Journal* 10(1), which describe the model
     classes mathematically;
   - Bates et al. (2015) *JSS* 67(1) on lme4's formula semantics, R's `?formula` /
     `model.matrix` semantics (Chambers & Hastie 1992);
   - the Stan User's Guide and Functions Reference (non-centred parameterisation, LKJ, Cholesky
     factors — the standard textbook constructions), Gelman & Hill (2007), McElreath (2020).

   The CHANGELOG entry for R1 states this, and each module of the generator names the published
   source of the construction it emits. Variable names in the generated program are **our own**
   (§6), not brms's.
2. **A new crate, `sc-bayesreg`** (layer 6, above `sc-stan`). The parser, the resolver and the
   code generator are pure Rust with no I/O, so all of them are unit-tested without CmdStan.
   `BayesRegProvider` wraps the server's `Arc<StanProvider>` and reuses its compile cache,
   process budget and runner; nothing about CmdStan is duplicated.
3. **The formula is the configuration; the Stan program and its bindings are derived.** They
   are regenerated on every save, preview and fit, and **snapshotted on the instance**, so an
   instance means what it meant when it was fitted even after the generator changes (the
   instance's program snapshot already does this for `stan`).
4. **The generated program depends only on the model's structure.** Column names, the number
   of predictors, the number of groups and levels, and every prior hyperparameter are **data**,
   never program text. Two models of the same structural form compile to byte-identical Stan, so
   they share one executable in the existing compile cache (§5). This is the point of the design:
   a Stan compile is a minute of C++, and the cache only helps if equivalent models hit it.
5. **Priors are data, too, wherever that is possible.** A prior is a family from a fixed menu
   plus numeric arguments. The family is an integer code and the arguments a real array, and the
   program branches on the code (a branch on data is cheap in Stan). Changing `normal(0, 5)` to
   `student_t(3, 0, 2.5)` therefore never recompiles. Only a prior that changes a parameter's
   *declaration* is structural: a bound on `b` (`lb`/`ub`), and `constant()`.
6. **No Stan is ever written by the admin into this provider.** Identifiers in the formula become
   positional data names. A non-linear expression is parsed into an AST over a whitelist of
   operators and functions and re-emitted by the generator. A prior is parsed into
   (family, numbers). There is no `stanvars`/`check = FALSE` equivalent: an admin who needs
   arbitrary Stan **ejects** the model (§9) into an ordinary `stan` model. No string from the
   admin reaches `stanc` verbatim, so there is nothing to inject.
7. **Terms are column names of the model's dataset**, plus a small whitelist of inline
   transforms (§3.3). Anything more is a Calculated column in the dataset (§14.4), the one
   formula language the system already has. A formula never learns a second way to reach
   across a foreign key.
8. **Grouping factors are columns.** `(1 | county)` indexes into the **distinct values** of
   `county` (the binder's `values` dimension), which is what brms and lme4 do. When the column is a
   foreign key and the model has a related dataset over the referenced table, the levels are
   that table's rows instead (a `rows` dimension). Then a county with no homes still gets a
   parameter and its label (§14.2 "Dimensions and coordinates"). The editor offers this as one
   click.
9. **Missing values drop the row** (the main dataset's `nulls` policy defaults to `drop`), counted
   and reported, as brms does. A level at predict time that was not present at fit time is an
   error, by name (§14.2 "The encoding belongs to the instance"). `allow_new_levels` is later
   work.
10. **No database change.** The configuration is the `_fd_models.configuration` JSON of a new
    provider. What is derived goes into the instance's `state`. The seam additions to
    `ModelProvider` (§7) have defaults, and the new binder kinds (§7) are additive, so existing
    `stan` models and running applications are untouched. No migration.

## 3. The formula language

Grammar summary. "R2" marks what the second milestone adds. Anything not listed here is refused
at save with a sentence naming the term ("`s(x)` (a smoothing spline) is not supported yet").

### 3.1 Lines

The formula field is multi-line. Each line is one formula. Line order does not matter, except
that the first line naming the response is the main formula.

```text
<response> [ | <aterms> ] ~ <rhs>          main formula (mu)
<dpar> ~ <rhs>                             a distributional parameter: sigma, shape, nu, phi, zi, hu, …
<dpar> = <number>                          a fixed distributional parameter (R2): nu = 4, disc = 1
<nlpar> ~ <rhs>                            R2: a non-linear parameter, when the main rhs is non-linear
<response2> [ | <aterms> ] ~ <rhs>         R2: a second response (multivariate)
mvbind(y1, y2) ~ <rhs>                     R2: the same rhs for several responses
```

Comments with `#` are allowed. A blank line is ignored. `bf(...)`/`brmsformula(...)` wrappers
are accepted and unwrapped, so a formula pasted from R works. Then `family`, `nl`, `center` and
`rescor` arguments inside it override the form's fields, and the editor says it did.

### 3.2 The right-hand side (population-level)

| syntax | meaning |
|---|---|
| `x` | numeric column → one coefficient; categorical (text, bool, an FK used as a factor) → treatment-coded, reference = the first level in the binder's order |
| `1`, `0`, `-1`, `+ 0` | intercept on (default) / off |
| `a:b` | interaction (numeric × numeric product; numeric × factor slopes per level; factor × factor cells) |
| `a * b` | `a + b + a:b` |
| `(a + b + c)^2` | all terms up to second order |
| `a %in% b`, `a / b` | nesting: `a / b` = `a + a:b` |
| `- term` | remove a term |
| `offset(<transform>)` | fixed coefficient 1 on the linear predictor |
| `0 + Intercept + …` | intercept as an ordinary `b` coefficient, not centred (prior on the real intercept) |
| `mo(x)` (R2) | monotonic effect of an ordinal column: a simplex over its steps |
| `cs(x)` (R2) | category-specific effect, ordinal families only |

The model matrix follows R's `model.matrix` rules: term order, which columns an interaction
gets when a main effect is absent, and the contrast choice. The tests (R1.2) take their cases from
R's documented semantics, not from running brms. When the intercept is removed and the first
factor is full-rank coded, `cmc` (cell-mean coding, default on) holds.

### 3.3 Inline transforms

`log(x)`, `log1p(x)`, `exp(x)`, `sqrt(x)`, `abs(x)`, `inv(x)`, `scale(x)` (centre and divide by
the SD of the **training rows**, constants stored), `poly(x, k)` (orthogonal polynomials, the
basis stored), and `I(<arithmetic>)` over columns and numbers with `+ - * / ^`. They are
evaluated in Rust by the model-matrix builder (§7), not in Stan, so they never reach the program.
A transform's constants live in the instance's encoding, the way `Standardised` does today.

### 3.4 Group-level terms

| syntax | meaning |
|---|---|
| `(1 \| g)` | varying intercept |
| `(1 + x \| g)` | varying intercept and slope, correlated (LKJ on the Cholesky factor) |
| `(1 + x \|\| g)` | the same, uncorrelated |
| `(1 \| g1:g2)`, `(1 \| g1/g2)` | interaction of grouping columns; `g1/g2` = `(1\|g1) + (1\|g1:g2)` |
| `(1 + x \| ID \| g)` | terms sharing `ID` (across lines too: `mu` and `sigma`, or two responses) are one correlated block |
| `(1 \| gr(g, cor = FALSE, id = "p", by = h))` | the `gr()` form; `by` gives one covariance matrix per level of `h` (R2) |
| `(1 \| mm(g1, g2))` (R2) | multi-membership, equal weights; `mmc()` later |

Each group-level term is one **block**: a grouping factor, M coefficients (a model matrix over
the term's own lhs, built with the same rules as §3.2), and correlated or not. Terms on the same
grouping factor with the same `ID` merge into one block.

### 3.5 Response addition terms

| term | R1 | meaning |
|---|---|---|
| `weights(w)` | ✓ | each observation's log-likelihood times `w` |
| `trials(n)` | ✓ | binomial trial count: a column or a number |
| `cens(c, y2)` | ✓ | `left`/`none`/`right`/`interval` or −1/0/1/2; interval is `(y, y2]` |
| `trunc(lb = , ub = )` | ✓ | numbers or columns |
| `se(s, sigma = FALSE)` | ✓ | known measurement SE (gaussian, student); meta-analysis |
| `rate(d)` | ✓ | rate denominator (poisson: offset `log(d)`; negbinomial: scales the mean and the shape) |
| `subset(x)` | R2 | multivariate: this response's rows |
| `thres(k)` | R2 | ordinal threshold count |
| `mi()`, `dec()`, `vreal()`, `vint()`, `index()` | — | not planned |

### 3.6 Families and links

R1: `gaussian`, `student`, `lognormal`, `skew_normal`, `bernoulli`, `binomial`, `poisson`,
`negbinomial`, `geometric`, `Gamma`, `weibull`, `exponential`, `Beta`.

R2: `zero_inflated_poisson`, `zero_inflated_negbinomial`, `hurdle_poisson`,
`hurdle_negbinomial`, `hurdle_gamma`, `hurdle_lognormal`, `zero_inflated_beta`,
`zero_one_inflated_beta`, `beta_binomial`, `cumulative`, `sratio`, `cratio`, `acat`
(flexible thresholds), `categorical`, `von_mises`, `exgaussian`, `asym_laplace`.

Each family is a **table entry**, not code paths scattered through the generator. The entry
holds the distributional parameters, each parameter's default link and allowed links, the
response type check (integer ≥ 0, in (0,1), positive, …), its Stan lpdf/lpmf, its lcdf/lccdf
for `cens`/`trunc`, and its default priors. The default links are brms's documented ones:
`mu` identity / log / logit by family; `sigma`, `shape`, `phi`, `kappa` log; `nu` logm1
(ν > 1); `zi`, `hu`, `zoi`, `coi` logit; `disc` log. A link can be overridden per parameter in
the form.

### 3.7 Priors

A prior row is `{ class, coef, group, dpar, nlpar, resp, prior, lb, ub }`, the shape of
`set_prior`. The classes are `b`, `Intercept`, `sd`, `cor`, the family's distributional
parameters (`sigma`, `shape`, `nu`, `phi`, …), and in R2 `simo` and the threshold intercepts.
The precedence, most specific first: (class, coef, group) → (class, group) → (class). An empty
`coef` is a **whole-class row**: it applies to every coefficient of the class that has no row of
its own (`b` for all population-level effects, `sd` for all of a group's SDs, and so on). The
editor shows the **resolved** prior of every parameter, with its default filled in, which is what
`get_prior()` is for (§7).

**Only explicit rows are stored.** The configuration key `priors` is the list of rows the admin
set. Defaults are never written into it; they are recomputed on every lower (§7), so clearing a
row returns it to the default. What a fit actually used is the full resolved list, every
parameter with its family and numbers, and that is stored in the instance's state.

**The prior menu.** Each prior is a code plus up to three numbers. The constraints on the
arguments are part of the menu, so the editor checks them as the admin types (§7) and the
server checks them again on save:

| prior | arguments | allowed on |
|---|---|---|
| `flat` | — | `b`, and `Intercept` only when `center = false` |
| `normal(μ, σ)` | μ real, σ > 0 | any real or positive class |
| `student_t(ν, μ, σ)` | ν > 0, μ real, σ > 0 | any real or positive class |
| `cauchy(μ, σ)` | μ real, σ > 0 | any real or positive class |
| `double_exponential(μ, σ)` | μ real, σ > 0 | any real or positive class |
| `logistic(μ, σ)` | μ real, σ > 0 | any real or positive class |
| `exponential(λ)` | λ > 0 | positive classes |
| `gamma(α, β)` | α > 0, β > 0 | positive classes |
| `inv_gamma(α, β)` | α > 0, β > 0 | positive classes |
| `lognormal(μ, σ)` | μ real, σ > 0 | positive classes |
| `uniform(a, b)` | a < b, and the row's `lb = a`, `ub = b` | `b` |
| `beta(α, β)` | α > 0, β > 0 | a class bounded to (0, 1) (none in R1) |
| `lkj(η)` | η > 0 | `cor` only |

The positive classes are `sd`, `sigma`, `shape`, `phi` and `kappa`, plus `nu` (bounded below
by 1). A prior on a positive class is automatically truncated at its bound: a `normal` on `sd`
is a half-normal. The truncation constant depends only on the prior's numbers, which are data,
so it is dropped from the density. R2 adds `horseshoe()` and `dirichlet()` (`simo`), both
structural. `constant(x)` is structural.

**Rows that no longer match.** An explicit row can stop naming anything when the formula
changes. For example, a row for `coef = regionNorth` stays behind after `region` is removed
from the formula. Such a row is **kept and flagged as unmatched**, never silently dropped: a
prior the admin chose disappearing without notice is a silent change of model. What can be
checked depends on what is known:

- At **save**, the dataset's shape is known. A row naming a class the family does not have, a
  group that is not a grouping factor, or a `coef` that cannot come from any term (no column or
  interaction of that name) is refused, with a sentence naming the row.
- A `coef` naming a **level of a categorical column** (`regionNorth`) can only be matched once
  the levels have been read. That happens at **preview**, which flags the row in the table, and
  at **fit**, which is refused with the same sentence. The admin then deletes the row or retargets
  it.

**Defaults computed from the data.** The `Intercept`, `sd` and `sigma` defaults below depend on
the response. Before the dataset has been read, the table shows the rule ("student_t(3, median,
scale) of `log_radon`"). After a data preview it shows the numbers ("student_t(3, 1.26, 2.5)").
The numbers are the ones a fit would use now. The fit recomputes them from its own rows and
stores them.

**Priors pasted as R.** The formula field also accepts the prior calls an R user would paste:
`prior(normal(0, 1), class = b, coef = x)`, `set_prior("normal(0, 1)", class = "b")`, and a
`c(…)` or `+` of them. They are parsed with the same menu and checks. When the field is
committed (on blur, and on save), they are **moved into the prior table**, and the formula text
keeps only formulas. A pasted row that has the same target as an existing explicit row replaces
it, and the editor names each row it replaced. A call that does not parse stays in the field,
underlined like any other error.

**Defaults**, as documented for brms. `b` is flat. `cor` is `lkj(1)`. `nu` is `gamma(2, 0.1)`.
`shape` is `gamma(0.01, 0.01)` (`inv_gamma(0.4, 0.3)` for negbinomial). `phi` is
`gamma(0.01, 0.01)`. The `Intercept`, `sd` and `sigma` defaults are data-dependent:
`student_t(3, median(ỹ), s)` for the intercept and `student_t(3, 0, s)` for the SDs, with
ỹ = the response on the link scale (where defined) and s = max(2.5, MAD(ỹ)). The exact rule
should be **confirmed against the `default_prior` / `get_prior` documentation**: the pages did
not load during research. Then this sentence should be corrected. Whatever the rule, the
resolved numbers are stored on the instance and shown, so a user never has to guess. Because
they are data, computing them from the rows costs no recompile.

## 4. What the generated program looks like

A **fixed skeleton**, the same one every time, in the order data → transformed data →
parameters → transformed parameters → model → generated quantities. Each feature present in the
structure contributes fixed chunks. Every chunk is a Rust function that emits text from the
structure alone, never from names or sizes.

For a block of group-level terms it emits the standard non-centred construction (Stan User's
Guide, "Multivariate priors for hierarchical models"): `z ~ std_normal()`,
`r = (diag_pre_multiply(sd, L) * z)'`, `L ~ lkj_corr_cholesky(η)`. For the population-level
part, X is centred in transformed data and the intercept recovered in generated quantities
(documented brms behaviour: the `Intercept` prior is on the centred intercept, "the expected
response when all predictors are at their means"). The linear predictor of each distributional
parameter is
`eta = Intercept + Xc * b + Σ_blocks r[J, ] · Z` (row-wise), then the inverse link. The
likelihood comes from the family entry, vectorised when there are no weights, censoring or
truncation, and as a loop with them.

The data names are ours and positional: `N`; per linear predictor *p* (`mu`, `sigma`, …;
numbered `1..` in canonical order) `K_p`, `X_p`, `means_X_p`; per block *k* `N_k` (levels),
`M_k` (coefficients), `J_k` (level index per row) and `Z_k` (an `N × M_k` matrix, so the
coefficient count is data, not structure); `Y`, and `trials`, `weights`, `cens`, `rate`, … as
present. The prior arguments are `prior_<param>_code` and `prior_<param>_args`.

The program starts with a comment holding the generator version and the canonical structure's
JSON, so reading a cached entry says what it is.

## 5. Normalisation and the cache

The compile cache already keys on the SHA-256 of the program's files (each file's **path** and
hash), the CmdStan version and the compile options (`crates/sc-stan/src/compile.rs`). So caching
reduces to one property: **structurally equal models generate byte-identical programs**. The
pipeline guarantees it:

```text
formula text ──parse──▶ Formula (AST, user names)
            ──resolve against the dataset's shape──▶ Resolved (column types, FKs, families, links)
            ──canonicalise──▶ Structure  +  DataPlan  +  Names
                               │             │            └─ positional name → user label (for the summary)
                               │             └─ bindings: how each data variable is computed (§7)
                               └─ only what changes the program text
Structure ──generate──▶ Stan text (path fixed: "model.stan")
```

**`Structure` contains**: the generator version; per response the family and links; per linear
predictor whether it has an intercept, whether centred, whether `K > 0`, the special terms
(R2: `mo`, `cs`) and which blocks feed it; per block correlated or not, and which predictors share
it (the `ID` pattern); the addition terms present; the prior **families** that are structural
(bounds, `constant`, horseshoe); and in R2 the canonical non-linear AST and the multivariate
layout. **It does not contain** any column name, any count (K, M, levels, N), any prior number,
or the order the user wrote terms in.

Canonicalisation rules:

1. **Names out.** Every column becomes a data slot. Its user name survives only in `Names`.
2. **Counts out.** K, M, number of levels and N are data. `y ~ x` and `y ~ a + b + c` are the
   same program. So are `(1 | g)` and `(1 + x | g)`: `Z_k` is an `N × M_k` matrix, and a
   correlated block declares `L_k` as `cholesky_factor_corr[M_k]`. With M_k = 1 that factor
   has no free parameters and the LKJ term is a constant, so the program is still correct.
   Because this rule merges the commonest pair, it is tested explicitly (§11).
3. **Order out.** Linear predictors are ordered by a fixed dpar order (`mu`, then the family's
   own order). Blocks are sorted by their structural signature, with ties kept in the order
   written (a tie is structurally identical, so it cannot change the text). Population terms
   are ordered as `model.matrix` orders them, but that order only reaches the data. R2:
   responses are sorted by their structural signature.
4. **Non-linear (R2).** The expression is parsed into an AST, its non-linear parameters renamed
   `nlp_1..` in order of first appearance, and its covariates renamed `C_1..` the same way.
   Literals are printed in one canonical form. Commutative reordering is **not** attempted:
   `a*b` and `b*a` are two programs (a cache miss, never a wrong answer).
5. **Versioned.** A change to the generator's output for any structure bumps
   `GENERATOR_VERSION`. Golden tests fail on any unintended change to the text (R1.5), so a
   silent cache-meaning change cannot be merged.

The **acceptance test of the design** is that these all produce byte-identical Stan and one
cache key:
`y ~ x + (1 | g)`, `price ~ area + (1 | county)`, `y ~ a + b + c + (1 + a | g)`, and
`y ~ x + (1|h) + (1|g)` vs `y ~ (1|g) + x + (1|h)`. A different family, an added `sigma ~`,
or `||` instead of `|` must give a different key.

**The editor does not show the cache state.** Whether a structure happens to be compiled
already must not steer which model an admin writes, so the model page has no "compiled" or
"new structure" indicator and no Compile button for this provider. Caching is invisible until a
fit has been started. After that, the fit's progress shows the existing `Compiling` stage when
there was a miss, as a `stan` fit does today.

**Warming.** `feldspar stan warm` (R2) compiles a list of the commonest structures (the R1
families × intercept-only / one block correlated / one block uncorrelated) so a fresh server's
first fits do not wait. The cache directory is per node and keyed by CmdStan version, as today.
Sharing it between nodes is a deployment choice, already safe because entries are renamed into
place.

## 6. Names in the output

Draw variables are the positional ones (`b_1`, `Intercept_1`, `sd_1`, `L_1`, `r_1`, `sigma`, …).
The summary and the API answer in user terms, through the existing labelling machinery:

- `b_p` is labelled by a new **`names` dimension**: a fixed list of labels computed at bind time
  (the model-matrix column names: `area`, `regionNorth`, `area:regionNorth`).
- `r_k` is labelled `[levels of g, coefficient names]`, the levels by the `values` or `rows`
  dimension of §2.8.
- `sd_k` and `cor_k` are labelled by coefficient names (`cor_k` by pairs).

The provider also adds a **brms-style parameter table** to `PosteriorResult.parameters`.
"Population-level effects" has rows `Intercept`, `area`, … and the columns Estimate, Est.Error,
l-95% CI, u-95% CI, R̂, Bulk ESS and Tail ESS. "Group-level effects ~county" has rows
`sd(Intercept)`, `sd(area)` and `cor(Intercept,area)`. "Family-specific" has `sigma`, `nu`, ….
It uses the host's R̂/ESS functions (`sc_model::diagnose`), so the numbers agree with the rest
of the page. The full positional summary stays available underneath, for anybody reading the
generated program.

## 7. The seams and where the code lives

**`sc-bayesreg`** (new): `parse` (formula text → AST, with errors as sentences carrying a
column span for the editor), `resolve` (against `DatasetShape`), `canon` (→ `Structure`,
`DataPlan`, `Names`), `gen` (→ Stan text), `family` (the table of §3.6), `prior` (the menu, the
defaults, the resolution of §3.7), `output` (the brms-style table) and `BayesRegProvider`.

**`sc-model` additions** (additive, all with defaults):

- `ModelProvider::lower(&self, cfg: &Attrs, data: LowerInput<'_>) -> Result<Option<Attrs>>`,
  where `LowerInput` is the dataset's shape (save, validate) or its frames (preview, fit). It
  returns the configuration the host should bind with: the provider's own keys plus generated
  `bindings`, `dimensions`, `labels` and `policies`. The default is `None` (use `cfg`
  unchanged). The posterior path in `fit.rs` (`fit_posterior`), `validate.rs` and
  `sc-server/src/posterior.rs` call it once and use the result everywhere they now read
  `model.configuration`. The fit stores the lowered configuration in the instance's
  `state`. Data-dependent default priors need the frames, which is why `LowerInput` can carry
  them.
- A binder kind **`terms`**: `{ dataset, terms: [Term], intercept: bool }` → an `N × K` matrix
  plus its column names. Term is a column, an interaction, or a transform from §3.3, with R's
  `model.matrix` coding. It goes through `encode` so categories and transform constants are
  fitted once and stored (TECHNICAL_DESIGN §14.2 "The encoding belongs to the instance"). It is
  the general version of `design` and lives in the host, as the binder does, so a second
  formula front end (PyMC) would get it free.
- A dimension kind **`names`**: a fixed list of labels, from a `terms` binding's columns.
- The binder's existing `index` (grouping levels into a `values` or `rows` dimension), `column`,
  `count`, `size` and `value` cover the rest.

**`sc-stan` change**: split `StanProvider::fit_posterior` into the part that loads the program
from a store and `run_program(&Program, input, cfg, ctx)`, which both providers call, so
compile → chains → draws → publish is one implementation. `interface()` for the new provider
parses the generated text with the existing declaration parser, so every host check (sizes,
types, bounds, draw plan) runs unchanged.

**`sc-server`**: register `BayesRegProvider` beside `stan` in `base_registry`, sharing the
`Arc<StanProvider>`.

**UI** (`ui/analytics/src/models/ModelEditor.tsx`): for this provider, a monospace formula
editor with completion of dataset column names and family dpars, and with parse errors
underlined at their span. Below it: the family and link pickers, the prior table (below), and a
collapsed **Show Stan program** that is read-only (the `make_stancode` equivalent). There is no
bindings table: the bindings are derived and shown read-only under "Data" (the `make_standata`
equivalent).

**The prior table** is the resolved list of §3.7, one row per parameter the formula creates,
recomputed as the formula changes (from a `checkPriors` call that takes the configuration and
answers the resolved rows, through `lower` with the shape, or with the frames after a preview).
For `y ~ x + region + (1 + x | g)` with family `gaussian`:

| class | coef | group | prior | source |
|---|---|---|---|---|
| b | *(all)* | | flat | default |
| b | x | | flat | default (from *(all)*) |
| b | regionNorth | | flat | default (from *(all)*) |
| Intercept | | | student_t(3, 4.1, 2.5) | default, from data |
| sd | *(all)* | g | student_t(3, 0, 2.5) | default, from data |
| sd | Intercept, x | g | (as above) | default (from *(all)*) |
| cor | | g | lkj(1) | default |
| sigma | | | student_t(3, 0, 2.5) | default, from data |

- **Display.** Defaults and inherited rows are shown greyed, with the row they come from named.
  Explicit rows are shown in full weight, with a ✕ that clears them back to the default.
- **Editing a row.** Editing any row makes it explicit, with that row's exact target. Editing an
  *(all)* row sets the whole class, and every coefficient without a row of its own follows it.
- **The prior cell.** It is a text input with completion of the menu's names. The argument
  constraints are checked as the admin types, and a violation is underlined with its sentence
  ("σ must be positive"). A prior not allowed on the class is refused the same way.
- **Bounds.** The `lb`/`ub` cells appear on `b` rows only. Setting one notes that it changes the
  program (a bound is structural, §2.5). The note says nothing about whether that program is
  cached (§5).
- **Unmatched rows.** They are listed at the bottom of the table under "Not matched by the
  formula", each with its reason and a button to delete it or to retarget it to a current
  coefficient.
- **Data-dependent defaults.** They show the rule until a data preview has run, and the
  numbers after (§3.7).
- **Pasted priors.** When the formula field is committed, the pasted rows move into this table,
  and they are highlighted briefly with any replacements named (§3.7).

## 8. Prediction (R2)

The provider knows its linear predictor, so it can predict **in Rust from the stored draws**
without CmdStan's standalone generated quantities, which §14.2 carried past. For a new frame it:

1. encodes the rows with the instance's stored `terms` encoding (unknown category or level →
   error by name);
2. for each draw, computes η = Intercept + X b + Σ r[J]·Z, then the inverse link;
3. answers the posterior mean of E[y | row] (`posterior_epred`). R2 also offers the 5%/95%
   quantiles as `predict("Radon", "q05")`-style variants if the `predict` call shape allows.

This turns `Outcome::Posterior { prediction: Some(label) }` on for this provider only, so
`predict("…")` in a calculated field and `m.predict(…)` work (TECHNICAL_DESIGN §14.2 says that
when picked up, it arrives "as those two accepting a posterior"). It needs the draws of
`Intercept`, `b`, `r` and the distributional parameters to be kept. With `keep_draws: false` a
prediction is refused with a sentence saying so. Draws are thinned to at most 1 000 for
prediction, to bound cost.

## 9. Eject to a Stan model

**"Convert to Stan program"** writes the generated program into a file store the admin picks,
and creates an ordinary `stan` model whose bindings are the lowered ones. That model can be edited
freely from there. It is the escape hatch for anything the formula cannot say, and the reason
the formula provider never accepts raw Stan.

## 10. Out of scope

Smooths (`s()`, `t2()`), Gaussian processes (`gp()`), measurement error and imputation
(`me()`, `mi()`), autocorrelation terms (`ar`, `arma`, `car`, …), mixtures, custom families,
`stanvars`, `sample_prior`, LOO/WAIC, `allow_new_levels`, the QR decomposition, and
`cov =` (pedigree/phylogeny) are all out of scope. Each is refused at save with a sentence. The
canonicalisation rules are written so that any of them can arrive later as a new structural
feature with its own chunk.

## 11. Risks

- **`model.matrix` fidelity.** Getting interactions with missing main effects, and contrasts,
  exactly right is where "the model you expected" is won or lost. The cases in R1.2 come from
  R's documented rules, with expected column names checked by hand.
- **M = 1 merged with M > 1** (§5 rule 2) must not cost sampling efficiency. If it does,
  split the two and accept the extra cache entry. Measure on case A.
- **Data-dependent default priors** (§3.7): the rule is unconfirmed (see §3.7).
- **Vector bounds on `b`** with infinite entries: check the CmdStan version in use before
  relying on it. Otherwise bounds stay structural, which they are in this design anyway.

---

# Part II — Milestones

## R1 — Core GLMM, end to end, cached

- [ ] **R1.1** Crate `sc-bayesreg` skeleton, layer 6 above `sc-stan`; add it to the crate
  table and dependency list in TECHNICAL_DESIGN §2 and §14.2 (a new "Bayesian regression
  from a formula" subsection replacing "a formula front end generating Stan" in *What was left
  out*). Note the clean-room sources (Part I §2.1) in the crate docs.
- [ ] **R1.2** Parser and resolver for §3.1–3.5 (R1 rows): multi-line formulas, `bf()`
  unwrapping, `+ - : * / ^ %in%`, `0/1/-1`, `offset()`, the §3.3 transforms, `|`, `||`,
  `|ID|`, `gr(cor, id)`, `g1:g2`, `g1/g2`, the R1 addition terms. Every error is a sentence
  with a span. Tests: a table of formulas → expected terms and model-matrix column names,
  taken from R's documented `model.matrix` behaviour.
- [ ] **R1.3** Family table (§3.6, R1 families) and the priors of §3.7: the menu with its
  argument constraints and allowed classes, whole-class rows and precedence, the defaults, the
  unmatched-row checks (at save from the shape; level coefficients at preview and fit), and
  the parsing of pasted `prior(…)`/`set_prior(…)`/`c(…)` calls into rows. Confirm the
  default-prior rule against the brms `default_prior` documentation and correct §3.7. Tests:
  each constraint violation's sentence; a row left unmatched by a formula edit is kept and
  flagged, never dropped; a level row unmatched at fit is refused; pasted calls give the same
  rows as the table.
- [ ] **R1.4** Host additions in `sc-model`: `ModelProvider::lower` + `LowerInput`, used by
  `fit_posterior`, `validate` and `sc-server/src/posterior.rs`, with the lowered configuration
  stored in the instance's state. Also the `terms` binder kind (through `encode`, so its
  constants are on the instance) and the `names` dimension. Tests: an existing `stan` model
  binds and fits exactly as before (`lower` → `None`). A `terms` binding gives the matrix and
  names of R1.2's cases.
- [ ] **R1.5** Canonicaliser and generator (§4, §5): `Structure`, `DataPlan`, `Names`, and
  the Stan text with the fixed path `model.stan` and the version header. Tests: the §5
  acceptance cases (byte-identical text and one `CompileCache::key`, and the
  must-differ cases); golden files of the program for every R1 family × {no blocks, one
  correlated block, one uncorrelated, two blocks with a shared `ID` across `mu` and `sigma`}
  × {no aterms, weights, cens, trunc}; and each golden program passes `stanc` when CmdStan is
  present.
- [ ] **R1.6** `sc-stan`: extract `run_program` from `StanProvider::fit_posterior`; the
  `stan` provider's tests pass unchanged.
- [ ] **R1.7** `BayesRegProvider`: config declaration (formula, family, links, priors,
  `center`, and the Stan sampler keys reused from `sc_stan::config_keys`), `validate`,
  `lower`, `interface`, `fit_posterior` via `run_program`, the brms-style parameter table
  (§6), and `program_changed` (the structure hash or the generator version changed).
  Register it in `sc-server`. Tests (need CmdStan, skipped with a notice otherwise; tests
  register `catalog.teardown_hook()`): parameter recovery on simulated data for cases A–E
  (true values inside the 90% intervals). A second model with renamed columns fits **without
  compiling** (`Compiled::cached`). The summary labels read `area`, `county` levels, not
  positions.
- [ ] **R1.8** Admin API and UI: parse/check endpoint with spans, resolved priors, generated
  program and data preview, and `checkPriors` (§7). In `ModelEditor.tsx`: the formula editor
  (with pasted priors moved into the table on commit), the prior table as §7 describes it
  (inherited and default rows greyed, as-you-type constraint checks, bounds on `b` only, the
  "Not matched by the formula" section, the rule shown until a preview gives the numbers), and
  Show Stan program — and no cache state or Compile button (§5). A tutorial
  `docs/tutorial-bayesian-regression.md` walking through radon (cases A, B, D), and an
  OPERATIONS.md note on cache warming.

## R2 — Breadth and prediction

- [ ] **R2.1** Distributional and family breadth: the R2 families of §3.6 (zero-inflated,
  hurdle, beta-binomial, ordinal with flexible thresholds and `thres()`, categorical),
  fixed dpars (`nu = 4`), `mo()` with `simo`, `cs()`, and `gr(by = )`. Golden files and
  recovery tests for each.
- [ ] **R2.2** Non-linear formulas (§5 rule 4): the expression AST, its whitelist (arithmetic,
  `^`, `exp`, `log`, `inv_logit`, `fmin`/`fmax`, …), nlpar lines, `nl` inferred and shown,
  and `center = FALSE` for nlpars. Recovery test for case F; normalisation test (renamed
  nlpars and covariates → one cache key).
- [ ] **R2.3** Multivariate: `mvbind`, several response lines, `subset()`, residual
  correlation for gaussian/student (`rescor`, default on where possible), shared `|ID|`
  blocks across responses, and responses sorted canonically. Test for case G.
- [ ] **R2.4** Prediction in Rust (§8): `Outcome::Posterior { prediction }` for this
  provider, `predict("…")` in formulas and `m.predict` on a handle, thinned draws, and the
  `keep_draws` refusal. Test: predictions match the generated-quantities `epred` of the same
  fit within floating-point error, for cases A, C and D.
- [ ] **R2.5** Eject to a Stan model (§9), and `feldspar stan warm` (§5). Test: an ejected
  model fits to the same summary with the same seed.
