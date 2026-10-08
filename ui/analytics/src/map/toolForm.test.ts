import { describe, expect, it } from "vitest";

import type { MapLayer } from "./spec";
import { columnChoices, groupTools, initialAnswers, missingAnswer, runParams, toolParams, type ToolItem } from "./toolForm";

const perRegion: ToolItem = {
  id: "sum_per_region",
  group: "Aggregate",
  label: "Sum per region",
  description: "",
  params: [
    { name: "layer", label: "Features", kind: "layer" },
    { name: "regions", label: "Regions", kind: "layer" },
    { name: "column", label: "Sum of", kind: "column", of: "layer", types: ["number"] },
  ],
};
const buffer: ToolItem = {
  id: "buffer",
  group: "Proximity",
  label: "Buffer",
  description: "",
  params: [
    { name: "layer", label: "Layer", kind: "layer" },
    { name: "distance", label: "Distance", kind: "number", unit: "m", default: 500 },
    { name: "keep", label: "Keep", kind: "choice", options: [{ value: "left", label: "all" }] },
  ],
};
const at = { kind: "column", column: "at" } as const;
const layers: MapLayer[] = [
  { id: "districts", name: "Districts", dataset: "d-districts", geometry: at },
  { id: "incidents", name: "Incidents", dataset: "d-incidents", geometry: at, filter: "kind == 1" },
];
const columnsOf = (dataset: string) =>
  dataset === "d-incidents"
    ? [
        { name: "id", type: "int" },
        { name: "district", type: "int", key: { table: "districts", field: "id" } },
        { name: "cost", type: "float" },
        { name: "kind", type: "text" },
        { name: "at", type: "geometry" },
      ]
    : [];

describe("a tool's form", () => {
  it("groups the tools in the order they came", () => {
    expect(groupTools([buffer, perRegion, { ...buffer, id: "b2" }]).map(([g, ts]) => [g, ts.map((t) => t.id)])).toEqual([
      ["Proximity", ["buffer", "b2"]],
      ["Aggregate", ["sum_per_region"]],
    ]);
  });

  it("starts each layer field on a different layer, the picked one first", () => {
    expect(initialAnswers(perRegion, layers, "incidents")).toEqual({ layer: "incidents", regions: "districts", column: "" });
    // With none picked: the top layer first.
    expect(initialAnswers(perRegion, layers, null)).toMatchObject({ layer: "incidents", regions: "districts" });
    expect(initialAnswers(buffer, layers, "districts")).toEqual({ layer: "districts", distance: "500", keep: "left" });
  });

  it("offers the picked layer's numbers for a number column, and says what is missing", () => {
    const answers = initialAnswers(perRegion, layers, "incidents");
    const column = toolParams(perRegion)[2];
    expect(columnChoices(column, answers, layers, columnsOf).map((c) => c.name)).toEqual(["id", "cost"]);
    expect(missingAnswer(perRegion, answers)).toBe("Sum of");
    expect(missingAnswer(perRegion, { ...answers, column: "cost" })).toBeNull();
  });

  it("answers with the layers themselves, and numbers as numbers", () => {
    expect(runParams(perRegion, { layer: "incidents", regions: "districts", column: "cost" }, layers)).toEqual({
      layer: layers[1],
      regions: layers[0],
      column: "cost",
    });
    expect(runParams(buffer, { layer: "districts", distance: "250", keep: "left" }, layers)).toEqual({
      layer: layers[0],
      distance: 250,
      keep: "left",
    });
  });
});
