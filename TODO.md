# Saltcorn v2 — The Analytics UI (milestones A1–A9)

Ordered, checkable task list for implementing [docs/analytics-ui-goals.md](./docs/analytics-ui-goals.md)
(**the goals document** below). Earlier lists are archived in [docs/TODO-mvp.md](./docs/TODO-mvp.md)
and `docs/TODO-post-mvp-1.md` … [docs/TODO-post-mvp-31.md](./docs/TODO-post-mvp-31.md) (most
recently: models without actions of their own). The models this builds on are
[docs/TECHNICAL_DESIGN.md](./docs/TECHNICAL_DESIGN.md) §14.2.

The nine milestones are the goals document's milestones 1–9, prefixed **A** so they are not
confused with the post-MVP milestone numbers. Tasks are numbered within their milestone (A1.1,
A1.2, …) and grouped in phases. Work through them in order: a milestone assumes every earlier
one is done.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Ground rules for every milestone

- **Runnable after every milestone.** When a milestone's last task is ticked, `feldspar serve`
  starts, the admin UI and the Analytics UI build and load, and every feature the milestone
  added can be tried by hand in the browser. The milestone's **Try it** section is that walk
  through, written as steps a person follows. Its final task turns the walkthrough into a
  tutorial section and a definition-of-done test. A milestone that leaves anything half-wired
  (a menu entry that errors, a button that does nothing) is not done. A workspace type or
  gallery item that a later milestone implements is shown disabled, labelled with what it is
  waiting for.
- **Demo data.** `feldspar demo analytics` (A1.18) creates a small, deterministic set of demo
  tables for trying the features. Later milestones extend it (paired measurements and a large
  table in A2, districts and incidents with geometry in A5, incidents over time in A8). Its
  data is synthetic, including the district polygons, so there is no licensing question.
- **Nothing breaks on the way.** The admin's *Predictive models* screens keep working until A3
  replaces them. Existing models keep fitting and predicting across A1's change to the dataset
  model.
- **Both databases.** Dataset operations work on Postgres and SQLite. Spatial features need
  Postgres with PostGIS; on SQLite, and on Postgres without PostGIS, they are refused with a
  sentence saying why. Tests that need PostGIS skip themselves with a message when it is
  absent, and `OPERATIONS.md` says how to install it.
- **Clients and strings.** An `sc-api` schema change regenerates `client.ts` in every UI that
  has a copy (`ui/admin`, `ui/ide`, `ui/builder` and, from A1, `ui/analytics`). User-facing
  strings of the Analytics UI go in a new `analytics` i18n domain.
- **Migrations.** No code for backwards compatibility. Where stored data must change shape,
  idempotent SQL goes in `TABLES_RENAME.sql`, for Postgres and SQLite.
- **Documentation follows the code.** Each milestone updates `docs/TECHNICAL_DESIGN.md` for
  what it built, and grows `docs/tutorial-analytics.md` by one part.

## Implementation decisions this plan makes

The goals document says *what*; these are the *where* and *how* this plan assumes. Deviate
where the code shows a better way, and record the deviation in the CHANGELOG.

- **`sc-dataset`**, a new crate (layer 5, above `sc-catalog`, `sc-expr` and `sc-query`) holds
  dataset definitions, their operations, the stage shapes, the compiler to `sc-query` and the
  `_fd_datasets` table. `sc-model`'s `dataset.rs` moves there; `sc-model` depends on it.
- **`sc-analytics`**, a new crate (layer 6, beside `sc-model`) holds workspaces
  (`_fd_workspaces`), panels, the plot spec and its stat compiler, the hypothesis tests, map
  layers and the analytics framework factory (A9).
- **`ui/analytics`**, a new single-page app (Vite, React, TypeScript, react-bootstrap, the same
  stack as `ui/admin`) served under `/analytics/` in the way the IDE is served under `/ide/`,
  and built into the binary in the way the admin SPA is. A separate bundle, rather than more
  admin screens, so that A9 can mount it as an application framework without the admin shell.
- **Renderers:** Apache ECharts for plots; MapLibre GL JS, with deck.gl for large layers, for
  maps (the goals document's "Rendering").
- **Statistics** (hypothesis tests, kernel density, spatial statistics) are implemented in
  Rust over `statrs`, which is already a dependency, and tested against reference values
  from R or PySAL recorded as fixtures.
- **PDF reports** in A4 use a print stylesheet and the browser's print-to-PDF. Server-side PDF
  generation (for scheduled or emailed reports) is out of scope for now.

---

# A1 — Workspaces and the dataset editor

The new dataset model (a base and an ordered list of operations, goals document "Dataset
operations"), workspace persistence, the Analytics UI shell, and the Dataset editor. The
Dataset editor is not a workspace: the front page lists the datasets beside the workspaces.

**Try it.** Run `feldspar demo analytics`, then `feldspar serve`, and log in as the admin.
1. The admin sidebar has an **Analytics** link (beside *Predictive models*, which stays until
   A3). It opens the Analytics UI's front page: an empty list of datasets, and below it an
   empty list of workspaces whose kinds are all listed but disabled, each labelled with the
   milestone that brings it.
2. Create a dataset "House prices by area" on the base table `houses`. The Dataset editor
   opens, and the spreadsheet shows the rows of `houses`.
3. Click the **+** in the last column header and add `price_per_m2 = price / area`. Add
   `neighbourhoodⱵname` the same way.
4. From the `price` column header's menu, add a Filter `price > 100000`. Then add an Aggregate
   by `neighbourhood` with the mean of `price_per_m2` and a count.
5. Click each operation in the side panel: the spreadsheet shows the data after that
   operation. Disable the Filter and watch the counts change. Rename the column the Aggregate
   uses in the Calculated column: the Aggregate is marked with an error naming the missing
   column.
6. Go back to **All datasets**: the front page lists the dataset with its four operations.
   Open it again: it is as it was saved.
7. In the admin's *Predictive models*, create a linear regression. Its dataset is picked from
   the named datasets, with a link to edit it in the Analytics UI. Fit it, and check that the
   `estimated_price` calculated field from the models tutorial still returns numbers.

## Phase 1 — The dataset model (`sc-dataset`)

- [x] A1.1 The `sc-dataset` crate: `DatasetDef { id, name, description, base, operations }`,
      `Base::Table(name) | Base::Dataset(id)`, `Operation { id, kind, enabled, params }`. The
      `_fd_datasets` table with bootstrap and a store (create, read, update, delete, clone,
      list; names unique). Move `sc-model`'s `Dataset` code here. Tests: the store round-trips
      every operation kind; a duplicate name is refused with a sentence.
- [x] A1.2 Stage shapes and grain: for each position in the list, the columns and their types
      after that operation, and the grain (`Table { table, key }`, `Group { keys }` or
      `Derived`). A foreign key column stays a foreign key through every operation. Formulas in
      an operation are validated against a `SchemaShape` built from the stage before it:
      `Ⱶ` from any foreign key column; `Ↄ` when the grain is a table, or a group on a single
      foreign key column (goals document, "How this fits Feldspar's relational model").
      Tests: `customerⱵregion` after an Aggregate grouped by `customer`; `ordersↃcustomer`
      refused after an Aggregate by month, with a sentence naming the grain.
- [x] A1.3 The operations that keep the grain: Calculated column, Filter, Select columns, Sort,
      Window column (lag, lead, difference, cumulative sum and mean, rank, row number, group
      summary, last non-missing value). Compiled to `sc-query` as nested subqueries, one per
      operation, collapsed where an operation can be merged into the one before. Tests on both
      drivers, comparing with rows computed by hand.
- [x] A1.4 The operations that change the grain: Aggregate (the summaries of the goals
      document except geometry union, which is A5; with no summaries it is `distinct`), Limit
      (first N, random sample with a seed, top N per group), Stack, Split (its new columns fixed
      when the operation is defined, pre-filled from the data), Complete (values from the data,
      from a date or number range, or from all rows of the table a foreign key refers to).
      Tests on both drivers.
- [x] A1.5 The operations that combine: Join (inner, left, full; equality keys; "nearest
      earlier" on a date column) and Union (columns matched by name, an optional source
      column). Add `UNION ALL` to `sc-query` and both dialects. Tests on both drivers,
      including an as-of join.
- [x] A1.6 Datasets over datasets, and invalid operations: a base that is another dataset
      contributes its operations first; a cycle is refused. Disabled operations are skipped.
      Evaluation stops at the first invalid enabled operation and reports it by id with its
      sentence, and the stages before it still read. Tests.
- [x] A1.7 Reading a stage: `read_stage(def, upto, page)` returns a page of rows, the column
      types and the total row count, reading as the caller (for now, only the admin reads).
      Tests: paging is stable under the dataset's order; the count matches.

## Phase 2 — Models use named datasets

- [x] A1.8 `Model.dataset` and each related dataset become references to named datasets
      (`dataset_id`, and `{ name, dataset_id, label }` for related ones). A fit snapshots the
      resolved definition and its hash into the instance, and the instance reports "the
      dataset has changed since this fit" when the hash differs. `DatasetSource` reads through
      `sc-dataset`. Tests: the existing `sc-model` and `sc-stan` tests pass with their
      datasets stored as named datasets; editing a dataset flags its existing fits.
- [x] A1.9 Row keys: a dataset that keeps its base table's grain has the table's primary key as
      its row key, so `predict("…")` in a calculated field works as before. Saving a
      `predict("…")` over a model whose dataset changes the grain is refused with a sentence.
      The Stan binder accepts a dataset that keeps the grain as before, and refuses one that
      does not with a sentence (A8 lifts this where it can). Tests.
- [x] A1.10 `TABLES_RENAME.sql`: an idempotent section, for Postgres and SQLite, creating a
      named dataset from each `_fd_models.dataset` and `related` entry and setting the model's
      references. Test: running it twice over a database with old-style models gives the
      same result as running it once, and those models then fit.
- [x] A1.11 The admin's model form: `DatasetBuilder.tsx` gives way to a picker of named
      datasets with "New dataset" and "Edit in Analytics" links. Tests (vitest).

## Phase 3 — Workspaces (`sc-analytics`)

- [x] A1.12 The `sc-analytics` crate and `_fd_workspaces { id, name, kind, state, created_by,
      updated_at }`. `kind` lists all eight workspace types of the goals document; creating
      one that is not implemented yet is refused with a sentence naming the milestone that
      brings it. `state` is JSON owned by the workspace type. Tests.
- [x] A1.13 `sc-api` endpoints: datasets (create, read, update, delete, clone, list),
      validating one operation, reading a stage, stage shapes; workspaces (create, read,
      update, delete, list, save state). Admin only in this milestone (A9 opens them up).
      Regenerate the clients. Tests in `sc-server`.

## Phase 4 — The Analytics UI

- [x] A1.14 `ui/analytics`: the bundle, served under `/analytics/` with its CSP, built into
      the binary, sharing the admin's session. A hash router, the `analytics` i18n domain, a
      light and dark theme following the admin's. The admin sidebar gains **Analytics**. Tests:
      an `analytics_spa_typecheck` test in `sc-server` like `admin_spa_typecheck`; a
      non-admin is refused.
- [x] A1.15 The workspace list: create by name and type (types not yet implemented shown
      disabled with their milestone), rename, delete with confirmation, open. The workspace
      frame saves state as it changes (debounced) and restores it on open. Tests (vitest).
- [x] A1.16 The Dataset editor workspace, list mode: the global list of datasets with edit,
      clone and delete (delete warns and lists the models that use the dataset), and new with
      a base picker (a table, or another dataset). Tests.
- [x] A1.17 The Dataset editor workspace, edit mode: the operations side panel (add from a
      menu, edit in a form for each kind, reorder by dragging, disable, delete, errors shown on
      the operation); the read-only spreadsheet of the selected stage, virtualised, reusing
      the admin's grid code where it fits; **+** in the last column header adds a Calculated
      column; each column header's menu offers Filter, Sort, Group by (Aggregate) and Stack
      with the other selected columns. The formula input offers columns and join paths as
      completions. Tests.

## Phase 5 — Demo data, documentation, definition of done

- [x] A1.18 `feldspar demo analytics [--replace]`: creates `neighbourhoods`, `houses` and
      `viewings` (compatible with the models tutorial) with deterministic synthetic rows, and
      refuses to touch existing tables without `--replace`. Tests.
- [x] A1.19 Documentation: `TECHNICAL_DESIGN.md` (§14.2's dataset rewritten for named
      datasets and operations; new sections for `sc-dataset`, `sc-analytics` and the
      Analytics UI bundle); `docs/tutorial-analytics.md` part 1 (the Try it above);
      `tutorial-models.md` updated for named datasets; `OPERATIONS.md` for the demo command.
- [x] A1.20 Definition of done: an `sc-server` test that creates the Try it's dataset through
      the API, reads every stage and checks the rows, breaks and repairs the Aggregate, and
      fits and predicts with a model over a named dataset. Walk the Try it by hand.
- [x] A1.21 The Dataset editor is not a workspace: the Analytics UI's front page lists the
      datasets and the workspaces, and a dataset opens in the Dataset editor at
      `#/datasets/<id>`. The `dataset_editor` workspace kind goes (no kind can be created
      until A2's Data explorer); the store keeps any kind and the API refuses the ones not
      here yet. Tests.

---

# A2 — The data explorer

The plot spec, stats on the server, ECharts rendering, the drop-zone interface and hypothesis
tests (goals document "Plots and the grammar of graphics" and "Hypothesis tests in the data
explorer"). No drag and drop yet.

**Try it.** After `feldspar demo analytics --replace`:
1. Create a *Data explorer* workspace. Pick the dataset "House prices by area" from A1, or
   create one on `houses`.
2. From the gallery, pick *Scatter plot*. Drag `area` to X, `price` to Y, `neighbourhood` to
   Color. Change the mark to *line* and back from the mark palette.
3. Drag `year_built` (binned) to Wrap: one small plot per bin.
4. Open the layers panel: add a linear smoother layer, set Y to a log scale, add a reference
   line.
5. Start again with `price` on Y and `neighbourhood` on X: a box plot appears with a one-way
   ANOVA, a Kruskal-Wallis test and pairwise comparisons, and a sentence saying what they mean.
   Filter the dataset down to two neighbourhoods: the explorer switches to a Welch t-test and
   Mann-Whitney.
6. Pick the demo `measurements` dataset, choose *paired* mode with `before` and `after` on Y:
   a paired t-test and a Wilcoxon signed-rank test.
7. Switch the same assignment to a *summary table*: rows by neighbourhood, cells with the mean
   price.
8. Pick the large demo table `events` (a million rows) and make a histogram: it draws in about
   a second. A scatter plot of it says it is showing a sample.
9. Reopen the workspace: everything is as it was left.

## Phase 1 — The plot spec

- [x] A2.1 The spec types in `sc-analytics`: data (a dataset reference), layers (mark,
      encodings, stat), scales, coordinates, facets and selections (declared now, used in
      A6), serialised as JSON. Validation against the dataset's shape: the columns exist and
      their types suit the encodings, with sentences for each refusal. Tests.
- [x] A2.2 Mark choice from column types (the "show me" rules) and the gallery presets as
      functions from a dataset shape to a spec: histogram, bar, line, scatter, box, heatmap,
      area, and the map item shown disabled until A5. Tests.

## Phase 2 — Stats on the server

- [x] A2.3 The stat compiler: a spec's stats become SQL over the dataset's compiled query, with
      facets and colour groups as extra `GROUP BY` keys. Bin (Freedman-Diaconis by default),
      count, aggregate, quantiles and the box plot's five-number summary (percentiles in SQL
      on Postgres; computed in memory on SQLite), summary with a confidence interval. Tests on
      both drivers.
- [x] A2.4 Density (kernel density estimate) and smoothers (linear from SQL regression
      aggregates; loess in memory on a sample) computed on the server and returned as shapes.
      Tests against R reference values (`crates/sc-analytics/tests/r/plot_reference.R`).
- [x] A2.5 Layers that draw rows take a random sample above a limit (10,000 by default) and
      return `sampled: true` with the total. Tests.
- [x] A2.6 The `render_plot(spec)` endpoint: each layer's data and the resolved scale domains,
      or the sentence saying why the spec cannot be drawn. Tests in `sc-server`, including a
      histogram of a million rows that returns only the bins.

## Phase 3 — Rendering

- [x] A2.7 ECharts in `ui/analytics` (imported per chart type, so unused parts are left out of
      the bundle) and the compiler from spec plus layer data to an ECharts option: layers to
      series, facets to grids, colour to series or a `visualMap`, log scales, flipped
      coordinates, themes. Tests (vitest): spec in, option out.
- [x] A2.8 The summary table renderer from the same drop zones: row and column dimensions,
      aggregate cells, totals. Tests.

## Phase 4 — The explorer

- [x] A2.9 The Data explorer workspace: dataset drop-down, gallery, the column list and the
      drop zones (X, Y, Color, Size, Shape, Label, Facet rows, Facet columns, Wrap), the mark
      palette, several columns on Y compared as one variable. State saved in the workspace.
      Tests.
- [x] A2.10 The layers panel: add and remove layers, change a layer's stat, scales, reference
      lines, coordinates. Tests.
- [x] A2.11 The presets that do their own reshaping: scatterplot matrix, parallel coordinates,
      correlation heatmap, mosaic plot. Tests.

## Phase 5 — Hypothesis tests

- [x] A2.12 The tests of the goals document's table, in `sc-analytics::stats`: one-sample t,
      normality (Shapiro-Wilk), chi-square goodness of fit, binomial, Welch t, Mann-Whitney,
      one-way ANOVA, Kruskal-Wallis, pairwise comparisons (Tukey HSD), chi-square test of
      independence, Fisher's exact, Pearson and Spearman correlation, simple linear and
      logistic regression, paired t and Wilcoxon signed-rank. Sufficient statistics are
      computed in SQL where the test allows it; rank tests read the column (sampling above a
      limit, and saying so). Each returns the statistic, degrees of freedom, p-value, effect
      size and confidence interval. Tests against R reference values.
- [x] A2.13 Choosing the tests from the Y, X and Wrap roles and the column types; assumption
      checks (group sizes, normality, equal variances) with the non-parametric alternative
      shown alongside; the plain-language sentence in the `analytics` domain. Tests.
- [x] A2.14 The results beside the plot as one panel; paired mode; Wrap repeating the analysis
      per group. Tests.

## Phase 6 — Demo data, documentation, definition of done

- [x] A2.15 Demo data: `patients` and `measurements` (before and after), and `events` with a
      million rows (generated in SQL so it is quick). Documentation: `TECHNICAL_DESIGN.md`
      (the plot spec, the stat compiler, the tests); `tutorial-analytics.md` part 2.
- [x] A2.16 Definition of done: an `sc-server` test that renders the Try it's specs and checks
      the returned bins, box statistics and test results. Walk the Try it by hand.

---

# A3 — Models in the Analytics UI

Models are created, fitted and inspected in the Analytics UI, with their outputs as panels,
and the admin's *Predictive models* screens are retired. No drag and drop yet.

A model is not a workspace, for the reason a dataset is not: it is a global, named entity that
other things refer to (`predict("…")`, the Model predictions operation, simulation, map layers),
and a workspace that only pointed at one would have no state of its own. The front page lists
the models between the datasets and the workspaces, and a model opens in the **model editor**
at `#/models/<id>`. What a workspace would have given — reopening as it was left — is the
model's **view state**: a dictionary on the model that the editor reads and writes freely and
that nothing about fitting or prediction reads.

**Try it.**
1. The admin sidebar's *Predictive models* is gone; **Analytics** is the way in. An old
   bookmark to a model opens it in the model editor.
2. The Analytics front page lists the same global models the admin saw. Create a linear
   regression on "House prices by area" (or a dataset on `houses`), predicting `price` from
   `area` and `neighbourhood`.
3. Fit it. Progress shows while it runs; the outputs appear below: a coefficient table, a
   plot of residuals against fitted values, and actual against predicted. A normal Q-Q plot
   of the residuals is in the "More plots" drop-down.
4. Open the Q-Q plot from "More plots" and collapse the coefficient table. Go back to the
   front page and open the model again: the Q-Q plot is still open and the table still
   collapsed. None of this marks the fit as out of date.
5. Clone the model, add `year_built`, fit it, then select both in the model list and press
   **Compare**: the two coefficient tables side by side.
6. Edit the model's dataset in the Dataset editor, then return: the fit says the
   dataset has changed since it was fitted.
7. With CmdStan installed, open the Radon model from the Stan tutorial: edit the program,
   check the bindings, fit, and see the posterior summary with trace and rank plots.
8. In the Data explorer, from a box plot with an ANOVA, press **Open as model**: a linear
   regression opens in the model editor with the same dataset, response and factor.

## Phase 0 — Models are not a workspace

- [x] A3.0 The `model_fit` workspace kind goes: the goals document lists the model editor
      beside the Dataset editor, and split view (A4.1) holds either editor as well as a
      workspace. `createWorkspace` refuses `model_fit` as not a kind. Tests.

## Phase 1 — Outputs as panels

- [x] A3.1 Model providers declare their outputs: tables, and plots as plot specs over **fit
      output data** (a new kind of data reference, `FitOutput { instance, name }`, read from
      the instance rather than through SQL, so stats on it are computed in memory). Plots
      can be marked optional. Tests.
- [x] A3.2 Outputs of the built-in providers: linear and logistic regression (coefficients,
      residuals against fitted values, actual against predicted, Q-Q), k-means (cluster
      sizes, centroids, a scatter plot coloured by cluster), Stan (the posterior summary, and
      trace, rank and density plots per parameter over the draws). Python module providers
      can declare outputs the same way. Tests.

## Phase 2 — The API

- [x] A3.3 Endpoints for the model editor: a model's outputs with each plot rendered by
      `render_plot`, fit progress (the existing `Progress`, pushed to the browser), cancelling
      a fit, and the list of a model's fits with the "dataset changed" flag. Tests in
      `sc-server`.
- [x] A3.4 **Model view state.** A `view_state` JSON object column on `_fd_models` (`{}` for
      a new model), outside the model's definition: `validate_model` does not look at it, a
      fit does not record it, the "dataset changed" and "settings changed since fit" checks
      ignore it, and `updateModel` neither reads nor writes it. It is written by its own
      endpoint, `patchModelViewState(id, { key: value | null })`, which sets or (with `null`)
      removes top-level keys and leaves the others, so two screens that keep different keys
      (the editor's open plots, a comparison's choices, a split view's other side) do not
      overwrite each other without a read first; `getModel` returns it. No schema: the keys
      are the screens' business, as a workspace's `state` is. Shared by everyone who opens
      the model, last write wins per key. Cloning copies it; deleting the model removes it.
      The column is added on bootstrap, and `TABLE_RENAME.sql` gets the idempotent
      `ALTER TABLE … ADD COLUMN IF NOT EXISTS` for running systems. Tests: a patch leaves the
      other keys, `null` removes one, `updateModel` leaves the view state alone, a clone
      carries it, and a patch does not mark the model's fits as out of date.

## Phase 3 — The model editor

- [x] A3.5 The model list on the front page (edit, clone, delete with a warning naming what
      uses the model, new — also from a dataset's row, which picks the dataset — and a
      multi-select **Compare**). The model editor at `#/models/<id>` and `#/models/new`: the
      dataset picker (with a link to the dataset in the Dataset editor), provider picker, the
      provider's configuration form from its `config_spec`, hyperparameters, split; fit with
      progress; the outputs below, with the optional plots in a drop-down; earlier fits. The
      editor keeps which outputs are open or collapsed, the optional plots chosen and the
      selected fit in the view state, and restores them on open. Compare shows the selected
      models' key outputs side by side, without persistence. Tests.
- [x] A3.6 Stan models in the model editor: the program in an embedded editor (opening the IDE
      for the file store as now), the bindings, the posterior plots. Move `ModelForm.tsx`,
      `ModelBindings.tsx`, `ModelInstance.tsx`, `PosteriorInstance.tsx` and
      `PosteriorPlots.tsx` from `ui/admin` into `ui/analytics`, replacing their plots with
      plot specs. Tests moved with them.
- [x] A3.7 **Open as model** in the Data explorer: a linear or logistic regression, by the
      response's type, with the explorer's dataset, Y and X, opened in the model editor.
      Tests.

## Phase 4 — Retiring *Predictive models*

- [x] A3.8 The admin sidebar entry and its routes go; `#/models/…` and `#/model-instances/…`
      redirect to the model editor. `repo_hygiene.rs` fragments and the admin's `models.ts`
      follow. Tests.

## Phase 5 — Documentation, definition of done

- [x] A3.9 `tutorial-models.md` and `tutorial-stan.md` rewritten around the model
      editor; `TECHNICAL_DESIGN.md` §14.2 (outputs, fit output data); `tutorial-analytics.md`
      part 3.
- [x] A3.10 Definition of done: an `sc-server` test that fits a linear regression and the stub
      posterior provider through the API and renders every declared output; the Radon half
      behind `#[ignore]` in `stan_models.rs`. Walk the Try it by hand.

---

# A4 — Reports, and drag and drop

Split view, the panel model, drag and drop from the data explorer and the model editor, and the
Report workspace with PDF output.

**Try it.**
1. Open the Data explorer workspace from A2, then press **Split** and open a new *Report*
   workspace beside it.
2. Drag the current plot from the explorer into the report. Change the plot in the explorer:
   the report's copy does not change.
3. Add a heading and a text block (Markdown) above the plot, and a page break. Drag a
   coefficient table and a residual plot from the model editor into the report.
   Reorder the blocks.
4. Add a row to `houses` in the admin, then reopen the report: its plots include the new row
   (panels are live views of their datasets).
5. Set the page to A4 landscape and press **Export PDF**: the browser's print dialog shows the
   report paginated, with sharp vector plots.
6. Drag a panel from this report into a second report.
7. Try to delete the dataset the report uses: the warning lists the report.

## Phase 1 — Panels and split view

- [x] A4.1 Split view: two things side by side with a movable divider, each a workspace, the
      Dataset editor or the model editor, each with its own state; the URL records both. A
      dataset or model edited on one side refreshes what the other side shows of it (a
      model beside its dataset, an explorer beside the model being built from it). Tests.
- [x] A4.2 The panel model in `sc-analytics`: `Panel { id, kind, content }` with kinds plot,
      summary table, test result, text and custom; panels reference datasets by id and render
      live. A usage index answers "what uses this dataset" for the delete warning, and a
      panel whose dataset is gone shows a sentence instead of failing. Tests.
- [x] A4.3 Drag and drop: a panel's JSON as the drag payload; sources are the explorer's
      current output and the model editor's output panels; the report is a sink; always a copy.
      Tests.

## Phase 2 — The Report workspace

- [x] A4.4 The Report workspace: a document of blocks (panel, heading, Markdown text, page
      break), added by dropping or from a menu, reordered by dragging, removed; page size and
      orientation. Panels render without interaction (no tooltips or brushing). Report blocks
      are themselves drag sources. Tests.
- [x] A4.5 PDF output: a print stylesheet with `@page` sizes and page breaks, ECharts' SVG
      renderer for printing, **Export PDF** opening the print dialog. Tests of the pagination
      logic (vitest).

## Phase 3 — Documentation, definition of done

- [x] A4.6 `TECHNICAL_DESIGN.md` (panels, drag and drop, reports); `tutorial-analytics.md`
      part 4.
- [x] A4.7 Definition of done: an `sc-server` test that builds a report through the API with a
      copied explorer panel and a model output panel, and checks that the usage index finds
      it. Walk the Try it by hand, including the PDF.

---

# A5 — Maps

The geometry field type and import, geometry functions and the Spatial join operation, map
panels in the explorer, and the Map workspace with layers, symbology, the attribute table,
selection, reference layers and the tools that dataset operations can express (goals document
"Map workspace"; generated grids, neighbourhoods, spatial models and time are A8).

**Try it.** On Postgres with PostGIS, after `feldspar demo analytics --replace`, which now adds
`districts` (polygons) and `incidents` (points with a category and a date):
1. In the admin, import a GeoJSON file as a new table: it has a geometry column and its rows
   show on a map in the Analytics UI.
2. In the Data explorer, pick `incidents` and the *Map* gallery item: points over a base map,
   coloured by category.
3. Press **Open in map**: a Map workspace opens with the incidents as its first layer.
4. From the toolbox, *Aggregate → Count per region* with `districts`: a new dataset (a Spatial
   join and an Aggregate, visible in the Dataset editor) is added as a layer. Style it with
   graduated colours in five natural-breaks classes.
5. Open the attribute table of the districts layer, sort by count and select the top three
   rows: they highlight on the map.
6. Select the incidents within 1 km of a clicked point, and **Save selection as dataset**.
7. Add a reference layer from a tile service URL and change the opacity of the layers.
8. Drag the whole map into the report from A4, and export the PDF.

## Phase 1 — Geometry in core

- [x] A5.1 A geometry field type in `sc-types` (point, line, polygon and the multi variants, in
      WGS84), stored as PostGIS `geometry(…, 4326)`. Bootstrap enables the `postgis`
      extension where the role may; otherwise, and on SQLite, a geometry field is refused
      with a sentence. REST and GraphQL represent geometry as GeoJSON. Tests.
- [x] A5.2 Importing GeoJSON, zipped Shapefiles and GeoPackage files into a new table: the
      geometry is loaded with its source coordinate system and transformed to WGS84 by
      PostGIS, so no projection library is needed. The admin's table import offers it.
      Tests with small fixture files.
- [x] A5.3 Geometry formula functions in `sc-expr`, translated to PostGIS: a point from
      longitude and latitude, buffer, centroid, area, length, distance, intersects, contains,
      within, and the square and hexagonal cell of a point. Distances and areas are in
      metres (geography casts). Tests.

## Phase 2 — Spatial operations and delivery

- [x] A5.4 The Spatial join operation (intersects, contains, within, within a distance,
      nearest by a lateral join), and geometry union as an Aggregate summary. Tests.
- [x] A5.5 Layer data for the browser: GeoJSON for small layers, and Mapbox vector tiles
      (`ST_AsMVT`) for large ones, with simplification by zoom level. Tests.

## Phase 3 — Map rendering and the map panel

- [x] A5.6 MapLibre GL JS and deck.gl in `ui/analytics`; the base map style URL as a setting
      (with a default), and the CSP entries its hosts need. The compiler from a map layer
      spec to MapLibre layers. Tests.
- [x] A5.7 The map panel in the Data explorer: the geometry source chosen automatically
      (geometry column, latitude and longitude columns, or a foreign key to a table with
      geometry), encodings colour, size, shape and label. Tests.

## Phase 4 — The Map workspace

- [x] A5.8 The Map workspace's state and layer list: dataset, geometry source, filter, popup
      fields, labels, visibility, opacity, order, legend. Tests.
- [x] A5.9 Symbology: single symbol, categories, graduated colours (quantile, equal interval,
      natural breaks computed on the server), proportional symbols, heatmap style. Tests.
- [x] A5.10 The attribute table below the map with selection linked both ways; selection by
      click, lasso, attribute condition and location; **Save selection as dataset**. Tests.
- [x] A5.11 Reference layers from tile or map service URLs. Tests.
- [x] A5.12 The toolbox, for what dataset operations can do: Proximity (buffer, distance to
      nearest, within a distance), Overlay (spatial join, intersection), Aggregate (count and
      sum per region, dissolve). Each creates a global dataset and adds it as a layer, and
      the dataset opens in the Dataset editor. Plugins can register tools. Tests.
- [x] A5.13 **Open in map** from the explorer's map panel; a whole map as a draggable panel,
      rendered as an image for reports. Tests.

## Phase 5 — Demo data, documentation, definition of done

- [x] A5.14 Demo data: synthetic `districts` (polygons generated from seeded points) and
      `incidents`. `OPERATIONS.md`: installing PostGIS. `TECHNICAL_DESIGN.md` (geometry type,
      spatial functions and operations, layer delivery, the Map workspace);
      `tutorial-analytics.md` part 5.
- [x] A5.15 Definition of done: an `sc-server` test (skipped with a message without PostGIS)
      that imports a GeoJSON fixture, runs the count-per-region tool through the API and
      checks the counts, and fetches a vector tile. Walk the Try it by hand.

---

# A6 — Dashboards

A tiled workspace combining panels from any source, with stat cards, cross-filtering and
drill-down.

**Try it.**
1. Create a *Dashboard* workspace. Split the view and drag in a bar chart of incidents by
   category from the explorer, the districts map from A5, and a plot from the report.
2. Add a stat card: the number of incidents, compared with the previous month, with a
   sparkline.
3. Arrange and resize the tiles.
4. Click a category's bar: the map and the stat card filter to that category. Brush a date
   range on a line chart: everything filters to it. Clear the filters from the filter bar.
5. Select a district on the map: panels on other datasets that have a foreign key to
   `districts` filter too.
6. Give the bar chart a drill path *district → category*: clicking a district shows its
   categories, with a breadcrumb back.

## Phase 1 — Layout and cards

- [ ] A6.1 The Dashboard workspace: a grid of tiles, dragged and resized, responsive to width;
      panels dropped in from any source. Tests.
- [ ] A6.2 The stat card panel kind: an aggregate of a column, number formatting, a comparison
      (with the previous period, or unfiltered) and an optional sparkline. Tests.

## Phase 2 — Cross-filtering and drill-down

- [ ] A6.3 Selections: clicks and brushes on ECharts and MapLibre panels become the spec's
      declared selections, and those become filter conditions on the encoded columns. Tests.
- [ ] A6.4 Propagation: a condition applies to panels on the same dataset through the column,
      and to panels on other datasets through a column with a foreign key to the same table
      (from the stage shapes). On the server, it is an extra Filter at the end of the
      dataset's operations. Tests: selecting a district filters a panel on a dataset that
      only has a `district` foreign key.
- [ ] A6.5 Drill paths on a panel, with a breadcrumb. Tests.
- [ ] A6.6 The filter bar: the active filters, removing one or all, and dashboard-wide filters
      on a column; an optional refresh interval. Tests.

## Phase 3 — Documentation, definition of done

- [ ] A6.7 `TECHNICAL_DESIGN.md` (selections and propagation); `tutorial-analytics.md` part 6.
- [ ] A6.8 Definition of done: an `sc-server` test rendering a dashboard's panels with a
      selection applied and checking the filtered results across two datasets. Walk the
      Try it by hand.

---

# A7 — Simulation

Prediction with uncertainty, the Model predictions operation, and the Simulation workspace
with the profiler, scenarios and scoring (goals document "Simulation workspace").

**Try it.**
1. Open the linear regression from A3 in a *Simulation* workspace. One input control per
   predictor, starting at typical values; the predicted price with its prediction interval;
   a profile curve for each predictor.
2. Move the `area` slider: the prediction and every curve update at once.
3. Save the scenario as "baseline", change the neighbourhood, save it as "north side", and
   compare the two predicted distributions side by side.
4. **Score** the dataset of unsold houses: a new dataset appears with prediction and interval
   columns, based on the source dataset. Open it in the Data explorer and plot the
   predictions.
5. Drag the profiler into the dashboard from A6: it stays interactive there. Drag it into a
   report: it shows the saved scenarios.
6. With CmdStan installed, do the same for the Radon model: predictions for a new home are
   posterior predictive distributions.

## Phase 1 — Prediction with uncertainty

- [ ] A7.1 Prediction for new rows from a posterior, carried from TODO-post-mvp-30 Phase 9
      (its §19 design): the `new` pseudo-dataset, standalone generated quantities,
      `Prediction::Distribution`, and `predict()`, `m.predict` and `predictRows` accepting a
      posterior, with an unknown group refused by name. Tests with the stub posterior
      provider; real CmdStan behind `#[ignore]`.
- [ ] A7.2 Providers declare and return uncertainty: prediction intervals for linear
      regression, class probabilities for classifiers, distributions for posteriors. Tests.
- [ ] A7.3 The Model predictions operation: inputs matched to columns by name, prediction and
      interval columns added. It runs after the SQL, so the operations after it are evaluated
      in memory over the frame (Calculated column through the reified evaluator, Filter,
      Select columns, Sort, Aggregate, Limit). Tests.

## Phase 2 — Profiler and scenarios

- [ ] A7.4 The profiler endpoint: typical values from the training data; the prediction and
      interval for given inputs; each predictor's profile curve over its range with the
      others fixed, in one batched prediction call. Tests.
- [ ] A7.5 Scenarios in the workspace's state, and a comparison endpoint returning each
      scenario's predicted distribution (draws where the model has them). Tests.

## Phase 3 — The workspace

- [ ] A7.6 The Simulation workspace: model picker, input controls by column type, the
      prediction, the profile curves, saving and comparing scenarios. Tests.
- [ ] A7.7 Scoring: choose a dataset; a new dataset based on it with a Model predictions
      operation is created and can be opened in the Dataset editor or the explorer. Tests.
- [ ] A7.8 The profiler and scenario comparison as panel kinds: interactive in dashboards,
      static at the saved scenarios in reports. Tests.

## Phase 4 — Documentation, definition of done

- [ ] A7.9 `TECHNICAL_DESIGN.md` §14.2 (uncertainty, the operation, the workspace);
      `tutorial-analytics.md` part 7; `tutorial-stan.md` (prediction from a posterior).
- [ ] A7.10 Definition of done: an `sc-server` test that profiles, compares two scenarios and
      scores a dataset for the linear regression and the stub posterior provider. Walk the
      Try it by hand.

---

# A8 — Spatial analysis

Generated grids, the Neighbourhood column operation, adjacency from geometry, fit outputs as
datasets, the spatial model providers and their tools, and time on maps (goals document "Map
workspace").

**Try it.** After `feldspar demo analytics --replace`, which now spreads the incidents over two
years:
1. In a Map workspace with the incidents layer, *Aggregate → Count per hexagon* at 500 m: a
   generated grid, joined and aggregated, styled by count.
2. *Neighbourhood → Count within distance* of 250 m: each incident gets the number of other
   incidents nearby.
3. *Surfaces → Kernel density*: a smooth density surface over a grid.
4. *Statistics → Hot spots* on the districts' incident counts: districts classified as hot,
   cold or not significant, and a Moran's I panel that can be dragged into a report.
5. *Statistics → Clusters (DBSCAN)*: the incidents coloured by cluster.
6. The worked example of the goals document: a dataset of burglaries by district and month,
   a Stan spatiotemporal model with district adjacency (from a bundled template), and its
   six-month forecast as a layer. The time slider steps through the months and **Play**
   animates them. A second layer shows the district effects.
7. Toggle the forecast layer to show the width of its intervals.

## Phase 1 — Dataset additions

- [ ] A8.1 The generated grid base: square or hexagonal cells of a size in metres, covering the
      extent of a dataset or a region, optionally crossed with a time range and step
      (PostGIS grid functions in a metric projection, returned in WGS84). Tests.
- [ ] A8.2 The Neighbourhood column operation: within a distance, the k nearest, or touching
      polygons, with the Aggregate summaries. Tests.
- [ ] A8.3 Adjacency from geometry: the pairs of touching polygons of a dataset, and a Stan
      binding kind for adjacency (the node arrays CAR and BYM2 programs take), lifting A1.9's
      refusal for datasets whose grain is a table with geometry. Tests.

## Phase 2 — Fit outputs as datasets, and time

- [ ] A8.4 Fit outputs become datasets: per-row outputs keyed by the training dataset's row
      key, per-group outputs keyed by the grouping columns (a Stan provider maps its array
      indexes back to keys through the binder's dimensions), forecasts keyed by group and
      time. A new base kind `FitOutput { model, output, fit }` (`fit` is the active fit or a
      fixed one), so they can be explored, plotted, mapped and used as the base of other
      datasets. Tests.
- [ ] A8.5 Time on maps: a layer's time encoding, the time slider with play filtering every
      layer with a time encoding, and small multiples by time for reports. Tests.

## Phase 3 — Spatial model providers

- [ ] A8.6 Kernel density: fitted on points (bandwidth by a rule, or set), scored over a grid.
      Tests against reference values.
- [ ] A8.7 Interpolation: inverse distance weighting and ordinary kriging with a fitted
      variogram. Tests against reference values.
- [ ] A8.8 Spatial statistics: Getis-Ord Gi* hot spots (with false discovery rate
      correction), global Moran's I as a result panel, local Moran's I per feature. Tests
      against PySAL reference values.
- [ ] A8.9 DBSCAN clustering. Tests.
- [ ] A8.10 A bundled Stan template for a spatiotemporal count model (BYM2 district effects and
      a time trend) with its bindings, and its forecast as a fit output. Tests with the stub
      posterior provider; real CmdStan behind `#[ignore]`.

## Phase 4 — Tools, documentation, definition of done

- [ ] A8.11 The toolbox groups Neighbourhood, Surfaces, Statistics and Models, each creating a
      dataset, or a model and a scored dataset or fit output, and adding it as a layer.
      Tests.
- [ ] A8.12 The uncertainty toggle for layers with interval columns. Tests.
- [ ] A8.13 Demo data over two years; `TECHNICAL_DESIGN.md` (grids, neighbourhoods,
      adjacency, fit outputs as datasets, the providers); `tutorial-analytics.md` part 8.
- [ ] A8.14 Definition of done: an `sc-server` test (skipped without PostGIS) running the hex
      count, a neighbourhood count, hot spots and the worked example with the stub posterior
      provider, checking each layer's data; the real CmdStan half behind `#[ignore]`. Walk the
      Try it by hand.

---

# A9 — Applications with a restricted Analytics UI

The Analytics UI as an application framework: an admin publishes a restricted subset of it to
end users (goals document, the introduction's "application").

**Try it.**
1. As the admin, create an application with the framework *Analytics*, in *fixed* mode,
   showing only the dashboard from A6, for the role `staff`.
2. Log in as a `staff` user at the application's address: the dashboard is there and
   interactive, and nothing else is: no workspace list, no admin links.
3. Create a second application in *self-serve* mode with the tables `houses` and
   `neighbourhoods` allowed, the Dataset editor on and the Data explorer type enabled.
4. As a `staff` user there, create a dataset on `houses` and explore it. `incidents` is not
   offered as a base, and asking the API for it directly is refused.
5. Give `staff` read access to only some `houses` rows (an ownership formula): the user's
   plots and tests only include those rows.

## Phase 1 — Authority

- [ ] A9.1 Every analytics endpoint takes the caller's authority. Datasets, stat queries,
      tests and layer data read as the caller, through table permissions and ownership
      formulas; the admin keeps full access. Datasets and workspaces gain an owner and
      sharing with roles. Tests: a user without read access is refused; an ownership formula
      restricts a histogram's counts.

## Phase 2 — The framework

- [ ] A9.2 The *Analytics* `FrameworkFactory`: its configuration (mode fixed or self-serve,
      the workspaces shown in fixed mode, the tables and workspace types allowed in
      self-serve mode, whether users may create workspaces) and validation. Tests.
- [ ] A9.3 Mounting: the Analytics UI bundle served in an application with the application's
      CSP and session, in a restricted shell without admin links. Workspaces belong to an
      application or to the unrestricted UI. Tests.
- [ ] A9.4 Enforcing the configuration on the server: base pickers, dataset reads and
      workspace types limited to what the application allows, whatever the client asks.
      Tests.

## Phase 3 — Documentation, definition of done

- [ ] A9.5 `TECHNICAL_DESIGN.md` (the framework, authority in analytics);
      `tutorial-analytics.md` part 9.
- [ ] A9.6 Definition of done: an `sc-server` test with both applications, checking what a
      `staff` user can see and read in each. Walk the Try it by hand.

---

# React Native: Android APK build targets (branch `react-native-plugin`)

Outside the analytics milestones. See CHANGELOG for what each item covers.

- [x] RN.1 `plugins/react-native`: an Expo project as a bundled application framework, with
      "Android SDK" / "Java" module settings and a "Server URL for the mobile app".
- [x] RN.2 Build targets: a framework declares them with their requirements; the admin sidebar
      offers "Build <target>" and says what is missing before anything runs.
- [x] RN.3 Target builds run as background jobs the admin UI polls; logs and the artifact go to
      the application's file store.
- [x] RN.4 Login from a native app: the CSRF token as a response header, a session cookie that
      outlives the app, and recovery of the token after the app is reopened.
- [x] RN.5 Extra base domains (`extra_base_domains`), so a phone or emulator reaches the server.
- [ ] RN.6 A build of the same project while an APK build runs. Only main's process-wide
      `BUILD_LOCK` exists, and an APK build holds it only for its install: a web build, a deep
      clean or a rewrite of `src/feldspar/` during an APK build can corrupt the APK. Needs a
      per-project lock that does not hold other builds for the length of a Gradle build.
- [ ] RN.7 Links to sibling applications (`RequestLinks::app_origin`) use `base_domain` even for
      a request that arrived on an extra base domain.
- [ ] RN.8 Cleartext HTTP is allowed in the APK only if the app's URL was `http://` when it was
      scaffolded; changing `mobile_url` to `http://` later breaks every API call at runtime.
- [ ] RN.9 The generated client takes "no `document`" to mean native, which is also true in a
      browser Web Worker; a login from a worker gets the lasting session cookie.
- [ ] RN.10 `listApplications` checks every target's readiness (environment, directories,
      `PATH`) on every call; compute it on demand or cache it.
- [ ] RN.11 `file_tail` reads a whole build log to keep its last 32 KB; seek to the end instead.
- [ ] RN.12 A build dropped while the server runs (a future "cancel build", or a server killed
      on its own) kills only `npm`, not the `sh` / `gradlew` / Gradle under it: run the command
      in its own process group and kill the group.
- [x] RN.13 The APK's own settings (application ID, version, icon from the app's file store,
      debug or release) as a target's `options`, shown in the application form under the target.
- [x] RN.14 Release signing with the admin's own keystore (file from the store, alias, password
      as a secret), shown only for a release build once "Sign with your own keystore" is ticked.
- [x] RN.15 "Generate a keystore": a target operation that makes one in the file store and fills
      in the signing settings; warns when the store is a git repository.
- [ ] RN.16 Keep secrets such as a generated keystore out of a git file store's commits.

# Not in a milestone yet

The goals document describes these, but its milestones do not schedule them. They are listed
here, without checkboxes, so that they are not lost and not picked up by accident. Say where
they go before starting them.

- **The Notebook workspace** (goals document, "Workspaces"): JavaScript, Python and LLM prompt
  cells whose functions create panels, fit models and create non-persisted datasets. It could
  follow A4, since its outputs are panels that need drag and drop.
- **The Bayesian workflow** (goals document, "Bayesian workflow"): the structured model
  builder generating Stan; prior-only runs and prior predictive checks; the `y_rep` and
  `log_lik` conventions; diagnostics with suggested remedies; the staged fit area; fake-data
  fits (A8.4 already makes fit outputs usable as dataset bases); PSIS-LOO and per-observation
  Pareto k; power-scaling prior sensitivity; the model comparison view; stacking as a
  provider; predictions with draws through dataset operations; background fitting from a
  queue with persisted fits and draws. Prior-only runs and diagnostics could join A3; the
  rest could be a milestone after A7.

# Explicitly out of scope

- Raster data analysis, network analysis and routing, geocoding, editing geometries on the
  map, 3D (goals document, "Map workspace").
- Server-side PDF generation, and scheduled or emailed reports.
- A coding agent for Analytics applications (goals document, "Additional changes to core").
- Spatial features on SQLite (SpatiaLite).

# Carried from milestone 31

The items carried past milestone 31 are listed in
[docs/TODO-post-mvp-31.md](./docs/TODO-post-mvp-31.md). This plan takes up three of them:
prediction for new rows from a posterior (A7.1), geometry types (A5.1) and adjacency from
geometry (A8.3). LOO and the comparison view, and the formula front end generating Stan, are
part of the unscheduled Bayesian workflow above. The rest remain carried.

# Feedback from an external MCP build ("Optino")

An external coding agent built two applications through the administration MCP server with
the grants `allow_create`, `allow_edit`, `allow_triggers` and `allow_applications`. What it
could not do, and what we agree is missing:

- [x] F.1 Response compression: gzip and brotli on every HTTP response the client accepts it
  for (tower-http `CompressionLayer`, its default predicate, which already skips SSE, images
  and tiny bodies; also skip zip archives). Test that an `/api` JSON response is compressed.
- [x] F.2 `edit_schema` can create **File** fields: `file_store` (and optional `file_folder`,
  `file_mime`) on `add_field` / `create_table` fields make a `DataFieldKind::File`. Say in
  the tool description that `file` is not a `type`. Tests.
- [x] F.3 MCP tool `update_application` (Applications area), replacing
  `set_application_tables`: one tool with `tables`, `static_dirs` and `csp` sections, so the tool
  count does not grow. `tables` and `static_dirs` need `allow_edit`; `csp` needs
  `allow_access_changes` because a CSP is a security boundary. Every section's grant is checked
  before anything changes. A store a static directory names is connected to the application.
  Saved through the admin's own `updateApplication`, so the running app serves the change at
  once. Tests.
- [x] F.4 (merged into F.3) Static directories and the CSP were first two tools of their own.
- [x] F.5 Update the module docs, TECHNICAL_DESIGN §13.6, OPERATIONS.md and the admin
  copilot's name list.

Not changed, by design: `save_api_query`'s `min_role` and `edit_schema`'s `min_role_*` stay
behind `allow_access_changes`, and deleting tables and triggers stays behind `allow_drop`.
