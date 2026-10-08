// The attribute table below the map (analytics TODO A5.10): the rows of the
// layer whose settings are open, each with its feature's id, so selecting
// rows highlights the features and clicking features selects the rows.
//
// A click selects a row alone; Ctrl (or ⌘) adds or takes one away; Shift
// selects the range from the last one clicked, as a spreadsheet does. A
// header click sorts by the column: ascending, descending, then not.

import { useMemo, useRef, useState, type MouseEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";
import { useVirtualizer } from "@tanstack/react-virtual";

import { display } from "../../../admin/src/grid/gridValues";
import { cellText, isNumeric } from "../datasets/ops";
import { useT } from "../i18n";
import { selectedOnly, tableOrder, type LayerRows, type TableSort } from "./table";

const ROW_HEIGHT = 28;

export function AttributeTable({
  answer,
  error,
  loading,
  selected,
  sort,
  onSort,
  onPick,
}: {
  answer: LayerRows | null;
  error: string | null;
  loading: boolean;
  /** The ids of the selected features of this layer. */
  selected: unknown[];
  sort: TableSort | undefined;
  onSort: (column: string) => void;
  /** A row clicked: alone, added (`add`), or a range from the last one
   * (`range`, the ids in the table's order between them). */
  onPick: (id: unknown, how: { add: boolean; range: unknown[] | null }) => void;
}) {
  const { t } = useT();
  const [onlySelected, setOnlySelected] = useState(false);
  const last = useRef<unknown>(null);
  const order = useMemo(() => (answer ? tableOrder(answer, sort) : []), [answer, sort]);
  const shown = useMemo(
    () => (answer && onlySelected ? selectedOnly(answer, order, selected) : order),
    [answer, order, onlySelected, selected],
  );
  const selectedSet = useMemo(() => new Set(selected), [selected]);
  const scroller = useRef<HTMLDivElement | null>(null);
  const virtualizer = useVirtualizer({
    count: shown.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  });

  if (error) return <Alert variant="warning" className="m-2 small">{error}</Alert>;
  if (!answer) return <Spinner animation="border" size="sm" className="m-2" />;

  const click = (e: MouseEvent, id: unknown) => {
    let range: unknown[] | null = null;
    if (e.shiftKey && last.current !== null) {
      const ids = shown.map((i) => answer.ids[i]);
      const a = ids.indexOf(last.current);
      const b = ids.indexOf(id);
      if (a >= 0 && b >= 0) range = ids.slice(Math.min(a, b), Math.max(a, b) + 1);
    }
    last.current = id;
    onPick(id, { add: e.ctrlKey || e.metaKey, range });
  };

  const cols = answer.columns;
  const width = `calc(${cols.length} * 9rem)`;
  return (
    <div className="an-attributes">
      <div className="an-attributes-bar">
        <span>
          {answer.total > answer.rows.length
            ? t("The first {shown} of {count} rows", {
                shown: answer.rows.length.toLocaleString(),
                count: answer.total.toLocaleString(),
              })
            : t("{count} rows", { count: answer.total.toLocaleString() })}
          {selected.length > 0 && ` · ${t("{count} selected", { count: selected.length.toLocaleString() })}`}
        </span>
        {loading && <Spinner animation="border" size="sm" />}
        <Form.Check
          type="switch"
          id="an-attributes-selected"
          className="ms-auto"
          label={t("Selected only")}
          checked={onlySelected}
          disabled={selected.length === 0 && !onlySelected}
          onChange={(e) => setOnlySelected(e.target.checked)}
        />
      </div>
      <div className="an-grid" ref={scroller} role="grid" aria-label={t("Attribute table")}>
        <div className="an-grid-head" style={{ width }} role="row">
          {cols.map((c) => (
            <button
              type="button"
              key={c.name}
              className="an-head-cell an-attributes-head"
              onClick={() => onSort(c.name)}
              aria-sort={
                sort?.column === c.name ? (sort.descending ? "descending" : "ascending") : undefined
              }
              title={c.key ? t("{type}, a key of {table}", { type: c.type, table: c.key.table }) : c.type}
            >
              <span className="text-truncate fw-bold">{c.name}</span>
              {sort?.column === c.name && <span aria-hidden>{sort.descending ? " ▼" : " ▲"}</span>}
            </button>
          ))}
        </div>
        <div style={{ height: virtualizer.getTotalSize(), position: "relative", width }}>
          {virtualizer.getVirtualItems().map((item) => {
            const i = shown[item.index];
            const id = answer.ids[i];
            const row = answer.rows[i];
            return (
              <div
                key={item.key}
                role="row"
                aria-selected={selectedSet.has(id)}
                className={selectedSet.has(id) ? "an-grid-row an-attributes-row selected" : "an-grid-row an-attributes-row"}
                style={{ top: item.start, height: ROW_HEIGHT, width }}
                onClick={(e) => click(e, id)}
              >
                {cols.map((c, j) => {
                  const value = row[j];
                  const missing = value === null || value === undefined;
                  return (
                    <div
                      key={c.name}
                      className={`an-cell${isNumeric(c.type) ? " numeric" : ""}${missing ? " missing" : ""}`}
                      title={missing ? undefined : display(value)}
                    >
                      {missing ? "—" : cellText(value, c.type)}
                    </div>
                  );
                })}
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}
