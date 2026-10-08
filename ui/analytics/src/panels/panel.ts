// Panels and dragging them (analytics TODO A4.2–A4.3; the goals document's
// "Panels").
//
// A panel is an elementary output — a plot, a summary table, the explorer's
// tests with their plot, text, a fit's table, a plugin's own kind — stored as
// what makes it (`sc_analytics::panel`): `{ id, title?, kind, content }`. A
// plot panel is its spec, so a panel shows its dataset as it is now wherever
// it is put, and dragging one copies the spec.
//
// **Drag and drop is always a copy.** The drag carries the panel's JSON
// (`PANEL_MIME`), written when the drag starts, so nothing the source does
// afterwards reaches the copy; the sink reads it with `readPanelDrag`, which
// gives it an identity of its own. Nothing is ever taken from the source.

import type { Translate } from "../datasets/ops";
import type { ModelOutput } from "../models/outputs";
import type { PlotSpec, TableSpec } from "../plot/spec";
import type { TestSpec } from "../explorer/tests";

/** The drag data type a panel travels as. */
export const PANEL_MIME = "application/x-feldspar-panel";

/** A panel's kind and what makes it. */
export type PanelBody =
  | { kind: "plot"; content: { spec: PlotSpec } }
  | { kind: "summary_table"; content: { spec: TableSpec } }
  | { kind: "test_result"; content: { tests: TestSpec; plot?: PlotSpec } }
  | { kind: "text"; content: { markdown: string } }
  | { kind: "fit_table"; content: { fit: string; output: string } }
  | { kind: "custom"; content: { renderer: string; config?: unknown } };

/** One panel. */
export type Panel = { id: string; title?: string } & PanelBody;

export type PanelKind = PanelBody["kind"];

export const PANEL_KINDS: PanelKind[] = ["plot", "summary_table", "test_result", "text", "fit_table", "custom"];

/** A new panel's id. */
export function newPanelId(): string {
  return crypto.randomUUID();
}

/** A new panel. */
export function makePanel(body: PanelBody, title?: string): Panel {
  const panel = { id: newPanelId(), ...body } as Panel;
  if (title && title.trim() !== "") panel.title = title.trim();
  return panel;
}

/** A copy with an identity of its own, sharing nothing with the original. */
export function copyPanel(panel: Panel): Panel {
  return { ...(JSON.parse(JSON.stringify(panel)) as Panel), id: newPanelId() };
}

function isObject(v: unknown): v is Record<string, unknown> {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}

/** A panel read from anything — a drag's data, a stored state — or `null`
 * when it is not one. Checks the shape the kind needs; whether a spec draws
 * is the server's to say when it renders. */
export function readPanel(raw: unknown): Panel | null {
  if (!isObject(raw) || typeof raw.id !== "string" || !isObject(raw.content)) return null;
  const c = raw.content;
  const ok = (() => {
    switch (raw.kind) {
      case "plot":
      case "summary_table":
        return isObject(c.spec) && isObject(c.spec.data);
      case "test_result":
        return isObject(c.tests) && isObject(c.tests.data) && (c.plot === undefined || isObject(c.plot));
      case "text":
        return typeof c.markdown === "string";
      case "fit_table":
        return typeof c.fit === "string" && typeof c.output === "string";
      case "custom":
        return typeof c.renderer === "string";
      default:
        return false;
    }
  })();
  if (!ok) return null;
  const panel = { id: raw.id, kind: raw.kind, content: c } as Panel;
  if (typeof raw.title === "string" && raw.title !== "") panel.title = raw.title;
  return panel;
}

// --- dragging -----------------------------------------------------------------

/** The part of a `DataTransfer` a drag needs: a browser's, or a test's. */
export type Transfer = {
  types: readonly string[];
  setData: (type: string, data: string) => void;
  getData: (type: string) => string;
  effectAllowed?: DataTransfer["effectAllowed"];
};

/** Start dragging `panel`: its JSON as it is now, and its title as text for
 * anything outside the Analytics UI. */
export function setPanelDrag(transfer: Transfer, panel: Panel): void {
  transfer.setData(PANEL_MIME, JSON.stringify(panel));
  transfer.setData("text/plain", panel.title ?? panel.kind);
  transfer.effectAllowed = "copy";
}

/** Whether a drag carries a panel. Only the type can be read before the drop. */
export function carriesPanel(transfer: Pick<Transfer, "types">): boolean {
  return Array.from(transfer.types).includes(PANEL_MIME);
}

/** The panel a drop carries, as a copy of its own — or `null`. */
export function readPanelDrag(transfer: Pick<Transfer, "getData">): Panel | null {
  try {
    const panel = readPanel(JSON.parse(transfer.getData(PANEL_MIME)));
    return panel ? copyPanel(panel) : null;
  } catch {
    return null;
  }
}

// --- the sources -----------------------------------------------------------------

/** What the explorer is showing, enough to make a panel of it. */
export type ExplorerOutput = {
  view: "plot" | "table";
  /** The plot drawn, with the layers panel's changes. */
  spec: PlotSpec | null;
  /** The summary table of the drop zones. */
  table: TableSpec | null;
  /** The tests beside the plot, when they are shown. */
  tests: TestSpec | null;
};

/** The explorer's current output as a panel (A4.3): the plot, with its tests
 * when they are shown (the two are one panel), or the summary table. `null`
 * while there is nothing drawn to drag. */
export function explorerPanel(output: ExplorerOutput, title?: string): Panel | null {
  if (output.view === "table") {
    return output.table ? makePanel({ kind: "summary_table", content: { spec: output.table } }, title) : null;
  }
  if (!output.spec) return null;
  if (output.tests) {
    return makePanel({ kind: "test_result", content: { tests: output.tests, plot: output.spec } }, title);
  }
  return makePanel({ kind: "plot", content: { spec: output.spec } }, title);
}

/** A title for the explorer's output: the columns on Y by the one on X, and
 * the dataset. */
export function explorerTitle(y: string[], x: string | undefined, dataset: string | undefined, t: Translate): string {
  const ys = y.join(", ");
  const what = ys !== "" && x ? t("{y} by {x}", { y: ys, x }) : ys !== "" ? ys : (x ?? "");
  if (!dataset) return what;
  return what === "" ? dataset : t("{what} — {dataset}", { what, dataset });
}

/** One of a fit's outputs as a panel (A4.3): a plot is its spec over the fit's
 * output data; a table names the fit and the output, since what fills it is
 * the fit's, not a dataset's. `null` for an output that cannot be shown. */
export function outputPanel(output: ModelOutput, fit: string, model: string | undefined, t: Translate): Panel | null {
  if (output.error) return null;
  const title = model ? t("{what} — {dataset}", { what: output.label, dataset: model }) : output.label;
  if (output.kind === "plot") {
    return output.spec ? makePanel({ kind: "plot", content: { spec: output.spec } }, title) : null;
  }
  return makePanel({ kind: "fit_table", content: { fit, output: output.name } }, title);
}
