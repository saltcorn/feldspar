// The Report workspace's state (analytics TODO A4.3–A4.4): a document of
// blocks, in order, and the page it is printed on.
//
// A block is a panel, a heading, Markdown text or a page break. The server
// reads the same shape (`sc_analytics::panel::check_state`): the panel of each
// block whose `kind` is `panel` is what the usage index finds, and a block or
// a page that is not one of these is refused when the state is saved.
//
// **Blocks are dragged too.** A block dragged within its report moves; dragged
// into another report it is copied, as a panel always is. The drag carries the
// block and the report it came from (`BLOCK_MIME`), and — for a panel or a
// text block — the panel as well, so anything that takes panels takes it.

import { copyPanel, newPanelId, readPanel, setPanelDrag, type Panel, type Transfer } from "../panels/panel";
import { DEFAULT_PAGE, PAGE_SIZES, type Page } from "./pages";

export type HeadingLevel = 1 | 2 | 3;

/** One block of a report. */
export type Block =
  | { id: string; kind: "panel"; panel: Panel }
  | { id: string; kind: "heading"; text: string; level: HeadingLevel }
  | { id: string; kind: "text"; markdown: string }
  | { id: string; kind: "page_break" };

export type BlockKind = Block["kind"];

/** A report's state. */
export type ReportState = { blocks: Block[]; page: Page };

function isObject(v: unknown): v is Record<string, unknown> {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}

/** One block read from anything, or `null`. */
export function readBlock(raw: unknown): Block | null {
  if (!isObject(raw) || typeof raw.id !== "string" || raw.id === "") return null;
  const id = raw.id;
  switch (raw.kind) {
    case "panel": {
      const panel = readPanel(raw.panel);
      return panel ? { id, kind: "panel", panel } : null;
    }
    case "heading": {
      const level = raw.level === 1 || raw.level === 3 ? raw.level : 2;
      return { id, kind: "heading", text: typeof raw.text === "string" ? raw.text : "", level };
    }
    case "text":
      return { id, kind: "text", markdown: typeof raw.markdown === "string" ? raw.markdown : "" };
    case "page_break":
      return { id, kind: "page_break" };
    default:
      return null;
  }
}

/** The page stored, or A4 portrait. */
function readPage(raw: unknown): Page {
  if (!isObject(raw)) return DEFAULT_PAGE;
  const size = PAGE_SIZES.find((s) => s === raw.size) ?? DEFAULT_PAGE.size;
  const orientation = raw.orientation === "landscape" ? "landscape" : "portrait";
  return { size, orientation };
}

/** The state a workspace stored, read leniently: what is not a block is
 * dropped rather than failing the report. */
export function readReport(raw: Record<string, unknown>): ReportState {
  const blocks = Array.isArray(raw.blocks) ? raw.blocks : [];
  return {
    blocks: blocks.flatMap((b) => {
      const block = readBlock(b);
      return block ? [block] : [];
    }),
    page: readPage(raw.page),
  };
}

/** A new, empty heading, text block or page break. */
export function newBlock(kind: "heading" | "text" | "page_break"): Block {
  const id = newPanelId();
  switch (kind) {
    case "heading":
      return { id, kind, text: "", level: 2 };
    case "text":
      return { id, kind, markdown: "" };
    case "page_break":
      return { id, kind };
  }
}

/** A copy of a block with an identity of its own (a panel's too). */
export function copyBlock(block: Block): Block {
  if (block.kind === "panel") {
    const panel = copyPanel(block.panel);
    return { id: panel.id, kind: "panel", panel };
  }
  return { ...block, id: newPanelId() };
}

/** The report with `block` inserted before `before` (a block's id), or at
 * the end. */
export function insertBlock(state: ReportState, block: Block, before?: string): ReportState {
  const at = before ? state.blocks.findIndex((b) => b.id === before) : -1;
  const blocks = [...state.blocks];
  blocks.splice(at === -1 ? blocks.length : at, 0, block);
  return { ...state, blocks };
}

/** The report with `panel` added as a block before `before`, or at the end.
 * The panel is the sink's own copy already (`readPanelDrag`). */
export function addPanel(state: ReportState, panel: Panel, before?: string): ReportState {
  return insertBlock(state, { id: panel.id, kind: "panel", panel }, before);
}

/** The report with the block `id` moved to before `before`, or to the end. */
export function moveBlock(state: ReportState, id: string, before?: string): ReportState {
  if (id === before) return state;
  const block = state.blocks.find((b) => b.id === id);
  if (!block) return state;
  return insertBlock({ ...state, blocks: state.blocks.filter((b) => b.id !== id) }, block, before);
}

/** The report with the block `id` one place up (`-1`) or down (`1`). */
export function moveBy(state: ReportState, id: string, by: -1 | 1): ReportState {
  const at = state.blocks.findIndex((b) => b.id === id);
  const to = at + by;
  if (at === -1 || to < 0 || to >= state.blocks.length) return state;
  const blocks = [...state.blocks];
  [blocks[at], blocks[to]] = [blocks[to], blocks[at]];
  return { ...state, blocks };
}

/** The report with the block `id` changed (its id and kind stay). */
export function editBlock<K extends BlockKind>(
  state: ReportState,
  id: string,
  kind: K,
  change: Partial<Omit<Extract<Block, { kind: K }>, "id" | "kind">>,
): ReportState {
  return {
    ...state,
    blocks: state.blocks.map((b) => (b.id === id && b.kind === kind ? ({ ...b, ...change } as Block) : b)),
  };
}

/** The report without the block `id`. */
export function removeBlock(state: ReportState, id: string): ReportState {
  return { ...state, blocks: state.blocks.filter((b) => b.id !== id) };
}

/** The report on another page. */
export function setPage(state: ReportState, page: Partial<Page>): ReportState {
  return { ...state, page: { ...state.page, ...page } };
}

// --- dragging blocks ------------------------------------------------------------

/** The drag data type a report's block travels as. */
export const BLOCK_MIME = "application/x-feldspar-report-block";

/** A dragged block, and the report (one open copy of it) it came from. */
export type BlockDrag = { source: string; block: Block };

/** Start dragging `block` out of the report `source`. A panel or a text
 * block goes as a panel too, for whatever takes panels. */
export function setBlockDrag(transfer: Transfer, source: string, block: Block): void {
  if (block.kind === "panel") setPanelDrag(transfer, block.panel);
  if (block.kind === "text") setPanelDrag(transfer, { id: block.id, kind: "text", content: { markdown: block.markdown } });
  transfer.setData(BLOCK_MIME, JSON.stringify({ source, block }));
  if (block.kind === "heading") transfer.setData("text/plain", block.text);
  transfer.effectAllowed = "copyMove";
}

/** Whether a drag carries a report's block. */
export function carriesBlock(transfer: Pick<Transfer, "types">): boolean {
  return Array.from(transfer.types).includes(BLOCK_MIME);
}

/** The block a drop carries, as it was dragged — or `null`. */
export function readBlockDrag(transfer: Pick<Transfer, "getData">): BlockDrag | null {
  try {
    const raw: unknown = JSON.parse(transfer.getData(BLOCK_MIME));
    if (!isObject(raw) || typeof raw.source !== "string") return null;
    const block = readBlock(raw.block);
    return block ? { source: raw.source, block } : null;
  } catch {
    return null;
  }
}

/** The report after a drop before `before` (or at the end) in the report
 * `self`: its own block moves, another report's block is copied, a panel from
 * anywhere is added. `null` when the drop carries nothing a report takes. */
export function dropInto(
  state: ReportState,
  self: string,
  dropped: { block: BlockDrag | null; panel: Panel | null },
  before?: string,
): ReportState | null {
  if (dropped.block) {
    const { source, block } = dropped.block;
    if (source === self && state.blocks.some((b) => b.id === block.id)) return moveBlock(state, block.id, before);
    return insertBlock(state, copyBlock(block), before);
  }
  return dropped.panel ? addPanel(state, dropped.panel, before) : null;
}
