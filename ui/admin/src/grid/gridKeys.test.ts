/**
 * A Key cell, labelled by its target's summary field.
 *
 * What is asserted here is the bookkeeping around the reads the grid makes of a
 * Key's target table: which keys a page still needs labels for, the filter that
 * asks for them in one read, and what the answer is filed as — including the
 * keys it did not find, which must not be asked about again.
 */

import { describe, expect, it } from "vitest";

import { inFilter, keyLabel, keyTargetOf, labelsFrom, unresolvedKeys } from "./gridKeys";

const PEOPLE = { table: "people", field: "id", summary: "full_name" };

describe("keyTargetOf", () => {
  it("reads a Key's target and summary off its kind", () => {
    expect(
      keyTargetOf({
        type: "key",
        target_table: "people",
        target_field: "id",
        summary_field: "full_name",
      }),
    ).toEqual(PEOPLE);
  });

  it("reports no summary as null, and any other kind as no key", () => {
    expect(
      keyTargetOf({ type: "key", target_table: "people", target_field: "id", summary_field: null }),
    ).toEqual({ table: "people", field: "id", summary: null });
    expect(keyTargetOf({ type: "file", store: "uploads" })).toBeNull();
    expect(keyTargetOf(null)).toBeNull();
  });
});

describe("inFilter", () => {
  it("lists the keys, quoting one with a comma of its own", () => {
    expect(inFilter(["1", "2", "3"])).toBe("in.(1,2,3)");
    expect(inFilter(["a,b", "c"])).toBe('in.("a,b",c)');
  });
});

describe("unresolvedKeys", () => {
  it("asks each distinct key once, and never one already answered", () => {
    const rows = [{ owner: 1 }, { owner: 2 }, { owner: 1 }, { owner: null }, { owner: 3 }];
    const known = new Map<string, string | null>([["3", "Ada"]]);
    expect(unresolvedKeys(rows, "owner", known)).toEqual(["1", "2"]);
  });
});

describe("labelsFrom and keyLabel", () => {
  it("labels each found key by its summary, and files a missing one as asked", () => {
    const labels = labelsFrom(PEOPLE, ["1", "2", "9"], [
      { id: 1, full_name: "Ada Lovelace" },
      { id: 2, full_name: "" },
    ]);
    expect(labels.get("1")).toBe("Ada Lovelace");
    // An empty summary and a dangling key both show the key itself.
    expect(labels.get("2")).toBeNull();
    expect(labels.has("9")).toBe(true);
    expect(keyLabel(1, labels)).toBe("Ada Lovelace");
    expect(keyLabel(2, labels)).toBe("2");
    expect(keyLabel(9, labels)).toBe("9");
    expect(keyLabel(null, labels)).toBe("");
  });

  it("shows the key itself before its label has arrived", () => {
    expect(keyLabel(17, undefined)).toBe("17");
  });
});
