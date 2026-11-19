// A fit's outputs drawn (analytics TODO A3.5): a table output's table, and a
// plot output's plot, drawn by the explorer's compiler — for the model
// comparison and for a `fit_table` panel. The model editor reads a fit's
// outputs in its report (`FitReportModal.tsx`), where each is a panel.

import Alert from "react-bootstrap/Alert";
import Table from "react-bootstrap/Table";

import { T } from "../i18n";
import { PlotView } from "../plot/PlotView";
import { useDocumentTheme } from "../theme";
import { formatParameterCell, isPValueColumn, significanceStars } from "./models";
import type { ModelOutput, OutputTable } from "./outputs";

/** A table output, its numbers formatted as a coefficient table's are: a
 * p-value never as `1.2e-16`, and its stars beside it. */
export function OutputTableView({ table }: { table: OutputTable }) {
  if (table.text) return <pre className="font-monospace small mb-0 p-3">{table.text}</pre>;
  const p = table.columns.findIndex(isPValueColumn);
  return (
    <div className="table-responsive">
      <Table size="sm" className="card-table table-vcenter mb-0">
        <thead>
          <tr>
            {table.columns.map((c) => (
              <th key={c}>{c}</th>
            ))}
            {p !== -1 && <th />}
          </tr>
        </thead>
        <tbody>
          {table.rows.map((row, i) => (
            <tr key={i}>
              {table.columns.map((c, j) => (
                <td key={c} className={j === 0 ? "" : "text-nowrap font-monospace"}>
                  {formatParameterCell(c, row[j])}
                </td>
              ))}
              {p !== -1 && (
                <td className="font-monospace" title="p < 0.001 ***, < 0.01 **, < 0.05 *, < 0.1 .">
                  {significanceStars(row[p])}
                </td>
              )}
            </tr>
          ))}
        </tbody>
      </Table>
      {table.truncated && (
        <p className="text-secondary small m-2">
          <T text="The first {count} rows." args={{ count: table.rows.length }} />
        </p>
      )}
    </div>
  );
}

/** One output's body: its table, its plot, or why it has neither. */
export function OutputBody({ output }: { output: ModelOutput }) {
  const theme = useDocumentTheme();
  if (output.error) {
    return (
      <Alert variant="secondary" className="m-3">
        {output.error}
      </Alert>
    );
  }
  if (output.kind === "table" && output.table) return <OutputTableView table={output.table} />;
  if (output.kind === "plot" && output.spec && output.plot) {
    return (
      <div className="an-output-plot">
        <PlotView spec={output.spec} data={output.plot} theme={theme} />
      </div>
    );
  }
  return (
    <p className="text-secondary m-3">
      <T text="Drawing…" />
    </p>
  );
}
