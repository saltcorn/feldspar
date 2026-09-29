# Tutorial: Predictive models — what the data implies

Everything else in Saltcorn **retrieves**. A query answers what is in your tables; a calculated
field answers what follows from a row; an agent answers a question about text. None of them
answers *"what will this house sell for"*, and none of them answers the question that is often
the real one: *"is the coefficient on floor area actually different from zero"*.

A **model** is that. It is a saved question about a table — which rows and which derived values
make up the data, which provider answers it, and with what settings — and **fitting** one leaves
a **fit** (a *model instance*) behind: the coefficients, the metrics, and enough state to apply
it to a row nobody has seen yet. Both halves are first class. You will read a coefficient table
in this tutorial, and you will also have a column that predicts the price of every house, sold
or not, each time the table is read.

By the end you will have:

- a `houses` table with some sold houses in it;
- a **House prices** model whose dataset mixes a field, an arithmetic expression, a join path and
  an aggregation, filtered to the sold ones;
- a linear-regression fit whose coefficient table has standard errors, *t* and *p*;
- an `estimated_price` calculated field whose formula is `predict("House prices")`, and a
  nightly trigger that refits the model;
- and a **second** fit of the *same dataset* from a scikit-learn gradient-boosting model supplied
  by a bundled Python module — with its RMSE next to the regression's, and nothing on the screen
  caring that one of the two answers came from Python.

This continues from [tutorial-triggers.md](tutorial-triggers.md): you have a server, an admin
login, and you know what a trigger is. The last step wants a Python-capable build and the
`feldspar-sklearn` module ([tutorial-python.md](tutorial-python.md) explains why Python is a
build and not a flag); the first six steps need neither.

---

## Step 1 — Two tables and some rows

Under **Data → Tables**, make `neighbourhoods` first, because `houses` will point at it:

| Field | Type | Notes |
|---|---|---|
| `name` | String | |
| `average_income` | Float | |

Add three rows: `Riverside` / `62000`, `Old Town` / `48000`, `Northgate` / `35000`.

Now `houses`:

| Field | Type | Notes |
|---|---|---|
| `address` | String | |
| `area` | Float | square metres |
| `bedrooms` | Integer | |
| `neighbourhood` | Key to `neighbourhoods` | |
| `sold` | Bool | |
| `price` | Float | what it sold for; empty until it does |

And a third, `viewings`, so the dataset has something to aggregate:

| Field | Type | Notes |
|---|---|---|
| `house` | Key to `houses` | |
| `attended` | Bool | |

Put twenty or thirty houses in — enough that a split has something on both sides. Mark most of
them `sold` with a `price`, leave a few unsold with `price` empty, and give some of them
viewings. A model is only as interesting as its rows; if you would rather not type, an
`insert_row` trigger with a `run_js_code` body will fill the table in a loop, and
[tutorial-triggers.md](tutorial-triggers.md) step 5 shows how.

---

## Step 2 — The Models tab, and a dataset that is a list of formulas

**Models** is in the sidebar. It is empty; press **New model** and call it `House prices`.

The first card is the **Dataset**, and it is where the interesting decision of this whole feature
lives. A dataset is a table, a list of named columns, and one optional filter — and every column
is a **formula in the language you already use for calculated fields and ownership rules**. There
is no separate "add a field / add a joinfield / add an aggregation" vocabulary: the picker below
the list writes a formula into the row, and you can type over what it wrote.

Set **Table** to `houses`, then build these five columns. Use the picker (its three groups are
the table's own fields, one join path per column of each table a key points at, and one
aggregation per incoming key) or type them:

| Column | Formula | What it is |
|---|---|---|
| `price` | `price` | a field — the label |
| `area` | `area` | a field |
| `bedrooms` | `bedrooms` | a field |
| `neighbourhood_income` | `neighbourhoodⱵaverage_income` | a **join path**: follow the key, take a column |
| `viewings_count` | `viewingsↃhouse.length` | an **aggregation** over the incoming key |

Then put this in **Filter**:

```
sold === true
```

The filter is one boolean formula, and it must be an explicit **comparison** — `sold` on its own
is refused with a sentence saying so, because a bare value in boolean position has no `WHERE` to
become. (The same rule governs every other boolean formula in Saltcorn, so it is one thing to
learn rather than a rule about datasets.)

The filter is why `price` is never null in this dataset: an unsold house has no price, and a fit
over rows whose label is missing is a fit over nothing.

**And the filter is about the fit, not about prediction.** It says which rows the model is
*computed from*; step 6 will ask this model about an **unsold** house, which it excludes, and get
an answer. That is the point of a model of house prices, and a prediction therefore reads past
the filter — while still reading through the dataset, so the join path and the aggregation are
computed exactly as they were at fit time.

Because it is the ordinary formula language, `price / area` is a column too if you want one, and
so is `log(price)`. Two things it may **not** mention are `user` and the operation flags
(`_insert` and friends) — a dataset has no caller, and a fit that meant something different
depending on who pressed the button would be indefensible.

### The preview is the point of the card

As you type, the **Preview** below the column list fills in: the first rows, and — above them —
**the type each column actually came back as**. That matters more than it looks. Nothing in your
schema says what `viewingsↃhouse.length` is; the preview does, and the provider's form further down the
page is built out of exactly those types. If a formula is wrong you get a sentence here rather
than a failed fit five minutes later.

---

## Step 3 — Choose a provider, and read what it asks for

The **Provider** card lists what this server can fit. On a default build:

| Provider | What it answers |
|---|---|
| `linear_regression` | a number per row, with coefficients you can read |
| `logistic_regression` | a class, with a probability |
| `random_forest` | a number *or* a class, according to the label's type |
| `kmeans` | a cluster number per row |
| `pca` | a vector per row |
| `t_test` | no per-row answer — the statistic *is* the answer |
| `anova` | likewise |

> If this list has only `t_test` and `anova` on it, the screen says so: this server was built
> with `--no-default-features` and the five smartcore providers were **compiled out**. That is a
> supported build, not a bug — see [OPERATIONS.md](OPERATIONS.md) §1.

Choose **`linear_regression`**. Two things appear.

**Settings** is the provider's own form, and it was built against *your dataset*: the **Label**
field is a dropdown of the numeric columns you just defined, not a text box. Choose `price`.
Leave **Fit an intercept** ticked.

**Outcome** now reads **Regression on price**. The outcome is a *function of the configuration* and
not a property of the provider — pick `random_forest` and a text label and it becomes a
classification — and it is what decides which metrics you get, what the fit may be written into,
and whether prediction means anything at all (for `t_test`, it does not).

**Hyperparameters** is empty for a linear regression, which has none. Step 7 uses one that has
three.

---

## Step 4 — The split, and why it is not a shuffle

The **Split** card is `train 0.8`, `validation 0`, `test 0.2` and a seed. Leave it.

The note under it is worth reading once. Which side a row falls on is a **hash of its primary key
and the seed** — not a shuffle. Three consequences:

- the split does not depend on row order, so re-reading the dataset in a different order splits
  identically;
- **new rows arriving keep every old row where it was**, so the test RMSE of this fit is
  comparable with the test RMSE of the one you do next week, which is the whole reason anybody
  looks at two fits of one model;
- and it is reproducible from the row, so "was this house in the training set" is answerable
  later without anybody storing a list of ids.

The price is that the fractions are approximate — 30 rows at 20% is whatever the hash gives, not
exactly 6 — so each fit records the counts it actually got, and step 5 shows them.

A table with **no single primary key** cannot be split this way and a fit of it is refused in
those words. There is nothing stable to hash.

---

## Step 5 — Fit it, and read the coefficients

Press **Fit**.

The button saves the model first, deliberately: a fit of what is on the screen and a save of what
is on the screen are the same intention, and the alternative is a button that quietly fits the
version you last saved rather than the one you have been editing.

The **Fits** table below gains a row saying `fitting`, and the screen polls. Fitting is a **job,
not a request** — a fit reads every row and runs an optimiser, which is seconds at best and
minutes at worst, and it must not be an HTTP request that a proxy gives up on while the work
carries on invisibly. The instance row *is* the job record; there is nothing in memory to wait
on. (Which is also why a server that is restarted mid-fit marks that fit **failed** at boot,
saying "the server restarted while this fit was running", rather than leaving a row that says
`fitting` for ever.)

A second later it says `fitted`. Open it.

**Rows** counts where everything went: *Selected*, *Train*, *Validation*, *Test*, *Dropped*. A
dropped row is one the encoding could not represent — a null in a feature, most often — and it is
counted and shown rather than quietly excluded, because a fit over 27 of 30 rows is a different
claim from a fit over 30.

**Metrics** is R², RMSE and MAE for train and test. Read the **test** column; the training one is
measured on the rows the fit was computed from and is optimistic by construction. These numbers
are computed by Saltcorn, not by the provider — the same code over the same splits for every
provider — which is what will make step 7's comparison mean something.

**Parameters** is the provider's own, and for a regression it is the point:

```
Coefficients
term                   estimate    std. error      t        p
(intercept)           41230.55      9128.31     4.517    0.000
area                   1873.42       132.07    14.185    0.000
bedrooms               8420.10      4102.66     2.052    0.051
neighbourhood_income      0.94         0.21     4.476    0.000
viewings_count          311.28       902.44     0.345    0.733
R²                        0.918
adjusted R²               0.905
residual standard error   14882.3
observations              24
residual degrees of freedom 19
```

Standard errors, *t* and *p* are computed here from the residual variance and `(XᵀX)⁻¹`;
smartcore does not supply them, and without them a slope coefficient is a number with no way of
telling whether it means anything. In the fit above, floor area and neighbourhood income are
real, bedrooms is marginal, and the number of viewings is noise.

A **categorical** feature — swap `neighbourhood_income` for `neighbourhoodⱵname` and refit — comes
back as one row per neighbourhood except the first, which is the baseline. That is reference
coding, and the coefficient is "compared with `Northgate`".

Finally, **Try a row** at the bottom: type an area, a bedroom count and the rest, press
**Predict**, and you get a price. Note that a categorical feature is a **dropdown** of the
categories this fit was shown, because a value it was not shown is refused by name rather than
encoded as a row of zeros — a row of zeros would be a confident answer from a model that was
never shown the input.

Press **Activate** at the top. Exactly one fit of a model can be active, and it is what lets the
next step name the *model* rather than pinning a particular fit.

---

## Step 6 — A calculated field that predicts

A prediction is a value computed from a row, so it is written where every other computed value
is: in a **formula**. The formula language has one function for it, `predict`, and there is no
separate "prediction" action.

Under **Data → Tables → houses**, press **Add field**:

| Field | Value |
|---|---|
| Name | `estimated_price` |
| Calculated (computed on read, no stored column) | ticked |
| Value type | Float |
| Formula | `predict("House prices")` |

Save, and open the table's rows. Every house has an `estimated_price`, including the unsold ones
with no `price`. Insert a new house (address, area, bedrooms, neighbourhood, `sold` unticked) and
it has one too. Nothing wrote it: the field is computed each time the table is read, through the
model's **active** fit. That includes every API that lists `houses`
(`GET /api/tables/houses/rows`, a REST provider, GraphQL).

What to know about it:

- **The argument is the model's name, as a quoted string.** It names the model and never a
  particular fit. Refit, press **Activate** on the new fit, and the next read follows it with
  nothing edited. A computed name, a second argument, or a `predict` inside an `=>` is refused
  when you save, naming the call.
- **It is checked when you save the field.** The model must exist and must be a model of
  *this* table ("`House prices` is a model of `houses`, and this formula is on `orders`"). It
  must answer something per row, so `predict("Price test")` over a t-test is refused. And the
  field's type must be able to hold the answer: a text field is refused with "the field is text
  and `House prices` predicts float". A model with no active fit is accepted, but the save
  tells you that every read of `houses` fails until a fit is active.
- **A page is one prediction.** A listing of fifty houses asks the provider once, with all fifty
  rows, not fifty times. For a Python provider that is the difference between usable and not.
- **The dataset's filter does not apply here.** An unsold house is not one of the rows the model
  was fitted from, and it is exactly the row you want an answer about. A prediction reads each
  house *through* the dataset, so `neighbourhood_income` and `viewings_count` are computed the
  way they were at fit time, but it reads past the `sold === true` filter.
- **You cannot filter or sort on it.** The database never sees the value, because it is
  computed after the rows are read. `?estimated_price=gt.300000` is refused with "cannot filter
  on `estimated_price`: `estimated_price` is computed after the rows are read, because it calls
  `predict`, so the database never sees it". The same applies to `order=`, and to GraphQL's
  `where` and `order_by`.
- **An error fails the read.** If the model cannot answer for a row, for example a neighbourhood
  the fit never saw, the listing fails. The error names the field, the model and the row ("the
  row whose id is 31"). A null in its place would be a wrong answer that looked like a missing
  one. It follows that a model with no active fit makes every read of `houses` fail. Keep a fit
  active once a field depends on it.
- **An ownership rule may not call `predict`.** A rule that waited on a model would make every
  read wait on it, and an error in a rule means "deny".

### The stored variant

A calculated field is always the current fit's answer. Sometimes you want the estimate a house
had **when it was listed**, kept even after the model changes. That is a stored value, and a
trigger stores it. Add a plain Float field `listing_estimate` to `houses`, then **Triggers → New
trigger**:

| Field | Value |
|---|---|
| Name | `estimate_on_insert` |
| Event | `A row is inserted` |
| Table | `houses` |
| Action | `update_rows` |
| Table (action setting) | `houses` |
| Where | `id === row.id` |
| Assignments | `{ "listing_estimate": "predict(\"House prices\")" }` |

The same `predict` works in any formula that is evaluated against a row. That includes an
`update_rows` assignment, an `only if`, and a `{{ predict("House prices") }}` in a `send_email`
subject. An `insert_row` value is not evaluated against a row, so it is refused there, and the
refusal says where it does work.

### Refit every night

Houses keep selling, so the model should keep learning. **Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `refit_house_prices` |
| Event | `daily`, at 03:30 |
| Action | `fit_model` |
| Model (action setting) | `House prices` |
| Make the new fit active | `if_clean` |
| Wait for the fit to finish | ticked |

Each night fits the model again over whatever is `sold` by then. With **`if_clean`**, the new fit
becomes the active one only if it fitted with **no warnings**, and the next read of
`estimated_price` follows it. A fit with a warning is kept for you to read on the Fits list but
left inactive, so yesterday's fit goes on answering. (`never` only fits. `always` activates any
fit that did not fail.) What counts as a warning is the provider's to say. A scikit-learn
estimator that did not converge is one, and so is a Stan fit with divergences.

The action answers `{ instance, status, active, error, warnings, metrics }`. In a workflow, a
later step can branch on `metrics.test.r2` if you would rather decide by the numbers yourself.

---

## Step 7 — The same dataset, answered by scikit-learn

This step needs a build with Python (`cargo build --release -p sc-cli --features python`) and a
`python3` with `pip` on the host.

Go to **Settings → Modules**. `feldspar-sklearn` is in the bundled catalog — it ships in the
release; scikit-learn itself does not, and pip fetches it now. Press **Install**. The card comes
back saying **5 model providers**.

Now open **Models → New model** and build the *same* five columns and the same `sold` filter
again, calling it `House prices (boosted)`. Retyping is on purpose: a dataset belongs to its
model and is not shared, which is why there is no Datasets tab. A shared, named dataset would
need a lifecycle — what happens to the four models fitted against it when somebody adds a column,
whether a fit made against version 1 is still readable — and that is a versioning problem bought
for a saving that copying a column list answers instead.

Change the provider to **`sklearn_gradient_boosting`**. The Settings card looks the way the
regression's did — a **Label** dropdown over your dataset's columns — because a Python provider
declares its fields the same way a Rust one does. Choose `price`. Outcome: **Regression on price**,
decided by the type of the column you chose, exactly as the built-in random forest's is.

**Hyperparameters** now has three: Trees, Learning rate, Maximum depth. Type a **list** into one:

```
Trees                50, 100, 200
Learning rate        0.1
Maximum depth        3
```

A value fits once. A **list** makes a grid: each point is fitted and scored on the **validation**
split, the winner is refit, and the reported test metrics are the winner's. So set the split to
`train 0.6`, `validation 0.2`, `test 0.2` — with no list you would leave validation at 0, and a
fit with no search must not pay for one.

Press **Fit**. When it finishes, the instance screen has a **Hyperparameter search** card: every
point tried, its validation score, and the chosen row highlighted. The search is inspectable
rather than a number that appeared.

**Parameters** is a feature-importance table and a "Trees fitted" scalar — scikit-learn's own
vocabulary, rendered by the same three block kinds (a scalar, a table, a block of text) that the
regression used. And **Metrics** is the same five numbers as before, computed by the same host
code over the same splits.

Which is the point of the whole arrangement: put the two instance screens side by side and
compare test RMSE. The comparison is honest because the rows are the same rows (the split is a
hash of the primary key, so both fits held out the same houses) and the metric is the same code.
Nothing on either screen knows that one of the two answers came from Python.

---

## What to remember

- **A dataset is a list of formulas**, in the language you already know, plus one filter. The
  picker writes them; you can type over them. The filter chooses the rows the model is fitted
  **from**, and never the rows it may be asked about.
- **A dataset belongs to its model.** There is no Datasets tab and no sharing — *Duplicate model*
  is the answer to wanting the same columns twice.
- **The split is a hash of the primary key**, so fits of one model are comparable and new rows do
  not reshuffle the old ones.
- **The encoding is fitted once, on the training rows, and stored on the fit.** A category at
  predict time that the fit never saw is an error naming the column and the value — not a row of
  zeros.
- **Metrics are Saltcorn's; parameters are the provider's.** That is what makes a scikit-learn
  RMSE comparable with a smartcore one on the same screen.
- **Fitting is a job.** The instance row is the job record, the screen polls, and a restart marks
  a running fit failed rather than leaving it running for ever.
- **Prediction is a formula.** `predict("House prices")` in a calculated field is a live
  column, computed a page at a time through the active fit. The same call in an `update_rows`
  trigger stores it. `fit_model` with `activate: if_clean` keeps the active fit current without
  anybody editing a formula. In code, `models.get("House prices")` answers a handle whose
  `predict` takes a row or an array of them ([tutorial-triggers.md](tutorial-triggers.md)).

A dataset is bounded: `--model-max-rows` (default 200 000) is the ceiling, the count is asked for
before the rows, and a dataset over it is refused in a sentence telling you to add a filter or
raise the flag. See [OPERATIONS.md](OPERATIONS.md) §8.3.

Next: [tutorial-python.md](tutorial-python.md) for the other half of step 7 — writing your own
model provider with `@sc.model_provider` — and [tutorial-modules.md](tutorial-modules.md) for the
JavaScript equivalent, which exports `modelproviders` beside its `actions`.
