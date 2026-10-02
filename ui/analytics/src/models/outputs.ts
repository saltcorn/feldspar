// A fit's outputs as the model editor shows them (analytics TODO A3.5): what
// `getModelOutputs` answers — tables filled from the fit, plots as specs over
// its output data, each drawn unless optional and not asked for — read off
// the wire, and the arithmetic of which are shown.
//
// Shown: every output that is not optional, in the provider's order, then the
// optional plots chosen from "More plots", in the order they were chosen.
// Compare lines several models' outputs up by name.

import { isRefused, type PlotData, type PlotSpec } from "../plot/spec";

/** A table output's contents. */
export type OutputTable = {
  columns: string[];
  rows: unknown[][];
  /** A text block (a provider's own summary), shown as it is. */
  text?: string | null;
  truncated: boolean;
};

/** One output of a fit. */
export type ModelOutput = {
  name: string;
  label: string;
  optional: boolean;
  kind: "table" | "plot";
  table?: OutputTable;
  spec?: PlotSpec;
  /** The drawn plot, when it was drawn. */
  plot?: PlotData;
  /** Why it cannot be shown: the output's own, or the plot's refusal. */
  error?: string;
};

/** The fit the outputs are of. */
export type OutputsFit = {
  id: string;
  name: string;
  status: string;
  created: string;
  active: boolean;
  error: string | null;
  dataset_changed: boolean;
};

/** One output off the wire, or `null` for one that is not an output. */
function readOutput(raw: unknown): ModelOutput | null {
  if (!raw || typeof raw !== "object") return null;
  const o = raw as Record<string, unknown>;
  if (typeof o.name !== "string") return null;
  const out: ModelOutput = {
    name: o.name,
    label: typeof o.label === "string" ? o.label : o.name,
    optional: o.optional === true,
    kind: o.kind === "plot" ? "plot" : "table",
  };
  if (o.table && typeof o.table === "object") out.table = o.table as OutputTable;
  if (o.spec && typeof o.spec === "object") out.spec = o.spec as PlotSpec;
  if (isRefused(o.plot)) {
    out.error = o.plot.error;
  } else if (o.plot && typeof o.plot === "object") {
    out.plot = o.plot as PlotData;
  }
  if (typeof o.error === "string") out.error = o.error;
  return out;
}

/** `getModelOutputs`' outputs. */
export function readOutputs(raw: unknown[]): ModelOutput[] {
  return raw.map(readOutput).filter((o): o is ModelOutput => o !== null);
}

/** `getModelOutputs`' fit, or `null` when the model has none to show. */
export function readOutputsFit(raw: unknown): OutputsFit | null {
  if (!raw || typeof raw !== "object") return null;
  const f = raw as Record<string, unknown>;
  if (typeof f.id !== "string") return null;
  return {
    id: f.id,
    name: typeof f.name === "string" ? f.name : "",
    status: typeof f.status === "string" ? f.status : "",
    created: typeof f.created === "string" ? f.created : "",
    active: f.active === true,
    error: typeof f.error === "string" ? f.error : null,
    dataset_changed: f.dataset_changed === true,
  };
}

/** The outputs on the screen: the ones that are not optional, then the
 * optional ones chosen, in the order chosen. */
export function shownOutputs(outputs: ModelOutput[], plots: string[]): ModelOutput[] {
  const chosen = plots
    .map((name) => outputs.find((o) => o.optional && o.name === name))
    .filter((o): o is ModelOutput => o !== undefined);
  return [...outputs.filter((o) => !o.optional), ...chosen];
}

/** The optional outputs "More plots" still offers. */
export function moreOutputs(outputs: ModelOutput[], plots: string[]): ModelOutput[] {
  return outputs.filter((o) => o.optional && !plots.includes(o.name));
}

/** The `include` parameter that has the chosen optional plots drawn. */
export function includeParam(plots: string[]): string | undefined {
  return plots.length > 0 ? plots.join(",") : undefined;
}

/**
 * The rows of a comparison: each output that is not optional in any of the
 * models, by name, in the order they first appear — so two linear regressions'
 * coefficient tables are side by side, and an output only one model has is a
 * row with gaps.
 */
export function compareRows(models: ModelOutput[][]): { name: string; label: string; kind: ModelOutput["kind"] }[] {
  const rows: { name: string; label: string; kind: ModelOutput["kind"] }[] = [];
  for (const outputs of models) {
    for (const o of outputs) {
      if (o.optional || rows.some((r) => r.name === o.name)) continue;
      rows.push({ name: o.name, label: o.label, kind: o.kind });
    }
  }
  return rows;
}
