/**
 * A cell's value, shown and typed.
 *
 * The property this file exists to hold is the round trip: copying a range and
 * pasting it straight back must change nothing. That only stays true if the
 * column's **type** decides what a piece of text means — `0` is `false` in a
 * `bool` column and the string `"0"` in a `text` one — rather than the shape of
 * the text, which would quietly turn a text column full of digits into numbers.
 */

import { describe, expect, it } from "vitest";

import {
  display,
  displayTemporal,
  fromPicker,
  isBoolType,
  isNumericType,
  parseCell,
  pickerType,
  sameValue,
  toPicker,
} from "./gridValues";

describe("display", () => {
  it("shows an empty cell for a missing value rather than the word for it", () => {
    expect(display(null)).toBe("");
    expect(display(undefined)).toBe("");
  });

  it("shows scalars as themselves and objects as their JSON", () => {
    expect(display(412)).toBe("412");
    expect(display(false)).toBe("false");
    expect(display("Dune")).toBe("Dune");
    expect(display({ a: 1 })).toBe('{"a":1}');
    expect(display([1, 2])).toBe("[1,2]");
  });
});

describe("parseCell", () => {
  it("clears a cell for empty text, whatever the column is", () => {
    expect(parseCell("text", "")).toBeNull();
    expect(parseCell("int", "   ")).toBeNull();
  });

  it("lets the column's type decide, not the shape of the text", () => {
    expect(parseCell("int", "412")).toBe(412);
    expect(parseCell("text", "412")).toBe("412");
    expect(parseCell("bool", "0")).toBe(false);
    expect(parseCell("text", "0")).toBe("0");
    expect(parseCell("bool", "YES")).toBe(true);
    expect(parseCell("json", '{"a":1}')).toEqual({ a: 1 });
  });

  it("leaves text the column cannot take as text, for the server to name", () => {
    // The server's own coercion gives the better message — it can name the
    // column and its type — than a guess made here could.
    expect(parseCell("int", "lots")).toBe("lots");
    expect(parseCell("json", "{oops")).toBe("{oops");
    expect(parseCell("bool", "maybe")).toBe("maybe");
  });

  it("round-trips every value a cell can hold", () => {
    for (const [type, value] of [
      ["text", "Dune"],
      ["text", "0"],
      ["int", 412],
      ["float", 1.5],
      ["bool", true],
      ["bool", false],
      ["json", { a: [1, 2] }],
      ["date", "2026-01-01"],
    ] as Array<[string, unknown]>) {
      expect(sameValue(parseCell(type, display(value)), value)).toBe(true);
    }
  });
});

describe("kinds and sameness", () => {
  it("knows which columns are ticks and which are right-aligned", () => {
    expect(isBoolType("bool")).toBe(true);
    expect(isBoolType("text")).toBe(false);
    expect(isNumericType("decimal")).toBe(true);
    expect(isNumericType("date")).toBe(false);
  });

  it("treats null and undefined as the same empty cell", () => {
    expect(sameValue(null, undefined)).toBe(true);
    expect(sameValue(null, "")).toBe(false);
    expect(sameValue({ a: 1 }, { a: 1 })).toBe(true);
  });
});

describe("date and time pickers", () => {
  it("edits each temporal type through its own picker, and nothing else through one", () => {
    expect(pickerType("date")).toBe("date");
    expect(pickerType("time")).toBe("time");
    expect(pickerType("timestamp")).toBe("datetime-local");
    expect(pickerType("text")).toBeNull();
    expect(pickerType("int")).toBeNull();
  });

  it("opens a date and a time on what the wire holds", () => {
    expect(toPicker("date", "2024-05-01")).toBe("2024-05-01");
    expect(toPicker("time", "10:30:00")).toBe("10:30:00");
    expect(toPicker("time", "10:30:15.250")).toBe("10:30:15");
    expect(toPicker("time", "10:30")).toBe("10:30:00");
    expect(toPicker("date", null)).toBe("");
    // A value the picker cannot hold opens it empty rather than on a wrong date.
    expect(toPicker("date", "soon")).toBe("");
  });

  it("opens a timestamp on its local reading, and turns that back into the same instant", () => {
    const stored = "2024-05-01T10:00:00+00:00";
    const reading = toPicker("timestamp", stored);
    expect(reading).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}$/);
    // Whatever zone the test runs in, the reading means the stored instant.
    expect(new Date(fromPicker("timestamp", reading) as string).getTime()).toBe(
      new Date(stored).getTime(),
    );
    expect(displayTemporal("timestamp", stored)).toBe(reading.replace("T", " "));
  });

  it("sends a cleared picker as null and a minute-only time with its seconds", () => {
    expect(fromPicker("date", "")).toBeNull();
    expect(fromPicker("timestamp", "  ")).toBeNull();
    expect(fromPicker("date", "2024-05-01")).toBe("2024-05-01");
    expect(fromPicker("time", "09:15")).toBe("09:15:00");
    expect(fromPicker("time", "09:15:30")).toBe("09:15:30");
  });

  it("shows a date and a time as themselves", () => {
    expect(displayTemporal("date", "2024-05-01")).toBe("2024-05-01");
    expect(displayTemporal("time", "10:30:00")).toBe("10:30:00");
    expect(displayTemporal("text", "x")).toBe("x");
  });
});
