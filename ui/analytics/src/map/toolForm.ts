// The toolbox's forms (analytics TODO A5.12), as data: a tool's fields filled
// in from the map, and the answers as `runMapTool` takes them.
//
// A tool's form is the server's (`listMapTools`): fields asking for a layer
// of the map, a column of a layer another field picked, a number in a unit, a
// choice, or text. A layer is answered with the layer itself — its dataset,
// its geometry source and its filter — so the tool starts from what the map
// shows.

import type { ListMapToolsResponse } from "../client";
import type { MapLayer } from "./spec";
import { isMeasure, type ColumnInfo } from "./workspace";

/** One tool, as `listMapTools` answers it. */
export type ToolItem = ListMapToolsResponse[number];

/** One field of a tool's form. */
export type ToolParam = {
  name: string;
  label: string;
  kind: "layer" | "column" | "number" | "choice" | "text";
  of?: string;
  types?: string[];
  unit?: string;
  options?: { value: string; label: string }[];
  default?: unknown;
  optional?: boolean;
};

/** A tool's fields. */
export function toolParams(tool: ToolItem): ToolParam[] {
  return (tool.params ?? []) as ToolParam[];
}

/** The tools in their groups, in the order they came. */
export function groupTools(tools: ToolItem[]): [string, ToolItem[]][] {
  const groups = new Map<string, ToolItem[]>();
  for (const tool of tools) {
    const list = groups.get(tool.group) ?? [];
    list.push(tool);
    groups.set(tool.group, list);
  }
  return [...groups.entries()];
}

/** A form's answers: a layer field holds the layer's id. */
export type Answers = Record<string, string>;

/** A tool's form filled in: each layer field with a layer of the map — the
 * selected one first, then the others in turn, so "count per region" starts
 * on two different layers — and every other field with its default. */
export function initialAnswers(tool: ToolItem, layers: MapLayer[], active: string | null): Answers {
  const answers: Answers = {};
  const ids = layers.map((l) => l.id ?? "").filter((id) => id !== "");
  const order = active && ids.includes(active) ? [active, ...ids.filter((id) => id !== active).reverse()] : [...ids].reverse();
  let next = 0;
  for (const p of toolParams(tool)) {
    if (p.kind === "layer") {
      answers[p.name] = order[next] ?? order[0] ?? "";
      next += 1;
    } else if (p.default !== undefined && p.default !== null) {
      answers[p.name] = String(p.default);
    } else if (p.kind === "choice") {
      answers[p.name] = p.options?.[0]?.value ?? "";
    } else {
      answers[p.name] = "";
    }
  }
  return answers;
}

/** The columns a column field offers: the picked layer's dataset's, of the
 * types it asks for (`number` a number that is not a key). */
export function columnChoices(
  param: ToolParam,
  answers: Answers,
  layers: MapLayer[],
  columnsOf: (dataset: string) => ColumnInfo[],
): ColumnInfo[] {
  const layer = layers.find((l) => l.id === answers[param.of ?? ""]);
  if (!layer) return [];
  const all = columnsOf(layer.dataset).filter((c) => c.type !== "geometry");
  const types = param.types ?? [];
  if (types.length === 0) return all;
  return all.filter((c) => types.some((t) => (t === "number" ? isMeasure(c) : c.type === t)));
}

/** What is missing before the tool can run, as the field's label; `null`
 * when nothing is. */
export function missingAnswer(tool: ToolItem, answers: Answers): string | null {
  for (const p of toolParams(tool)) {
    if (p.optional) continue;
    if ((answers[p.name] ?? "").trim() === "") return p.label;
  }
  return null;
}

/** The answers as `runMapTool` takes them: a layer as the layer, a number as
 * a number. */
export function runParams(tool: ToolItem, answers: Answers, layers: MapLayer[]): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const p of toolParams(tool)) {
    const raw = answers[p.name] ?? "";
    if (p.kind === "layer") {
      const layer = layers.find((l) => l.id === raw);
      if (layer) out[p.name] = layer;
    } else if (p.kind === "number") {
      if (raw.trim() !== "") out[p.name] = Number(raw);
    } else if (raw !== "") {
      out[p.name] = raw;
    }
  }
  return out;
}
