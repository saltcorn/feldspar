import { describe, expect, it } from "vitest";

import { parseRoute, routeHash, type Route } from "./router";

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
