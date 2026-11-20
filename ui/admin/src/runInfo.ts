// What a coding run is doing and has done, read from the run the API serves:
// what it cost, its plan, and its diff as lines to draw (TODO 10.6).
//
// Pure functions over `getRun` and `getRunDiff`, so the chat and the run
// screen draw the same numbers and the numbers are tested without a browser.

import type { GetRunDiffResponse, GetRunResponse } from "./client";

/** What a run spent, its sessions included. */
export type RunTotals = {
  /** Model calls, sessions' included. */
  steps: number;
  inputTokens: number;
  cachedTokens: number;
  outputTokens: number;
  /** `null` when any call's model has no price: an unknown cost is not zero. */
  cost: number | null;
};

type Usage = {
  input_tokens?: number;
  output_tokens?: number;
  cached_input_tokens?: number;
};
type Priced = { usage?: Usage; cost?: number | null };

/** A run's totals, from the ledger in its stored loop state. */
export function runTotals(context: unknown): RunTotals {
  const ledger = (context as { ledger?: Record<string, unknown> } | null)?.ledger ?? {};
  const totals: RunTotals = {
    steps: 0,
    inputTokens: 0,
    cachedTokens: 0,
    outputTokens: 0,
    cost: 0,
  };
  const add = (priced: Priced, steps: number) => {
    totals.steps += steps;
    totals.inputTokens += priced.usage?.input_tokens ?? 0;
    totals.cachedTokens += priced.usage?.cached_input_tokens ?? 0;
    totals.outputTokens += priced.usage?.output_tokens ?? 0;
    totals.cost =
      totals.cost === null || typeof priced.cost !== "number" ? null : totals.cost + priced.cost;
  };
  const list = (key: string): Priced[] =>
    Array.isArray(ledger[key]) ? (ledger[key] as Priced[]) : [];
  for (const step of list("steps")) add(step, 1);
  // Summaries and commit messages are calls too, but not steps of the loop.
  for (const call of [...list("summaries"), ...list("asides")]) add(call, 0);
  const children = Array.isArray(ledger.children)
    ? (ledger.children as { totals?: Record<string, Priced & { steps?: number }> }[])
    : [];
  for (const child of children) {
    for (const role of Object.values(child.totals ?? {})) add(role, role.steps ?? 0);
  }
  return totals;
}

/** A run's cost and size, as one line. */
export function totalsLabel(totals: RunTotals): string {
  const cost =
    totals.cost === null
      ? "cost unknown (a model has no price)"
      : `cost ${totals.cost.toFixed(totals.cost < 1 ? 4 : 2)}`;
  const tokens = (n: number) => (n >= 10_000 ? `${Math.round(n / 1000)}k` : String(n));
  const cached = totals.cachedTokens > 0 ? `, ${tokens(totals.cachedTokens)} cached` : "";
  return (
    `${cost} · ${totals.steps} step${totals.steps === 1 ? "" : "s"} · ` +
    `${tokens(totals.inputTokens)} in${cached}, ${tokens(totals.outputTokens)} out`
  );
}

/** A token count, short: 812, 21.4k, 219k, 1.2M. */
export function compactCount(n: number): string {
  if (n < 1000) return String(n);
  if (n < 100_000) return `${(n / 1000).toFixed(1).replace(/\.0$/, "")}k`;
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
  return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
}

/** How long a run took, from its start to its last change, in the two largest
 * units: "42 s", "3 min 5 s", "1 h 12 min". Empty when the times do not say. */
export function elapsedLabel(from: string, to: string): string {
  const ms = new Date(to).getTime() - new Date(from).getTime();
  if (!Number.isFinite(ms) || ms < 0) return "";
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s} s`;
  const min = Math.floor(s / 60);
  if (min < 60) return s % 60 ? `${min} min ${s % 60} s` : `${min} min`;
  const h = Math.floor(min / 60);
  return min % 60 ? `${h} h ${min % 60} min` : `${h} h`;
}

export type PlanFeature = NonNullable<GetRunResponse["plan"]>["features"][number];

/** A feature's status as a checklist mark and a word, as the planner sees it. */
export function featureMark(status: string): { mark: string; label: string; done: boolean } {
  switch (status) {
    case "done":
      return { mark: "✓", label: "done", done: true };
    case "in_progress":
      return { mark: "…", label: "in progress", done: false };
    case "failed":
      return { mark: "✗", label: "failed", done: false };
    case "blocked":
      return { mark: "–", label: "blocked", done: false };
    default:
      return { mark: "○", label: "to do", done: false };
  }
}

/** `2 of 3 done`, for the checklist's heading. */
export function planProgress(plan: NonNullable<GetRunResponse["plan"]>): string {
  const done = plan.features.filter((f) => f.status === "done").length;
  return `${done} of ${plan.features.length} done`;
}

/** One line of a unified diff, classified for drawing. */
export type DiffLine = { kind: "file" | "hunk" | "add" | "remove" | "context"; text: string };

/** A unified diff as lines to colour. */
export function diffLines(unified: string): DiffLine[] {
  const lines = unified.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines.map((text) => {
    if (text.startsWith("--- ") || text.startsWith("+++ ")) return { kind: "file", text };
    if (text.startsWith("@@")) return { kind: "hunk", text };
    if (text.startsWith("+")) return { kind: "add", text };
    if (text.startsWith("-")) return { kind: "remove", text };
    return { kind: "context", text };
  });
}

/** A scope's heading: the store, and the directory within it. */
export function scopeLabel(scope: GetRunDiffResponse["scopes"][number]): string {
  return scope.root === "" ? scope.store : `${scope.store}/${scope.root}`;
}
