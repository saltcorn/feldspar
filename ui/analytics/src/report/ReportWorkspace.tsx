// The Report workspace (analytics TODO A4.3–A4.5): a document of blocks —
// panels, headings, Markdown text and page breaks — on a page that prints.
//
// Panels arrive by dropping: from the Data explorer or the model editor on the
// other side of a split view, or from another report. Headings, text and page
// breaks are added from the **Add** menu, or above a block from its own menu.
// A block is moved by dragging its grip (or Move up / Move down), and dragged
// into another report it is copied there.
//
// The page is drawn at the paper's width, in millimetres, so the screen shows
// what will print; dashed markers show where the pages will break
// (`paginate`, over the blocks' measured heights). Panels are drawn still —
// no tooltips or highlighting — in vector graphics, on white paper whatever
// the screen's scheme. **Export PDF** opens the print dialog (`printReport`).

import {
  Fragment,
  createElement,
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent,
  type KeyboardEvent,
} from "react";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";

import { T, useT } from "../i18n";
import { Markdown } from "../panels/Markdown";
import { carriesPanel, newPanelId, readPanelDrag } from "../panels/panel";
import { PanelView, type PanelLook } from "../panels/PanelView";
import type { WorkspaceProps, WorkspaceState } from "../workspaces/WorkspaceFrame";
import { MARGIN_MM, PAGE_SIZES, PX_PER_MM, paginate, paper, printable, type Orientation, type PageSize } from "./pages";
import { printReport } from "./print";
import {
  carriesBlock,
  dropInto,
  editBlock,
  insertBlock,
  moveBy,
  newBlock,
  readBlockDrag,
  readReport,
  removeBlock,
  setBlockDrag,
  setPage,
  type Block,
  type HeadingLevel,
  type ReportState,
} from "./state";

/** How a report draws its panels. */
const LOOK: PanelLook = { still: true, renderer: "svg", theme: "light" };

type Translate = ReturnType<typeof useT>["t"];

export function ReportWorkspace({ state: raw, setState, name }: WorkspaceProps) {
  const { t } = useT();
  const report = useMemo(() => readReport(raw), [raw]);
  const update = useCallback(
    (change: (r: ReportState) => ReportState) =>
      setState((current) => ({ ...current, ...change(readReport(current)) }) as WorkspaceState),
    [setState],
  );
  // This open copy of the report: a block dragged from it and dropped back
  // into it moves; dropped anywhere else, it is copied.
  const self = useMemo(() => newPanelId(), []);
  // Where a drop would go: before a block, or "end".
  const [target, setTarget] = useState<string | null>(null);
  // The heading or text block being written.
  const [editing, setEditing] = useState<string | null>(null);
  const [printing, setPrinting] = useState(false);
  const page = useRef<HTMLDivElement>(null);

  // The blocks' heights as drawn, and the pages they make.
  const [heights, setHeights] = useState<Record<string, number>>({});
  const measure = useCallback(
    (id: string, height: number) => setHeights((h) => (h[id] === height ? h : { ...h, [id]: height })),
    [],
  );
  const sheet = paper(report.page);
  const inside = printable(report.page);
  const pagination = useMemo(
    () =>
      paginate(
        report.blocks.map((b) => ({ id: b.id, kind: b.kind, height: heights[b.id] ?? 0 })),
        inside.height * PX_PER_MM,
      ),
    [report.blocks, heights, inside.height],
  );
  // The page each block begins, when it is the first thing on that page.
  const startsPage = useMemo(() => {
    const starts: Record<string, number> = {};
    pagination.pages.forEach((blocks, i) => {
      const first = blocks[0];
      if (i > 0 && first && pagination.pageOf[first] === i + 1) starts[first] = i + 1;
    });
    return starts;
  }, [pagination]);

  const accepts = (e: DragEvent) => carriesBlock(e.dataTransfer) || carriesPanel(e.dataTransfer);
  const accept = (e: DragEvent, at: string) => {
    if (!accepts(e)) return;
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = carriesBlock(e.dataTransfer) ? "move" : "copy";
    setTarget(at);
  };
  const drop = (e: DragEvent, before?: string) => {
    if (!accepts(e)) return;
    e.preventDefault();
    e.stopPropagation();
    setTarget(null);
    const block = readBlockDrag(e.dataTransfer);
    const panel = block ? null : readPanelDrag(e.dataTransfer);
    update((r) => dropInto(r, self, { block, panel }, before) ?? r);
  };

  const add = (kind: "heading" | "text" | "page_break", before?: string) => {
    const block = newBlock(kind);
    update((r) => insertBlock(r, block, before));
    // After the menu has closed: closing gives its toggle the focus back,
    // which would end the editing as soon as it began.
    if (kind !== "page_break") setTimeout(() => setEditing(block.id), 0);
  };

  const exportPdf = async () => {
    if (!page.current) return;
    setEditing(null);
    setPrinting(true);
    try {
      await printReport(page.current, report.page, name, t("Report"));
    } finally {
      setPrinting(false);
    }
  };

  const pages = pagination.pages.length;
  return (
    <div className="an-report">
      <div className="an-report-toolbar">
        <AddMenu label={t("Add")} onAdd={(kind) => add(kind)} t={t} />
        <Form.Select
          size="sm"
          className="w-auto"
          aria-label={t("Page size")}
          value={report.page.size}
          onChange={(e) => update((r) => setPage(r, { size: e.target.value as PageSize }))}
        >
          {PAGE_SIZES.map((s) => (
            <option key={s} value={s}>
              {sizeName(s, t)}
            </option>
          ))}
        </Form.Select>
        <Form.Select
          size="sm"
          className="w-auto"
          aria-label={t("Orientation")}
          value={report.page.orientation}
          onChange={(e) => update((r) => setPage(r, { orientation: e.target.value as Orientation }))}
        >
          <option value="portrait">{t("Portrait")}</option>
          <option value="landscape">{t("Landscape")}</option>
        </Form.Select>
        <span className="text-secondary small" aria-live="polite">
          {pages === 1 ? t("1 page") : t("{count} pages", { count: pages })}
        </span>
        <Button
          size="sm"
          variant="primary"
          className="ms-auto"
          disabled={printing || report.blocks.length === 0}
          onClick={() => void exportPdf()}
        >
          {printing ? t("Preparing…") : t("Export PDF")}
        </Button>
      </div>
      <div
        className="an-report-desk"
        onDragOver={(e) => accept(e, "end")}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setTarget(null);
        }}
        onDrop={(e) => drop(e)}
      >
        <div
          ref={page}
          className="an-report-page"
          data-bs-theme="light"
          style={{ width: `${inside.width}mm`, padding: `${MARGIN_MM}mm`, minHeight: `${sheet.height}mm` }}
        >
          {report.blocks.length === 0 && (
            <p className="text-secondary an-report-empty">
              <T text="Drag a plot from the Data explorer, or an output from the model editor, into this report. Split the view to have both on the screen. Headings, text and page breaks are in the Add menu." />
            </p>
          )}
          {report.blocks.map((block, i) => (
            <Fragment key={block.id}>
              {startsPage[block.id] && (
                <div className="an-page-marker" aria-hidden>
                  <span>{t("Page {n}", { n: startsPage[block.id] })}</span>
                </div>
              )}
              <BlockView
                block={block}
                first={i === 0}
                last={i === report.blocks.length - 1}
                editing={editing === block.id}
                setEditing={(on) => setEditing(on ? block.id : null)}
                dropTarget={target === block.id}
                onDragOver={(e) => accept(e, block.id)}
                onDrop={(e) => drop(e, block.id)}
                self={self}
                update={update}
                add={(kind) => add(kind, block.id)}
                measure={measure}
              />
            </Fragment>
          ))}
          <div className={target === "end" ? "an-report-end an-drop-before" : "an-report-end"} aria-hidden />
        </div>
      </div>
    </div>
  );
}

function sizeName(size: PageSize, t: Translate): string {
  switch (size) {
    case "Letter":
      return t("US Letter");
    case "Legal":
      return t("US Legal");
    default:
      return size;
  }
}

function AddMenu({
  label,
  onAdd,
  t,
}: {
  label: string;
  onAdd: (kind: "heading" | "text" | "page_break") => void;
  t: Translate;
}) {
  return (
    <Dropdown>
      <Dropdown.Toggle size="sm" variant="outline-secondary">
        + {label}
      </Dropdown.Toggle>
      <Dropdown.Menu>
        <AddItems onAdd={onAdd} t={t} />
      </Dropdown.Menu>
    </Dropdown>
  );
}

function AddItems({ onAdd, t }: { onAdd: (kind: "heading" | "text" | "page_break") => void; t: Translate }) {
  return (
    <>
      <Dropdown.Item onClick={() => onAdd("heading")}>{t("Heading")}</Dropdown.Item>
      <Dropdown.Item onClick={() => onAdd("text")}>{t("Text")}</Dropdown.Item>
      <Dropdown.Item onClick={() => onAdd("page_break")}>{t("Page break")}</Dropdown.Item>
    </>
  );
}

/** What a block is called in its menu's labels. */
function blockName(block: Block, t: Translate): string {
  switch (block.kind) {
    case "panel":
      return block.panel.title ?? t("this panel");
    case "heading":
      return block.text.trim() || t("this heading");
    case "text":
      return t("this text");
    case "page_break":
      return t("this page break");
  }
}

function BlockView({
  block,
  first,
  last,
  editing,
  setEditing,
  dropTarget,
  onDragOver,
  onDrop,
  self,
  update,
  add,
  measure,
}: {
  block: Block;
  first: boolean;
  last: boolean;
  editing: boolean;
  setEditing: (on: boolean) => void;
  dropTarget: boolean;
  onDragOver: (e: DragEvent) => void;
  onDrop: (e: DragEvent) => void;
  self: string;
  update: (change: (r: ReportState) => ReportState) => void;
  add: (kind: "heading" | "text" | "page_break") => void;
  measure: (id: string, height: number) => void;
}) {
  const { t } = useT();
  const box = useRef<HTMLElement>(null);

  // The block's height as it will print: the controls float over it.
  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    const read = () => measure(block.id, el.offsetHeight);
    read();
    const observer = new ResizeObserver(read);
    observer.observe(el);
    return () => observer.disconnect();
  }, [block.id, measure]);

  const name = blockName(block, t);
  const classes = ["an-report-block", `an-block-${block.kind.replace("_", "-")}`];
  if (dropTarget) classes.push("an-drop-before");

  return (
    <section
      ref={box}
      className={classes.join(" ")}
      onDragOver={onDragOver}
      onDrop={onDrop}
      data-block-kind={block.kind}
      data-panel-kind={block.kind === "panel" ? block.panel.kind : undefined}
    >
      <div className="an-report-controls">
        <span
          className="an-block-grip"
          draggable
          role="button"
          tabIndex={-1}
          title={t("Drag to move, or into another report")}
          aria-label={t("Drag {name}", { name })}
          onDragStart={(e) => {
            setBlockDrag(e.dataTransfer, self, block);
            if (box.current) e.dataTransfer.setDragImage(box.current, 16, 16);
          }}
        >
          ⠿
        </span>
        <Dropdown align="end">
          <Dropdown.Toggle size="sm" variant="link" className="an-block-menu p-0 px-1" aria-label={t("Options for {name}", { name })}>
            ⋯
          </Dropdown.Toggle>
          <Dropdown.Menu>
            {(block.kind === "heading" || block.kind === "text") && (
              <Dropdown.Item onClick={() => setEditing(true)}>{t("Edit")}</Dropdown.Item>
            )}
            {block.kind === "heading" &&
              ([1, 2, 3] as HeadingLevel[]).map((level) => (
                <Dropdown.Item
                  key={level}
                  active={block.level === level}
                  onClick={() => update((r) => editBlock(r, block.id, "heading", { level }))}
                >
                  {t("Heading {level}", { level })}
                </Dropdown.Item>
              ))}
            <Dropdown.Header>{t("Insert above")}</Dropdown.Header>
            <AddItems onAdd={add} t={t} />
            <Dropdown.Divider />
            <Dropdown.Item disabled={first} onClick={() => update((r) => moveBy(r, block.id, -1))}>
              {t("Move up")}
            </Dropdown.Item>
            <Dropdown.Item disabled={last} onClick={() => update((r) => moveBy(r, block.id, 1))}>
              {t("Move down")}
            </Dropdown.Item>
          </Dropdown.Menu>
        </Dropdown>
        <Button
          size="sm"
          variant="link"
          className="p-0 px-1 text-secondary an-report-remove"
          aria-label={t("Remove {name} from the report", { name })}
          onClick={() => update((r) => removeBlock(r, block.id))}
        >
          ×
        </Button>
      </div>
      <BlockBody block={block} editing={editing} setEditing={setEditing} update={update} />
    </section>
  );
}

function BlockBody({
  block,
  editing,
  setEditing,
  update,
}: {
  block: Block;
  editing: boolean;
  setEditing: (on: boolean) => void;
  update: (change: (r: ReportState) => ReportState) => void;
}) {
  const { t } = useT();
  const done = (e: KeyboardEvent) => {
    if (e.key === "Escape" || (e.key === "Enter" && (block.kind === "heading" || e.ctrlKey || e.metaKey))) {
      e.preventDefault();
      setEditing(false);
    }
  };
  // Clicking the words starts writing — but a link in them is followed.
  const edit = {
    role: "button",
    tabIndex: 0,
    title: t("Click to edit"),
    onClick: (e: { target: EventTarget }) => {
      if (!(e.target instanceof Element && e.target.closest("a"))) setEditing(true);
    },
    onKeyDown: (e: KeyboardEvent) => {
      if (e.key === "Enter") setEditing(true);
    },
  };

  switch (block.kind) {
    case "panel":
      return (
        <>
          {block.panel.title && <h3 className="h5 an-panel-title">{block.panel.title}</h3>}
          <PanelView panel={block.panel} look={LOOK} />
        </>
      );
    case "heading":
      if (editing) {
        return (
          <Form.Control
            autoFocus
            className={`an-heading-edit an-heading-${block.level}`}
            value={block.text}
            placeholder={t("Heading")}
            aria-label={t("Heading")}
            onChange={(e) => update((r) => editBlock(r, block.id, "heading", { text: e.target.value }))}
            onBlur={() => setEditing(false)}
            onKeyDown={done}
          />
        );
      }
      return createElement(
        `h${block.level}`,
        { className: `an-block-heading-text an-heading-${block.level}`, ...edit },
        block.text.trim() || <span className="an-placeholder">{t("Heading")}</span>,
      );
    case "text":
      if (editing) {
        return (
          <div>
            <Form.Control
              as="textarea"
              autoFocus
              rows={Math.max(4, block.markdown.split("\n").length + 1)}
              value={block.markdown}
              placeholder={t("Write in Markdown: **bold**, *italic*, - a list, [a link](https://…)")}
              aria-label={t("Text, in Markdown")}
              onChange={(e) => update((r) => editBlock(r, block.id, "text", { markdown: e.target.value }))}
              onBlur={() => setEditing(false)}
              onKeyDown={done}
            />
            <div className="form-text">
              <T text="Markdown. Ctrl+Enter or a click outside to finish." />
            </div>
          </div>
        );
      }
      return (
        <div className="an-block-text-body" {...edit}>
          {block.markdown.trim() ? (
            <Markdown source={block.markdown} className="an-markdown" />
          ) : (
            <span className="an-placeholder">{t("Click to write text, in Markdown.")}</span>
          )}
        </div>
      );
    case "page_break":
      return (
        <div className="an-page-break-line">
          <span>{t("Page break")}</span>
        </div>
      );
  }
}
