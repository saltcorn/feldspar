// The table data grid: a spreadsheet over a table's rows.
//
// The old screen was a form above an HTML table of every row. That is fine for
// six rows and useless for six thousand: it fetched the whole table to draw it,
// changing one cell meant a round trip through a form at the top of the page,
// and there was no way to sort, to narrow, or to stop looking at the twelve
// columns that were not the two in question. This is the thing an admin already
// knows how to use — a grid you arrow around, type into, and copy out of.
//
// It is built on TanStack Table v9, and specifically on its `cellSelectionFeature`,
// which owns the part that is genuinely hard: a selection is a list of rectangles
// with an anchor and a focus, so shift-click resizes one range instead of leaving
// a trail, ctrl-click adds a second, and the arithmetic that turns those into
// "which sides of this cell are on the boundary" is memoized once per change
// rather than asked per cell. The features registered below are exactly the ones
// used; the rest of the library is not in the bundle.
//
// Three decisions are worth stating, because each is the reason a piece of this
// looks unusual:
//
// **The row model is placeholders, and the values come from a page cache.** The
// grid hands the library one `{ index }` object per row of the table and reads
// the actual values out of a `Map` of fetched pages. That is what makes a
// selection survive a scroll — a cell's position in the selection *is* its row
// number in the table, not its position in some window — while a page arriving
// re-renders only the thirty rows on screen instead of rebuilding a row model of
// fifty thousand.
//
// **Sorting and filtering are the server's.** `order` and the filter boxes go
// into the query string (`sc_api::query_string`, the same vocabulary an
// application's REST read takes) and come back as a different page of rows. A
// client-side sort would sort the page, which is the wrong answer presented
// convincingly.
//
// **A write names one row by its primary key.** Everything that changes data —
// typing in a cell, pasting a rectangle, pressing Delete over a range — becomes
// one `updateRow` per row, and a row the grid has not fetched is skipped rather
// than guessed at. A table with no single-column key can be read and added to but
// not edited in place, and says so.

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ClipboardEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import {
  cellSelectionFeature,
  columnOrderingFeature,
  columnResizingFeature,
  columnSizingFeature,
  columnVisibilityFeature,
  tableFeatures,
  useTable,
  type Cell,
  type ColumnDef,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";

import { api, errorMessage } from "../api";
import {
  PAGE_SIZE,
  ROW_CEILING,
  activeFilterCount,
  filterQuery,
  missingPages,
  orderSpec,
  pageBounds,
  pageOf,
  pagesCovering,
  toggleSort,
  type Sort,
} from "../grid/gridQuery";
import { decodeTsv, encodeTsv, pastePlan } from "../grid/gridClipboard";
import {
  loadLayout,
  moveBefore,
  nudge,
  saveLayout,
  showAll,
  toggleField,
  visibleFields,
  type ColumnLayout,
} from "../grid/gridColumns";
import {
  display,
  isBoolType,
  isNumericType,
  parseCell,
  sameValue,
  type RowRecord,
} from "../grid/gridValues";
import {
  IconArrowUp,
  IconChevronDown,
  IconFilter,
  IconLayoutColumns,
  IconPlus,
  IconTrash,
} from "../icons";
import type { ListFieldsResponse } from "../client";
import type { TableWrites } from "../tableWrites";
import { T, useT } from "../i18n";

/** One merged field as `listFields` reports it. */
export type FieldInfo = ListFieldsResponse[number];

/** A field's kind, narrowed from the `unknown` the API types it as. */
type FieldKind = { type?: string; store?: string } | null;

/** Whether a field is a non-stored calculated field (computed on read, refused on write). */
export function isCalc(field: FieldInfo): boolean {
  return (field.kind as FieldKind)?.type === "calc";
}

/** Whether a field holds a collaborative document (`collab_doc`): never read by
 * the API and never written by it — a whole value would overwrite the edits other
 * people are making, so it is edited only through its document stream. */
export function isDocument(field: FieldInfo): boolean {
  return field.type === "collab_doc";
}

/** The store a `File` field points at, or `null` for any other kind. */
export function fileStoreOf(field: FieldInfo): string | null {
  const kind = field.kind as FieldKind;
  return kind && kind.type === "file" ? (kind.store ?? "") : null;
}

/** The row-number gutter's column id. Not a field name, and cannot be: a field
 * name is an SQL identifier and this is not one. */
const GUTTER = "__row__";

/** A row's height in pixels, which the virtualizer measures everything against. */
const ROW_HEIGHT = 33;

/** A column's width before anybody drags its edge. */
const DEFAULT_WIDTH = 180;

/** The features this grid registers — and only these. */
const features = tableFeatures({
  cellSelectionFeature,
  columnVisibilityFeature,
  columnOrderingFeature,
  columnSizingFeature,
  columnResizingFeature,
});

/** What the row model holds: a row's position in the table, and nothing else.
 * The values live in the page cache, keyed by the same number. */
type Placeholder = { index: number };

/** One cell a write will change. */
type CellEdit = { row: number; column: string; value: unknown };

/** Which sides of a cell sit on the boundary of the selection. */
type Edges = { top: boolean; bottom: boolean; left: boolean; right: boolean };

export function DataGrid({
  table,
  fields,
  writes,
  onOpenRow,
  reloadToken,
}: {
  /** The table whose rows these are. */
  table: string;
  /** Its fields, in declaration order. */
  fields: FieldInfo[];
  /** What may be written to it — all three for a table in a database (§8.3). */
  writes: TableWrites;
  /** Open the row form: on an existing row, or on `null` for a new one. The form
   * is where a File field gets its store browser and a required column gets a
   * label, neither of which fits in a cell. */
  onOpenRow: (row: RowRecord | null) => void;
  /** Bumped by the screen when something outside the grid changed the rows. */
  reloadToken: number;
}) {
  const { t } = useT();
  // --- what is asked for -----------------------------------------------------
  const [sorting, setSorting] = useState<Sort[]>([]);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [showFilters, setShowFilters] = useState(false);
  const [layout, setLayout] = useState<ColumnLayout | null>(null);
  const [error, setError] = useState<string | null>(null);

  const fieldNames = useMemo(() => fields.map((f) => f.name), [fields]);
  const byName = useMemo(() => new Map(fields.map((f) => [f.name, f])), [fields]);
  const types = useMemo(() => Object.fromEntries(fields.map((f) => [f.name, f.type])), [fields]);

  // The column a row is addressed by. A composite key has no single value to put
  // in a URL, so it is "not addressable" here for the same reason no key is — the
  // row endpoints take one id (§13.1).
  const pk = useMemo(() => {
    const keys = fields.filter((f) => f.primary_key);
    return keys.length === 1 ? keys[0].name : null;
  }, [fields]);

  /** Whether a column's cells can be typed into. A calculated field is computed
   * on read; the key is what identifies the row a write names, so changing it in
   * place would mean "move this row to another key", which the grid is not for. */
  const readOnly = useCallback(
    (name: string) => {
      if (!writes.update || pk === null) return true;
      const field = byName.get(name);
      return field === undefined || isCalc(field) || isDocument(field) || name === pk;
    },
    [byName, pk, writes.update],
  );

  // The arrangement is per table and survives the visit; reconciled against the
  // fields the table has now every time it is read back.
  useEffect(() => {
    setLayout(loadLayout(table, fieldNames));
  }, [table, fieldNames]);

  const setAndSaveLayout = useCallback(
    (next: ColumnLayout) => {
      setLayout(next);
      saveLayout(table, next);
    },
    [table],
  );

  const filters = useMemo(() => filterQuery(types, drafts), [types, drafts]);
  const order = useMemo(() => orderSpec(sorting), [sorting]);
  // One string that changes exactly when the population or its order does. It is
  // what resets the cache: a page of the old query is not a page of the new one.
  const queryKey = useMemo(
    () => JSON.stringify({ table, order, filters, reloadToken }),
    [table, order, filters, reloadToken],
  );

  // --- the rows themselves ---------------------------------------------------
  const pages = useRef(new Map<number, RowRecord[]>());
  const inFlight = useRef(new Set<number>());
  const [version, setVersion] = useState(0);
  const [total, setTotal] = useState<number | null>(null);
  const bump = useCallback(() => setVersion((v) => v + 1), []);

  const rowAt = useCallback((index: number): RowRecord | undefined => {
    return pages.current.get(pageOf(index))?.[index % PAGE_SIZE];
  }, []);

  // A new question: forget every answer to the old one, then ask how many rows
  // it has. The count and the pages come from the same filters, so the scroller
  // is as long as the rows the filter row leaves.
  useEffect(() => {
    let live = true;
    pages.current = new Map();
    inFlight.current = new Set();
    setTotal(null);
    bump();
    api
      .countRows(table, { filter: filters })
      .then(({ count }) => {
        if (live) setTotal(count);
      })
      .catch((e: unknown) => {
        if (live) setError(errorMessage(e, "Could not count the rows."));
      });
    return () => {
      live = false;
    };
    // `queryKey` is the whole of what this depends on, spelled as one value.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [queryKey]);

  const fetchPages = useCallback(
    (wanted: number[]) => {
      const held = [...pages.current.keys(), ...inFlight.current];
      const key = queryKey;
      for (const page of missingPages(wanted, held)) {
        inFlight.current.add(page);
        api
          .listRows(table, { ...pageBounds(page), order, filter: filters })
          .then((rows) => {
            // A page of the question that has since been replaced is not an
            // answer to the one on screen.
            if (key !== queryKey) return;
            pages.current.set(page, rows as RowRecord[]);
            bump();
          })
          .catch((e: unknown) => setError(errorMessage(e, "Could not load the rows.")))
          .finally(() => inFlight.current.delete(page));
      }
    },
    [table, order, filters, queryKey, bump],
  );

  /** Re-read the pages holding these rows — after a write, so the cell shows what
   * the database made of it (a trigger, a calculated field, a coercion). */
  const refetch = useCallback(
    (rows: number[]) => {
      const touched = new Set(rows.map((r) => pageOf(r)));
      for (const page of touched) pages.current.delete(page);
      fetchPages([...touched]);
    },
    [fetchPages],
  );

  // The grid shows at most `ROW_CEILING` rows: one object per row is built for
  // the row model, and past that the building costs more than the screen is
  // worth. Narrowing with the filter row is how the rest is reached.
  const rowCount = Math.min(total ?? 0, ROW_CEILING);
  const data = useMemo<Placeholder[]>(
    () => Array.from({ length: rowCount }, (_, index) => ({ index })),
    [rowCount],
  );

  // --- the columns -----------------------------------------------------------
  const shown = useMemo(() => (layout === null ? [] : visibleFields(layout)), [layout]);

  const [editing, setEditing] = useState<{ row: number; column: string; text: string } | null>(
    null,
  );
  const editingRef = useRef(editing);
  editingRef.current = editing;

  const columns = useMemo<Array<ColumnDef<typeof features, Placeholder>>>(() => {
    const gutter: ColumnDef<typeof features, Placeholder> = {
      id: GUTTER,
      header: "",
      size: 62,
      enableResizing: false,
      // Not a value, so not part of a range: selecting it would put the row
      // number into a copy and make Delete look like it might clear it.
      enableCellSelection: false,
      cell: ({ row }) => (
        <div className="grid-gutter">
          <span className="grid-rownum">{row.original.index + 1}</span>
          <button
            type="button"
            className="grid-expand"
            title={t("Open this row")}
            onClick={() => {
              const record = rowAt(row.original.index);
              if (record) onOpenRow(record);
            }}
          >
            <IconChevronDown className="icon-2" />
          </button>
        </div>
      ),
    };
    return [
      gutter,
      ...shown.map<ColumnDef<typeof features, Placeholder>>((name) => ({
        id: name,
        header: name,
        size: DEFAULT_WIDTH,
        cell: ({ row, column }) => (
          <GridCell
            value={rowAt(row.original.index)?.[column.id]}
            type={types[column.id] ?? "text"}
            loaded={rowAt(row.original.index) !== undefined}
          />
        ),
      })),
    ];
    // `version` is in here on purpose: a page arriving changes what these cells
    // read, and the column definitions are what close over the reader.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shown, types, rowAt, onOpenRow, version]);

  const tableApi = useTable<typeof features, Placeholder>({
    features,
    data,
    columns,
    getRowId: (row) => String(row.index),
    columnResizeMode: "onChange",
    // Ranges are row-and-column ids, and the ids here are absolute row numbers.
    // A page arriving must not drop the selection, and `data` only changes
    // identity when the population does — at which point resetting is right.
    autoResetCellSelection: true,
  });

  const visibleColumns = tableApi.getVisibleLeafColumns();
  /** The grid's columns without the gutter — selection's own column index space
   * counts every visible column, so a value column's index there is one ahead. */
  const valueColumns = useMemo(
    () => visibleColumns.filter((c) => c.id !== GUTTER).map((c) => c.id),
    [visibleColumns],
  );

  // --- scrolling -------------------------------------------------------------
  const scroller = useRef<HTMLDivElement | null>(null);
  const rows = tableApi.getRowModel().rows;
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 10,
  });
  const items = virtualizer.getVirtualItems();

  // What is on screen decides what is fetched. `useLayoutEffect` so the request
  // goes out in the same frame the scroll landed in rather than one after it.
  useLayoutEffect(() => {
    if (total === null || rowCount === 0) return;
    const first = items[0]?.index ?? 0;
    const last = items[items.length - 1]?.index ?? Math.min(rowCount - 1, PAGE_SIZE);
    fetchPages(pagesCovering(first, last));
  }, [items, total, rowCount, fetchPages]);

  // --- editing ---------------------------------------------------------------

  /** Write a set of cells, one `updateRow` per row it touches. */
  const applyEdits = useCallback(
    async (edits: CellEdit[]) => {
      if (pk === null || !writes.update || edits.length === 0) return;
      const byRow = new Map<number, CellEdit[]>();
      for (const edit of edits) {
        const list = byRow.get(edit.row);
        if (list) list.push(edit);
        else byRow.set(edit.row, [edit]);
      }
      const touched: number[] = [];
      let skipped = 0;
      for (const [index, cells] of byRow) {
        const record = rowAt(index);
        const id = record?.[pk];
        // A row the grid has not fetched has no key to name it by, and a write
        // to a row it cannot name is one it must not invent.
        if (record === undefined || id === null || id === undefined) {
          skipped += 1;
          continue;
        }
        const body: RowRecord = {};
        for (const cell of cells) {
          if (readOnly(cell.column)) continue;
          if (!sameValue(cell.value, record[cell.column])) body[cell.column] = cell.value;
        }
        if (Object.keys(body).length === 0) continue;
        try {
          await api.updateRow(table, String(id), body);
          touched.push(index);
        } catch (e) {
          setError(errorMessage(e, "Could not save the change."));
          break;
        }
      }
      if (skipped > 0) {
        setError(
          `${skipped} row${skipped === 1 ? "" : "s"} had not been loaded yet and ` +
            `${skipped === 1 ? "was" : "were"} left unchanged. Scroll them into view and try again.`,
        );
      }
      if (touched.length > 0) refetch(touched);
    },
    [pk, writes.update, rowAt, readOnly, table, refetch],
  );

  const beginEdit = useCallback(
    (row: number, column: string, text: string) => {
      if (readOnly(column)) return;
      setEditing({ row, column, text });
    },
    [readOnly],
  );

  const commitEdit = useCallback(
    (move: "down" | "right" | null) => {
      const current = editingRef.current;
      setEditing(null);
      if (current === null) return;
      const type = types[current.column] ?? "text";
      void applyEdits([
        { row: current.row, column: current.column, value: parseCell(type, current.text) },
      ]);
      if (move !== null) tableApi.moveCellSelection(move);
    },
    [applyEdits, types, tableApi],
  );

  // --- the selection, as rows and columns ------------------------------------

  /** The selection as one rectangle per region, in row and column indexes. */
  const regions = tableApi.getCellSelectionBounds();

  /** Every selected cell, as a row index and a column name. */
  const selectedCells = useCallback((): Array<{ row: number; column: string }> => {
    const out: Array<{ row: number; column: string }> = [];
    for (const region of regions) {
      for (let r = region.minRowIndex; r <= region.maxRowIndex; r += 1) {
        for (let c = region.minColumnIndex; c <= region.maxColumnIndex; c += 1) {
          const column = valueColumns[c - 1];
          if (column !== undefined) out.push({ row: r, column });
        }
      }
    }
    return out;
  }, [regions, valueColumns]);

  const selectedRows = useMemo(() => {
    const set = new Set<number>();
    for (const region of regions) {
      for (let r = region.minRowIndex; r <= region.maxRowIndex; r += 1) set.add(r);
    }
    return [...set].sort((a, b) => a - b);
  }, [regions]);

  const copyText = useCallback((): string => {
    const region = regions[0];
    if (region === undefined) return "";
    const grid: string[][] = [];
    for (let r = region.minRowIndex; r <= region.maxRowIndex; r += 1) {
      const record = rowAt(r);
      const line: string[] = [];
      for (let c = region.minColumnIndex; c <= region.maxColumnIndex; c += 1) {
        const column = valueColumns[c - 1];
        line.push(column === undefined ? "" : display(record?.[column]));
      }
      grid.push(line);
    }
    return encodeTsv(grid);
  }, [regions, rowAt, valueColumns]);

  const onCopy = (event: ClipboardEvent<HTMLDivElement>) => {
    if (editing !== null) return;
    const text = copyText();
    if (text === "") return;
    event.clipboardData.setData("text/plain", text);
    event.preventDefault();
  };

  const onCut = (event: ClipboardEvent<HTMLDivElement>) => {
    if (editing !== null) return;
    onCopy(event);
    void applyEdits(
      selectedCells()
        .filter((c) => !readOnly(c.column))
        .map((c) => ({ ...c, value: null })),
    );
  };

  const onPaste = (event: ClipboardEvent<HTMLDivElement>) => {
    if (editing !== null || !writes.update) return;
    const text = event.clipboardData.getData("text/plain");
    if (text === "") return;
    event.preventDefault();
    const region = regions[0];
    if (region === undefined) return;
    const plan = pastePlan(
      decodeTsv(text),
      {
        top: region.minRowIndex,
        left: region.minColumnIndex - 1,
        bottom: region.maxRowIndex,
        right: region.maxColumnIndex - 1,
      },
      { rows: rowCount, columns: valueColumns.length },
    );
    void applyEdits(
      plan.flatMap((cell) => {
        const column = valueColumns[cell.column];
        if (column === undefined || readOnly(column)) return [];
        return [{ row: cell.row, column, value: parseCell(types[column] ?? "text", cell.text) }];
      }),
    );
  };

  // --- the keyboard ----------------------------------------------------------
  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (editing !== null) return;
    const focused = tableApi.getFocusedCell();
    const mod = event.ctrlKey || event.metaKey;
    switch (event.key) {
      case "ArrowUp":
      case "ArrowDown":
      case "ArrowLeft":
      case "ArrowRight": {
        const direction = {
          ArrowUp: "up",
          ArrowDown: "down",
          ArrowLeft: "left",
          ArrowRight: "right",
        }[event.key] as "up" | "down" | "left" | "right";
        event.preventDefault();
        if (event.shiftKey) tableApi.extendCellSelection(direction);
        else tableApi.moveCellSelection(direction);
        return;
      }
      case "Tab":
        if (focused === undefined) return;
        event.preventDefault();
        tableApi.moveCellSelection(event.shiftKey ? "left" : "right");
        return;
      case "Enter":
        if (focused === undefined) return;
        event.preventDefault();
        // Enter opens the cell for editing where a spreadsheet would, and moves
        // down where there is nothing to open.
        if (readOnly(focused.column.id)) tableApi.moveCellSelection("down");
        else beginEdit(rowIndexOf(focused), focused.column.id, cellText(focused, rowAt));
        return;
      case "F2":
        if (focused === undefined) return;
        event.preventDefault();
        beginEdit(rowIndexOf(focused), focused.column.id, cellText(focused, rowAt));
        return;
      case "Escape":
        tableApi.resetCellSelection(true);
        return;
      case "Delete":
      case "Backspace": {
        if (!writes.update) return;
        event.preventDefault();
        void applyEdits(
          selectedCells()
            .filter((c) => !readOnly(c.column))
            .map((c) => ({ ...c, value: null })),
        );
        return;
      }
      case "a":
      case "A":
        if (mod) {
          event.preventDefault();
          tableApi.selectAllCells();
        }
        return;
      default:
        break;
    }
    // Anything else printable starts an edit with that character, which is what
    // every spreadsheet does and what makes the grid feel like one.
    if (!mod && !event.altKey && event.key.length === 1 && focused !== undefined) {
      const column = focused.column.id;
      if (readOnly(column)) return;
      event.preventDefault();
      beginEdit(rowIndexOf(focused), column, event.key);
    }
  };

  // --- reordering columns by dragging a header -------------------------------
  const [dragging, setDragging] = useState<string | null>(null);

  const dropOn = (target: string | null) => {
    if (dragging === null || layout === null) return;
    setAndSaveLayout({ ...layout, order: moveBefore(layout.order, dragging, target) });
    setDragging(null);
  };

  if (layout === null) return null;

  const filterCount = activeFilterCount(types, drafts);
  const totalWidth = visibleColumns.reduce((sum, c) => sum + c.getSize(), 0);
  const headers = tableApi.getHeaderGroups()[0]?.headers ?? [];

  return (
    <div className="grid-shell">
      <div className="grid-toolbar">
        <span className="text-muted small">
          {total === null
            ? t("Counting…")
            : t("{count} rows", { count: total })}
          {total !== null && total > ROW_CEILING && (
            <>
              {t(" — showing the first {shown}; narrow with a filter", {
                shown: ROW_CEILING.toLocaleString(),
              })}
            </>
          )}
        </span>
        <div className="flex-grow-1" />
        <Button
          size="sm"
          variant={showFilters ? "primary" : "outline-secondary"}
          onClick={() => setShowFilters((s) => !s)}
        >
          <IconFilter className="icon-2" />
          <T text="Filter" />
          {filterCount > 0 && <span className="badge bg-secondary ms-1">{filterCount}</span>}
        </Button>
        <FieldsMenu layout={layout} fields={fields} onChange={setAndSaveLayout} />
        {sorting.length > 0 && (
          <Button size="sm" variant="outline-secondary" onClick={() => setSorting([])}>
            <T text="Clear sort" />
          </Button>
        )}
        {writes.delete && selectedRows.length > 0 && pk !== null && (
          <Button
            size="sm"
            variant="outline-danger"
            onClick={() => {
              void deleteRows(table, pk, selectedRows, rowAt).then((outcome) => {
                if (outcome.error !== null) setError(outcome.error);
                tableApi.resetCellSelection(true);
                setTotal((t) => (t === null ? t : t - outcome.deleted));
                pages.current = new Map();
                bump();
              });
            }}
          >
            <IconTrash className="icon-2" />
            {t("Delete {count} rows", { count: selectedRows.length })}
          </Button>
        )}
        {writes.insert && (
          <Button size="sm" variant="primary" onClick={() => onOpenRow(null)}>
            <IconPlus className="icon-2" />
            <T text="New row" />
          </Button>
        )}
      </div>

      {error !== null && (
        <Alert variant="danger" dismissible onClose={() => setError(null)} className="mb-2">
          {error}
        </Alert>
      )}
      {pk === null && fields.length > 0 && (
        <Alert variant="warning" className="mb-2">
          <T text="This table has no single-column primary key, so a row cannot be picked out to change or delete. Rows can still be read and added." />{" "}
          <T
            text="Give one field the {tick} tick on the table page to edit them here."
            values={{
              tick: (
                <strong>
                  <T text="Primary key" />
                </strong>
              ),
            }}
          />
        </Alert>
      )}

      <div
        className="grid-scroll"
        ref={scroller}
        tabIndex={0}
        onKeyDown={onKeyDown}
        onCopy={onCopy}
        onCut={onCut}
        onPaste={onPaste}
      >
        <div className="grid-table" style={{ width: totalWidth }}>
          <div className="grid-head">
            <div className="grid-row">
              {headers.map((header) => {
                const column = header.column;
                const sort = sorting.find((s) => s.id === column.id);
                const isGutter = column.id === GUTTER;
                return (
                  <div
                    key={column.id}
                    className={`grid-th${isGutter ? " grid-th-gutter" : ""}`}
                    style={{ width: column.getSize() }}
                    draggable={!isGutter}
                    onDragStart={() => setDragging(column.id)}
                    onDragOver={(e) => e.preventDefault()}
                    onDrop={() => dropOn(isGutter ? null : column.id)}
                  >
                    {!isGutter && (
                      <button
                        type="button"
                        className="grid-th-label"
                        title={`Sort by ${column.id} (shift-click to add a tiebreaker)`}
                        onClick={(e: ReactMouseEvent) =>
                          setSorting((s) => toggleSort(s, column.id, e.shiftKey))
                        }
                      >
                        <span className="text-truncate">{column.id}</span>
                        {sort !== undefined && (
                          <IconArrowUp
                            className={`icon-2 grid-sort${sort.desc ? " grid-sort-desc" : ""}`}
                          />
                        )}
                      </button>
                    )}
                    {column.getCanResize() && (
                      <span
                        className="grid-resizer"
                        onMouseDown={header.getResizeHandler()}
                        onTouchStart={header.getResizeHandler()}
                        onDragStart={(e) => e.preventDefault()}
                      />
                    )}
                  </div>
                );
              })}
            </div>
            {showFilters && (
              <div className="grid-row grid-filters">
                {visibleColumns.map((column) => (
                  <div
                    key={column.id}
                    className="grid-th grid-filter-cell"
                    style={{ width: column.getSize() }}
                  >
                    {column.id !== GUTTER && (
                      <Form.Control
                        size="sm"
                        value={drafts[column.id] ?? ""}
                        placeholder={filterHint(types[column.id] ?? "text")}
                        onChange={(e) => setDrafts((d) => ({ ...d, [column.id]: e.target.value }))}
                      />
                    )}
                  </div>
                ))}
              </div>
            )}
          </div>

          <div className="grid-body" style={{ height: virtualizer.getTotalSize() }}>
            {items.map((item) => {
              const row = rows[item.index];
              if (row === undefined) return null;
              return (
                <div
                  key={row.id}
                  className="grid-row grid-body-row"
                  style={{ transform: `translateY(${item.start}px)` }}
                >
                  {row.getVisibleCells().map((cell) => {
                    const edges = cell.getSelectionEdges();
                    const isEditing =
                      editing !== null &&
                      editing.row === row.original.index &&
                      editing.column === cell.column.id;
                    return (
                      <div
                        key={cell.id}
                        className={cellClass(cell, edges, readOnly(cell.column.id))}
                        style={{ width: cell.column.getSize() }}
                        onMouseDown={cell.getSelectionStartHandler()}
                        onMouseEnter={cell.getSelectionExtendHandler()}
                        onDoubleClick={() =>
                          beginEdit(
                            row.original.index,
                            cell.column.id,
                            display(rowAt(row.original.index)?.[cell.column.id]),
                          )
                        }
                      >
                        {isEditing ? (
                          <CellEditor
                            text={editing.text}
                            onChange={(text) => setEditing((e) => (e === null ? e : { ...e, text }))}
                            onCommit={commitEdit}
                            onCancel={() => setEditing(null)}
                          />
                        ) : (
                          <tableApi.FlexRender cell={cell} />
                        )}
                      </div>
                    );
                  })}
                </div>
              );
            })}
          </div>
          {total !== null && rowCount === 0 && (
            <div className="grid-empty text-muted">
              {filterCount > 0 ? "No rows match these filters." : "No rows yet."}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

/** One cell's contents: a tick for a `bool` column, text for everything else, and
 * nothing at all while the page it is on is still on its way. */
function GridCell({ value, type, loaded }: { value: unknown; type: string; loaded: boolean }) {
  if (!loaded) return <span className="grid-pending" />;
  if (isBoolType(type)) {
    return value === null || value === undefined ? null : (
      <input type="checkbox" checked={value === true} readOnly tabIndex={-1} />
    );
  }
  return (
    <span className={`text-truncate${isNumericType(type) ? " ms-auto" : ""}`}>
      {display(value)}
    </span>
  );
}

/** The input a cell is edited through: it opens focused, with the caret at the
 * end, and every way out of it says what to do next. */
function CellEditor({
  text,
  onChange,
  onCommit,
  onCancel,
}: {
  text: string;
  onChange: (text: string) => void;
  onCommit: (move: "down" | "right" | null) => void;
  onCancel: () => void;
}) {
  const input = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    input.current?.focus();
    input.current?.setSelectionRange(text.length, text.length);
    // Only on open: moving the caret on every keystroke would fight the typist.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return (
    <input
      ref={input}
      className="grid-editor"
      value={text}
      onChange={(e) => onChange(e.target.value)}
      onBlur={() => onCommit(null)}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          onCommit("down");
        } else if (e.key === "Tab") {
          e.preventDefault();
          onCommit("right");
        } else if (e.key === "Escape") {
          e.preventDefault();
          onCancel();
        }
        // Arrow keys belong to the caret while an editor is open, so the grid's
        // own handler must not also see them.
        e.stopPropagation();
      }}
    />
  );
}

/** The Fields menu: what is shown, and in what order. */
function FieldsMenu({
  layout,
  fields,
  onChange,
}: {
  layout: ColumnLayout;
  fields: FieldInfo[];
  onChange: (layout: ColumnLayout) => void;
}) {
  const { t } = useT();
  const kinds = new Map(fields.map((f) => [f.name, f.type]));
  const hiddenCount = layout.hidden.length;
  return (
    <Dropdown autoClose="outside">
      <Dropdown.Toggle size="sm" variant="outline-secondary">
        <IconLayoutColumns className="icon-2" />
        <T text="Fields" />
        {hiddenCount > 0 && (
          <span className="badge bg-secondary ms-1">
            {t("{count} hidden", { count: hiddenCount })}
          </span>
        )}
      </Dropdown.Toggle>
      <Dropdown.Menu className="grid-fields-menu">
        {layout.order.map((name, at) => (
          <div className="grid-fields-row" key={name}>
            <Form.Check
              type="checkbox"
              id={`field-${name}`}
              checked={!layout.hidden.includes(name)}
              onChange={() => onChange(toggleField(layout, name))}
              label={
                <>
                  {name} <span className="text-muted small">{kinds.get(name)}</span>
                </>
              }
            />
            <div className="flex-grow-1" />
            <button
              type="button"
              className="grid-fields-move"
              title={t("Move earlier")}
              disabled={at === 0}
              onClick={() => onChange(nudge(layout, name, -1))}
            >
              ↑
            </button>
            <button
              type="button"
              className="grid-fields-move"
              title={t("Move later")}
              disabled={at === layout.order.length - 1}
              onClick={() => onChange(nudge(layout, name, 1))}
            >
              ↓
            </button>
          </div>
        ))}
        <Dropdown.Divider />
        <div className="d-flex gap-2 px-3 pb-1">
          <Button
            size="sm"
            variant="link"
            className="p-0"
            onClick={() => onChange(showAll(layout, true))}
          >
            <T text="Show all" />
          </Button>
          <Button
            size="sm"
            variant="link"
            className="p-0"
            onClick={() => onChange(showAll(layout, false))}
          >
            <T text="Hide all" />
          </Button>
        </div>
      </Dropdown.Menu>
    </Dropdown>
  );
}

/** What a filter box suggests, which is also the vocabulary it accepts. */
function filterHint(type: string): string {
  return type === "text" || type === "string" ? "contains…" : "= value, or > 10";
}

/** A cell's row number — its position in the table, which is its id. */
function rowIndexOf(cell: Cell<typeof features, Placeholder, unknown>): number {
  return cell.row.original.index;
}

/** A cell's current text, for an editor opening on it. */
function cellText(
  cell: Cell<typeof features, Placeholder, unknown>,
  rowAt: (index: number) => RowRecord | undefined,
): string {
  return display(rowAt(rowIndexOf(cell))?.[cell.column.id]);
}

/** A cell's classes: selected, focused, whether it can be typed into, and which
 * of its sides are on the boundary of the selection — which is what draws the
 * spreadsheet outline without each cell asking about its neighbours. */
function cellClass(
  cell: Cell<typeof features, Placeholder, unknown>,
  edges: Edges,
  readOnly: boolean,
): string {
  const parts = ["grid-td"];
  if (cell.column.id === GUTTER) parts.push("grid-td-gutter");
  if (cell.getIsSelected()) parts.push("grid-selected");
  if (cell.getIsFocused()) parts.push("grid-focused");
  if (readOnly && cell.column.id !== GUTTER) parts.push("grid-readonly");
  if (edges.top) parts.push("grid-edge-top");
  if (edges.bottom) parts.push("grid-edge-bottom");
  if (edges.left) parts.push("grid-edge-left");
  if (edges.right) parts.push("grid-edge-right");
  return parts.join(" ");
}

/** Delete the rows at these indexes, stopping at the first refusal. */
async function deleteRows(
  table: string,
  pk: string,
  indexes: number[],
  rowAt: (index: number) => RowRecord | undefined,
): Promise<{ deleted: number; error: string | null }> {
  let deleted = 0;
  for (const index of indexes) {
    const id = rowAt(index)?.[pk];
    if (id === null || id === undefined) continue;
    try {
      await api.deleteRow(table, String(id));
      deleted += 1;
    } catch (e) {
      return { deleted, error: errorMessage(e, "Could not delete the row.") };
    }
  }
  return { deleted, error: null };
}
