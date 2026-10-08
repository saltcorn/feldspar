// The Report workspace's state (analytics TODO A4.3): its blocks, in order.
//
// A4.3 brings the report as the place panels are dropped, so its blocks are
// panels for now; A4.4 adds headings, Markdown text and page breaks, and the
// page's size. The server reads the same shape to find a report's panels for
// the usage index (`sc_analytics::panel::panels_in_state`): each block whose
// `kind` is `panel` holds one under `panel`.

import { readPanel, type Panel } from "../panels/panel";

/** One block of a report. */
export type Block = { id: string; kind: "panel"; panel: Panel };

/** A report's state. */
export type ReportState = { blocks: Block[] };

/** The state a workspace stored, read leniently: what is not a block is
 * dropped rather than failing the report. */
export function readReport(raw: Record<string, unknown>): ReportState {
  const blocks = Array.isArray(raw.blocks) ? raw.blocks : [];
  return {
    blocks: blocks.flatMap((b): Block[] => {
      if (!b || typeof b !== "object") return [];
      const o = b as Record<string, unknown>;
      const panel = o.kind === "panel" ? readPanel(o.panel) : null;
      return panel && typeof o.id === "string" ? [{ id: o.id, kind: "panel", panel }] : [];
    }),
  };
}

/** The report with `panel` added as a block before `before` (a block's id),
 * or at the end. The panel is the sink's own copy already (`readPanelDrag`). */
export function addPanel(state: ReportState, panel: Panel, before?: string): ReportState {
  const block: Block = { id: panel.id, kind: "panel", panel };
  const at = before ? state.blocks.findIndex((b) => b.id === before) : -1;
  const blocks = [...state.blocks];
  blocks.splice(at === -1 ? blocks.length : at, 0, block);
  return { ...state, blocks };
}

/** The report without the block `id`. */
export function removeBlock(state: ReportState, id: string): ReportState {
  return { ...state, blocks: state.blocks.filter((b) => b.id !== id) };
}
