# Analytics UI goals

This document contains the goals for a new analytics interface and application framework for feldspar, the Analytics UI, along with some changes to the core framework.

The scope for the framework is predictive analytics, including:

* the creating and editing of datasets, which are derived from database tables
* exploratory data analysis
* dashboards for non-technical users 
* fitting statistical models
* hypothesis testing
* notebook interfaces
* GIS work: maps with layers, spatiotemporal data analysis
* reports
* simulation: using fitted models to predict outcomes and compare what-if scenarios

The goal here is not to be the most powerful data analytics package but to provide the 50% of features that are sufficient for 95% of users, ideally with a plug-in architecture so that entity types can provide expert functionality. Prioritise ease of use over feature completeness.

The unrestricted version of the analytics UI appears as the Analytics link in the admin UI sidebar menu (replacing the "Predictive models" link). This leads to a version of the analytics UI where all tables and workspaces are accessible. The admin can also create an application using the Analytics UI framework, which is a restricted subset of the Analytics UI functionality. It might be only a particular workspace (e.g. dashboard, report or simulation), or it might give the end user more power with a self-serve analytics environment but that has access only to a restricted subset of tables.

### Workspaces

The full analytics UI consists of the dataset editor and a number of workspaces each of which takes a specific form. When entering the analytics UI, the user sees the list of datasets and the list of existing workspaces. They can open a dataset in the dataset editor, or enter a workspace, or create a new one by name and type.

Dataset editor (not a workspace): the list of datasets on the front page has, for each dataset, a link to edit, clone and delete, or create new. Each dataset is based on a base (a table, another dataset or a generated grid) which is picked when creating new but cannot be changed. A dataset is defined by its base and an ordered list of operations (see [Dataset operations](#dataset-operations)). Clicking a dataset opens the dataset editor, which shows the list of operations beside a spreadsheet like read only view of the data as it is after the selected operation. New columns can be added with a plus in the last column header, which adds a Calculated column operation at the end. The persisted list of datasets is global.

Workspace type:
* Data explorer: interactively creating different visualizations and summary tables without any persistence other than opening up in the same state where it was left off last time. A single screen where the data set is chosen in a drop-down and then the plot / summary table type from a gallery. Columns are then dragged onto drop zones (X, Y, Color, Facet, ...) and the plot is shown (see [Plots and the grammar of graphics](#plots-and-the-grammar-of-graphics)). The data explorer can also produce a map panel of a single dataset for quick spatial exploration; multi-layer GIS work is done in the Map workspace. The data explorer can also perform simple hypothesis tests, which sit alongside a plot type. Large models like general linear models are done through the model fit interface. The tests are chosen from the types of the variables, as in JMP's "Fit Y by X" (see [Hypothesis tests in the data explorer](#hypothesis-tests-in-the-data-explorer)).
* Dashboard: a tiled view including multiple plots and summary table and summary statistics cards, it may be interactive to enable drill down / cross filtering. There is no base data set for a dashboard; it can freely combine plots and summary tables across multiple datasets.
* Model fit: a workspace for creating and editing model fits to datasets. Starts with a list of existing models each of which can be edited, cloned or deleted or the user can create a new model. Each model has a dataset and the model provider. There is an interface for editing the model parameters, fitting to a model, seeing fit progress, and then the fit output for that model below when done. Like the dataset editor, the list of models is global and the model fit it tied to one of them, unless it is in the initial state of picking a model to edit. For Bayesian models, the workspace is organised around the stages of the Bayesian workflow (see [Bayesian workflow](#bayesian-workflow)).
* Notebook: the notebook is a jupyter- style notebook that contains code blocks, text blocks and output blocks. The code blocks are in a language that is set at creation time; it can be either JavaScript, Python or it can be natural language prompts to an LLM. The functions available allow it to generate panels, fit models or create non-persisted datasets.
* Report: similar to a dashboard but intended to generate printable PDFs. Not interactive for drill down statistics.
* Map: a map for GIS work. Has a base map and layers of data that can be added. The data for these layers comes from datasets. Layers are styled with the same encodings as plot layers (color, size, shape and label by column). A toolbox of spatial analysis tools creates new datasets, or models and scored datasets, which are added as layers (see [Map workspace](#map-workspace)).
* Simulation: a workspace for using a fitted model rather than building it: what-if exploration of inputs, named scenarios compared side by side, and scoring a dataset with the model's predictions. Like model fit, it is tied to one model from the global list of models, unless it is in the initial state of picking one (see [Simulation workspace](#simulation-workspace)).

Initially only one workspace is open at a time, however the display can be split side by side to have two open workspaces. 

#### Dataset operations

A dataset is a base followed by an ordered list of operations. The base is a table, another dataset whose operations then come first, or a generated grid (see below). Each operation takes the rows produced by the operation before it and produces new rows, like a pipeline of tidyverse verbs (`table |> filter(…) |> mutate(…) |> summarise(…)`). The operations are modelled on those of dplyr, tidyr and sf, adapted to Feldspar's relational model.

**Grain.** The grain of a dataset is what one of its rows represents: a row of the base table (an order), or something else (a customer-month, a region). Some operations keep the grain, and others change it. This distinction matters for which relations formulas can follow, and for how models fitted on the dataset can be used.

**How this fits Feldspar's relational model.** Much of what needs an explicit join in the tidyverse is a formula in Feldspar, because formulas can follow foreign keys (Ⱶ join fields) and aggregate over child tables (Ↄ aggregations):

* a lookup in a parent table (`left_join` along a foreign key) is a calculated column, e.g. `customerⱵregion`;
* a per-row summary of a child table is a calculated column, e.g. `ordersↃcustomer.sum(o => o.total)`;
* `semi_join` and `anti_join` along a foreign key are filters, e.g. `ordersↃcustomer.length > 0`.

The Join operation is therefore only needed for keys that are not foreign keys, and for joining datasets. Relations remain available after grain-changing operations wherever the rows still correspond to database rows:

* Every column keeps its type through the operations, and a column that is a foreign key stays one, so join fields can be followed from it in any later operation.
* Aggregations over child tables need the row to correspond to a row of a table. That holds for the base table's rows until an operation changes the grain. It also holds after an Aggregate grouped by a single foreign key column, because each group then corresponds to the referenced row. For example, rows of incidents aggregated by district can still aggregate over the district's other child tables.
* A model whose dataset keeps its base table's grain can, as today, be used in `predict("…")` in a calculated field on that table. A model whose dataset changes the grain cannot, as a row of that table is not an input it understands.

Operations that keep the grain (one output row per input row, except Filter, which removes rows):

| Operation | tidyverse | What it does |
|---|---|---|
| Calculated column | `mutate` | Add or replace a column computed by a formula over the current row. The formula can follow foreign keys and aggregate over child tables. |
| Filter | `filter`, `drop_na`, `semi_join`, `anti_join` | Keep the rows that satisfy a condition. |
| Select columns | `select`, `rename`, `relocate` | Keep, drop, rename and reorder columns. |
| Sort | `arrange` | Order the rows. |
| Window column | `group_by` + `mutate`, `fill` | A column computed over the ordered rows of a group: lag, lead, difference from the previous row, cumulative sum or mean, rank, row number, a group summary (e.g. each row's share of its group's total) or the last non-missing value. |
| Neighbourhood column | `st_join` + `summarise` over neighbours | A column computed over each row's spatial neighbours: the rows within a given distance, the k nearest rows, or the polygons that touch it. For example, the mean income within 1 km, the number of neighbours, or a spatial lag of a value. The spatial counterpart of Window column. |
| Model predictions | `predict`, `augment` | Add columns with a fitted model's prediction and, where the model provides it, the prediction's uncertainty interval. The model's inputs are matched to columns by name. Optionally outputs draws instead, one row per input row and draw, so that later operations propagate the uncertainty (see [Bayesian workflow](#bayesian-workflow)). |

Operations that change the grain:

| Operation | tidyverse | What it does |
|---|---|---|
| Aggregate | `group_by` + `summarise`, `count`, `distinct` | One row per combination of the group-by expressions, with summary columns: count, sum, mean, median, min, max, standard deviation, first or last by an order, and geometry union (to dissolve regions). Group-by expressions can bin values, e.g. a date truncated to the month or a point assigned to a grid cell. Without summary columns it is `distinct`. |
| Limit | `slice_head`, `slice_sample`, `slice_max` | Keep the first N rows, a random sample, or the top N rows of each group by some column. |
| Stack | `pivot_longer` | Turn a chosen set of columns into rows of name/value pairs, repeating the other columns. |
| Split | `pivot_wider` | Turn the values of a name column into separate columns, giving one row per combination of the id columns. |
| Complete | `complete` | Add rows for missing combinations of values (e.g. every region × every month), with a fill value for the other columns. The values come from the data, from a range (e.g. the months between two dates), or, for a foreign key column, from all rows of the referenced table, so that regions with no data still appear. Needed for time series and spatiotemporal models. |

Operations that combine with another table or dataset:

| Operation | tidyverse | What it does |
|---|---|---|
| Join | `inner_join`, `left_join`, `full_join`, `join_by(closest())` | Join on key columns: inner, left or full. The key on a date column can also be "nearest earlier" (an as-of join), e.g. the price in force when an order was placed. |
| Union | `bind_rows` | Append the rows of another table or dataset, matching columns by name, optionally with a column recording which source a row came from. |
| Spatial join | `st_join` | Join where the geometries intersect, where one contains the other, where they are within a given distance, or to the nearest. |

Geometry is a column type, and most of what sf does is formula functions rather than operations: building a point from coordinates, buffer, centroid, area, length, distance between two geometries, and assigning a point to a square or hexagonal grid cell. Operations are only needed where rows of another dataset are involved. Aggregating points to regions is a Spatial join followed by an Aggregate by region, and dissolving regions is an Aggregate with a geometry union. The dataset editor can offer such combinations as one action (e.g. "count per region") that creates the operations.

**Generated grid.** Besides a table or another dataset, the base of a dataset can be a generated grid: square or hexagonal cells of a given size covering the extent of a dataset or a region, optionally crossed with a range of time steps. Each row is a cell, with its geometry (and time step). Grids are generated in SQL (PostGIS). A grid is Feldspar's form of a raster: an analytical surface such as a density or an interpolation is a model scored over a grid (see [Map workspace](#map-workspace)).

Also formula functions rather than operations: splitting and joining text (`separate`, `unite`) and replacing missing values (`replace_na`). Applying the same formula to several columns (`across`) is a convenience in the editor that creates one Calculated column per selected column. List columns (`nest`, `unnest`) are left out.

The columns an operation produces are fixed when it is defined. For Split, the new columns are pre-filled from the values currently in the data, not recomputed when the data changes, so later operations, panels and models do not break when a new value appears. Later operations refer to columns by name.

In the dataset editor, the operations are listed in a side panel. Selecting an operation shows the data as it is after it, so the user can see the effect of each operation in isolation. Operations can be edited, reordered, temporarily disabled and deleted. An operation that becomes invalid (e.g. because an earlier operation removed a column it uses) is marked with an error rather than removed. The plus in the last column header adds a Calculated column at the end, and each column header has a menu with the operations that apply to that column (filter on it, sort by it, group by it, stack it with other selected columns).

The rows of a dataset are not materialised. The operations are compiled into a single SQL query where possible, so that large tables are not loaded into memory. Spatial operations and functions need a spatial database extension (e.g. PostGIS). Model predictions cannot be expressed in SQL, so the operations after one are evaluated in memory on the rows it returns. Plugins can provide additional operations.

#### Plots and the grammar of graphics

Every plot is represented internally as a grammar of graphics specification, but users are not asked to learn the grammar. This is the approach of JMP's Graph Builder and of Tableau: users drag columns onto X, Y, Color and so on, and never meet the words "geom" or "stat".

**The plot spec.** A plot panel is stored as a declarative spec, similar to a subset of Vega-Lite:

* data: a dataset reference;
* layers: each with a mark (point, line, bar, area, box, band, text, ...), encodings (which column maps to x, y, color, size, shape, label) and a stat;
* scales: e.g. linear or log axes, colour schemes;
* coordinates: e.g. cartesian, flipped or polar;
* facets: rows, columns or wrap;
* selections: what a click or brush on the plot selects (see cross-filtering below).

The spec is Feldspar's own format, independent of the library that renders it, so that the stats can be executed on the server (see below) and the renderer can be changed.

**The user interface has three levels:**

1. A gallery of plot types (histogram, scatter plot, box plot, line chart, bar chart, heatmap, map, ...) is the entry point. Each plot type is a preset that fills in a spec.
2. Drop zones, as in JMP's Graph Builder: X, Y, Color, Size, Shape, Label, Facet rows, Facet columns and Wrap. The explorer chooses the mark from the types of the columns dropped (e.g. continuous × continuous gives points, categorical × continuous gives a box plot), and a small mark palette lets the user switch. Dropping several columns on Y compares them as one variable, for example several measurements over time as one line each.
3. An advanced layers panel for adding layers, changing a layer's stat, and setting scales, reference lines and coordinates. Most users never open it.

**Stats** are the transforms inside a plot: bin, count, aggregate (mean, sum, median, ...), density, smoother (linear or loess), summary with confidence interval, and quantiles (for box plots). Plot-level transforms are limited to these stats. Reshaping and joining data belong in dataset operations, so that there is one place where data changes shape. The only exception is dropping several columns on Y, which is an implicit Stack.

**Facets** (small multiples) split any plot into one panel per value of a column, e.g. one per region or year, with one drag.

**Layers** combine data and models. A model layer draws a fitted model's predictions and uncertainty band over the data, which connects the data explorer to the model fit and simulation workspaces. Checking whether a model fits the data is then a plot with two layers.

**Cross-filtering** in dashboards comes from selections. A click or brush on a plot turns into a filter condition on the columns it encodes, and that condition is applied to the other panels. The relational model makes this work across datasets: selecting a district in one panel filters every panel whose dataset has a column that is a foreign key to the districts table, not only panels on the same dataset.

**Summary tables** use the same drop zones: columns dropped on rows and columns give the table's dimensions, and the cells are aggregates. Switching between a plot and a summary table of the same data keeps the column assignments.

**Map panels.** The data explorer can produce a map panel: a plot with geographic coordinates over a base map, showing one dataset with a geometry column, e.g. points or a choropleth coloured by a column. It is for quick spatial exploration and is a panel like any other plot. Multi-layer GIS work is done in the Map workspace, which remains a workspace type of its own. Map panels and the Map workspace both render with MapLibre (see Rendering below). An "Open in map" button creates a Map workspace with the explorer's map as its first layer. Map layers use the same encodings (color, size, shape and label by column) as plot layers.

**Plots that fit the grammar poorly**, such as scatterplot matrices, parallel coordinate plots, correlation heatmaps and mosaic plots, are offered as gallery presets that do the necessary reshaping internally. Most are grammar plots over reshaped data; a scatterplot matrix, for example, is a faceted scatter plot over stacked columns. Where a plot cannot be expressed as a spec, a custom panel type is the escape hatch, and plugins can provide custom panel types.

**Execution.** Datasets are not materialised and tables can be large, so the browser must not fetch every row to compute a histogram. A spec's stats are compiled into SQL through the same query engine as the dataset operations, and the browser receives only binned or aggregated rows. Plots that draw individual rows (e.g. scatter plots) sample above a row limit and say so.

**Rendering.** Plots are rendered with [Apache ECharts](https://echarts.apache.org/), and maps (map panels and the Map workspace) with [MapLibre GL JS](https://maplibre.org/), using [deck.gl](https://deck.gl/) layers for layers with too many features for MapLibre alone. The reasons:

* Because stats are computed on the server, the renderer only has to draw marks from rows that are already binned or aggregated. The browser-side transforms of grammar-native libraries such as Vega-Lite would go unused, so the deciding factors are drawing performance and interaction.
* ECharts renders to canvas, with progressive rendering for large series, and is light per chart, which matters for dashboards with many panels. It can also render SVG, including to an SVG string without a browser, for printable reports.
* ECharts' `dataset` and `encode` model is close to the spec's encodings. A layer compiles to a series on shared axes, facets compile to a set of grids laid out by Feldspar, and a colour scale compiles to a `visualMap`. Plots ECharts lacks, such as violins and density curves, arrive from the server as computed shapes and are drawn as areas or custom series; box plots take the server's five-number summary directly.
* ECharts' brush component and events are enough for the selections that drive cross-filtering.
* The chart libraries have no real support for tile base maps and layered GIS work, which MapLibre provides. MapLibre is open source, unlike Mapbox GL since version 2.

The specs are compiled to ECharts options and MapLibre layers, and the renderer is not visible in the spec. A custom panel type from a plugin can use a different renderer.

**Where specs come from.** Besides the data explorer, specs are produced by model fits, notebooks (JavaScript and Python cells call functions that return specs, and LLM prompt cells generate them) and the simulation workspace. Because every plot panel is a spec, dragging a panel copies its spec, and a dashboard or report stores the specs of its panels.

#### Hypothesis tests in the data explorer

The data explorer follows the approach of JMP's "Fit Y by X". The user does not pick a test from a menu of named tests. Instead they assign columns to roles: a response Y, optionally a factor X, and optionally a "by" column to repeat the analysis for each group. These roles are the Y, X and Wrap drop zones of the plot interface, so a test is available for whatever plot the user has built. The explorer chooses the plot and the applicable tests from the types of Y and X:

| Y | X | Plot | Tests |
|---|---|---|---|
| continuous | none | histogram, box plot | one-sample t-test, normality test |
| categorical | none | bar chart | chi-square goodness of fit, binomial test (two levels) |
| continuous | categorical, 2 levels | box plot / dot plot by group | t-test (Welch), Mann-Whitney |
| continuous | categorical, >2 levels | box plot / dot plot by group | one-way ANOVA, Kruskal-Wallis, pairwise comparisons |
| categorical | categorical | mosaic plot, contingency table | chi-square test of independence, Fisher's exact test |
| continuous | continuous | scatter plot with fitted line | Pearson and Spearman correlation, simple linear regression |
| categorical | continuous | logistic curve | simple logistic regression |

Paired data (e.g. before and after measurements on the same subject) is handled by choosing two continuous Y columns in a "paired" mode, giving the paired t-test and Wilcoxon signed-rank test. Data in long form can be brought into the wide form this needs with a Split operation in the dataset.

The plot and the test results form a single panel, so they are dragged together. Results are shown as a short table (the test statistic, degrees of freedom, p-value, effect size and confidence interval) together with a plain-language sentence, for example "The mean of weight differs between groups A and B (p = 0.003)". Where a test's assumptions are doubtful (small groups, clearly non-normal data, unequal variances) the explorer says so and shows the non-parametric alternative alongside.

The boundary with the model fit workspace: the data explorer handles one response and at most one factor, and nothing is persisted beyond the explorer's own state. Anything with several predictors, covariates, interactions or random effects, or that needs to be saved and reused, is a model. An "Open as model" button creates a model in the model fit workspace with the same dataset, response and factor, so that the user can extend a simple analysis without starting over.

#### Simulation workspace

The model fit workspace is where an analyst builds a model. The simulation workspace is where the analyst, or an end user in a restricted application, uses it. It has three parts:

* Profiler: one input control per predictor (slider, drop-down or date picker), starting at typical values (the mean or the most common level). It shows the predicted outcome with its uncertainty interval and, for each predictor, a profile curve showing how the prediction changes as that predictor varies with the others held fixed. Changing an input updates the prediction and all the curves immediately.
* Scenarios: the current input values can be saved as a named scenario, e.g. "price +10%" or "baseline". Scenarios are compared side by side, showing the distribution of the predicted outcome where the model provides one, and not only point predictions. Scenarios are persisted with the workspace.
* Scoring: applying the model to a dataset creates a new dataset whose base is the source dataset, followed by a Model predictions operation. The scored dataset therefore follows changes to the source dataset. As with any dataset, the rows are not materialised.

Scoring is how model outputs reach the other workspaces. A scored dataset can be explored, summarised in a dashboard or shown as a map layer. For example, a spatiotemporal model scored over a dataset of regions or grid cells can be shown as a choropleth map of predictions, without the map needing to know anything about models.

The profiler and the scenario comparison are draggable panels. In a dashboard the profiler stays interactive, so a non-technical user can try out inputs. In a report it is rendered statically at the saved scenarios.

Later, the simulation workspace could support decisions: choose the value of a decision variable (e.g. price or stock level) that maximises an expected outcome, taking the model's uncertainty into account.

#### Map workspace

The Map workspace is where multi-layer GIS work is done. Its design follows three rules, which together make any spatial analysis displayable on a map without the map needing to know how the analysis was done:

1. **A layer is a dataset, a geometry source and a style.** The map never computes anything itself.
2. **Every spatial analysis is either a dataset operation or a model, and both produce datasets.** Whatever can be expressed in SQL (with PostGIS) is a dataset operation or formula function. Anything that needs computation beyond that is a model, whose results come back as datasets through scoring and fit outputs.
3. **Geometry travels by key.** A layer's geometry comes from a geometry column, from latitude and longitude columns, or from a column that is a foreign key to a table that has a geometry column (e.g. `districtⱵgeom`, followed automatically). A result keyed by district, such as a model's per-district effects or an Aggregate grouped by district, can therefore be mapped without carrying the polygons through the analysis.

Because datasets are not materialised, layers are live: changing an input dataset or refitting a model updates every layer derived from it.

**How each kind of spatial analysis reaches the map:**

| Kind of analysis | Examples | Mechanism | Result |
|---|---|---|---|
| Proximity, overlay and aggregation | buffer, points in polygon, nearest facility, dissolve, clip | Geometry formula functions, Spatial join, Aggregate | Dataset → layer |
| Neighbourhood features | mean income within 1 km, number of neighbours, spatial lag | Neighbourhood column operation | Column on the same rows → layer |
| Surfaces | kernel density, interpolation (inverse distance weighting, kriging) | A model fitted on the points and scored over a generated grid | Grid dataset → layer |
| Spatial statistics and clustering | hot spots (Getis-Ord Gi*), global and local Moran's I, DBSCAN | Model providers: the global statistic is a result panel, the per-feature values (z-score, class, cluster) are fit outputs | Dataset → layer, plus a panel |
| Spatial and spatiotemporal models | Stan models with spatially correlated region effects (CAR/BYM), Gaussian processes, forecasts per region | Fit outputs (fitted values and residuals per row, effects per region keyed by foreign key) and scoring | Dataset → layer, with time |

**The workspace:**

* Layer list: the order, visibility and opacity of the layers, with a legend for each. Each layer has a dataset, a geometry source (chosen automatically when there is only one), an optional filter, popup fields, labels and symbology.
* Symbology uses the plot encodings (color, size, shape and label by column) and adds map-specific classification: single symbol, categories, graduated colours (quantile, equal interval or natural breaks), proportional symbols, and a heatmap rendering style (a visual style, distinct from the analytical kernel density). For a layer with prediction intervals, a toggle shows the width of the interval, as a map of the uncertainty.
* Attribute table: the rows of the selected layer shown below the map. Selection is linked both ways: selecting features on the map selects the rows, and selecting rows highlights the features.
* Selection by click, lasso, attribute condition or location (e.g. "within 5 km of the selected hospitals"). "Save selection as dataset" creates a dataset whose base is the layer's dataset, followed by a Filter.
* Toolbox: the analysis tools, grouped as Proximity, Overlay, Aggregate, Neighbourhood, Surfaces, Statistics and Models. Each tool is a form that creates an ordinary global dataset, or a model and a scored dataset, and adds the result as a new layer. Nothing is hidden: the dataset editor shows the operations a tool created and the user can change them. A tool is a shortcut, not a black box. Plugins can provide additional tools.
* Reference layers: external tile or map services (e.g. satellite imagery or cadastral maps) shown for context. They are not datasets and cannot be analysed.
* Time: a layer can encode a time column. The map then shows a time slider, with play, that filters every layer with a time encoding. In a report, a map can instead be faceted into small multiples by time.
* No projections for users to choose. Geometry is stored in WGS84 and displayed in Web Mercator. Distances and areas are always computed in metres (using PostGIS geography types or an automatically chosen local projection), so users never meet a coordinate reference system.

The whole map is a single draggable panel, so it can be placed in a dashboard or report.

**Worked example.** "Forecast burglaries per district for the next six months, and show where they rise":

1. A dataset of burglaries by district and month: base table `incidents`, Filter to burglaries, Spatial join to `districts`, Aggregate by district and month, and Complete over all districts × months with a count of 0.
2. A model: a Stan spatiotemporal model on that dataset, with district effects from district adjacency and a time trend.
3. The model's forecast for each district and each of the next six months is a fit output: a dataset keyed by the `district` foreign key column and the month.
4. A map layer of the forecast dataset: its geometry comes via `districtⱵgeom`, it is coloured by the prediction, and the time slider steps through the months.
5. A second layer shows the model's per-district effects, which are fit outputs keyed by district.

**Out of scope**, as the expert end of GIS that belongs in plugins: raster data analysis (imagery, elevation, terrain), network analysis and routing, geocoding, editing geometries on the map, and 3D.

#### Bayesian workflow

The framework should support the Bayesian workflow described by Gelman et al., [Bayesian Workflow](https://arxiv.org/abs/2011.01808) (2020): building models from templates and modular pieces, checking priors, fitting fast and failing fast, validating with fake data, diagnosing computational problems, checking and comparing fitted models, and propagating uncertainty into predictions. This does not need a workspace type of its own. Most of it follows from decisions made elsewhere in this document: fit outputs are datasets, every plot is a grammar spec (so the standard Bayesian diagnostic plots are gallery presets over datasets of draws), and dataset operations can carry uncertainty through aggregation.

How the stages of the workflow map onto the framework:

| Stage (section of the paper) | Where it lives | What is needed |
|---|---|---|
| Initial models from templates, built from modular pieces (§2) | Model fit workspace | A structured model builder (outcome family, predictors, varying effects, priors) that generates Stan, in the style of brms. Raw Stan remains available for experts. For most users this is the entry point to Bayesian modelling. |
| Prior predictive checks (§2.4) | A "simulate from prior" action before fitting. The simulated data is a fit output, and the check is a plot of prior predictive draws over the observed data. | Prior-only runs in the provider. |
| Fit fast, fail fast, approximate algorithms (§3) | Fit settings: quick (e.g. Pathfinder or Laplace approximation) or full (sampling) | |
| Diagnostics: R-hat, effective sample size, divergences (§3, §5) | Fit output tables and a warnings panel. Trace, rank and pair plots are presets over the draws dataset. | Plain-language explanations with suggested remedies, e.g. "divergences: try stronger priors or a non-centred parameterisation". |
| Fake-data simulation (§4.1) | Fitting the model to a dataset whose base is a prior predictive fit output, filtered to one draw | Fit outputs usable as dataset bases |
| Simulation-based calibration (§4.2) | Notebook workspace, since it is a loop over many fits | Background fitting |
| Posterior predictive checks (§6.1) | Replicated data draws as a fit output; model layers in plots; residual maps | A naming convention for replicated data and log-likelihood in Stan programs (`y_rep`, `log_lik`), generated automatically by the model builder |
| Cross-validation and influential observations (§6.2) | Per-row fit outputs (each observation's LOO contribution and Pareto k), which can be plotted and mapped | PSIS-LOO in the provider |
| Prior sensitivity (§6.3) | A table in the fit output | Power-scaling sensitivity analysis, which is computed from the existing draws without refitting |
| Propagating uncertainty, poststratification (§6.4) | Scoring and dataset operations | Model predictions with draws (below) |
| Modifying and expanding models (§7) | Cloning a model, editing and refitting | Recording which model a model was cloned from |
| Comparing and combining models (§8) | A comparison view in the model fit workspace for selected models: a LOO table and key estimates side by side | Stacking as a model provider whose configuration is a list of models. Its predictions are usable in the simulation workspace and in scoring like those of any other model. |
| Modelling as software development (§9) | Source control; fits recording their dataset definition; notebooks as the record of an analysis | |

The workflow requires four changes to the framework:

1. **Data flows back from models.** The framework otherwise flows one way, from tables to datasets to models to outputs. The workflow also runs the other way, from a model to simulated data to a new fit. A fit output can therefore be the base of a dataset.
2. **Predictions with draws.** Aggregating point predictions and intervals loses the uncertainty (averaging the bounds of intervals is wrong). The Model predictions operation can instead output draws: one row per input row and draw, with a `draw` column. An Aggregate grouped by the original groups and `draw`, followed by a summary, then propagates the uncertainty correctly through any operations. Multilevel regression and poststratification then needs no special support: score the population cells, then aggregate with population weights by region and draw. As this multiplies the number of rows, it uses a subsample of a few hundred draws.
3. **The model fit workspace follows the workflow.** For Bayesian providers, the fit area is organised as stages: prior check, fit, diagnostics, posterior check and comparison. Each stage shows a status (good, warning or problem) and the user can return to any stage at any time, since the workflow is iterative rather than linear. This guides users who have never heard of a prior predictive check, without constraining those who have.
4. **Many fits.** Simulation-based calibration, model comparison and the general advice to fit many models all assume that fits are cheap to start and are kept. Fits run in the background from a queue, with a limit on concurrent fits, and fits and their draws are persisted.

The computational remedies of the paper's §5 (reparameterisation, marginalisation, handling multimodality) are skills exercised in Stan code and do not get their own interface. The framework suggests them through the diagnostics, and the model builder applies the common ones by default (e.g. non-centred parameterisations of varying effects).

### Panels

One thing that is bringing these workspace types together is a unifying notion of panels. Panel is an elementary output, most importantly a plot, but also summary tables. A plot panel is stored as its plot spec (see [Plots and the grammar of graphics](#plots-and-the-grammar-of-graphics)), so dragging a panel copies its spec. Panels can then be dragged between workspaces. Drag and drop is always copy it never deletes a panel in the source

Some rules for drag and drop of panels

Sources:

* The current output of the data explorer is a draggable panel
* The model fit producers and number of draggable panels 
* The notebook output may be a panel that can be dragged. 
* Reports or dashboards are also sources
* Any map as a whole is a single panel that can be dragged. A map panel from the data explorer is draggable like any other plot.
* The simulation profiler and scenario comparison are draggable panels.

Sinks:
* Anything can be dragged into a report or dashboard

### Additional changes to core
* Feldspar's dataset model changes. Today a dataset is a list of formula columns, a filter and an order, stored as JSON on the model that uses it (TECHNICAL_DESIGN.md §14.2). It becomes a persistent, named definition in its own table: a base (a table, another dataset or a generated grid) and an ordered list of operations (see [Dataset operations](#dataset-operations)). Today's model is the special case of a series of Calculated column operations, one Filter and one Sort. The rows are still not materialised.
* Datasets are shared, so one dataset can be used by several models, panels and other datasets. A model fit records the dataset definition it was fitted with, so that changing a dataset does not silently change the meaning of existing fits, and the model fit workspace shows when the dataset has changed since the fit.
* The predictive models menu link is replaced by a link to the unrestricted analytics UI
* Models fits have outputs: tables and plots. Some plots may be optional, i.e. not initially shown but available in a drop-down. Where possible, a provider's plots are plot specs over a dataset of the fit's outputs rather than images, so that they can be restyled, faceted and layered like any other plot.
* the definition of models and models providers is still open and should be tweaked to align with the goals in this specification
* To support the simulation workspace and the Model predictions operation, a model provider must be able to predict for new rows of inputs and, where possible, give the uncertainty of the prediction (an interval or draws). This includes predicting from a Stan posterior, which is currently not supported.
* Fit outputs are datasets. A fit exposes its per-row outputs (e.g. fitted values, residuals, cluster assignments) as a dataset at the grain of its training dataset, and its per-group outputs (e.g. the effect of each region, or a forecast for each region and month) as a dataset keyed by the grouping columns. A Stan provider therefore maps array indexes back to keys (index 17 of the district effects is the district with id 42). Fit outputs can be explored, plotted and mapped like any other dataset. A fit output can be the base of a dataset, e.g. to fit a model to data simulated from another model.
* Fits run in the background from a queue with a limit on concurrent fits, and fits and their draws are persisted, so that they survive a restart. The Bayesian workflow depends on fitting many models.
* A geometry field type, backed by PostGIS: point, line, polygon and their multi variants, stored in WGS84. Tables with geometry can be created by importing GeoJSON, Shapefile and GeoPackage files, since boundary files are where most GIS work starts.
* Adjacency from geometry (which polygons touch), for the Neighbourhood column operation and for spatial models such as CAR/BYM.

There is no coding agent for this application type at this point. 

### Milestones

1. Workspace persistence+UI, the new dataset model, the Dataset editor
2. Add Data explorer workspace which defines the plot spec, the drop zones and the available plot types. No drag and drop
3. Model fit workspace with output panels. No drag and drop.
4. Reports and enabling drag and drop of panels from the data explorer.
5. Maps: the geometry field type and import, geometry functions and the Spatial join operation, the Map workspace with layers, symbology, attribute table, selection and reference layers, and toolbox tools for what dataset operations can do.
6. Dashboards
7. Simulation workspace, and the Model predictions operation.
8. Spatial analysis: generated grids, the Neighbourhood column operation, adjacency from geometry, fit outputs as datasets, spatial model providers (density, interpolation, hot spots, clustering) and their toolbox tools, and the map time slider.
9. Application framework for restricted analytics UIs. Everything before this milestone is the unrestricted analytics UI accessed by the admin through the site bar link.
