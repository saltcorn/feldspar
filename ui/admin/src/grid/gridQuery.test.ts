/**
 * What the grid asks the server for.
 *
 * The assertions here are the ones an admin would notice immediately if they
 * were wrong: that a bare word in a filter box means "contains" on text and
 * "equals" on a number (so a filter on an `int` column is not a 400), that
 * anything the server's own vocabulary can say can be typed straight in, that
 * clicking a header three times gets back to unsorted, and that the pages a
 * viewport asks for cover it — including at the top, where a virtualizer asks
 * for rows before the first one.
 */

import { describe, expect, it } from "vitest";

import {
  PAGE_SIZE,
  activeFilterCount,
  filterQuery,
  filterSpec,
  missingPages,
  orderSpec,
  pagesToFetch,
  pageBounds,
  pageOf,
  pagesCovering,
  toggleSort,
  type Sort,
} from "./gridQuery";

describe("filterSpec", () => {
  it("reads a bare word as a pattern on text and an equality on anything else", () => {
    expect(filterSpec("text", "rock")).toBe("ilike.%rock%");
    expect(filterSpec("string", "rock")).toBe("ilike.%rock%");
    // `ilike` is not a comparison a number or a date column has, so a bare word
    // there is an equality rather than a query the server would refuse.
    expect(filterSpec("int", "42")).toBe("eq.42");
    expect(filterSpec("date", "2026-01-01")).toBe("eq.2026-01-01");
    expect(filterSpec("bool", "true")).toBe("eq.true");
  });

  it("passes the server's own vocabulary through unaltered", () => {
    expect(filterSpec("text", "eq.rock")).toBe("eq.rock");
    expect(filterSpec("date", "gte.2020-01-01")).toBe("gte.2020-01-01");
    expect(filterSpec("text", "is_null.true")).toBe("is_null.true");
    expect(filterSpec("int", "in.(1,2,3)")).toBe("in.(1,2,3)");
    // A value with a dot in it is not an operator: the server splits on the
    // first dot, and `rock` is not one of the comparisons.
    expect(filterSpec("text", "rock.and.roll")).toBe("ilike.%rock.and.roll%");
  });

  it("takes the comparison symbols somebody types in a spreadsheet", () => {
    expect(filterSpec("int", ">= 100")).toBe("gte.100");
    expect(filterSpec("int", ">100")).toBe("gt.100");
    expect(filterSpec("int", "<= 3")).toBe("lte.3");
    expect(filterSpec("int", "<3")).toBe("lt.3");
    expect(filterSpec("text", "!= draft")).toBe("ne.draft");
    expect(filterSpec("text", "=draft")).toBe("eq.draft");
  });

  it("filters nothing for an empty box", () => {
    expect(filterSpec("text", "")).toBeNull();
    expect(filterSpec("text", "   ")).toBeNull();
  });
});

describe("filterQuery", () => {
  const types = { title: "text", pages: "int" };

  it("keeps one entry per typed-in box and drops the empty ones", () => {
    expect(filterQuery(types, { title: "rock", pages: "" })).toEqual({
      title: "ilike.%rock%",
    });
    expect(activeFilterCount(types, { title: "rock", pages: ">3" })).toBe(2);
  });

  it("drops a box on a column the table no longer has", () => {
    // Sending it would make the server refuse the whole read by name — right for
    // a caller who meant it, wrong for a filter left behind by a dropped field.
    expect(filterQuery(types, { gone: "x", title: "rock" })).toEqual({
      title: "ilike.%rock%",
    });
  });
});

describe("orderSpec and toggleSort", () => {
  it("is undefined when nothing is sorted", () => {
    expect(orderSpec([])).toBeUndefined();
  });

  it("renders precedence in list order", () => {
    const sorting: Sort[] = [
      { id: "pages", desc: true },
      { id: "title", desc: false },
    ];
    expect(orderSpec(sorting)).toBe("pages.desc,title.asc");
  });

  it("ends on the primary key, so offset pages are pages of one order", () => {
    expect(orderSpec([], "id")).toBe("id.asc");
    expect(orderSpec([{ id: "pages", desc: true }], "id")).toBe("pages.desc,id.asc");
    // Already sorted on it: it is not added a second time, nor its direction changed.
    expect(orderSpec([{ id: "id", desc: true }], "id")).toBe("id.desc");
    // No single-column key: nothing to end on.
    expect(orderSpec([], null)).toBeUndefined();
  });

  it("cycles a header through ascending, descending and unsorted", () => {
    let sorting: Sort[] = [];
    sorting = toggleSort(sorting, "title", false);
    expect(sorting).toEqual([{ id: "title", desc: false }]);
    sorting = toggleSort(sorting, "title", false);
    expect(sorting).toEqual([{ id: "title", desc: true }]);
    sorting = toggleSort(sorting, "title", false);
    expect(sorting).toEqual([]);
  });

  it("replaces the sort on a plain click and appends on a shift-click", () => {
    const one = toggleSort([], "title", false);
    expect(toggleSort(one, "pages", false)).toEqual([{ id: "pages", desc: false }]);
    expect(toggleSort(one, "pages", true)).toEqual([
      { id: "title", desc: false },
      { id: "pages", desc: false },
    ]);
  });
});

describe("paging", () => {
  it("puts a row in its page and a page at its offset", () => {
    expect(pageOf(0)).toBe(0);
    expect(pageOf(PAGE_SIZE - 1)).toBe(0);
    expect(pageOf(PAGE_SIZE)).toBe(1);
    expect(pageBounds(3)).toEqual({ offset: 3 * PAGE_SIZE, limit: PAGE_SIZE });
  });

  it("covers a viewport, and clamps the overscan above the first row", () => {
    expect(pagesCovering(0, 10, 100)).toEqual([0]);
    expect(pagesCovering(90, 210, 100)).toEqual([0, 1, 2]);
    // A virtualizer asked for overscan at the top reports a negative start; a
    // negative page is an offset the server refuses.
    expect(pagesCovering(-20, 30, 100)).toEqual([0]);
  });

  it("asks only for what is neither held nor already in flight", () => {
    expect(missingPages([0, 1, 2], [0, 2])).toEqual([1]);
    expect(missingPages([4], [])).toEqual([4]);
    expect(missingPages([], [1])).toEqual([]);
  });

  it("at rest, asks for every page the viewport is missing", () => {
    expect(pagesToFetch([40, 41], [0, 1], false)).toEqual([40, 41]);
  });

  it("while scrolling, asks only for the pages next to held ones", () => {
    // A wheel through the rows: page 2 is next to page 1, which is held.
    expect(pagesToFetch([1, 2], [0, 1], true)).toEqual([2]);
    // The thumb dragged half way down: nothing near page 250 is held, so it
    // waits for the scroll to settle rather than being one of hundreds of reads.
    expect(pagesToFetch([250, 251], [0, 1], true)).toEqual([]);
    // A page in flight counts as held.
    expect(pagesToFetch([3], [0, 1, 2], true)).toEqual([3]);
    // Nothing held at all — the first read of a new question — asks.
    expect(pagesToFetch([7], [], true)).toEqual([7]);
  });
});
