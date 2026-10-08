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
