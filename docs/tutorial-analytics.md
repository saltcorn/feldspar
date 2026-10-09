# Tutorial: The Analytics UI

The Analytics UI is where you look at your data rather than at your application: you build
**datasets**, and from later milestones on you explore them, plot them, test hypotheses, fit
models, draw maps and write reports. Its design is in
[analytics-ui-goals.md](analytics-ui-goals.md). This tutorial grows by one part per milestone.

- **Part 1 — Workspaces and the dataset editor.**
- **Part 2 — The data explorer:** plots, summary tables and hypothesis tests.
- **Part 3 — Models:** the model editor, a fit's outputs, comparing models, and a model from a
  box plot.
- **Part 4 — Reports:** split view, dragging plots and model outputs into a report, and printing
  it to PDF.
- **Part 5 — Maps:** importing a map file, a map in the explorer, the Map workspace with its
  layers, styles, attribute table, selections, reference layers and toolbox, and a map in a
  report.
- **Part 6 — Dashboards:** tiles from any source, stat cards, and filtering every tile by a
  click, a brush or a district on a map, with drill paths and a filter bar.

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

The first command makes three tables for this part: `neighbourhoods` (5 rows), `houses` (200,
each with a key to its neighbourhood, some not yet sold and so with no price) and `viewings` (a
key to a house, a date, and whether anyone came). It also makes the tables and datasets part 2
explores: ignore them for now. The rows are generated from a fixed seed, so what you see here is
what everyone sees. It refuses to touch tables that are already there; `--replace` drops and
remakes the demo's tables ([OPERATIONS.md](OPERATIONS.md) §8.6).

Sign in to the admin UI as an administrator.

### Step 2 — The front page

The admin sidebar has an **Analytics** link. It opens the Analytics UI at `/analytics/`, a
separate application from the admin UI that shares its sign-in and its light or dark setting.

The front page has three lists. **Datasets** are the definitions of rows this part is about; each
opens in the **Dataset editor**. Three are there already — `Houses`, `Measurements` and `Events`,
which the demo made for part 2. **Models** are part 3's. **Workspaces** are places you work in,
each of one kind and remembering where you were: a Data explorer, a Report, a Map and so on.
There are none yet.
Look at the kinds under **New workspace**: all are listed, and all but the Data explorer
(part 2) and the Report (part 4) are disabled, each labelled with the milestone that brings it.

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

A model picks a dataset rather than building one. Follow [tutorial-models.md](tutorial-models.md)
step 2 to make the `Sold houses` dataset, then press **New model** on its row: the model editor
opens with the dataset chosen. Make it a linear regression of `price` and fit it. The
`estimated_price` calculated field from the models tutorial (`predict("House prices")`) returns a
number for every house, sold or not. Part 3 is the model editor itself.

A fit records the version of the dataset it read. Change the dataset afterwards, and the model
editor says the dataset has changed since the fit; its predictions keep reading the rows the
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

---

## Part 2 — The data explorer

A **Data explorer** workspace turns a dataset into plots, summary tables and hypothesis tests
by dropping columns on **drop zones**. You never choose a chart type or a test by name unless
you want to: a number by a category is a box plot with an analysis of variance beside it,
because that is what it usually is. Everything is computed by the database and the server, so
a table of a million rows draws as quickly as one of a hundred.

### Step 1 — The demo's other tables

`feldspar demo analytics` from part 1 also made `patients` (90, a third each on a placebo, a
low dose and a high dose of a drug), `measurements` (each patient's blood pressure before and
after the treatment) and `events` (a million requests to a web site: a kind, a duration in
milliseconds, a size in kilobytes and an hour of the day), and three datasets over them:

- **Houses** — every row of `houses`.
- **Measurements** — every row of `measurements`, with two calculated columns: `treatment`
  (`patientⱵtreatment`, along the key) and `change` (`after - before`).
- **Events** — every row of `events`.

If you ran part 1 before these existed, run `feldspar demo analytics --replace`: it remakes the
demo's tables and makes the datasets, and leaves your own datasets (and any of these three that
is already there) alone.

### Step 2 — A workspace

On the Analytics front page, under **New workspace**, call it `Exploring houses`, choose the
kind *Data explorer* and press **Create**. It opens at `#/w/<id>`. Pick `Houses` from the
**Dataset** drop-down at the top left. Under it are the dataset's columns, each with a mark for
its type (`#` a number, `Aa` text, `→` a key, `✓` a yes/no); along the top, the **gallery**; under
that, nine drop zones: X, Y, Color, Size, Shape, Label, Facet rows, Facet columns and Wrap.

### Step 3 — A scatter plot

Press **Scatter plot** in the gallery. It fills X and Y with the first two numbers, `area` and
`bedrooms`. Drag `price` from the column list onto Y (a drop on Y replaces what is there; hold
Shift to add instead), and `neighbourhood` onto Color: one colour per neighbourhood, with a
legend. The neighbourhoods are shown by their key, 1 to 5; a Calculated column
`neighbourhoodⱵname` in the dataset would show their names.

Beside the plot, the **Tests** panel already says something: *price rises with area: r = 0.94
(p < 0.001)*, with Pearson's correlation, the linear regression (a slope of about 2,121 per
square metre) and Spearman's correlation as the alternative.

The row of marks under the drop zones is the **mark palette**. *Auto* underlines the mark the
explorer chose — *Points*. Press *Line*: the same columns, drawn as a line through the rows.
Press *Auto* again to go back.

### Step 4 — Small multiples

Drag `year_built` onto **Wrap**. A year has too many values to make a plot of each, so it is
binned — the chip says *bins* — into decades: eight small plots, 1950–1960 to 2020–2030, sharing
their axes. Click the chip's *bins* to see the problem it solves: a plot for every year is more
than the 48 the explorer draws, and it says so in a sentence; click again to bin it back. The tests repeat for each decade, one
section each, headed by its decade.

Remove `year_built` from Wrap with its **×**.

### Step 5 — Layers

Press **Layers**. A panel opens on the right:

- **Add layer → Linear fit**: a straight line per neighbourhood with its 95% confidence band.
  Switch its *One per colour group* off for one line through all the houses.
- Under **Scales**, set Y to **Log**. Prices are positive, so nothing is left out; a column with
  zeros would be, and the explorer would say how many rows.
- Under **Reference lines**, add one on Y at `300000` with the label `300k`: a dashed line
  across the plot.

The layers panel's changes are laid over whatever the drop zones make, so they stay when you
drop other columns. Close the panel, and remove the fit and the log scale again, or press
**Clear** to start from nothing.

### Step 6 — A box plot and its tests

Put `price` on Y and `neighbourhood` on X (press **Clear** first if anything else is on the
zones). A number by a category: a **box plot**, one box per neighbourhood — the quartiles, the
whiskers to the furthest prices within 1.5 box-lengths, and any prices beyond them as points.

The **Tests** panel now compares the groups:

- **One-way ANOVA**: F = 0.74, p = 0.56.
- **Kruskal–Wallis test**, its *alternative* that does not assume the prices are normal:
  p = 0.63.
- **Pairwise comparisons (Tukey)**, folded away: every pair of neighbourhoods, the difference
  of their means with an interval, and a p-value adjusted for making ten comparisons at once.

Its sentence reads *No clear difference in price between the groups of neighbourhood
(p = 0.63)*. That is the honest answer for this data: a house's price here depends far more on
its area than on its neighbourhood, and the boxes overlap almost entirely. Under the table, the
notes say why the sentence reports Kruskal–Wallis rather than the ANOVA: the prices of
neighbourhood 4 are *clearly not normal* (Shapiro–Wilk, p = 0.024). The explorer always shows
both tests and reports the one whose assumptions hold.

Now compare only two neighbourhoods. Press **Edit dataset** under the Dataset drop-down: the
Dataset editor opens on `Houses`. Add a **Filter** `neighbourhood <= 2` and go back to the
workspace (the browser's back button, or the front page). With two groups the explorer switches
to **Welch's t-test** (t = 0.10, p = 0.92) and the **Mann–Whitney test** (p = 0.99): with two
groups there is nothing pairwise to compare. Switch the Filter off in the Dataset editor when you
are done, so that `Houses` is every house again.

### Step 7 — Paired measurements

Pick `Measurements` from the Dataset drop-down and press **Clear**. Drag `before` onto Y, then
Shift-drag `after` onto Y too: two columns on Y are compared as one variable — a histogram of
both, bars along Y, coloured by which column a value came from.

In the Tests panel, switch on **Paired**: the two columns are measurements of the same patients,
so what matters is each patient's difference, not the two columns' spread. *before and after
differ by 6.744 on average (p < 0.001)*: the **paired t-test** (t = 8.22 on 89 degrees of
freedom, the mean difference 6.74 mmHg with a 95% interval of 5.11 to 8.37) and the **Wilcoxon
signed-rank test** beside it. Switch Paired off and there is no test: two columns on Y are
either paired, or one too many, and the panel says so.

Did the drug work, or would the pressure have fallen anyway? Clear the zones and put `change` on
Y and `treatment` on X: a box plot per treatment, an ANOVA that finds a difference, and Tukey's
comparisons naming the pairs that differ — each dose against the placebo.

### Step 8 — A summary table

Back on `Houses`, put `price` on Y and `neighbourhood` on X, and press **Summary table** (beside
**Plot**). The same drop zones, as a table: a row per neighbourhood, the number of houses and the
mean price, with a total row. X, Facet rows and Wrap become rows; Color and Facet columns
become columns; each number on Y is a column of cells. The **Cells** drop-down changes the mean
to a median, a sum, a count and so on, and **Totals** switches the totals off. Note that the
count is of houses — unsold ones too — while the mean is of the prices there are.

### Step 9 — A million rows

Pick `Events`, press **Clear**, then **Histogram** in the gallery and put `duration_ms` on X. A
million rows are counted into a few hundred bins by the database, and only the bins reach the
browser: it draws in about a second. Drag `kind` onto Color: the bars split by kind, stacked —
pages are slow, API calls quicker, assets quickest.

Now press **Scatter plot**, and put `duration_ms` on X and `size_kb` on Y (unbinned: a binned
number on X is a category, and would make box plots). Two numbers make a scatter plot, and a
million points is more than a browser can usefully draw: under the plot, *Showing a random sample of 10,000 of 1,000,000 rows*.
The sample is the same every time you draw it, so the plot does not flicker as you work. The
tests do the same where they must: an ANOVA's sums are computed by the database over every row,
but a rank test reads a sample of at most 5,000 values (for each Wrap group), and says so.

### Step 10 — Coming back

Go back to the front page and open `Exploring houses` again: the dataset, the drop zones, the
mark, the layers and the tests' settings are as you left them. The workspace stores what you
chose, not the plot itself, so it is redrawn from the dataset's rows as they are now.

### What to remember

- **Drop columns, not chart types.** The types of the columns on X and Y choose the plot — the
  mark palette and the gallery are there when you want something else.
- **Tests come from the same roles.** Y, X and Wrap decide the test; its assumptions are checked
  and the alternative is shown beside it; the sentence says what the numbers mean.
- **The database does the counting.** Bins, quartiles, means and sums of squares are SQL, so a
  million rows is quick; rows that are drawn one by one are sampled, and the plot says so.
- **A workspace remembers choices**, so the plot follows the data.

---

## Part 3 — Models

A **model** is a dataset and a provider that answers a question about it: a linear regression's
coefficients, a classifier's classes, a posterior's draws. Models are listed on the front page
between the datasets and the workspaces, and a model opens in the **model editor** at
`#/models/<id>`. A model is not a workspace: like a dataset, it is a named thing other things
refer to — a `predict("…")` in a calculated field, a `fit_model` trigger — and there is one of
each, whoever opens it. ([tutorial-models.md](tutorial-models.md) is the longer story of what a
model is; this part is the editor.)

### Step 1 — The way in

The admin sidebar's *Predictive models* is gone: **Analytics** is the way in. An old link to a
model — `/#/models/<id>` in the admin UI, or a fit's `/#/model-instances/<id>` — lands in the
model editor, the fit selected.

### Step 2 — A model on a dataset

A model's features are every column of its dataset but the label, so the dataset is where you
choose them. Make a dataset `Price, area and neighbourhood` on the table `houses` with three
operations: a Filter `sold === true` (an unsold house has no price to learn from), a Calculated
column `neighbourhood_name = neighbourhoodⱵname`, and a Select columns keeping `price`, `area`
and `neighbourhood_name`.

Why the name and not `neighbourhood` itself: the key's values are numbers, the ids of rows of
`neighbourhoods`, and a regression would fit one slope across them as if neighbourhood 4 were
twice neighbourhood 2. A text column is a **category**, and gets a coefficient per level.

On its row on the front page, press **New model**. The model editor opens with the dataset
chosen and its first rows previewed, each column with the type its values came back as. Call the
model `House prices`, choose the provider **linear_regression**, and set its **Label** to
`price`. The outcome reads **Regression on price**. Leave the split as it is.

### Step 3 — Fit it, and read the outputs

Press **Fit**. The model is saved first, then a card shows what the fit is doing — reading the
data, fitting, scoring — sent by the server as it happens, with **Cancel**. A moment later the
**outputs** appear below the form:

- **Coefficients** — an intercept, `area`, and a row for each neighbourhood but the first
  (`Harbour`, alphabetically), the baseline the others are compared with. Each has its standard error, *t*, *p* and stars.
- **Statistics** — R², adjusted R², the residual standard error.
- **Metrics** — R², RMSE and MAE on the training and the test rows.
- **Residuals against fitted values**, with a smoother: flat and centred on zero when a straight
  line suits the data.
- **Actual against predicted**, with the line a perfect model would lie on.

The **More plots** drop-down has the two a regression does not show unasked: a **Normal Q-Q
plot of the residuals** and their histogram. Every plot is a plot spec over the fit's output
data — its scored rows, with fitted values and residuals — drawn by the same code as the Data
explorer's plots.

Below the outputs, **Try a row** asks this fit about a house that is not in the table, and the
**Fits** table lists every fit of the model with what it scored: click one to see its outputs,
**Activate** the one `predict("House prices")` should use.

### Step 4 — The editor remembers

Open the Q-Q plot from **More plots**, and fold **Coefficients** by clicking its header. Go back
to the front page and open the model again: the Q-Q plot is still open and the table still
folded. That is the model's **view state**, a dictionary kept beside the model that the editor
writes as you go. Nothing about fitting reads it, so none of this marks the fit as out of date.

### Step 5 — Clone, change, compare

On the front page, **Clone** the model. The copy, `House prices (copy)`, opens in the editor with
the same dataset — shared, so changing it would change the original model's too. Press **Use a
copy** beside the dataset: the dataset is cloned and the copy chosen. **Edit dataset** opens it
in the Dataset editor; add `year_built` to its Select columns and press **Back to the model**.
Fit.

Back on the front page, tick both models and press **Compare**: their outputs side by side, the
coefficient tables next to each other — the copy's has a `year_built` row — and the metrics and
plots beneath. Nothing about a comparison is kept; it is read and left.

### Step 6 — When the dataset changes

Open `Price, area and neighbourhood` in the Dataset editor and add a Filter `area > 60`. Return to
`House prices`: the fit says *the dataset has changed since this fit*, and its row in **Fits** is
marked *dataset changed*. The fit keeps reading the rows the way it read them; fit again to use
the new definition. `House prices (copy)`, on its own copy of the dataset, is not affected.

### Step 7 — A Stan model

With CmdStan installed, follow [tutorial-stan.md](tutorial-stan.md) part 1 in the model editor:
the program is shown in an editor pane beside the bindings and saved back to its file store
(**Open in IDE** is still there for anything bigger), **Bind automatically** and **Preview data**
check the bindings, and **Fit** shows a progress bar per chain. The warnings come first, since
they say whether anything below can be trusted; then the outputs — a summary table per variable
and **Trace plots**, with **Rank plots** and **Posterior densities** in More plots; then the
diagnostics, and one variable at a time with its trace, its histogram and, for a variable over
a dimension, its forest plot.

### Step 8 — A model from a box plot

In the Data explorer workspace of part 2, put `price` on Y and `neighbourhood` on X: a box plot,
with a one-way ANOVA beside it. Press **Open as model** at the top of the tests. A linear
regression opens in the model editor, named `price by neighbourhood`, on a new dataset
`Houses: price by neighbourhood` — the explorer's dataset with the neighbourhood named by its
`name` (a key is a category, not a number) and a Select columns keeping the two columns. Fit it:
the coefficients are the differences between the neighbourhoods' means, the ANOVA's question
asked as a model. (Y a category instead of a number makes it a logistic
regression.)

### Step 9 — Deleting a model

Delete warns with what uses the model by name: a calculated field calling `predict("House
prices")`, a trigger whose `fit_model` fits it, a code body that asks `models.get("House
prices")`. Its fits go with it.

### What to remember

- **A model is a dataset and a provider.** The dataset chooses the rows and the features; the
  provider's settings choose the label.
- **A fit's outputs are what its provider declared**: tables, and plots over the fit's own output
  data, the optional ones a drop-down away.
- **The editor remembers** how you left it, in the model's view state, which no fit reads.
- **A clone shares its dataset**; *Use a copy* gives it one of its own to change.
- **Compare** puts models side by side; **Open as model** turns a box plot into one.

---

## Part 4 — Reports

A **Report** workspace is a document: headings, text and **panels** — plots, summary tables,
tests and model outputs — on a page that prints. You do not build its plots in it. You make them
where they are made, in the Data explorer and the model editor, and drag them in. A panel in a
report keeps what made the plot, not a picture of it, so it is redrawn from the data each time
the report is opened.

### Step 1 — Two screens side by side

Open `Exploring houses`, the Data explorer workspace from part 2, and pick `Houses`. Press
**Split** in the header: a second screen opens on the right, showing the front page. There, under
**New workspace**, call it `House prices report`, choose *Report* and press **Create**. The report
opens on the right, the explorer stays on the left, and the divider between them can be dragged
(or moved with the arrow keys once it has the focus).

The address now records both sides, so a reload or a bookmark brings both back. Each side has its
own bar, with a way back to the front page and **×** to close it. Press **Split** again to close
the right side.

### Step 2 — Drag a plot in

In the explorer, put `area` on X, `price` on Y and `neighbourhood` on Color: the scatter plot of
part 2, with the tests beside it. The explorer's toolbar has a **⠿ Drag** handle. Drag it onto the
report. The plot and its tests arrive as one panel, titled *price by area — Houses*.

Now change the explorer: drop `bedrooms` on Y. The explorer's plot changes and the report's does
not. A drop is always a **copy**, taken as the plot was when you started dragging, and nothing the
explorer does afterwards reaches it. (Had **Summary table** been showing, the table would have
been dragged instead.)

The report draws its plots **still**: no tooltips, no highlighting on hover and no legend to
click, because a report is for reading and printing. They are drawn as vector graphics on white
paper, even in the dark theme.

### Step 3 — Headings, text, and model outputs

The report's toolbar has an **Add** menu: add a **Heading**, type `House prices`, press Enter.
Add **Text** and write in Markdown:

```markdown
Prices rise with **area** in every neighbourhood. The model below puts a number on it:
- about 2,100 a square metre,
- and little difference between the neighbourhoods.
```

Ctrl+Enter (or a click outside) finishes. Click either block to edit it again. New blocks go at
the end; each block's own menu (**⋯**, shown when you point at it) inserts one above it, moves it
up or down, or sets a heading's level. Add a **Page break** too.

On the left side, go to the front page and open the `House prices` model from part 3. Each output
card has a **⠿ Drag** handle in its header. Drag the **Coefficients** table and the **Residuals
against fitted values** plot into the report. The table is the fit's own table. The plot is drawn
from the fit's scored rows, by the same code as the explorer's.

Arrange the blocks: drag each by its grip (**⠿** at its left) to where it belongs. Put the heading
first, then the text and the scatter plot, then the page break, and the model's outputs after it.
**×** removes a block.

### Step 4 — The report follows the data

In the admin UI, add a row to `houses`: a sold house, with a price. Come back to the report and
reload it. The scatter plot has the new house, because a panel is a **live view** of its dataset.
The residual plot does not. It shows the rows the model was fitted on, and changes when the model
is fitted again.

A report never breaks because what it shows has gone. Delete a dataset or a fit that a panel
reads, and the panel says so in a sentence where its plot was.

### Step 5 — Print it

Set **Page size** to *A4* and **Orientation** to *Landscape*. The paper on the screen widens to
the width it will print at, so lines break and plots are sized as they will be on paper. Dashed
**Page 2** markers show where the pages will break, and the toolbar shows how many pages there are.
A block is never split across pages. A heading moves with the block after it. A page break starts
a new page.

Press **Export PDF**. The report waits until every panel has drawn, then opens the browser's print
dialog showing the report and nothing else: no toolbar, no markers, no other side of the split.
Choose **Save as PDF**. The file is named after the report, and its plots are vectors, sharp at any
zoom. The PDF is made by your browser, not by the server.

### Step 6 — From one report into another

Make a second report, `Summary for the board`, on the right side (front page → New workspace), and
open `House prices report` on the left. Drag the scatter plot's grip from the first report into the
second. Dragged within a report, a block moves. Dragged into another report, it is copied, and the
two copies are independent from then on.

### Step 7 — What uses a dataset

On the front page, press **Delete** on the `Houses` dataset, and read the warning without
confirming. Under *These workspaces show it* it lists the explorer and both reports, each with the
number of its panels that read `Houses`. The model's delete warning likewise lists `House prices
report`, whose two panels show its fit. Press **Cancel**.

### What to remember

- **Make plots where they are made, and drag them in.** The explorer and the model editor are the
  sources, a report is where they go, and a drop is always a copy.
- **A panel is a live view.** It keeps what made the plot, so it shows the data as it is now. A fit's
  outputs show the fit.
- **What you see is what prints.** The page is drawn at its paper width, the page markers are where
  the pages will break, and Export PDF is the browser's print dialog.
- **Split view** puts any two screens side by side, and a change on one side reaches the other.

---

## Part 5 — Maps

A map in the Analytics UI is a stack of **layers**, and a layer is a dataset, the place its rows
are on the ground, and a style. The map never computes anything itself. Counting incidents per
district, finding what is near a point and saving a selection all make ordinary datasets, which
you can open in the Dataset editor, plot in the explorer and use in a model.

Maps need **PostgreSQL with PostGIS** in Feldspar's database ([OPERATIONS.md](OPERATIONS.md)
§10 says how to install it). On SQLite, or on Postgres without the extension, the demo skips its
map tables and says why, and the rest of this part is not available.

### Step 1 — The demo's map tables

Run the demo again with `--replace`:

```sh
feldspar demo analytics --replace
```

On a database with PostGIS it now also makes two tables:

- `districts`: twelve districts of an invented city, each with a `name`, a `population` and an
  `outline` (a polygon). They are the Voronoi cells of twelve seeded points, so every place in
  the city is in exactly one district.
- `incidents`: 2,400 incidents reported in 2025, each with a `category` (theft, burglary,
  vehicle crime, vandalism, antisocial behaviour), a `reported_on` date and a `location` (a
  point). Most gather around a few hot spots in the north and east of the city, where burglary
  is commoner. The rest are scattered everywhere.

It also makes the datasets `Districts` and `Incidents`. The city is laid over Lyon so that the
base map has streets under it, but the districts and incidents are made up. As before, the rows
are the same on every run.

### Step 2 — Import a map file

In the admin UI, go to **Tables → + New table → Create from a map file** and choose
[`docs/tutorial-data/police-stations.geojson`](tutorial-data/police-stations.geojson) from the
Feldspar source tree. Call the table `police_stations`. The import finds a point geometry, a
`name`, a number of `officers` and a flag `open_all_hours`, and the file's ids become the key.
A Shapefile (zipped) or a GeoPackage is imported the same way. If it is in another coordinate
system, PostGIS converts it to longitude and latitude.

In the Analytics UI, make a dataset `Police stations` on the new table, then a Data explorer
workspace on it, and press **Map** in the gallery: four points over the city.

### Step 3 — Incidents on a map

Make a Data explorer workspace `Exploring incidents`, pick `Incidents` and press **Map** in the
gallery. The explorer finds the geometry by itself: the **Geometry** picker says *Automatic
(`location`)*. It would also find longitude and latitude columns, or a foreign key to a table
with geometry. Drop `category` on **Color**. Each category gets the colour it has in a bar
chart, and the legend lists them.

The hot spots stand out at once. Zoom in and out. A layer this size is sent to the browser
whole. One with more than 5,000 features, or very detailed outlines, is sent as vector tiles,
so only what is in view is loaded.

### Step 4 — Open in map

Press **Open in map**. A Map workspace opens with the incidents as its first layer, named after
the dataset. Its left side lists the layers, with the top layer first. Each layer can be shown
or hidden with its check box, moved with the arrows or by dragging, and opened in the Dataset
editor or removed from its **⌄** menu. Click a layer's name to open its settings below the
list: the name, geometry, a filter, the style, the Color/Size/Shape/Label columns, the opacity,
whether it is in the legend, and the fields its popup shows when you click a feature.

Add the districts too: **Add layer → Districts**. Set its style to *Single symbol*.

### Step 5 — Count per district

Open **Toolbox → Aggregate → Count per region**. For *Features* pick the incidents layer, for
*Regions* the districts, and press **Run**. The tool makes a dataset, `Incidents per
Districts`, and adds it to the map as a new layer on top. It is an ordinary dataset with three
operations: a **Spatial join** (which district each incident is within), an **Aggregate** by
district with a count, and a **Complete** that gives every district a row, with 0 if it has no
incidents. Open it in the Dataset editor from the layer's menu to see them.

The new layer is drawn by each district's outline, found through its `district` key. Set its
style to **Graduated colours**, **Natural breaks**, **5 classes**. The server works out the
classes from the counts, and the legend shows them: from 73–107 for the quiet south and west,
to 574 for the busiest district on its own.

### Step 6 — The attribute table

Press **Table**. The rows of the picked layer appear below the map: each district's key and its
count. Click the `count` header twice to sort by it, descending. Click the first row, then
Shift-click the third: the three busiest districts (Kingsmead, Larkspur and Highbury) are
selected and outlined on the map. Selection works the other way too. Click a district on the map
and its row is selected, and Ctrl-click adds another. **Selected only** shows just the selected
rows.

### Step 7 — What is near a point

Pick the incidents layer, set the distance next to **Near a point** to `1000` m, press **Near a
point** and click the middle of Kingsmead. Every incident within 1 km is selected. The database
measures the distances on the Earth's surface, in metres.

Now press **Save selection as dataset**. The new dataset is based on `Incidents`, with a Filter
that keeps the incidents within 1 km of that point. Because it keeps the condition, not the
ids, it stays right when incidents are added. It is added as a layer, and you can open it in
the explorer like any other dataset. A selection made by clicking is saved by its keys instead.
**Lasso** draws a shape to select by, and the box next to it selects by a condition such as
`category == "burglary"`.

### Step 8 — A reference layer

Under **Reference layers**, press **Add reference layer**. Choose *Tiles*, call it
`OpenStreetMap` and give the address `https://tile.openstreetmap.org/{z}/{x}/{y}.png`, with the
attribution `© OpenStreetMap contributors`. A browser may only load images from hosts the
server allows. So the layer first says its host is blocked, and **Allow it** adds the host to
Settings → Maps and reloads the page. A Web Map Service and an ArcGIS map service work the same
way. Reference layers are drawn under your data. Lower their opacity with their slider, and the
incidents' opacity with theirs, until both can be read.

### Step 9 — A map in a report

Press **Split**, and open the `House prices report` of part 4 on the right, or make a new report.
Drag the map's **⠿ Drag** handle into the report. The whole map, with its layers, styles,
reference layers and view, becomes a panel. Like a plot, it is a live view, redrawn from the
datasets when the report is opened. In a report a map is still: it is drawn once and turned into
an image, which is what a browser can print. Press **Export PDF**.

Finally, on the front page, press **Delete** on the `Incidents` dataset and read the warning
without confirming. Among what uses it are the explorer, the map (with the number of its layers
that show it) and the report. Press **Cancel**.

### What to remember

- **A layer is a dataset.** Its geometry is a column, longitude and latitude, or a key to a
  table that has one. Its style decides how it looks, and the map computes nothing else.
- **Tools make datasets.** Count per region is a Spatial join, an Aggregate and a Complete,
  which you can open, change and reuse. Saving a selection makes a dataset too.
- **Selections are conditions.** Near a point, a lasso or a condition is evaluated by the
  database, and the attribute table and the map share one selection.
- **Maps need PostGIS.** Everything else in the Analytics UI works without it.


---

## Part 6 — Dashboards

A **Dashboard** workspace is a page of **tiles**: plots, maps, summary tables and stat cards
side by side. Like a report, it is made of panels dragged in from where they were made. Unlike a
report, it is for using rather than reading. Its tiles keep their tooltips and legends, and they
work together: a click on a bar, a brush along a time axis or a district picked on a map
**filters every other tile**. This part uses the map tables of part 5, so it needs PostGIS.

### Step 1 — A dashboard, and what goes in it

On the front page, under **New workspace**, call it `Incidents dashboard`, choose *Dashboard*
and press **Create**. It opens empty. Press **Split**, and on the left open `Exploring
incidents`, the explorer of part 5.

In the explorer, pick `Incidents`, clear the drop zones and drop `category` on **X**: a bar chart
of the incidents in each category. Drag its **⠿ Drag** handle onto the dashboard. The tile
appears where you drop it. Now drop `reported_on` on **X** instead of `category`: a line of the
incidents reported each day. Drag it into the `House prices report` of part 4 (or any report),
then open the report on the left and drag the line from there into the dashboard. A tile can come
from anywhere that has a drag handle: the explorer, the model editor, a map, a report or another
dashboard. A tile dragged from a report is a copy, as a report's is.

Then open the `Incidents map` of part 5 on the left and drag its **⠿ Drag** handle in. The whole
map comes, with its layers and styles. On a dashboard it stays a real map: zoom, pan, popups.

### Step 2 — Two more datasets

The tiles so far all read `Incidents`. Two more datasets will show how a filter crosses from one
dataset to another. Both are made by the map's toolbox, so stay in `Incidents map`:

- **Toolbox → Aggregate → Count per region**, with the incidents as *Features* and the districts
  as *Regions*, as in part 5. If that layer is still on the map from part 5, you can skip this.
- **Toolbox → Overlay → Spatial join**, with the incidents as *Layer* and the districts as
  *With*. It makes `Incidents with Districts`: each incident with the columns of the district it
  is in. The district's key comes across as `id_right`, a **foreign key** to `districts`.

Open `Incidents with Districts` in the Dataset editor from its layer's **⌄** menu. Add a
**Select columns** operation that keeps `id`, `category`, `reported_on` and `location`, and
renames `id_right` to `district`. A renamed foreign key is still a foreign key.

In the explorer, pick `Incidents with Districts`, drop `district` on **X**, and drag the bar chart
of incidents per district into the dashboard.

### Step 3 — A stat card

Press **+ Add → Stat card**. Choose the dataset `Incidents` and the value *Count*. Under
**Period**, choose *Month*, from *The latest*: the card shows the latest month that has any
incidents, so a year-old demo still has a "this month". Compare with *The previous period* and
tick **Sparkline**, 12 periods. **Preview** shows it as it will be. Press **Add**.

The card shows 189, the incidents in December 2025, with the change since November and a line
of the last twelve months. If a rise were good news you would leave **Higher is
better** ticked. For incidents, untick it, so a rise is shown in red.

Add a second card on `Incidents per Districts`, the *Sum* of `count`, with no period. It shows
2,400: every incident is counted in one district.

### Step 4 — Arrange the tiles

Drag a tile by its grip (**⠿** in its header) to move it. The cell it will land on is outlined,
and the tiles in the way move down to make room. Drag the corner handle at its bottom right to
resize it, in whole cells. The grid is twelve columns wide. Put the two cards on the left, the
bar chart and the line beside them, and the map and the per-district bars below. Each tile's
**⋯** menu also moves and resizes it one cell at a time, and renames it.

On a narrow screen, such as a phone, the tiles are stacked one per row, and moving and resizing
wait for a wider screen.

### Step 5 — Click, brush, and the filter bar

Click the **burglary** bar (726 incidents). The other tiles are drawn again with only the burglaries: the map's
incidents layer, the line and the stat card, which now counts this month's burglaries against
last month's. The bar chart itself keeps showing every category, so you can pick another. A
click on another bar replaces the selection. Shift-click or Ctrl-click adds a bar to it, and a
click on the selected bar again lets it go.

Each tile's header says what filtered it: *by category* on the line and the card. The map's
header says *by category* too, because its incidents layer was filtered. Point at the badge to
see the rest: the districts and the counts per district are not incidents, so the burglary did
not reach them. The per-district bars and the second card say *not filtered*. Their datasets
do have a `category` column, or are made from the incidents, but a filter only crosses to
another dataset through a **key to a table both refer to**. A column with the same name is not
assumed to mean the same thing.

Now drag across the line chart, from March to the end of May. The brushed range is selected, and
every other tile filters to it too, the burglaries and the spring at once. The card now shows May
against April, the latest month left.

Above the tiles, the **filter bar** shows each selection as an outlined chip, *category:
burglary* and *reported_on: 2025-03-01 – 2025-05-31*. Press **×** on one to remove it, or
**Clear all**. Selections are not saved: they are where you are looking, not what the dashboard
is.

A filter that should stay is made with **+ Filter**. Choose `Incidents`, the column `category`,
tick *burglary* and press **Add filter**. A filled chip appears, and this filter is saved with the
dashboard and filters every tile, the bar chart too. Remove it again with its **×**.

Next to **+ Add**, **Refresh** draws every tile again every 30 seconds to every hour, for a
dashboard left on a screen while its data changes. **↻** draws them now.

### Step 6 — A district on the map

On the map tile, click a district of the per-district layer, say Kingsmead, the busiest. The
per-district bars now show only that district, and the second card shows its count, 574. Both datasets have a
foreign key to `districts`: `district` in `Incidents with Districts`, and `district` in
`Incidents per Districts`. Clicking the same district on the plain districts layer does the same.
There, the district is picked by the row itself, and a row of `districts` is what those keys
refer to.

The first stat card is not filtered, and its badge says why: `Incidents` has no column that
refers to `districts`. To make it follow the map, base it on `Incidents with Districts` instead
(**⋯ → Edit**).

### Step 7 — A drill path

On the per-district bar chart, choose **⋯ → Add a drill path…**. Drill down along **X**. The
first level is `district`, what the plot shows. Add `category` as the next level, and press
**Save**. Above the plot it now says *district · click a value to drill down to category*.

Click the busiest district's bar, 11 (Kingsmead). The tile shows that district's categories,
with a breadcrumb above it: *district › 11 · category*. Only this tile is filtered, and the other
tiles are left as they were. Click **district** in the breadcrumb to go back up. At the last level a
click selects, as on any tile. A path can have up to eight levels, along X, Y or Color.

Close the split, and reopen the dashboard from the front page: the tiles, their places, the
drill path, the refresh and any filters of its own are as they were. On the front page, the
delete warning for `Incidents` now lists the dashboard too.

### What to remember

- **A dashboard is for using.** Tiles come from anywhere with a drag handle and stay
  interactive, and the grid keeps itself tidy as you move them.
- **A click is a filter.** Clicking or brushing a tile filters every other tile. The filter bar
  shows what is filtering. Selections are not saved, and filters made in the filter bar are.
- **Filters cross datasets through keys.** A filter reaches its own dataset through its column,
  and another dataset only through a foreign key to the same table. A tile's badge says what
  filtered it, and why something did not.
- **On the server, a filter is a Filter.** The dashboard's conditions become one more Filter
  operation at the end of each dataset, so a plot, a card, a table, a test and a map are filtered
  the same way.
