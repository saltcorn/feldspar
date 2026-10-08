import { describe, expect, it } from "vitest";

import { layoutHash, parseLayout, parseRoute, routeHash, withSide, type Layout, type Route } from "./router";

describe("the Analytics UI's routes", () => {
  it("reads each route from its hash and writes it back", () => {
    const routes: Route[] = [
      { name: "home" },
      { name: "workspace", id: "w 1" },
      { name: "dataset", id: "d1" },
      { name: "newDataset", table: "houses" },
      { name: "newDataset", table: null },
      { name: "dataset", id: "d1", back: "#/models/m1" },
      { name: "model", id: "m1" },
      { name: "model", id: "m1", fit: "f 2" },
      { name: "newModel", dataset: "d1" },
      { name: "newModel", dataset: null },
      { name: "compareModels", ids: ["m1", "m2"] },
      { name: "fit", id: "f1" },
    ];
    for (const route of routes) {
      expect(parseRoute(routeHash(route))).toEqual(route);
    }
  });

  it("treats an empty hash as the list and an unknown one as not found", () => {
    expect(parseRoute("")).toEqual({ name: "home" });
    expect(parseRoute("#")).toEqual({ name: "home" });
    expect(parseRoute("#/nope/x")).toEqual({ name: "notFound", path: "/nope/x" });
    expect(parseRoute("#/models/compare")).toEqual({ name: "compareModels", ids: [] });
  });
});

describe("split view's addresses (A4.1)", () => {
  it("records both sides, each keeping its own parameters", () => {
    const layouts: Layout[] = [
      { main: { name: "workspace", id: "w1" }, side: null },
      { main: { name: "workspace", id: "w1" }, side: { name: "workspace", id: "w2" } },
      { main: { name: "model", id: "m1", fit: "f1" }, side: { name: "dataset", id: "d1", back: "#/models/m1" } },
      { main: { name: "home" }, side: { name: "home" } },
    ];
    for (const layout of layouts) {
      expect(parseLayout(layoutHash(layout))).toEqual(layout);
    }
    // The main side's own route reads as it always did.
    const hash = layoutHash(layouts[2]);
    expect(parseRoute(hash)).toEqual({ name: "model", id: "m1", fit: "f1" });
    expect(hash.split("?").length).toBe(2);
  });

  it("opens, replaces and closes a side", () => {
    const one: Layout = { main: { name: "workspace", id: "w1" }, side: null };
    const two = withSide(one, "side", { name: "workspace", id: "w2" });
    expect(two).toEqual({ main: { name: "workspace", id: "w1" }, side: { name: "workspace", id: "w2" } });
    expect(withSide(two, "main", { name: "model", id: "m" }).side).toEqual({ name: "workspace", id: "w2" });
    // Closing the right leaves the left; closing the left makes the right the
    // whole screen.
    expect(withSide(two, "side", null)).toEqual(one);
    expect(withSide(two, "main", null)).toEqual({ main: { name: "workspace", id: "w2" }, side: null });
  });
});
