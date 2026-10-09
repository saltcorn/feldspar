import { describe, expect, it } from "vitest";

import { changeOf, formatValue, isCardData, periodLabel, readCard, sparkPath, tidyCard, type CardData, type StatCard } from "./card";
import { makePanel, readPanel } from "./panel";

const t = (text: string, args: Record<string, string | number> = {}) =>
  text.replace(/\{(\w+)\}/g, (_, k: string) => String(args[k] ?? `{${k}}`));

const incidents: StatCard = {
  dataset: "d1",
  value: { function: "count" },
  time: { column: "reported_on", period: "month" },
  comparison: "previous_period",
  sparkline: true,
  higher_is_better: false,
};

describe("stat cards (A6.2)", () => {
  it("writes the number in the card's format", () => {
    expect(formatValue(1234.5678)).toBe("1,234.57");
    expect(formatValue(1234, { decimals: 1 })).toBe("1,234.0");
    expect(formatValue(1_234_567, { compact: true })).toBe("1.2M");
    expect(formatValue(0.234, { style: "percent" })).toBe("23.4%");
    expect(formatValue(152_000, { style: "currency", currency: "EUR", decimals: 0 })).toBe("€152,000");
    expect(formatValue(41.5, { suffix: "m²" })).toBe("41.5 m²");
    expect(formatValue(null)).toBe("—");
    expect(formatValue(Number.NaN)).toBe("—");
    // An unknown currency writes the number alone rather than failing.
    expect(formatValue(5, { style: "currency", currency: "ZZZZ" })).toBe("5");
    expect(formatValue(1234.5, {}, "de")).toBe("1.234,5");
  });

  it("names the period", () => {
    expect(periodLabel({ start: "2024-03-01", end: "2024-04-01", period: "month" }, t)).toBe("March 2024");
    expect(periodLabel({ start: "2024-04-01", end: "2024-07-01", period: "quarter" }, t)).toBe("Q2 2024");
    expect(periodLabel({ start: "2024-03-11", end: "2024-03-18", period: "week" }, t)).toBe("week of Mar 11, 2024");
    expect(periodLabel({ start: "2024-03-15", end: "2024-03-16", period: "day" }, t)).toBe("Mar 15, 2024");
    expect(periodLabel({ start: "2024-01-01", end: "2025-01-01", period: "year" }, t)).toBe("2024");
  });

  it("says how the value compares, and whether that is good news", () => {
    const data: CardData = {
      value: 12,
      label: "count of rows",
      period: { start: "2024-03-01", end: "2024-04-01", period: "month" },
      comparison: {
        kind: "previous_period",
        value: 10,
        period: { start: "2024-02-01", end: "2024-03-01", period: "month" },
        change: 2,
        ratio: 1.2,
      },
    };
    // More incidents is bad news.
    expect(changeOf(incidents, data, t)).toEqual({ direction: "up", good: false, text: "+20%", context: "vs February 2024" });
    expect(changeOf({ ...incidents, higher_is_better: undefined }, data, t)?.good).toBe(true);
    const fewer = { ...data, comparison: { ...data.comparison!, change: -0.5, ratio: 0.95 } };
    expect(changeOf(incidents, fewer, t)).toMatchObject({ direction: "down", good: true, text: "-5.0%" });
    // From nothing there is no ratio: the difference itself.
    const fromNothing = { ...data, comparison: { ...data.comparison!, value: 0, change: 12, ratio: undefined } };
    expect(changeOf(incidents, fromNothing, t)?.text).toBe("+12");
    const same = { ...data, comparison: { ...data.comparison!, change: 0, ratio: 1 } };
    expect(changeOf(incidents, same, t)).toMatchObject({ direction: "flat", good: null });

    const share: CardData = { value: 1, label: "count of rows", comparison: { kind: "unfiltered", value: 40, change: -39, ratio: 0.25 } };
    expect(changeOf({ ...incidents, comparison: "unfiltered" }, share, t)).toEqual({
      direction: "flat",
      good: null,
      text: "25%",
      context: "of all 40",
    });
    expect(changeOf(incidents, { value: 3, label: "x" }, t)).toBeNull();
  });

  it("draws the sparkline, breaking it where a value is missing", () => {
    const points = [{ value: 0 }, { value: 10 }, { value: null }, { value: 5 }];
    expect(sparkPath(points, 30, 10)).toBe("M0 10L10 0M30 5");
    expect(sparkPath([{ value: 3 }, { value: 3 }], 10, 10)).toBe("M0 5L10 5");
    expect(sparkPath([{ value: null }], 10, 10)).toBe("");
  });

  it("is saved without what it does not use", () => {
    expect(
      tidyCard({
        dataset: "d1",
        value: { function: "sum", column: "price" },
        filter: "  ",
        comparison: "previous_period",
        sparkline: true,
        periods: 12,
        format: { style: "number", currency: "EUR", suffix: " " },
        higher_is_better: true,
      }),
    ).toEqual({ dataset: "d1", value: { function: "sum", column: "price" }, comparison: "none" });
    expect(tidyCard({ ...incidents, periods: 24, format: { style: "currency", currency: "eur", compact: true } })).toEqual({
      ...incidents,
      periods: 24,
      format: { style: "currency", currency: "EUR", compact: true },
    });
  });

  it("is a kind of panel", () => {
    const panel = makePanel({ kind: "stat_card", content: incidents }, "Incidents");
    expect(readPanel(JSON.parse(JSON.stringify(panel)))).toEqual(panel);
    expect(readPanel({ id: "x", kind: "stat_card", content: { dataset: "d1", value: { function: "mode" } } })).toBeNull();
    expect(readPanel({ id: "x", kind: "stat_card", content: { value: { function: "count" } } })).toBeNull();
    expect(readCard(incidents)).toBe(incidents);
    expect(isCardData({ value: 1, label: "x" })).toBe(true);
    expect(isCardData({ error: "no" })).toBe(false);
  });
});
