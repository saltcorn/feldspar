/**
 * The store of a static directory is picked from the application's own declared
 * file stores — the claim of TODO §5, which is a claim about `storeOptions`.
 */

import { describe, expect, it } from "vitest";

import {
  blankStaticRow,
  staticDirsToRequest,
  storeOptions,
} from "./staticDirs";

describe("storeOptions", () => {
  it("offers the application's declared subset, not the server's stores", () => {
    expect(storeOptions(["Assets", "uploads"], "")).toEqual([
      { value: "Assets", declared: true },
      { value: "uploads", declared: true },
    ]);
  });

  it("changes with the subset, because it is read from the live form state", () => {
    // Ticking a store above has to make it pickable below without a round trip.
    expect(storeOptions(["Assets"], "").map((o) => o.value)).toEqual(["Assets"]);
    expect(storeOptions(["Assets", "media"], "").map((o) => o.value)).toEqual([
      "Assets",
      "media",
    ]);
  });

  it("keeps a stored store the subset no longer offers, and marks it", () => {
    // Snapping to the first declared store would save a different directory
    // under the same mount, without saying so.
    const options = storeOptions(["Assets"], "archive");
    expect(options).toEqual([
      { value: "Assets", declared: true },
      { value: "archive", declared: false },
    ]);
  });

  it("offers nothing extra for a row with no store yet", () => {
    expect(storeOptions([], "")).toEqual([]);
    expect(blankStaticRow()).toEqual({ mount: "", store: "", path: "" });
  });
});

describe("staticDirsToRequest", () => {
  it("drops the rows nobody filled in", () => {
    expect(
      staticDirsToRequest([
        { mount: "/img", store: "Assets", path: "media" },
        blankStaticRow(),
      ]),
    ).toEqual([{ mount: "/img", store: "Assets", path: "media" }]);
  });

  it("passes a store outside the subset through untouched", () => {
    const rows = [{ mount: "/img", store: "archive", path: "media" }];
    expect(staticDirsToRequest(rows)).toEqual(rows);
  });
});
