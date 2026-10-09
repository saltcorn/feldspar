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
//
// **Cross-filtering** (A6.3–A6.6, `filters.ts`). A click on a bar or a map's
// feature, or a range brushed along a plot's axis, selects; every other tile
// is drawn again with the selection as a condition, which the server applies
// to the tiles on the same dataset and, through foreign keys, to the tiles on
// datasets that refer to the same table. A tile with a drill path (`drill.ts`)
// goes down a level instead, with a breadcrumb back. The filter bar shows
// what is filtering, removes it, and adds filters of the dashboard's own; the
// dashboard can refresh itself every few minutes.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type DragEvent, type KeyboardEvent, type PointerEvent } from "react";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";

import { api } from "../api";
import type { Translate } from "../datasets/ops";
import { T, useT } from "../i18n";
import { useChanges } from "../panes";
import { carriesPanel, makePanel, newPanelId, readPanelDrag, type Panel, type PanelKind } from "../panels/panel";
import { FilteredBadge, PanelView, type Applied, type PanelPick } from "../panels/PanelView";
import type { PickHow } from "../plot/PlotView";
import { drillConditions, drillDataset, drillDown, drillSpec, drilledPanel } from "./drill";
import { DrillForm } from "./DrillForm";
import { FilterBar, FilterForm } from "./FilterBar";
import {
  MAX_FILTERS,
  REFRESH_CHOICES,
  conditionsFor,
  datasetsOf,
  select,
  selectionOf,
  valueText,
  type Condition,
  type Selected,
} from "./filters";
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
  setDrill,
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

/** What is being edited in a dialog: a new or existing stat card, some text,
 * a tile's drill path, a new filter of the dashboard's own. */
type Editing =
  | { kind: "stat_card"; id: string | null }
  | { kind: "text"; id: string }
  | { kind: "drill"; id: string }
  | { kind: "filter" };

/** A refresh interval in words. */
function every(seconds: number, t: Translate): string {
  if (seconds === 0) return t("Never");
  if (seconds < 60) return t("Every {count} s", { count: seconds });
  if (seconds < 3600) return t("Every {count} min", { count: seconds / 60 });
  return t("Every hour");
}

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

  // What is filtering the tiles: selections (not saved), the values picked
  // down each tile's drill path (not saved), the dashboard's own filters.
  const [selections, setSelections] = useState<Selected[]>([]);
  const [drilled, setDrilled] = useState<Record<string, unknown[]>>({});
  const filters = useMemo(() => dashboard.filters ?? [], [dashboard.filters]);
  // A selection or a drill-down of a tile that has gone goes with it.
  const ids = dashboard.tiles.map((tile) => tile.id).join(",");
  useEffect(() => {
    const here = new Set(ids.split(","));
    setSelections((s) => (s.every((x) => here.has(x.source)) ? s : s.filter((x) => here.has(x.source))));
    setDrilled((d) => (Object.keys(d).every((k) => here.has(k)) ? d : Object.fromEntries(Object.entries(d).filter(([k]) => here.has(k)))));
  }, [ids]);
  const setFilters = (change: (f: Condition[]) => Condition[]) =>
    update((d) => {
      const next = change(d.filters ?? []);
      const out: DashboardState = { ...d, filters: next };
      if (next.length === 0) delete out.filters;
      return out;
    });

  // Datasets' names, for the filter bar.
  const [names, setNames] = useState<Record<string, string>>({});
  const [namesVersion, setNamesVersion] = useState(0);
  useChanges(["dataset"], () => setNamesVersion((v) => v + 1));
  useEffect(() => {
    let live = true;
    api
      .listDatasets()
      .then((ds) => live && setNames(Object.fromEntries(ds.map((d) => [d.id, d.name]))))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, [namesVersion]);

  // Refreshing: every so often, and on demand.
  const [tick, setTick] = useState(0);
  const refresh = dashboard.refresh ?? 0;
  useEffect(() => {
    if (refresh <= 0) return;
    const timer = window.setInterval(() => setTick((n) => n + 1), refresh * 1000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const picked = (tile: Tile) => (pick: PanelPick, how: PickHow) => {
    if (tile.drill && how.by === "click") {
      const down = drillDown(tile.drill, drilled[tile.id] ?? [], pick.picks);
      if (down) {
        setDrilled((d) => ({ ...d, [tile.id]: down }));
        return;
      }
    }
    const made = selectionOf(tile.id, pick.dataset, pick.picks, how.by);
    setSelections((s) => select(s, tile.id, made, how.by === "click" && how.add));
  };
  const drawnWith = (tile: Tile) => {
    const down = drilled[tile.id] ?? [];
    const panel = tile.drill ? drilledPanel(tile.panel, tile.drill, down) : tile.panel;
    const drill = tile.drill ? drillConditions(tile.id, drillDataset(tile.panel), tile.drill, down) : [];
    return { panel, conditions: conditionsFor(tile.id, { filters, selections, drill }) };
  };
  const tileDatasets = useMemo(() => [...new Set(dashboard.tiles.flatMap((tile) => datasetsOf(tile.panel)))], [dashboard.tiles]);

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
    const id = editing?.kind === "stat_card" ? editing.id : null;
    update((d) => (id ? editTile(d, id, panel) : addTile(d, panel)));
    setEditing(null);
  };
  const editingTile = editing && "id" in editing && editing.id ? dashboard.tiles.find((t) => t.id === editing.id) : undefined;

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
          <span className="text-secondary small">
            <T text="Narrow screen: tiles are stacked. Widen it to arrange them." />
          </span>
        )}
        <span className="ms-auto d-flex align-items-center gap-1">
          <Form.Select
            size="sm"
            className="an-refresh"
            value={refresh}
            aria-label={t("Refresh")}
            title={t("How often the tiles are drawn again")}
            onChange={(e) =>
              update((d) => {
                const seconds = Number(e.target.value);
                const out: DashboardState = { ...d, refresh: seconds };
                if (seconds === 0) delete out.refresh;
                return out;
              })
            }
          >
            {REFRESH_CHOICES.map((seconds) => (
              <option key={seconds} value={seconds}>
                {seconds === 0 ? t("Refresh: never") : every(seconds, t)}
              </option>
            ))}
          </Form.Select>
          <Button size="sm" variant="outline-secondary" title={t("Refresh now")} aria-label={t("Refresh now")} onClick={() => setTick((n) => n + 1)}>
            ↻
          </Button>
        </span>
      </div>
      {dashboard.tiles.length > 0 && (
        <FilterBar
          filters={filters}
          selections={selections}
          names={names}
          onRemoveFilter={(id) => setFilters((f) => f.filter((c) => c.id !== id))}
          onRemoveSelection={(id) => setSelections((s) => s.filter((c) => c.id !== id))}
          onClear={() => {
            setSelections([]);
            setFilters(() => []);
          }}
          onAdd={() => setEditing({ kind: "filter" })}
        />
      )}
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
              editDrill={() => setEditing({ kind: "drill", id: tile.id })}
              drawn={drawnWith(tile)}
              tick={tick}
              onSelect={picked(tile)}
              down={drilled[tile.id] ?? []}
              goUp={(level) => setDrilled((d) => ({ ...d, [tile.id]: (d[tile.id] ?? []).slice(0, level) }))}
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
      {editing?.kind === "drill" && editingTile && drillSpec(editingTile.panel) && (
        <DrillForm
          spec={drillSpec(editingTile.panel) as NonNullable<ReturnType<typeof drillSpec>>}
          drill={editingTile.drill ?? null}
          onSave={(drill) => {
            update((d) => setDrill(d, editing.id, drill));
            setDrilled((d) => ({ ...d, [editing.id]: [] }));
            setEditing(null);
          }}
          onCancel={() => setEditing(null)}
        />
      )}
      {editing?.kind === "filter" && (
        <FilterForm
          datasets={tileDatasets}
          onSave={(c) => {
            setFilters((f) => [...f, c].slice(-MAX_FILTERS));
            setEditing(null);
          }}
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
  editDrill,
  drawn,
  tick,
  onSelect,
  down,
  goUp,
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
  editDrill: () => void;
  /** The panel as drawn here — at its drill level — and its conditions. */
  drawn: { panel: Panel; conditions: Condition[] };
  tick: number;
  onSelect: (pick: PanelPick, how: PickHow) => void;
  /** The values picked down its drill path. */
  down: unknown[];
  goUp: (level: number) => void;
}) {
  const { t } = useT();
  const box = useRef<HTMLElement>(null);
  const name = tile.panel.title ?? kindName(tile.panel.kind, t);
  // What the dashboard's filters did to it, as the server said.
  const [applied, setApplied] = useState<Applied[]>([]);
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
        {drawn.conditions.length > 0 && <FilteredBadge applied={applied} />}
        <Dropdown align="end" className="ms-auto">
          <Dropdown.Toggle size="sm" variant="link" className="an-block-menu p-0 px-1" aria-label={t("Options for {name}", { name })}>
            ⋯
          </Dropdown.Toggle>
          <Dropdown.Menu>
            {editable && <Dropdown.Item onClick={() => setTimeout(edit, 0)}>{t("Edit")}</Dropdown.Item>}
            <Dropdown.Item onClick={() => setTimeout(() => setRenaming(true), 0)}>{t("Rename")}</Dropdown.Item>
            {drillSpec(tile.panel) && (
              <Dropdown.Item onClick={() => setTimeout(editDrill, 0)}>{tile.drill ? t("Drill path…") : t("Add a drill path…")}</Dropdown.Item>
            )}
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
      {tile.drill && <Breadcrumb path={tile.drill.path} down={down} goUp={goUp} />}
      <div className="an-tile-body" onDoubleClick={editable ? edit : undefined}>
        {tile.panel.kind === "text" && tile.panel.content.markdown.trim() === "" ? (
          <span className="an-placeholder">{t("Double-click to write text, in Markdown.")}</span>
        ) : (
          <PanelView panel={drawn.panel} filters={drawn.conditions} tick={tick} onSelect={onSelect} onFiltered={setApplied} />
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

/** Where a drilled tile is on its path, each level above a way back up:
 * "district › North › burglary". */
function Breadcrumb({ path, down, goUp }: { path: string[]; down: unknown[]; goUp: (level: number) => void }) {
  const { t } = useT();
  const missing = t("(missing)");
  const level = Math.min(down.length, path.length - 1);
  return (
    <nav className="an-drill-crumbs" aria-label={t("Drill path")}>
      {level === 0 ? (
        <span>
          {path[0]} <span className="text-secondary">· {t("click a value to drill down to {column}", { column: path[1] })}</span>
        </span>
      ) : (
        <button type="button" className="btn btn-link p-0" title={t("Back to every {column}", { column: path[0] })} onClick={() => goUp(0)}>
          {path[0]}
        </button>
      )}
      {down.slice(0, level).map((value, i) => (
        <span key={i}>
          <span className="an-drill-sep">›</span>
          {i === level - 1 ? (
            <span title={path[i]}>
              {valueText(value, missing)} <span className="text-secondary">· {path[i + 1]}</span>
            </span>
          ) : (
            <button type="button" className="btn btn-link p-0" title={path[i]} onClick={() => goUp(i + 1)}>
              {valueText(value, missing)}
            </button>
          )}
        </span>
      ))}
    </nav>
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
