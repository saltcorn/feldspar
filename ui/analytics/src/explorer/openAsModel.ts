// **Open as model** (analytics TODO A3.7): the explorer's question — Y by X —
// asked again as a model, so its coefficients, residuals and predictions are
// one press away from the box plot and its ANOVA.
//
// A model's features are every column of its dataset but the label, so the
// model gets a dataset of its own: based on the explorer's dataset, with one
// Select columns operation keeping Y and X. The provider is chosen by Y's type:
// a number is a linear regression, anything else (text, a true/false, a key)
// a logistic regression's class. Paired mode, several columns on Y, or no X
// are not one response and one factor, and offer nothing.
//
// A factor that is a **foreign key** is named by its target's label instead
// (`neighbourhoodⱵname`, a Calculated column): a key's values are numbers,
// and a regression on them would fit one slope across arbitrary ids rather
// than a level per group. A text column is encoded as categories.

import type { StageShape } from "../datasets/ops";
import type { TestSpec } from "./tests";

/** What pressing the button makes. */
export type ModelPlan = {
  provider: "linear_regression" | "logistic_regression";
  /** The response: the model's label. */
  label: string;
  /** The factor, as the model's dataset names it. */
  feature: string;
  /** When the factor is a key named by its target's label: the Calculated
   * column's formula (`neighbourhoodⱵname`). */
  featureFormula?: string;
  /** The new model's name. */
  name: string;
  /** The new dataset's name. */
  datasetName: string;
  /** The explorer's dataset, the new one's base. */
  base: string;
};

/** The column types a linear regression's label may have. */
const NUMBERS = ["int", "float", "decimal"];

/** `base`, or `base (2)`, `base (3)`… — the first `taken` does not have. */
export function freeName(base: string, taken: string[]): string {
  if (!taken.includes(base)) return base;
  for (let n = 2; ; n += 1) {
    const name = `${base} (${n})`;
    if (!taken.includes(name)) return name;
  }
}

/** The model the explorer's roles describe, or `null` when they describe none. */
export function modelPlan(
  spec: TestSpec | null,
  shape: StageShape | null,
  datasetName: string,
  takenModels: string[],
  takenDatasets: string[],
  /** A text column of the table X's key points at, when X is a key. */
  keyLabel?: string,
): ModelPlan | null {
  if (!spec || !shape || spec.paired || spec.data.kind !== "dataset") return null;
  if (spec.y.length !== 1 || !spec.x) return null;
  const label = spec.y[0].field;
  const feature = spec.x.field;
  if (label === feature) return null;
  const column = shape.columns.find((c) => c.name === label);
  if (!column || !shape.columns.some((c) => c.name === feature)) return null;
  const numeric = NUMBERS.includes(column.type) && !column.key;
  const name = `${label} by ${feature}`;
  const isKey = Boolean(shape.columns.find((c) => c.name === feature)?.key);
  const named = isKey && keyLabel ? `${feature}_${keyLabel}` : null;
  return {
    provider: numeric ? "linear_regression" : "logistic_regression",
    label,
    feature: named ?? feature,
    ...(named ? { featureFormula: `${feature}Ⱶ${keyLabel}` } : {}),
    name: freeName(name, takenModels),
    datasetName: freeName(`${datasetName}: ${name}`, takenDatasets),
    base: spec.data.dataset,
  };
}

/** The new dataset's definition, as `createDataset` takes it. */
export function planDataset(plan: ModelPlan) {
  return {
    name: plan.datasetName,
    description: "",
    base: { kind: "dataset", dataset: plan.base },
    operations: [
      ...(plan.featureFormula
        ? [
            {
              id: "op1",
              enabled: true,
              kind: "calculated",
              params: { name: plan.feature, formula: plan.featureFormula },
            },
          ]
        : []),
      {
        id: plan.featureFormula ? "op2" : "op1",
        enabled: true,
        kind: "select",
        params: { columns: [{ column: plan.label }, { column: plan.feature }] },
      },
    ],
  };
}

/** The new model, as `saveModel` takes it, over the dataset `dataset`. */
export function planModel(plan: ModelPlan, dataset: string) {
  return {
    id: null,
    name: plan.name,
    description: "",
    provider: plan.provider,
    dataset: { dataset_id: dataset },
    related: [],
    configuration: { label: plan.label },
    hyperparameters: {},
    split: { train: 0.8, validation: 0, test: 0.2, seed: 0 },
    attributes: {},
  };
}
