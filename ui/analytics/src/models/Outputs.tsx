// A fit's outputs in the model editor (analytics TODO A3.5): a card per
// output — a table, or a plot drawn by the explorer's compiler — each folded
// and unfolded from its header, and the optional plots added from a "More
// plots" drop-down and taken away from their own card. Which are folded and
// which optional plots are on the screen is the model's view state, kept by
// the editor; this draws what it is told. Each output is also a panel, dragged
// by its header into a report (A4.3).

import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Dropdown from "react-bootstrap/Dropdown";
import Table from "react-bootstrap/Table";

import { T, useT } from "../i18n";
import { outputPanel } from "../panels/panel";
import { DragHandle } from "../panels/PanelView";
import { PlotView } from "../plot/PlotView";
import { useDocumentTheme } from "../theme";
import { formatParameterCell, isPValueColumn, significanceStars } from "./models";
import { moreOutputs, shownOutputs, type ModelOutput, type OutputTable } from "./outputs";

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

export function OutputsPanel({
  outputs,
  collapsed,
  plots,
  onToggle,
  onPlots,
  fit,
  model,
}: {
  outputs: ModelOutput[];
  /** The outputs folded, by name. */
  collapsed: string[];
  /** The optional plots on the screen, by name. */
  plots: string[];
  onToggle: (name: string) => void;
  onPlots: (plots: string[]) => void;
  /** The fit the outputs are of: given, each output is a panel that can be
   * dragged into a report (A4.3). */
  fit?: string;
  /** The model's name, for a dragged panel's title. */
  model?: string;
}) {
  const { t } = useT();
  const more = moreOutputs(outputs, plots);
  return (
    <>
      {shownOutputs(outputs, plots).map((output) => {
        const folded = collapsed.includes(output.name);
        return (
          <Card className="mb-3" key={output.name} data-output={output.name}>
            <Card.Header className="d-flex align-items-center gap-2">
              <Button
                variant="link"
                className="p-0 text-reset text-decoration-none fw-bold"
                aria-expanded={!folded}
                onClick={() => onToggle(output.name)}
              >
                <span className="an-fold" aria-hidden>
                  {folded ? "▸" : "▾"}
                </span>{" "}
                {output.label}
              </Button>
              {fit && !output.error && (
                <span className="ms-auto">
                  <DragHandle
                    make={() => outputPanel(output, fit, model, t)}
                    label={t("Drag {output} into a report or a dashboard", { output: output.label })}
                  />
                </span>
              )}
              {output.optional && (
                <Button
                  size="sm"
                  variant="outline-secondary"
                  className={fit && !output.error ? undefined : "ms-auto"}
                  aria-label={t("Close {plot}", { plot: output.label })}
                  onClick={() => onPlots(plots.filter((p) => p !== output.name))}
                >
                  ×
                </Button>
              )}
            </Card.Header>
            {!folded && <OutputBody output={output} />}
          </Card>
        );
      })}
      {more.length > 0 && (
        <Dropdown className="mb-3">
          <Dropdown.Toggle variant="outline-secondary" size="sm" id="more-plots">
            <T text="More plots" />
          </Dropdown.Toggle>
          <Dropdown.Menu>
            {more.map((o) => (
              <Dropdown.Item key={o.name} onClick={() => onPlots([...plots, o.name])}>
                {o.label}
              </Dropdown.Item>
            ))}
          </Dropdown.Menu>
        </Dropdown>
      )}
    </>
  );
}
