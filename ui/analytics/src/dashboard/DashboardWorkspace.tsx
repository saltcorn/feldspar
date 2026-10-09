// The Dashboard workspace (analytics TODO A6.1–A6.2): tiles of panels on a
// grid, arranged by dragging and resized by their corners.
//
// Panels arrive by dropping, from anything that drags one: the Data
// explorer, the Map workspace, the model editor, a report's blocks, another
// dashboard's tiles. Stat cards and text are added from the **Add** menu. A
// tile is moved by dragging its grip to the cell its top-left corner should
// take, and resized by its bottom-right corner; the tiles in the way are
// pushed down and every tile then rises as far as it can (`layout.ts`). Its
// menu does the same by steps, from the keyboard.
//
// Unlike a report's, a dashboard's panels are interactive — tooltips, legend
// toggles, a map that pans — and follow the screen's colour scheme. On a
// narrow screen the tiles stack in one column; moving and resizing wait for a
// wider one.

import { useCallback, useLayoutEffect, useMemo, useRef, useState, type DragEvent, type KeyboardEvent, type PointerEvent } from "react";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";

import type { Translate } from "../datasets/ops";
import { T, useT } from "../i18n";
import { carriesPanel, makePanel, newPanelId, readPanelDrag, type Panel, type PanelKind } from "../panels/panel";
import { PanelView } from "../panels/PanelView";
import type { WorkspaceProps, WorkspaceState } from "../workspaces/WorkspaceFrame";
import {
  COLUMNS,
  GAP_PX,
  NARROW_PX,
  ROW_PX,
  addTile,
  carriesTile,
  cellAt,
  clamp,
  columnWidth,
  defaultSize,
  dropInto,
  editTile,
  moveTile,
  readDashboard,
  readTileDrag,
  removeTile,
  resizeTile,
  rowsOf,
  setTileDrag,
  spanOf,
  stacked,
  type DashboardState,
  type Grid,
  type Rect,
  type Tile,
} from "./layout";
import { StatCardForm } from "./StatCardForm";

/** Rows left free below the tiles while something is dragged, to drop into. */
const SPARE_ROWS = 3;

/** What is being edited in a dialog: a new or existing stat card, some text. */
type Editing = { kind: "stat_card"; id: string | null } | { kind: "text"; id: string };

/** A tile dragged from this dashboard: its size, and where in it the pointer
 * took hold, so the drop puts its corner where the corner was shown. */
type Held = { id: string; w: number; h: number; dx: number; dy: number };

export function DashboardWorkspace({ state: raw, setState }: WorkspaceProps) {
  const { t } = useT();
  const dashboard = useMemo(() => readDashboard(raw), [raw]);
  const update = useCallback(
    (change: (d: DashboardState) => DashboardState) =>
      setState((current) => ({ ...current, ...change(readDashboard(current)) }) as WorkspaceState),
    [setState],
  );
  // This open copy of the dashboard: its own tile dropped back moves; a tile
  // dropped anywhere else is copied.
  const self = useMemo(() => newPanelId(), []);

  // The grid's width, for the cells under the pointer and the narrow layout.
  const grid = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = grid.current;
    if (!el) return;
    const read = () => setWidth(el.clientWidth);
    read();
    const observer = new ResizeObserver(read);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  const narrow = width > 0 && width < NARROW_PX;
  const geometry: Grid = { width, columns: COLUMNS };

  const held = useRef<Held | null>(null);
  const [target, setTarget] = useState<Rect | null>(null);
  const [dragging, setDragging] = useState(false);
  const [resizing, setResizing] = useState<{ id: string; w: number; h: number } | null>(null);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);

  // What is drawn: the stored layout, with a resize under way, stacked when
  // narrow.
  const shown = useMemo(() => {
    const tiles = resizing ? resizeTile(dashboard, resizing.id, resizing).tiles : dashboard.tiles;
    return narrow ? stacked(tiles) : tiles;
  }, [dashboard, resizing, narrow]);
  const rows = Math.max(rowsOf(shown), target ? target.y + target.h : 0) + (dragging ? SPARE_ROWS : 0);

  const accepts = (e: DragEvent) => carriesTile(e.dataTransfer) || carriesPanel(e.dataTransfer);
  const over = (e: DragEvent) => {
    if (!accepts(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = held.current ? "move" : "copy";
    setDragging(true);
    const el = grid.current;
    if (narrow || !el) {
      setTarget(null);
      return;
    }
    const box = el.getBoundingClientRect();
    const h = held.current;
    const size = h ? { w: h.w, h: h.h } : defaultSize(undefined);
    // A held tile's corner goes to the nearest cell; anything else to the
    // cell under the pointer.
    const half = h ? { x: (columnWidth(geometry) + GAP_PX) / 2, y: (ROW_PX + GAP_PX) / 2 } : { x: 0, y: 0 };
    const cell = cellAt(geometry, e.clientX - box.left - (h?.dx ?? 0) + half.x, e.clientY - box.top - (h?.dy ?? 0) + half.y);
    const at = clamp({ ...cell, ...size });
    setTarget((old) => (old && old.x === at.x && old.y === at.y && old.w === at.w && old.h === at.h ? old : at));
  };
  const finish = () => {
    held.current = null;
    setTarget(null);
    setDragging(false);
  };
  const drop = (e: DragEvent) => {
    if (!accepts(e)) return;
    e.preventDefault();
    const tile = readTileDrag(e.dataTransfer);
    const panel = tile ? null : readPanelDrag(e.dataTransfer);
    const at = target ? { x: target.x, y: target.y } : undefined;
    update((d) => dropInto(d, self, { tile, panel }, narrow ? undefined : at) ?? d);
    finish();
  };

  const startResize = (tile: Tile, el: HTMLElement | null, e: PointerEvent<HTMLElement>) => {
    if (!el || narrow) return;
    e.preventDefault();
    e.stopPropagation();
    const handle = e.currentTarget;
    handle.setPointerCapture(e.pointerId);
    const box = el.getBoundingClientRect();
    const start = { x: e.clientX, y: e.clientY };
    const span = (ev: globalThis.PointerEvent) => spanOf(geometry, box.width + ev.clientX - start.x, box.height + ev.clientY - start.y);
    const move = (ev: globalThis.PointerEvent) => setResizing({ id: tile.id, ...span(ev) });
    const stop = (ev: globalThis.PointerEvent, commit: boolean) => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      handle.removeEventListener("pointercancel", cancel);
      setResizing(null);
      if (commit) update((d) => resizeTile(d, tile.id, span(ev)));
    };
    const up = (ev: globalThis.PointerEvent) => stop(ev, true);
    const cancel = (ev: globalThis.PointerEvent) => stop(ev, false);
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
    handle.addEventListener("pointercancel", cancel);
  };

  const addText = () => {
    const panel = makePanel({ kind: "text", content: { markdown: "" } });
    update((d) => addTile(d, panel));
    // After the menu has closed: closing gives its toggle the focus back.
    setTimeout(() => setEditing({ kind: "text", id: panel.id }), 0);
  };
  const saveCard = (panel: Panel) => {
    const id = editing?.id;
    update((d) => (id ? editTile(d, id, panel) : addTile(d, panel)));
    setEditing(null);
  };
  const editingTile = editing?.id ? dashboard.tiles.find((t) => t.id === editing.id) : undefined;

  return (
    <div className="an-dashboard">
      <div className="an-dashboard-toolbar">
        <Dropdown>
          <Dropdown.Toggle size="sm" variant="outline-secondary">
            + {t("Add")}
          </Dropdown.Toggle>
          <Dropdown.Menu>
            <Dropdown.Item onClick={() => setTimeout(() => setEditing({ kind: "stat_card", id: null }), 0)}>
              {t("Stat card")}
            </Dropdown.Item>
            <Dropdown.Item onClick={addText}>{t("Text")}</Dropdown.Item>
          </Dropdown.Menu>
        </Dropdown>
        <span className="text-secondary small">
          {dashboard.tiles.length === 1 ? t("1 tile") : t("{count} tiles", { count: dashboard.tiles.length })}
        </span>
        {narrow && dashboard.tiles.length > 0 && (
          <span className="text-secondary small ms-auto">
            <T text="Narrow screen: tiles are stacked. Widen it to arrange them." />
          </span>
        )}
      </div>
      <div className="an-dashboard-desk">
        {dashboard.tiles.length === 0 && !dragging && (
          <p className="text-secondary an-dashboard-empty">
            <T text="Drag plots, tables and maps here from the Data explorer, the Map workspace, the model editor or a report — split the view to have both on the screen — or add a stat card or text from the Add menu." />
          </p>
        )}
        <div
          ref={grid}
          className={dragging ? "an-dashboard-grid an-dragging" : "an-dashboard-grid"}
          style={{
            gridTemplateColumns: `repeat(${COLUMNS}, minmax(0, 1fr))`,
            gridTemplateRows: `repeat(${Math.max(rows, 1)}, ${ROW_PX}px)`,
            gap: `${GAP_PX}px`,
            minHeight: dashboard.tiles.length === 0 ? `${6 * (ROW_PX + GAP_PX)}px` : undefined,
          }}
          onDragOver={over}
          onDragLeave={(e) => {
            if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
              setTarget(null);
              setDragging(false);
            }
          }}
          onDrop={drop}
        >
          {shown.map((tile) => (
            <TileView
              key={tile.id}
              tile={tile}
              narrow={narrow}
              resizing={resizing?.id === tile.id}
              renaming={renaming === tile.id}
              setRenaming={(on) => setRenaming(on ? tile.id : null)}
              onDragStart={(e, el) => {
                setTileDrag(e.dataTransfer, self, tile);
                const box = el.getBoundingClientRect();
                const dx = e.clientX - box.left;
                const dy = e.clientY - box.top;
                held.current = { id: tile.id, w: tile.w, h: tile.h, dx, dy };
                e.dataTransfer.setDragImage(el, dx, dy);
              }}
              onDragEnd={finish}
              startResize={startResize}
              update={update}
              edit={() => {
                if (tile.panel.kind === "stat_card" || tile.panel.kind === "text") {
                  setEditing({ kind: tile.panel.kind, id: tile.id });
                }
              }}
            />
          ))}
          {target && (
            <div
              className="an-tile-target"
              aria-hidden
              style={{ gridColumn: `${target.x + 1} / span ${target.w}`, gridRow: `${target.y + 1} / span ${target.h}` }}
            />
          )}
        </div>
      </div>
      {editing?.kind === "stat_card" && (
        <StatCardForm
          panel={editingTile?.panel.kind === "stat_card" ? editingTile.panel : null}
          onSave={saveCard}
          onCancel={() => setEditing(null)}
        />
      )}
      {editing?.kind === "text" && editingTile?.panel.kind === "text" && (
        <TextForm
          markdown={editingTile.panel.content.markdown}
          onSave={(markdown) => {
            update((d) => editTile(d, editing.id, { ...editingTile.panel, content: { markdown } } as Panel));
            setEditing(null);
          }}
          onCancel={() => setEditing(null)}
        />
      )}
    </div>
  );
}

/** A kind's name, for a tile without a title. */
export function kindName(kind: PanelKind, t: Translate): string {
  switch (kind) {
    case "plot":
      return t("Plot");
    case "summary_table":
      return t("Summary table");
    case "test_result":
      return t("Tests");
    case "text":
      return t("Text");
    case "fit_table":
      return t("Table of a fit");
    case "map":
      return t("Map");
    case "stat_card":
      return t("Stat card");
    case "custom":
      return t("Panel");
  }
}

function TileView({
  tile,
  narrow,
  resizing,
  renaming,
  setRenaming,
  onDragStart,
  onDragEnd,
  startResize,
  update,
  edit,
}: {
  tile: Tile;
  narrow: boolean;
  resizing: boolean;
  renaming: boolean;
  setRenaming: (on: boolean) => void;
  onDragStart: (e: DragEvent, el: HTMLElement) => void;
  onDragEnd: () => void;
  startResize: (tile: Tile, el: HTMLElement | null, e: PointerEvent<HTMLElement>) => void;
  update: (change: (d: DashboardState) => DashboardState) => void;
  edit: () => void;
}) {
  const { t } = useT();
  const box = useRef<HTMLElement>(null);
  const name = tile.panel.title ?? kindName(tile.panel.kind, t);
  const editable = tile.panel.kind === "stat_card" || tile.panel.kind === "text";
  const step = (change: (d: DashboardState) => DashboardState) => () => update(change);
  const by = (dx: number, dy: number) => step((d) => moveTile(d, tile.id, { x: tile.x + dx, y: tile.y + dy }));
  const grow = (dw: number, dh: number) => step((d) => resizeTile(d, tile.id, { w: tile.w + dw, h: tile.h + dh }));
  const rename = (title: string) =>
    update((d) => editTile(d, tile.id, { ...tile.panel, title: title.trim() === "" ? undefined : title.trim() } as Panel));
  const classes = ["an-tile", `an-tile-${tile.panel.kind.replace("_", "-")}`];
  if (resizing) classes.push("an-resizing");
  if (tile.panel.kind === "text" && tile.panel.content.markdown.trim() === "") classes.push("an-tile-empty");

  return (
    <section
      ref={box}
      className={classes.join(" ")}
      style={{ gridColumn: `${tile.x + 1} / span ${tile.w}`, gridRow: `${tile.y + 1} / span ${tile.h}` }}
      data-panel-kind={tile.panel.kind}
      aria-label={name}
    >
      <header className="an-tile-head">
        <span
          className="an-block-grip"
          draggable
          role="button"
          tabIndex={-1}
          title={t("Drag to move, or into a report or another dashboard")}
          aria-label={t("Drag {name}", { name })}
          onDragStart={(e) => box.current && onDragStart(e, box.current)}
          onDragEnd={onDragEnd}
        >
          ⠿
        </span>
        {renaming ? (
          <Form.Control
            size="sm"
            autoFocus
            defaultValue={tile.panel.title ?? ""}
            placeholder={kindName(tile.panel.kind, t)}
            aria-label={t("Title")}
            onBlur={(e) => {
              rename(e.target.value);
              setRenaming(false);
            }}
            onKeyDown={(e: KeyboardEvent<HTMLInputElement>) => {
              if (e.key === "Enter") e.currentTarget.blur();
              if (e.key === "Escape") setRenaming(false);
            }}
          />
        ) : (
          <span className="an-tile-title" title={name} onDoubleClick={() => setRenaming(true)}>
            {name}
          </span>
        )}
        <Dropdown align="end" className="ms-auto">
          <Dropdown.Toggle size="sm" variant="link" className="an-block-menu p-0 px-1" aria-label={t("Options for {name}", { name })}>
            ⋯
          </Dropdown.Toggle>
          <Dropdown.Menu>
            {editable && <Dropdown.Item onClick={() => setTimeout(edit, 0)}>{t("Edit")}</Dropdown.Item>}
            <Dropdown.Item onClick={() => setTimeout(() => setRenaming(true), 0)}>{t("Rename")}</Dropdown.Item>
            {!narrow && (
              <>
                <Dropdown.Divider />
                <Dropdown.Item disabled={tile.y === 0} onClick={by(0, -1)}>
                  {t("Move up")}
                </Dropdown.Item>
                <Dropdown.Item onClick={by(0, 1)}>{t("Move down")}</Dropdown.Item>
                <Dropdown.Item disabled={tile.x === 0} onClick={by(-1, 0)}>
                  {t("Move left")}
                </Dropdown.Item>
                <Dropdown.Item disabled={tile.x + tile.w >= COLUMNS} onClick={by(1, 0)}>
                  {t("Move right")}
                </Dropdown.Item>
                <Dropdown.Divider />
                <Dropdown.Item disabled={tile.x + tile.w >= COLUMNS} onClick={grow(1, 0)}>
                  {t("Wider")}
                </Dropdown.Item>
                <Dropdown.Item disabled={tile.w <= 2} onClick={grow(-1, 0)}>
                  {t("Narrower")}
                </Dropdown.Item>
                <Dropdown.Item onClick={grow(0, 1)}>{t("Taller")}</Dropdown.Item>
                <Dropdown.Item disabled={tile.h <= 2} onClick={grow(0, -1)}>
                  {t("Shorter")}
                </Dropdown.Item>
              </>
            )}
          </Dropdown.Menu>
        </Dropdown>
        <Button
          size="sm"
          variant="link"
          className="p-0 px-1 text-secondary"
          aria-label={t("Remove {name} from the dashboard", { name })}
          onClick={() => update((d) => removeTile(d, tile.id))}
        >
          ×
        </Button>
      </header>
      <div className="an-tile-body" onDoubleClick={editable ? edit : undefined}>
        {tile.panel.kind === "text" && tile.panel.content.markdown.trim() === "" ? (
          <span className="an-placeholder">{t("Double-click to write text, in Markdown.")}</span>
        ) : (
          <PanelView panel={tile.panel} />
        )}
      </div>
      {!narrow && (
        <span
          className="an-tile-resize"
          role="separator"
          aria-label={t("Resize {name}", { name })}
          title={t("Drag to resize")}
          onPointerDown={(e) => startResize(tile, box.current, e)}
        />
      )}
    </section>
  );
}

function TextForm({ markdown, onSave, onCancel }: { markdown: string; onSave: (markdown: string) => void; onCancel: () => void }) {
  const { t } = useT();
  const [text, setText] = useState(markdown);
  return (
    <Modal show onHide={onCancel}>
      <Modal.Header closeButton>
        <Modal.Title>{t("Text")}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <Form.Control
          as="textarea"
          autoFocus
          rows={Math.max(6, text.split("\n").length + 1)}
          value={text}
          placeholder={t("Write in Markdown: **bold**, *italic*, - a list, [a link](https://…)")}
          aria-label={t("Text, in Markdown")}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) onSave(text);
          }}
        />
        <div className="form-text">
          <T text="Markdown. Ctrl+Enter to save." />
        </div>
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button variant="primary" onClick={() => onSave(text)}>
          <T text="Save" />
        </Button>
      </Modal.Footer>
    </Modal>
  );
}
