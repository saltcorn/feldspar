/**
 * The model screens' helpers: the four things task 6.6 names, plus the picker.
 *
 * Each one is here because getting it wrong is invisible on the screen rather
 * than loud:
 *
 *   - a **hyperparameter grid** that lost the difference between `8` and
 *     `[8, 12]` would silently stop searching, and one that turned `8` into
 *     `"8"` would be refused by a server naming a type the admin never typed;
 *   - a **metric set** is chosen by the outcome, so a regression must have no
 *     row where an accuracy would go — a merged set would print `—` and read
 *     like a fit that failed to compute one;
 *   - a **p-value** printed in full makes a coefficient table unreadable, and
 *     the stars are the column people actually scan;
 *   - the **instance order** decides which fit an admin checks first, and the
 *     active one is what everything outside the screen means by "the model";
 *   - and the **picker** writes formulas (§2), so what it writes has to be the
 *     formula language and not a second vocabulary that looks like it.
 */

import { describe, expect, it } from "vitest";

import {
  analyticsDatasetUrl,
  buildHyperparameters,
  featureInputs,
  formatNumber,
  formatParameterCell,
  formatPValue,
  gridPoints,
  headlineMetric,
  instanceLabel,
  metricRows,
  newDatasetUrl,
  orderInstances,
  outcomeSummary,
  parseGridValue,
  predictionSummary,
  printGridValue,
  readEncoding,
  readHyperparameters,
  readModelDataset,
  readOutcome,
  readSplit,
  relatedBody,
  significanceStars,
  typedFeatureValue,
  type Metrics,
} from "./models";

describe("the hyperparameter grid", () => {
  it("reads one number as a value and several as a search", () => {
    expect(parseGridValue("100", "int")).toBe(100);
    expect(parseGridValue("100, 200, 400", "int")).toEqual([100, 200, 400]);
    // The whitespace is the admin's, not the value's.
    expect(parseGridValue("  0.1 ,0.01 ", "float")).toEqual([0.1, 0.01]);
  });

  it("leaves a blank box unsent, so the provider's own default applies", () => {
    expect(parseGridValue("", "int")).toBeUndefined();
    expect(parseGridValue("   ", "int")).toBeUndefined();
    expect(buildHyperparameters([field("n_trees", "int")], { n_trees: "" })).toEqual({});
  });

  it("passes a value of the wrong type through as text, for the server to name", () => {
    // `buildConfig`'s rule: one authority for what a valid setting is, and its
    // message says which hyperparameter and what was wrong with it.
    expect(parseGridValue("many", "int")).toBe("many");
  });

  it("round-trips through print, up to a one-element list being the same search", () => {
    for (const text of ["100", "100, 200", "0.5, 0.25"]) {
      expect(printGridValue(parseGridValue(text, "float"))).toBe(text);
    }
    // §11: "a list of one and a scalar are the same search", so the shorter
    // spelling is what comes back.
    expect(printGridValue([8])).toBe("8");
    expect(parseGridValue("8,", "int")).toEqual([8]);
  });

  it("reads a stored object back into the boxes that edit it", () => {
    expect(readHyperparameters({ k: [2, 3, 4], seed: 7 })).toEqual({
      k: "2, 3, 4",
      seed: "7",
    });
  });

  it("counts the fits a space comes to — one when nothing is a list", () => {
    expect(gridPoints({ n_trees: 100, depth: 3 })).toBe(1);
    expect(gridPoints({ n_trees: [100, 200], depth: [1, 2, 3] })).toBe(6);
    // An empty list is a search over nothing; the server refuses it, and the
    // form can say so while it is still being typed.
    expect(gridPoints({ k: [] })).toBe(0);
  });

  it("builds the object saveModel takes, hyperparameter by hyperparameter", () => {
    const spec = [field("n_trees", "int"), field("depth", "int"), field("bagging", "bool")];
    expect(buildHyperparameters(spec, { n_trees: "100, 200", depth: "5", bagging: "true" })).toEqual(
      { n_trees: [100, 200], depth: 5, bagging: true },
    );
  });
});

describe("the outcome and its metric set", () => {
  it("says what a fit answers per row", () => {
    expect(outcomeSummary({ outcome: "regression", label: "price" })).toBe("Regression on price");
    expect(outcomeSummary({ outcome: "classification", label: "sold" })).toBe(
      "Classification of sold",
    );
    expect(outcomeSummary({ outcome: "cluster" })).toBe("Clustering");
    expect(outcomeSummary({ outcome: "embedding", dimensions: 3 })).toBe("Embedding (3 components)");
    expect(outcomeSummary({ outcome: "test" })).toBe("Hypothesis test");
    expect(outcomeSummary(null)).toBe("—");
  });

  it("gives a regression R², RMSE and MAE — and no accuracy at all", () => {
    const rows = metricRows({ metrics: "regression", r2: 0.943, rmse: 12345.6, mae: 8000, rows: 48 });
    expect(rows.map((r) => r.label)).toEqual(["R²", "RMSE", "MAE", "Rows"]);
    expect(rows[0].value).toBe("0.943");
    expect(rows[3].value).toBe("48");
  });

  it("gives a classification its accuracy, and a clustering its sizes", () => {
    const classification: Metrics = {
      metrics: "classification",
      accuracy: 0.8125,
      classes: [
        { class: "yes", precision: 0.9, recall: 0.8, f1: 0.847, support: 20 },
        { class: "no", precision: 0.7, recall: 0.6, f1: 0.646, support: 12 },
      ],
      confusion: [
        [16, 4],
        [5, 7],
      ],
      rows: 32,
    };
    expect(metricRows(classification)).toEqual([
      { label: "Accuracy", value: "0.8125" },
      { label: "Classes", value: "2" },
      { label: "Rows", value: "32" },
    ]);
    expect(metricRows({ metrics: "clustering", sizes: [10, 7, 3], wcss: 41.2, rows: 20 })[2]).toEqual(
      { label: "Cluster sizes", value: "10, 7, 3" },
    );
  });

  it("gives a hypothesis test nothing, because its parameters are the answer", () => {
    expect(metricRows({ metrics: "none" })).toEqual([]);
    expect(metricRows(null)).toEqual([]);
  });

  it("reads the headline off the held-out rows, not the fitted ones", () => {
    const headline = headlineMetric({
      train: { metrics: "regression", r2: 0.99, rmse: 1, mae: 1, rows: 40 },
      test: { metrics: "regression", r2: 0.87, rmse: 3, mae: 2, rows: 10 },
    });
    expect(headline).toBe("R² 0.87");
    expect(headlineMetric({})).toBeNull();
  });
});

describe("numbers on a screen", () => {
  it("prints a metric to four significant figures and a count as itself", () => {
    expect(formatNumber(0.9432712)).toBe("0.9433");
    expect(formatNumber(48)).toBe("48");
    expect(formatNumber(123456.789)).toBe("123500");
  });

  it("prints a null as a dash — a NaN metric is missing, not zero", () => {
    expect(formatNumber(null)).toBe("—");
    expect(formatNumber(undefined)).toBe("—");
  });

  it("does not print a p-value as 1.2e-16 in a table", () => {
    expect(formatPValue(1.2e-16)).toBe("< 0.001");
    expect(formatPValue(0.0009)).toBe("< 0.001");
    expect(formatPValue(0.049)).toBe("0.049");
    expect(formatPValue(0.051)).toBe("0.051");
    expect(formatPValue(0.6)).toBe("0.600");
  });

  it("marks significance the way every table of these does", () => {
    expect(significanceStars(1e-9)).toBe("***");
    expect(significanceStars(0.004)).toBe("**");
    expect(significanceStars(0.03)).toBe("*");
    expect(significanceStars(0.08)).toBe(".");
    expect(significanceStars(0.5)).toBe("");
    expect(significanceStars(null)).toBe("");
  });

  it("formats a parameter cell by what its column holds", () => {
    expect(formatParameterCell("term", "bedrooms")).toBe("bedrooms");
    expect(formatParameterCell("estimate", 20000.0000001)).toBe("20000");
    expect(formatParameterCell("p", 3.4e-20)).toBe("< 0.001");
    // The other spellings a provider from a module might use.
    expect(formatParameterCell("p-value", 0.02)).toBe("0.020");
    expect(formatParameterCell("Pr(>|t|)", 0.02)).toBe("0.020");
    expect(formatParameterCell("estimate", null)).toBe("—");
  });

  it("says a prediction in one line, whatever kind it is", () => {
    expect(predictionSummary({ prediction: "number", value: 160000 })).toBe("160000");
    expect(predictionSummary({ prediction: "class", class: "sold", probability: 0.82 })).toBe(
      "sold (p 0.82)",
    );
    expect(predictionSummary({ prediction: "class", class: "sold" })).toBe("sold");
    expect(predictionSummary({ prediction: "cluster", cluster: 2 })).toBe("cluster 2");
    expect(predictionSummary({ prediction: "vector", values: [1.5, -0.25] })).toBe("[1.5, -0.25]");
  });
});

describe("the instance list", () => {
  const instance = (over: Partial<{ id: string; active: boolean; created: string; name: string }>) => ({
    id: "a",
    name: "",
    active: false,
    created: "2026-01-01T00:00:00Z",
    ...over,
  });

  it("puts the active fit first and the newest next", () => {
    const ordered = orderInstances([
      instance({ id: "old", created: "2026-01-01T00:00:00Z" }),
      instance({ id: "new", created: "2026-03-01T00:00:00Z" }),
      instance({ id: "active", created: "2026-02-01T00:00:00Z", active: true }),
    ]);
    expect(ordered.map((i) => i.id)).toEqual(["active", "new", "old"]);
  });

  it("keeps the server's order for two fits of the same instant, so a poll does not shuffle", () => {
    const ordered = orderInstances([
      instance({ id: "first" }),
      instance({ id: "second" }),
      instance({ id: "third" }),
    ]);
    expect(ordered.map((i) => i.id)).toEqual(["first", "second", "third"]);
  });

  it("falls back to when a fit happened for one that was not named", () => {
    expect(instanceLabel({ name: "with income", created: "2026-01-01T00:00:00Z" })).toBe(
      "with income",
    );
    expect(instanceLabel({ name: "  ", created: "not a time" })).toBe("not a time");
  });
});

describe("reading the API's JSON blobs", () => {
  it("reads a model's dataset: a named one, by reference, as the server resolved it", () => {
    expect(
      readModelDataset({
        dataset_id: "d1",
        name: "House prices",
        table: "houses",
        columns: [{ name: "price", type: "float", expr: "price" }, { junk: 1 }],
        error: null,
      }),
    ).toEqual({
      dataset_id: "d1",
      name: "House prices",
      table: "houses",
      columns: [{ name: "price", type: "float", expr: "price" }],
      error: null,
    });
    // The shape it used to have — the formulas written on the model — is not one.
    expect(readModelDataset({ table: "houses", columns: [] })).toBeNull();
    expect(readModelDataset(null)).toBeNull();
  });

  it("links a dataset to the Analytics UI, where datasets are edited", () => {
    expect(analyticsDatasetUrl("a b")).toBe("/analytics/#/datasets/a%20b");
    expect(newDatasetUrl()).toBe("/analytics/#/datasets/new");
    expect(newDatasetUrl("houses")).toBe("/analytics/#/datasets/new?table=houses");
  });

  it("sends a related dataset as its name, its dataset's id and its label", () => {
    expect(
      relatedBody([
        { name: " counties ", dataset_id: "d2", label: " name ", columns: [] },
        { name: "edges", dataset_id: "d3", label: "  " },
      ]),
    ).toEqual([
      { name: "counties", dataset_id: "d2", label: "name" },
      { name: "edges", dataset_id: "d3" },
    ]);
  });

  it("reads a split, and falls back to four fifths fitted", () => {
    expect(readSplit({ train: 0.6, validation: 0.2, test: 0.2, seed: 7 })).toEqual({
      train: 0.6,
      validation: 0.2,
      test: 0.2,
      seed: 7,
    });
    expect(readSplit(undefined)).toEqual({ train: 0.8, validation: 0, test: 0.2, seed: 0 });
  });

  it("refuses an outcome it does not recognise rather than rendering against it", () => {
    expect(readOutcome({ outcome: "regression", label: "price" })).toEqual({
      outcome: "regression",
      label: "price",
    });
    expect(readOutcome({ outcome: "quantum" })).toBeNull();
    expect(readOutcome(null)).toBeNull();
  });
});

describe("asking a fit about a row", () => {
  const encoding = {
    columns: [
      { encoding: "standardised" as const, column: "area", mean: 100, sd: 10 },
      { encoding: "one_hot" as const, column: "region", categories: ["north", "south"] },
      { encoding: "epoch" as const, column: "listed" },
    ],
  };

  it("asks for the encoding's features — never the label, which is the answer", () => {
    const dataset = {
      dataset_id: "d1",
      name: "House prices",
      table: "houses",
      error: null,
      columns: [
        { name: "price", type: "float", expr: "price" },
        { name: "area", type: "float", expr: "area" },
        { name: "region", type: "text", expr: "neighbourhoodⱵname" },
        { name: "listed", type: "date", expr: "listed_at" },
      ],
    };
    // A column that is just itself has no formula worth showing.
    expect(featureInputs(encoding, dataset)).toEqual([
      { name: "area", kind: "number", categories: undefined, expr: undefined },
      {
        name: "region",
        kind: "category",
        categories: ["north", "south"],
        expr: "neighbourhoodⱵname",
      },
      { name: "listed", kind: "date", categories: undefined, expr: "listed_at" },
    ]);
    expect(featureInputs(null, dataset)).toEqual([]);
  });

  it("sends a number as a number, because a row is encoded the way the fit was", () => {
    // The server refuses `"100"` in a float column by name — rightly — so the
    // coercion belongs where the box is.
    expect(typedFeatureValue("100", "number")).toBe(100);
    expect(typedFeatureValue(" 2.5 ", "number")).toBe(2.5);
    expect(typedFeatureValue("true", "number")).toBe(1);
    expect(typedFeatureValue("false", "number")).toBe(0);
  });

  it("sends a category as the text it is, and a number it cannot read as typed", () => {
    expect(typedFeatureValue("north", "category")).toBe("north");
    expect(typedFeatureValue("2", "category")).toBe("2");
    // Not a number: sent as typed, so the server's refusal names the column and
    // the value rather than this form inventing one.
    expect(typedFeatureValue("about a hundred", "number")).toBe("about a hundred");
  });

  it("sends a date as epoch seconds when it is one, and as text when it is a date string", () => {
    expect(typedFeatureValue("1700000000", "date")).toBe(1700000000);
    expect(typedFeatureValue("2026-01-01", "date")).toBe("2026-01-01");
  });

  it("reads an encoding, and refuses one it does not recognise", () => {
    expect(readEncoding(encoding)?.columns).toHaveLength(3);
    expect(readEncoding({ columns: [{ encoding: "quantum", column: "x" }] })?.columns).toEqual([]);
    expect(readEncoding(null)).toBeNull();
  });
});

/** A hyperparameter declaration, with only what these tests state. */
function field(name: string, type: string) {
  return { name, label: name, type, required: false, options: [], multiline: false };
}
