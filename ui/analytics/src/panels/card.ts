// Stat cards (analytics TODO A6.2): one number from a dataset, as a dashboard
// shows it.
//
// A card is stored as what makes it (`sc_analytics::card::StatCard`): the
// dataset, an aggregate of a column, an optional filter, the date column its
// periods come from, a comparison, a sparkline and a number format. The
// server works the numbers out (`renderPanel` answers them in `card`); this
// file writes them: the number in the card's format, the comparison as a
// change or a share, the current period's name and the sparkline's line.

import type { Translate } from "../datasets/ops";

export type CardFunction = "count" | "count_distinct" | "sum" | "mean" | "median" | "min" | "max";
export const CARD_FUNCTIONS: CardFunction[] = ["count", "count_distinct", "sum", "mean", "median", "min", "max"];

export type Period = "day" | "week" | "month" | "quarter" | "year";
export const PERIODS: Period[] = ["day", "week", "month", "quarter", "year"];

export type Comparison = "none" | "previous_period" | "unfiltered";

export type NumberStyle = "number" | "percent" | "currency";

/** How a card writes its number. */
export type CardFormat = {
  style?: NumberStyle;
  decimals?: number;
  currency?: string;
  compact?: boolean;
  suffix?: string;
};

/** A stat card: a `stat_card` panel's content. */
export type StatCard = {
  dataset: string;
  value: { function: CardFunction; column?: string };
  filter?: string;
  time?: { column: string; period: Period; anchor?: "latest" | "today" };
  comparison?: Comparison;
  sparkline?: boolean;
  periods?: number;
  format?: CardFormat;
  /** `false` when a rise is bad news (incidents). */
  higher_is_better?: boolean;
};

/** A period, as the server answers it: its first day, the day after its last. */
export type Span = { start: string; end: string; period: Period };

/** What the server answers for a card. */
export type CardData = {
  value: number | null;
  label: string;
  period?: Span;
  comparison?: { kind: Comparison; value: number | null; period?: Span; change?: number; ratio?: number };
  sparkline?: { start: string; value: number | null }[];
};

/** The sparkline's periods when the card does not say. */
export const DEFAULT_PERIODS = 12;

/** Whether `f` needs a column of numbers. */
export function needsNumbers(f: CardFunction): boolean {
  return f !== "count" && f !== "count_distinct";
}

/** A new card over `dataset`: the number of rows. */
export function newCard(dataset: string): StatCard {
  return { dataset, value: { function: "count" }, comparison: "none" };
}

/** The card as it is saved: empty parts left out, and what needs a date
 * column dropped when there is none. */
export function tidyCard(card: StatCard): StatCard {
  const out: StatCard = { dataset: card.dataset, value: { function: card.value.function } };
  if (card.value.column) out.value.column = card.value.column;
  if (card.filter && card.filter.trim() !== "") out.filter = card.filter.trim();
  if (card.time?.column) out.time = { ...card.time };
  const comparison = card.comparison ?? "none";
  out.comparison = comparison === "previous_period" && !out.time ? "none" : comparison;
  if (card.sparkline && out.time) {
    out.sparkline = true;
    if (card.periods && card.periods !== DEFAULT_PERIODS) out.periods = card.periods;
  }
  const f = card.format ?? {};
  const format: CardFormat = {};
  if (f.style && f.style !== "number") format.style = f.style;
  if (f.decimals !== undefined && Number.isFinite(f.decimals)) format.decimals = f.decimals;
  if (f.style === "currency" && f.currency) format.currency = f.currency.trim().toUpperCase();
  if (f.compact) format.compact = true;
  if (f.suffix && f.suffix.trim() !== "") format.suffix = f.suffix.trim();
  if (Object.keys(format).length > 0) out.format = format;
  if (card.higher_is_better === false) out.higher_is_better = false;
  return out;
}

/** `value` written in the card's format, in `locale`. */
export function formatValue(value: number | null | undefined, format: CardFormat = {}, locale = "en"): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "—";
  const options: Intl.NumberFormatOptions = {};
  if (format.style === "percent") options.style = "percent";
  if (format.style === "currency" && format.currency) {
    options.style = "currency";
    options.currency = format.currency;
  }
  if (format.compact) options.notation = "compact";
  if (format.decimals !== undefined) {
    options.minimumFractionDigits = format.decimals;
    options.maximumFractionDigits = format.decimals;
  } else if (!format.compact && options.style !== "currency") {
    // A mean of whole numbers is rarely whole: two decimals at most, and
    // none when there are none.
    options.maximumFractionDigits = options.style === "percent" ? 1 : 2;
  }
  let text: string;
  try {
    text = new Intl.NumberFormat(locale, options).format(value);
  } catch {
    // An unknown currency code: the number alone.
    text = new Intl.NumberFormat(locale).format(value);
  }
  return format.suffix ? `${text} ${format.suffix}` : text;
}

/** The name of a period: "March 2024", "Q1 2024", "week of 11 Mar 2024". */
export function periodLabel(span: Span, t: Translate, locale = "en"): string {
  const [y, m, d] = span.start.split("-").map(Number);
  const date = new Date(Date.UTC(y, m - 1, d));
  const fmt = (o: Intl.DateTimeFormatOptions) => new Intl.DateTimeFormat(locale, { timeZone: "UTC", ...o }).format(date);
  switch (span.period) {
    case "day":
      return fmt({ day: "numeric", month: "short", year: "numeric" });
    case "week":
      return t("week of {day}", { day: fmt({ day: "numeric", month: "short", year: "numeric" }) });
    case "month":
      return fmt({ month: "long", year: "numeric" });
    case "quarter":
      return t("Q{quarter} {year}", { quarter: Math.floor((m - 1) / 3) + 1, year: y });
    case "year":
      return String(y);
  }
}

/** How a card's value compares, ready to show: the arrow's direction,
 * whether that is good news, and the words. `null` with no comparison or
 * nothing to compare. */
export type Change = { direction: "up" | "down" | "flat"; good: boolean | null; text: string; context: string };

export function changeOf(card: StatCard, data: CardData, t: Translate, locale = "en"): Change | null {
  const c = data.comparison;
  if (!c || c.kind === "none") return null;
  const format = card.format ?? {};
  if (c.kind === "unfiltered") {
    if (c.ratio === undefined || c.ratio === null) return null;
    const share = formatValue(c.ratio, { style: "percent", decimals: c.ratio < 0.1 ? 1 : 0 }, locale);
    return {
      direction: "flat",
      good: null,
      text: share,
      context: t("of all {value}", { value: formatValue(c.value, format, locale) }),
    };
  }
  if (c.change === undefined || c.change === null) return null;
  const direction = c.change > 0 ? "up" : c.change < 0 ? "down" : "flat";
  const better = card.higher_is_better !== false;
  const good = direction === "flat" ? null : (direction === "up") === better;
  let text: string;
  if (c.ratio !== undefined && c.ratio !== null && format.style !== "percent") {
    const pct = c.ratio - 1;
    text = formatValue(pct, { style: "percent", decimals: Math.abs(pct) < 0.1 ? 1 : 0 }, locale);
    if (pct > 0) text = `+${text}`;
  } else {
    // From nothing, or a percentage already: the difference itself.
    text = formatValue(c.change, format, locale);
    if (c.change > 0) text = `+${text}`;
  }
  const context = c.period
    ? t("vs {period}", { period: periodLabel(c.period, t, locale) })
    : t("vs the previous period");
  return { direction, good, text, context };
}

/** The sparkline as an SVG path in a `width` × `height` box: missing values
 * break the line, and a line of one value is flat across the middle. */
export function sparkPath(points: { value: number | null }[], width: number, height: number): string {
  const values = points.map((p) => p.value).filter((v): v is number => v !== null && Number.isFinite(v));
  if (values.length === 0 || points.length === 0) return "";
  const lo = Math.min(...values);
  const hi = Math.max(...values);
  const step = points.length > 1 ? width / (points.length - 1) : 0;
  const y = (v: number) => (hi === lo ? height / 2 : height - ((v - lo) / (hi - lo)) * height);
  const round = (n: number) => Math.round(n * 100) / 100;
  let path = "";
  let pen = false;
  points.forEach((p, i) => {
    if (p.value === null || !Number.isFinite(p.value)) {
      pen = false;
      return;
    }
    path += `${pen ? "L" : "M"}${round(i * step)} ${round(y(p.value))}`;
    pen = true;
  });
  return path;
}

/** Whether the server's answer is a card's numbers rather than its refusal. */
export function isCardData(v: unknown): v is CardData {
  return Boolean(v) && typeof v === "object" && "label" in (v as object);
}

/** A card read from anything, or `null`. */
export function readCard(raw: unknown): StatCard | null {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const c = raw as Record<string, unknown>;
  const value = c.value as Record<string, unknown> | undefined;
  if (typeof c.dataset !== "string" || !value || typeof value !== "object") return null;
  if (!CARD_FUNCTIONS.includes(value.function as CardFunction)) return null;
  return c as unknown as StatCard;
}
