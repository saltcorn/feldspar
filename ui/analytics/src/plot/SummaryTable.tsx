// A summary table (analytics TODO A2.8): the row dimensions' labels down the
// left, a header row per column dimension (and one naming the cells when there
// are several), the Total column at the right and the Total row at the bottom.

import { useMemo } from "react";

import { useT } from "../i18n";
import { tableModel } from "./table";
import type { TableData } from "./spec";

export function SummaryTable({ data }: { data: TableData }) {
  const { t } = useT();
  const model = useMemo(() => tableModel(data, t("(missing)"), t("Total")), [data, t]);
  const k = Math.max(model.cellNames.length, 1);
  const labelColumns = Math.max(model.rowNames.length, 1);
  const headerRows = Math.max(model.columnNames.length, 1);
  return (
    <div className="an-table-wrap">
      <table className="table table-sm an-summary">
        <thead>
          {Array.from({ length: headerRows }, (_, d) => (
            <tr key={d}>
              {d === 0 &&
                Array.from({ length: labelColumns }, (_, i) => (
                  <th key={i} rowSpan={headerRows + (k > 1 ? 1 : 0)} className="an-summary-dim">
                    {model.rowNames[i] ?? ""}
                    {i === labelColumns - 1 && model.columnNames.length > 0 && (
                      <span className="an-summary-across"> / {model.columnNames.join(" · ")}</span>
                    )}
                  </th>
                ))}
              {model.groups.map((g, i) => (
                <th key={i} colSpan={k} className={g.total ? "an-summary-total numeric" : "numeric"}>
                  {model.columnNames.length === 0 ? (k === 1 ? model.cellNames[0] : "") : g.labels[d]}
                </th>
              ))}
            </tr>
          ))}
          {k > 1 && (
            <tr>
              {model.groups.flatMap((g, i) =>
                model.cellNames.map((c, j) => (
                  <th key={`${i}-${j}`} className={g.total ? "an-summary-total numeric" : "numeric"}>
                    {c}
                  </th>
                )),
              )}
            </tr>
          )}
        </thead>
        <tbody>
          {model.rows.map((r, i) => (
            <tr key={i} className={r.total ? "an-summary-total" : undefined}>
              {Array.from({ length: labelColumns }, (_, j) => (
                <th key={j} scope="row">
                  {r.labels[j] ?? ""}
                </th>
              ))}
              {r.values.map((v, j) => (
                <td key={j} className={model.groups[Math.floor(j / k)]?.total ? "an-summary-total numeric" : "numeric"}>
                  {v}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {data.truncated && (
        <p className="text-secondary small">{t("The table has more rows than are shown; only the first are.")}</p>
      )}
    </div>
  );
}
