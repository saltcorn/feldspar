import { describe, expect, it } from "vitest";

import type { StageShape } from "../datasets/ops";
import { freeName, modelPlan, planDataset, planModel } from "./openAsModel";
import type { TestSpec } from "./tests";

const shape: StageShape = {
  columns: [
    { name: "price", type: "float" },
    { name: "sold", type: "bool" },
    { name: "neighbourhood", type: "int", key: { table: "neighbourhoods", field: "id" } },
    { name: "area", type: "float" },
  ],
  grain: { kind: "table", table: "houses", key: "id" },
};

const spec = (y: string[], x?: string, paired = false): TestSpec => ({
  data: { kind: "dataset", dataset: "d1" },
  y: y.map((field) => ({ field })),
  x: x ? { field: x } : undefined,
  paired,
  mu: 0,
});

describe("Open as model", () => {
  it("makes a linear regression of a number by a factor", () => {
    expect(modelPlan(spec(["price"], "neighbourhood"), shape, "Houses", [], [])).toEqual({
      provider: "linear_regression",
      label: "price",
      feature: "neighbourhood",
      name: "price by neighbourhood",
      datasetName: "Houses: price by neighbourhood",
      base: "d1",
    });
  });

  it("makes a logistic regression of anything that is not a number", () => {
    expect(modelPlan(spec(["sold"], "area"), shape, "Houses", [], [])?.provider).toBe("logistic_regression");
    // A key is a category, though its values are numbers.
    expect(modelPlan(spec(["neighbourhood"], "area"), shape, "Houses", [], [])?.provider).toBe("logistic_regression");
  });

  it("offers nothing that is not one response and one factor", () => {
    expect(modelPlan(spec(["price"]), shape, "Houses", [], [])).toBeNull();
    expect(modelPlan(spec(["price", "area"], "neighbourhood"), shape, "Houses", [], [])).toBeNull();
    expect(modelPlan(spec(["price"], "area", true), shape, "Houses", [], [])).toBeNull();
    expect(modelPlan(spec(["price"], "price"), shape, "Houses", [], [])).toBeNull();
    expect(modelPlan(spec(["gone"], "area"), shape, "Houses", [], [])).toBeNull();
    expect(modelPlan(null, shape, "Houses", [], [])).toBeNull();
  });

  it("picks names nothing has", () => {
    expect(freeName("a", [])).toBe("a");
    expect(freeName("a", ["a", "a (2)"])).toBe("a (3)");
    const plan = modelPlan(spec(["price"], "area"), shape, "Houses", ["price by area"], ["Houses: price by area"]);
    expect(plan?.name).toBe("price by area (2)");
    expect(plan?.datasetName).toBe("Houses: price by area (2)");
  });

  it("names a key factor by its target's label, so it is fitted as categories", () => {
    const plan = modelPlan(spec(["price"], "neighbourhood"), shape, "Houses", [], [], "name")!;
    expect(plan.feature).toBe("neighbourhood_name");
    expect(plan.name).toBe("price by neighbourhood");
    expect(planDataset(plan).operations).toEqual([
      { id: "op1", enabled: true, kind: "calculated", params: { name: "neighbourhood_name", formula: "neighbourhoodⱵname" } },
      {
        id: "op2",
        enabled: true,
        kind: "select",
        params: { columns: [{ column: "price" }, { column: "neighbourhood_name" }] },
      },
    ]);
    // A factor that is not a key is kept as it is.
    expect(modelPlan(spec(["price"], "area"), shape, "Houses", [], [], "name")?.featureFormula).toBeUndefined();
  });

  it("gives the model a dataset of its own keeping the two columns", () => {
    const plan = modelPlan(spec(["price"], "neighbourhood"), shape, "Houses", [], [])!;
    expect(planDataset(plan)).toEqual({
      name: "Houses: price by neighbourhood",
      description: "",
      base: { kind: "dataset", dataset: "d1" },
      operations: [
        {
          id: "op1",
          enabled: true,
          kind: "select",
          params: { columns: [{ column: "price" }, { column: "neighbourhood" }] },
        },
      ],
    });
    const model = planModel(plan, "d2");
    expect(model.provider).toBe("linear_regression");
    expect(model.dataset).toEqual({ dataset_id: "d2" });
    expect(model.configuration).toEqual({ label: "price" });
  });
});
