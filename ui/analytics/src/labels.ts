// The Analytics UI's names for things, translated.
//
// A message is found in the source by its literal (`t("Filter")`): the
// extractor cannot follow `t(kind.label)` to the string it will hold. So the
// names kept as data — the operation kinds, the window functions, the
// summaries, the workspace kinds — are translated here, each by a call with
// its English written out, and the rest of the bundle asks for a name by its
// key.

import type { Translate } from "./datasets/ops";

/** An operation kind's name. */
export function opKindName(kind: string, t: Translate): string {
  switch (kind) {
    case "calculated":
      return t("Calculated column");
    case "filter":
      return t("Filter");
    case "select":
      return t("Select columns");
    case "sort":
      return t("Sort");
    case "window":
      return t("Window column");
    case "aggregate":
      return t("Aggregate");
    case "limit":
      return t("Limit");
    case "stack":
      return t("Stack");
    case "split":
      return t("Split");
    case "complete":
      return t("Complete");
    case "join":
      return t("Join");
    case "union":
      return t("Union");
    case "spatial_join":
      return t("Spatial join");
    default:
      return kind;
  }
}

/** What an operation kind does, in a sentence — the Add menu's tooltip. */
export function opKindAbout(kind: string, t: Translate): string {
  switch (kind) {
    case "calculated":
      return t("Add or replace a column computed by a formula.");
    case "filter":
      return t("Keep the rows a condition holds for.");
    case "select":
      return t("Keep, drop, rename and reorder columns.");
    case "sort":
      return t("Order the rows.");
    case "window":
      return t("A column computed over the ordered rows of a group: lag, running total, rank, share of the group.");
    case "aggregate":
      return t("One row per group, with summaries: count, mean, median…");
    case "limit":
      return t("The first rows, a random sample, or the top rows of each group.");
    case "stack":
      return t("Turn columns into rows of name/value pairs.");
    case "split":
      return t("Turn the values of a column into columns of their own.");
    case "complete":
      return t("Add rows for missing combinations of values.");
    case "join":
      return t("Join another table or dataset on key columns.");
    case "union":
      return t("Append the rows of another table or dataset.");
    case "spatial_join":
      return t("Join another table or dataset where the geometries meet, or to the nearest.");
    default:
      return "";
  }
}

/** A Window column function's name. */
export function windowFunctionName(fn: string, t: Translate): string {
  switch (fn) {
    case "lag":
      return t("Previous value (lag)");
    case "lead":
      return t("Next value (lead)");
    case "difference":
      return t("Difference from previous");
    case "cumulative_sum":
      return t("Running total");
    case "cumulative_mean":
      return t("Running mean");
    case "rank":
      return t("Rank");
    case "row_number":
      return t("Row number");
    case "group_sum":
      return t("Group total");
    case "group_mean":
      return t("Group mean");
    case "group_count":
      return t("Group count");
    case "group_min":
      return t("Group minimum");
    case "group_max":
      return t("Group maximum");
    case "share":
      return t("Share of group total");
    case "fill":
      return t("Last value that was not missing");
    default:
      return fn;
  }
}

/** An Aggregate summary's name. */
export function summaryName(fn: string, t: Translate): string {
  switch (fn) {
    case "count":
      return t("Count");
    case "count_distinct":
      return t("Count distinct");
    case "sum":
      return t("Sum");
    case "mean":
      return t("Mean");
    case "median":
      return t("Median");
    case "min":
      return t("Minimum");
    case "max":
      return t("Maximum");
    case "sd":
      return t("Standard deviation");
    case "first":
      return t("First");
    case "last":
      return t("Last");
    case "union":
      return t("Union of geometries");
    default:
      return fn;
  }
}

/** A Spatial join relation's name, as it reads between the two geometries. */
export function spatialRelationName(relation: string, t: Translate): string {
  switch (relation) {
    case "within":
      return t("is within");
    case "contains":
      return t("contains");
    case "intersects":
      return t("intersects");
    case "within_distance":
      return t("is within a distance of");
    case "nearest":
      return t("is nearest to");
    default:
      return relation;
  }
}

/** A workspace kind's name. */
export function workspaceKindName(kind: string, t: Translate): string {
  switch (kind) {
    case "data_explorer":
      return t("Data explorer");
    case "dashboard":
      return t("Dashboard");
    case "notebook":
      return t("Notebook");
    case "report":
      return t("Report");
    case "map":
      return t("Map");
    case "simulation":
      return t("Simulation");
    default:
      return kind;
  }
}

/** A drop zone's name. */
export function zoneName(zone: string, t: Translate): string {
  switch (zone) {
    case "x":
      return t("X");
    case "y":
      return t("Y");
    case "color":
      return t("Color");
    case "size":
      return t("Size");
    case "shape":
      return t("Shape");
    case "label":
      return t("Label");
    case "row":
      return t("Facet rows");
    case "column":
      return t("Facet columns");
    case "wrap":
      return t("Wrap");
    default:
      return zone;
  }
}

/** A mark's name, as the mark palette shows it. */
export function markName(mark: string, t: Translate): string {
  switch (mark) {
    case "point":
      return t("Points");
    case "line":
      return t("Line");
    case "bar":
      return t("Bars");
    case "area":
      return t("Area");
    case "box":
      return t("Box plot");
    case "rect":
      return t("Heatmap");
    case "text":
      return t("Text");
    case "band":
      return t("Band");
    case "errorbar":
      return t("Error bars");
    case "mosaic":
      return t("Mosaic");
    default:
      return mark;
  }
}

/** A gallery item's name. */
export function presetName(preset: string, t: Translate): string {
  switch (preset) {
    case "histogram":
      return t("Histogram");
    case "bar":
      return t("Bar chart");
    case "line":
      return t("Line chart");
    case "scatter":
      return t("Scatter plot");
    case "box":
      return t("Box plot");
    case "heatmap":
      return t("Heatmap");
    case "area":
      return t("Area chart");
    case "splom":
      return t("Scatterplot matrix");
    case "parallel":
      return t("Parallel coordinates");
    case "correlation":
      return t("Correlation heatmap");
    case "mosaic":
      return t("Mosaic plot");
    case "map":
      return t("Map");
    default:
      return preset;
  }
}

/** A stat's name, as the layers panel shows it. */
export function statName(kind: string, t: Translate): string {
  switch (kind) {
    case "identity":
      return t("The rows as they are");
    case "count":
      return t("Count");
    case "aggregate":
      return t("Summary");
    case "quantiles":
      return t("Quantiles");
    case "boxplot":
      return t("Box plot");
    case "summary":
      return t("Mean with interval");
    case "density":
      return t("Density");
    case "smooth":
      return t("Smoother");
    case "correlation":
      return t("Correlation");
    default:
      return kind;
  }
}

/** A layer the layers panel adds. */
export function layerKindName(id: string, t: Translate): string {
  switch (id) {
    case "linear":
      return t("Linear fit");
    case "loess":
      return t("Loess smoother");
    case "points":
      return t("Points");
    case "line":
      return t("Line through the rows");
    case "mean":
      return t("Mean with interval");
    case "density":
      return t("Density curve");
    case "counts":
      return t("Count labels");
    default:
      return id;
  }
}

/** A hypothesis test's name. */
export function testName(kind: string, t: Translate): string {
  switch (kind) {
    case "one_sample_t":
      return t("One-sample t-test");
    case "shapiro_wilk":
      return t("Shapiro-Wilk normality test");
    case "signed_rank":
      return t("Wilcoxon signed-rank test");
    case "chi_square_fit":
      return t("Chi-square goodness of fit");
    case "binomial":
      return t("Binomial test");
    case "welch_t":
      return t("Welch's t-test");
    case "mann_whitney":
      return t("Mann-Whitney test");
    case "anova":
      return t("One-way ANOVA");
    case "kruskal_wallis":
      return t("Kruskal-Wallis test");
    case "levene":
      return t("Levene's test");
    case "chi_square_independence":
      return t("Chi-square test of independence");
    case "fisher_exact":
      return t("Fisher's exact test");
    case "pearson":
      return t("Pearson correlation");
    case "spearman":
      return t("Spearman correlation");
    case "linear_regression":
      return t("Linear regression");
    case "logistic_regression":
      return t("Logistic regression");
    case "paired_t":
      return t("Paired t-test");
    case "paired_signed_rank":
      return t("Wilcoxon signed-rank test (paired)");
    default:
      return kind;
  }
}

/** What a test estimates. */
export function estimateName(of: string, t: Translate): string {
  switch (of) {
    case "mean":
      return t("Mean");
    case "mean_difference":
      return t("Mean difference");
    case "location_shift":
      return t("Location shift");
    case "pseudomedian":
      return t("Pseudomedian");
    case "pseudomedian_difference":
      return t("Median difference");
    case "proportion":
      return t("Proportion");
    case "odds_ratio":
      return t("Odds ratio");
    case "correlation":
      return t("Correlation");
    case "slope":
      return t("Slope");
    default:
      return of;
  }
}

/** An effect size's name. */
export function effectName(kind: string, t: Translate): string {
  switch (kind) {
    case "cohens_d":
      return t("Cohen's d");
    case "eta_squared":
      return t("η²");
    case "epsilon_squared":
      return t("ε²");
    case "rank_biserial":
      return t("Rank-biserial r");
    case "cohens_w":
      return t("Cohen's w");
    case "cohens_h":
      return t("Cohen's h");
    case "cramers_v":
      return t("Cramér's V");
    case "r_squared":
      return t("R²");
    case "mcfadden_r_squared":
      return t("McFadden's R²");
    default:
      return kind;
  }
}
