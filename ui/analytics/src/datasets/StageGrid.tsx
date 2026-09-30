// The spreadsheet of one stage (analytics TODO A1.17): read-only, virtualised,
// fetched a page at a time as it scrolls — the admin grid's paging
// (`ui/admin/src/grid/gridQuery.ts`) and its cell rendering
// (`gridValues.ts`), over `readDatasetStage` rather than a table's rows.
//
// The header is where operations start from, as in a spreadsheet: each
// column's menu offers what applies to it (filter on it, sort by it, group by
// it, stack it with the other selected columns), and the **+** after the last
// column adds a Calculated column. They are added after the stage being
// looked at, since those are the columns the admin is looking at.

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import { useVirtualizer } from "@tanstack/react-virtual";

import { PAGE_SIZE, missingPages, pagesCovering } from "../../../admin/src/grid/gridQuery";
import { display } from "../../../admin/src/grid/gridValues";
import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { cellText, describeGrain, isNumeric, type DatasetDef, type Grain, type StageColumn } from "./ops";

const ROW_HEIGHT = 30;

/** What a column header's menu asks for. */
export type HeaderAction =
  | { kind: "filter"; column: string }
  | { kind: "sort"; column: string; descending: boolean }
  | { kind: "group"; column: string }
  | { kind: "stack"; columns: string[] };

export function StageGrid({
  def,
  upto,
  selected,
  onToggle,
  onAction,
  onAddColumn,
}: {
  def: DatasetDef;
  /** The stage: after the first `upto` operations. */
  upto: number;
  /** The columns ticked, for Stack. */
  selected: string[];
  onToggle: (column: string) => void;
  onAction: (action: HeaderAction) => void;
  onAddColumn: () => void;
}) {
  const { t } = useT();
  const [columns, setColumns] = useState<StageColumn[] | null>(null);
  const [grain, setGrain] = useState<Grain | null>(null);
  const [total, setTotal] = useState<number | null>(null);
  const [rows, setRows] = useState<Map<number, unknown[][]>>(new Map());
  const [error, setError] = useState<string | null>(null);
  const requested = useRef<Set<number>>(new Set());
  // A new definition or another stage is another table: start again.
  const key = JSON.stringify({ def, upto });
  const current = useRef(key);

  const fetchPages = useCallback(
    (pages: number[]) => {
      for (const page of missingPages(pages, requested.current)) {
        requested.current.add(page);
        const asked = current.current;
        api
          .readDatasetStage({ dataset: def, upto, offset: page * PAGE_SIZE, limit: PAGE_SIZE })
          .then((answer) => {
            if (current.current !== asked) return;
            setColumns(answer.columns as StageColumn[]);
            setGrain(answer.grain as Grain);
            setTotal(answer.total);
            setRows((held) => new Map(held).set(page, answer.rows as unknown[][]));
          })
          .catch((err: unknown) => {
            if (current.current !== asked) return;
            requested.current.delete(page);
            setError(errorMessage(err, t("This stage could not be read.")));
          });
      }
    },
    [def, upto, t],
  );

  useEffect(() => {
    current.current = key;
    requested.current = new Set();
    setRows(new Map());
    setTotal(null);
    setError(null);
    fetchPages([0]);
  }, [key]);

  const scroller = useRef<HTMLDivElement | null>(null);
  const virtualizer = useVirtualizer({
    count: total ?? 0,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 10,
  });
  const items = virtualizer.getVirtualItems();

  useLayoutEffect(() => {
    if (!total) return;
    const first = items[0]?.index ?? 0;
    const last = items[items.length - 1]?.index ?? 0;
    fetchPages(pagesCovering(first, last));
  }, [items, total, fetchPages]);

  const rowAt = (index: number): unknown[] | undefined =>
    rows.get(Math.floor(index / PAGE_SIZE))?.[index % PAGE_SIZE];

  if (error) {
    return (
      <div className="p-3">
        <Alert variant="warning">{error}</Alert>
      </div>
    );
  }

  const cols = columns ?? [];
  const width = `calc(3.5rem + ${cols.length} * 10rem + 3rem)`;
  return (
    <>
      <div className="px-3 py-1 small text-secondary border-bottom">
        {total === null
          ? t("Reading…")
          : t("{count} rows · {grain}", { count: total.toLocaleString(), grain: describeGrain(grain, t) })}
      </div>
      <div className="an-grid" ref={scroller}>
        <div className="an-grid-head" style={{ width }}>
          <div className="an-rownum">#</div>
          {cols.map((c) => (
            <div key={c.name} className={`an-head-cell${selected.includes(c.name) ? " selected" : ""}`}>
              <div className="d-flex align-items-center gap-1">
                <Form.Check
                  type="checkbox"
                  aria-label={t("Select {column}", { column: c.name })}
                  checked={selected.includes(c.name)}
                  onChange={() => onToggle(c.name)}
                />
                <span className="text-truncate fw-bold" title={c.name}>
                  {c.name}
                </span>
                <Dropdown className="ms-auto">
                  <Dropdown.Toggle size="sm" variant="link" className="p-0" aria-label={t("Operations on {column}", { column: c.name })} />
                  <Dropdown.Menu>
                    <Dropdown.Item onClick={() => onAction({ kind: "filter", column: c.name })}>
                      <T text="Filter…" />
                    </Dropdown.Item>
                    <Dropdown.Item onClick={() => onAction({ kind: "sort", column: c.name, descending: false })}>
                      <T text="Sort ascending" />
                    </Dropdown.Item>
                    <Dropdown.Item onClick={() => onAction({ kind: "sort", column: c.name, descending: true })}>
                      <T text="Sort descending" />
                    </Dropdown.Item>
                    <Dropdown.Item onClick={() => onAction({ kind: "group", column: c.name })}>
                      <T text="Group by…" />
                    </Dropdown.Item>
                    <Dropdown.Item
                      onClick={() =>
                        onAction({
                          kind: "stack",
                          columns: selected.includes(c.name) ? selected : [...selected, c.name],
                        })
                      }
                    >
                      {selected.filter((s) => s !== c.name).length > 0
                        ? t("Stack with the selected columns…")
                        : t("Stack…")}
                    </Dropdown.Item>
                  </Dropdown.Menu>
                </Dropdown>
              </div>
              <div className="an-head-type">
                {c.type}
                {c.key ? ` → ${c.key.table}` : ""}
              </div>
            </div>
          ))}
          <div className="an-head-cell" style={{ flex: "0 0 3rem", width: "3rem" }}>
            <Button size="sm" variant="outline-primary" onClick={onAddColumn} aria-label={t("Add a calculated column")}>
              +
            </Button>
          </div>
        </div>
        <div style={{ height: virtualizer.getTotalSize(), position: "relative", width }}>
          {items.map((item) => {
            const row = rowAt(item.index);
            return (
              <div
                key={item.key}
                className="an-grid-row"
                style={{ top: item.start, height: ROW_HEIGHT, width }}
              >
                <div className="an-rownum">{item.index + 1}</div>
                {cols.map((c, i) => {
                  const value = row?.[i];
                  const missing = value === null || value === undefined;
                  return (
                    <div
                      key={c.name}
                      className={`an-cell${isNumeric(c.type) ? " numeric" : ""}${missing ? " missing" : ""}`}
                      title={missing ? undefined : display(value)}
                    >
                      {row === undefined ? "" : missing ? "—" : cellText(value, c.type)}
                    </div>
                  );
                })}
              </div>
            );
          })}
        </div>
      </div>
    </>
  );
}
