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
    default:
      return fn;
  }
}

/** A workspace kind's name. */
export function workspaceKindName(kind: string, t: Translate): string {
  switch (kind) {
    case "data_explorer":
      return t("Data explorer");
    case "dashboard":
      return t("Dashboard");
    case "model_fit":
      return t("Model fit");
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
