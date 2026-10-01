# Tutorial: The Analytics UI

The Analytics UI is where you look at your data rather than at your application: you build
**datasets**, and from later milestones on you explore them, plot them, test hypotheses, fit
models, draw maps and write reports. Its design is in
[analytics-ui-goals.md](analytics-ui-goals.md). This tutorial grows by one part per milestone.

- **Part 1 — Workspaces and the dataset editor** (this part).

You need a server and an admin login. Nothing else: the data comes from a command.

---

## Part 1 — Workspaces and the dataset editor

A **dataset** is a named definition of rows: a **base** — a table, or another dataset — and an
**ordered list of operations**, each taking the rows the one before produced. It is the same idea
as a pipeline of tidyverse verbs or a chain of pandas calls, except that every formula in it is
in the language Saltcorn's calculated fields already use, and the whole thing becomes one SQL
query run by the database. Nothing is copied: a dataset is read afresh each time.

Datasets are shared. A model reads one ([tutorial-models.md](tutorial-models.md) step 2), and
later milestones' plots, tests and maps read the same ones.

### Step 1 — Demo data

```bash
feldspar demo analytics
feldspar serve
```

The first command makes three tables: `neighbourhoods` (5 rows), `houses` (200, each with a
key to its neighbourhood, some not yet sold and so with no price) and `viewings` (a key to a
house, a date, and whether anyone came). The rows are generated from a fixed seed, so what you
see here is what everyone sees. It refuses to touch tables that are already there;
`--replace` drops and remakes those three ([OPERATIONS.md](OPERATIONS.md) §8.6).

Sign in to the admin UI as an administrator.

### Step 2 — The front page

The admin sidebar has an **Analytics** link, beside *Predictive models* (which stays until a later
milestone folds it in). It opens the Analytics UI at `/analytics/`, a separate application from
the admin UI that shares its sign-in and its light or dark setting.

The front page has two lists, both empty. **Datasets** are the definitions of rows this part is
about; each opens in the **Dataset editor**. **Workspaces** are places you work in, each of one
kind and remembering where you were: a Data explorer, a Model fit, a Map and so on. Look at the
kinds under **New workspace**: they are all listed and disabled, each labelled with the
milestone that brings it, because none is here yet. The first, the Data explorer, comes in
part 2.

### Step 3 — A dataset on a table

In the **New dataset** box, call it `House prices by area`, choose `houses` under **Based on**,
and press **Create**. The base cannot be changed later: every operation is written against the
columns it provides. (To start the same work from another base, clone the dataset and edit the
clone.)

The Dataset editor opens. On the left is the **side panel**: the base, then the operations in order,
none yet. On the right is a **spreadsheet** of the rows: the 200 rows of `houses`, scrolling
smoothly because only what is on screen is drawn, with the row count and what a row is — *one
row per houses* — above it.

### Step 4 — Calculated columns

Press the **+** in the last column header. A form opens for a *Calculated column*. Name it
`price_per_m2` and type the formula `price / area`. The formula box checks as you type: the
server compiles the operation where it would go, so a misspelt column is a sentence under the
box, not an error later.

Press **+** again and add a column `neighbourhood_name`. Start typing `neighbourhoodⱵna` and
the box offers `neighbourhoodⱵname`: it knows `neighbourhood` is a key to `neighbourhoods`, and
offers one step along every key, and — while each row is a row of `houses` — each child table's
count and totals (`viewingsↃhouse.length`, `viewingsↃhouse.sum("…")`). These are the formulas
the calculated-fields tutorial uses. Nothing here is a new language.

Each column is an operation in the side panel, and each was saved as you added it.

### Step 5 — Filter and aggregate

Open the `price` column header's menu (the small toggle beside its name). It offers **Filter…**, **Sort
ascending**, **Sort descending**, **Group by…** and **Stack…**. Choose **Filter…**: the form
opens with `price ` already typed. Make it `price > 100000` and apply it. The count above the
spreadsheet drops, and the unsold houses, which have no price, are gone with the cheap ones.

Now open the `neighbourhood` header's menu and choose **Group by…**. An *Aggregate* form opens,
grouped by `neighbourhood`, with a count already in it. Add a summary: name it
`mean_price_per_m2`, function *mean*, column `price_per_m2`. Apply it. The spreadsheet has one
row per neighbourhood: its key, the count and the mean. Above it, the grain reads *one row per
neighbourhood*.

Grouping by a key keeps the key. After this Aggregate, `neighbourhoodⱵname` still works in a
formula. A row is now a neighbourhood rather than a house, though, so the child tables on offer
are those of `neighbourhoods` — `housesↃneighbourhood.length` counts every house in it, sold or
not — and `viewingsↃhouse` is no longer one of them. The formula box only offers what makes
sense for what a row is at that point, and the server refuses the rest with a sentence naming
the grain. Group by a month instead, and there is no table a row is a row of, so no child
tables at all.

The **Add an operation** button under the side panel offers every operation, grouped by what it
does: those that **keep the rows** (Calculated column, Filter, Select columns, Sort, Window
column), those that **change what a row is** (Aggregate, Limit, Stack, Split, Complete), and
those that **combine** (Join, Union). A new operation goes after the one selected.

### Step 6 — One stage at a time

Click the base in the side panel: the spreadsheet shows the rows of `houses` as they are before
anything. Click the first Calculated column, then the second, then the Filter, then the
Aggregate: each time, the spreadsheet shows the rows **as they are after that operation**. This
is how you check a dataset — one step at a time, looking at the rows each step makes.

Switch the Filter off with its switch. It stays in the list, dimmed; the Aggregate's counts go
up, because the cheap houses are back in. Switch it on again.

Now break something on purpose. Double-click the first Calculated column and rename it from
`price_per_m2` to `ppm`. The Aggregate is marked in red with a sentence: its summary reads
`price_per_m2`, and that is not a column at this point. The operations after it are marked *not
reached*, and the side panel's heading says the dataset *has an error*. Nothing was thrown
away — the dataset was saved as it is, with the error shown where it is — so edit the Aggregate's
summary to read `ppm`, or rename the column back, and the red goes.

Operations can be dragged to reorder. Drag the Filter above the Calculated columns: nothing
changes in the result, because the Filter only reads `price`. Drag the Aggregate above the Filter
and the Filter is marked: after the Aggregate there is no `price` column any more.

### Step 7 — Back to the list

There is no Save button: every change was saved as you made it. Press **All datasets** above
the side panel to go back to the front page, where `House prices by area` is listed with its
four operations and what a row is. Click it and it opens as you left it. The editor's address
is `#/datasets/<id>`, so a bookmark or a reload comes back to it too.

On the front page, **Clone** copies a dataset (as `House prices by area
(copy)`), and **Delete** warns you first: it lists the models that use the dataset, which
will not fit until they are given another one, and it refuses while another dataset reads it.

### Step 8 — A model reads it

The *Predictive models* screens in the admin UI now pick a dataset instead of building one.
Follow [tutorial-models.md](tutorial-models.md) step 2 to make the `Sold houses` dataset, then
create a linear regression on it. The **Dataset** card is a dropdown of the named datasets, with
**Edit in Analytics** beside it, and the preview under it shows the rows and their types. Fit it.
The `estimated_price` calculated field from the models tutorial (`predict("House prices")`)
returns a number for every house, sold or not.

A fit records the version of the dataset it read. Change the dataset afterwards, and the fit's
screen says the dataset has changed since it was fitted; its predictions keep reading the rows the
way they were read when it was fitted, until you fit again.

### What to remember

- **A dataset is a base and a list of operations**, and each operation's formulas are the
  calculated-field language. It becomes one SQL query, on Postgres or SQLite.
- **Every operation is a stage you can look at.** Click it, and the spreadsheet shows the rows
  after it.
- **A row has a grain** — a row of a table, a group, or something derived — and the grain decides
  what a formula may reach. Grouping by a key keeps the key usable.
- **An operation that stops working is marked, not removed.** The one that repairs it can then
  be made in its place.
- **Workspaces remember**, and datasets are shared: the model you fit and the plots of later
  milestones read the same definition.
