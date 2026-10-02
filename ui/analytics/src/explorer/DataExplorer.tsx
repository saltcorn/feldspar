// The Data explorer workspace (analytics TODO A2.9): a dataset from the
// drop-down, a plot type from the gallery, columns dragged from the list onto
// the drop zones, and the plot — or the summary table of the same drop zones.
//
// The server chooses: the drop zones go to `suggestPlot`, which answers the
// spec the "show me" rules (or a gallery preset, or the mark palette's choice)
// make of them; the layers panel's changes are laid over it; `renderPlot`
// draws it. Beside it, the hypothesis tests the Y, X and Wrap drop zones make
// (`runTests`, A2.12–A2.14), as one panel with the plot. Every choice is the
// workspace's state, saved by the frame as it changes, so reopening the
// workspace shows the same plot and tests.

import { useCallback, useEffect, useMemo, useState, type DragEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import ButtonGroup from "react-bootstrap/ButtonGroup";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { ListDatasetsResponse, PlotGalleryResponse } from "../client";
import type { Grain, StageColumn, StageShape } from "../datasets/ops";
import { T, useT } from "../i18n";
import { markName, presetName, summaryName, zoneName } from "../labels";
import { keyOf, plotNotes } from "../plot/echarts";
import { PlotView } from "../plot/PlotView";
import {
  AGGREGATES,
  MARKS,
  isRefused,
  type AggregateFn,
  type Mark,
  type PlotData,
  type PlotSpec,
  type TableData,
} from "../plot/spec";
import { SummaryTable } from "../plot/SummaryTable";
import { navigate } from "../router";
import { useDocumentTheme } from "../theme";
import type { WorkspaceProps, WorkspaceState } from "../workspaces/WorkspaceFrame";
import { LayersPanel } from "./LayersPanel";
import { modelPlan, planDataset, planModel } from "./openAsModel";
import { TestResults } from "./TestResults";
import { isAnalysis, testSpecOf, type Analysis } from "./tests";
import {
  ZONES,
  clear,
  composeSpec,
  drop,
  onZone,
  pickDataset,
  pickMark,
  readState,
  remove,
  setTests,
  tableSpecOf,
  toggleBin,
  type Assignment,
  type ExplorerState,
  type Zone,
} from "./state";

type DatasetItem = ListDatasetsResponse[number];
type GalleryItem = PlotGalleryResponse[number];

/** What a dragged column carries. */
const DRAG_TYPE = "application/x-feldspar-column";

/** How long after the last drop the explorer asks the server. */
const SETTLE_MS = 120;

/** A column's type, in two characters, for the column list. */
function typeBadge(c: StageColumn): string {
  if (c.key) return "→";
  switch (c.type) {
    case "int":
    case "float":
    case "decimal":
      return "#";
    case "text":
      return "Aa";
    case "bool":
      return "✓";
    case "date":
    case "timestamp":
    case "time":
      return "◷";
    default:
      return "?";
  }
}

export function DataExplorer({ state: raw, setState }: WorkspaceProps) {
  const { t } = useT();
  const theme = useDocumentTheme();
  const state = useMemo(() => readState(raw), [raw]);
  const update = useCallback(
    (change: (s: ExplorerState) => ExplorerState) =>
      setState((current) => change(readState(current)) as unknown as WorkspaceState),
    [setState],
  );

  const [datasets, setDatasets] = useState<DatasetItem[] | null>(null);
  const [gallery, setGallery] = useState<GalleryItem[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [layersOpen, setLayersOpen] = useState(false);

  useEffect(() => {
    let live = true;
    Promise.all([api.listDatasets(), api.plotGallery()])
      .then(([ds, g]) => {
        if (!live) return;
        setDatasets(ds);
        setGallery(g);
      })
      .catch((err: unknown) => live && setLoadError(errorMessage(err, t("Could not load the datasets."))));
    return () => {
      live = false;
    };
  }, [t]);

  const dataset = datasets?.find((d) => d.id === state.dataset);
  const shape: StageShape | null = useMemo(
    () => (dataset ? { columns: dataset.columns as StageColumn[], grain: dataset.grain as Grain } : null),
    [dataset],
  );

  // --- the spec: the server's answer to the drop zones ----------------------
  const [suggested, setSuggested] = useState<{ spec?: PlotSpec; error?: string } | null>(null);
  const assignmentKey = keyOf(state.assignment);
  useEffect(() => {
    if (!state.dataset || state.view !== "plot") return;
    let live = true;
    const timer = window.setTimeout(() => {
      api
        .suggestPlot({
          dataset: state.dataset as string,
          assignment: state.assignment,
          preset: state.preset ?? null,
          mark: state.mark ?? null,
        })
        .then((answer) => {
          if (!live) return;
          if (answer.error) {
            setSuggested({ error: answer.error });
            return;
          }
          setSuggested({ spec: answer.spec as PlotSpec });
          // A reshaping preset says which columns it used.
          if (state.preset && answer.assignment && keyOf(answer.assignment) !== assignmentKey) {
            update((s) => ({ ...s, assignment: answer.assignment as Assignment }));
          }
        })
        .catch((err: unknown) => live && setSuggested({ error: errorMessage(err, t("Could not make a plot.")) }));
    }, SETTLE_MS);
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
    // The assignment is compared by value (`assignmentKey`).
  }, [state.dataset, assignmentKey, state.mark, state.preset, state.view, t, update]);

  const spec = useMemo(
    () => (suggested?.spec && !suggested.error ? composeSpec(suggested.spec, state.extras) : null),
    [suggested, state.extras],
  );

  // --- the plot's data -------------------------------------------------------
  const [drawn, setDrawn] = useState<{ spec: PlotSpec; data: PlotData } | null>(null);
  const [drawError, setDrawError] = useState<string | null>(null);
  const [drawing, setDrawing] = useState(false);
  const specKey = spec ? JSON.stringify(spec) : "";
  useEffect(() => {
    if (!spec || state.view !== "plot") return;
    let live = true;
    setDrawing(true);
    api
      .renderPlot({ spec })
      .then((answer) => {
        if (!live) return;
        if (isRefused(answer)) {
          setDrawError(answer.error);
        } else {
          setDrawError(null);
          setDrawn({ spec, data: answer as unknown as PlotData });
        }
      })
      .catch((err: unknown) => live && setDrawError(errorMessage(err, t("Could not draw the plot."))))
      .finally(() => live && setDrawing(false));
    return () => {
      live = false;
    };
  }, [specKey, state.view, t]);

  // --- the summary table -----------------------------------------------------
  const tableSpec = useMemo(() => tableSpecOf(state, shape), [state, shape]);
  const tableKey = tableSpec ? JSON.stringify(tableSpec) : "";
  const [table, setTable] = useState<{ data?: TableData; error?: string } | null>(null);
  useEffect(() => {
    if (!tableSpec || state.view !== "table") return;
    let live = true;
    api
      .renderTable({ spec: tableSpec })
      .then((answer) => {
        if (!live) return;
        setTable(isRefused(answer) ? { error: answer.error } : { data: answer as unknown as TableData });
      })
      .catch((err: unknown) => live && setTable({ error: errorMessage(err, t("Could not make the table.")) }));
    return () => {
      live = false;
    };
  }, [tableKey, state.view, t]);

  // --- the hypothesis tests --------------------------------------------------
  const testSpec = useMemo(() => (state.tests.show ? testSpecOf(state) : null), [state]);
  const testKey = testSpec ? JSON.stringify(testSpec) : "";
  const [tests, setTestResults] = useState<{ analysis?: Analysis; error?: string } | null>(null);
  const [testing, setTesting] = useState(false);

  // Open as model (A3.7): Y by X as a regression, on a dataset of its own
  // keeping the two columns, opened in the model editor.
  const canOpenAsModel = Boolean(dataset && modelPlan(testSpec, shape, dataset.name, [], []));
  const openAsModel = async () => {
    if (!dataset) return;
    try {
      const [models, all] = await Promise.all([api.listModels(), api.listDatasets()]);
      // A key on X is named by a text column of the table it points at.
      const key = shape?.columns.find((c) => c.name === testSpec?.x?.field)?.key;
      const keyLabel = key
        ? (await api.listFields(key.table)).find((f) => f.type === "text" && !f.primary_key)?.name
        : undefined;
      const plan = modelPlan(
        testSpec,
        shape,
        dataset.name,
        models.map((m) => m.name),
        all.map((d) => d.name),
        keyLabel,
      );
      if (!plan) return;
      const made = await api.createDataset(planDataset(plan));
      const model = await api.saveModel(planModel(plan, (made.dataset as { id: string }).id));
      navigate({ name: "model", id: model.id });
    } catch (err) {
      setTestResults({ error: errorMessage(err, t("Could not make the model.")) });
    }
  };
  useEffect(() => {
    if (!testSpec) {
      setTestResults(null);
      return;
    }
    let live = true;
    setTesting(true);
    const timer = window.setTimeout(() => {
      api
        .runTests({ spec: testSpec })
        .then((answer) => {
          if (!live) return;
          setTestResults(isAnalysis(answer) ? { analysis: answer } : { error: answer.error ?? t("No test applies.") });
        })
        .catch((err: unknown) => live && setTestResults({ error: errorMessage(err, t("Could not run the tests.")) }))
        .finally(() => live && setTesting(false));
    }, SETTLE_MS);
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
    // The spec is compared by value (`testKey`).
  }, [testKey, t]);

  // --- the gallery -----------------------------------------------------------
  const [presetError, setPresetError] = useState<string | null>(null);
  const pickPreset = async (item: GalleryItem) => {
    if (!state.dataset) return;
    setPresetError(null);
    try {
      const answer = await api.suggestPlot({
        dataset: state.dataset,
        assignment: state.assignment,
        preset: item.preset,
      });
      if (answer.error) {
        setPresetError(answer.error);
        return;
      }
      const made = answer.spec as PlotSpec;
      update((s) => ({
        ...s,
        view: "plot",
        assignment: (answer.assignment ?? {}) as Assignment,
        // A preset that reshapes is read again on every drop; any other is
        // its mark.
        preset: item.reshapes ? item.preset : undefined,
        mark: item.reshapes ? undefined : made.layers[0]?.mark,
        extras: { ...s.extras, stat: undefined },
      }));
    } catch (err) {
      setPresetError(errorMessage(err, t("Could not make a plot.")));
    }
  };

  if (loadError) {
    return (
      <div className="an-page">
        <Alert variant="danger">{loadError}</Alert>
      </div>
    );
  }
  if (!datasets) {
    return (
      <div className="an-page">
        <Spinner animation="border" size="sm" />
      </div>
    );
  }

  const columns = shape?.columns ?? [];
  const categorical = columns.filter((c) => c.key || !["int", "float", "decimal"].includes(c.type)).map((c) => c.name);
  const notes = drawn && state.view === "plot" ? plotNotes(drawn.data, t) : [];
  const message = state.view === "plot" ? (suggested?.error ?? drawError) : table?.error;

  return (
    <div className="an-explorer">
      <aside className="an-columns" aria-label={t("Columns")}>
        <Form.Select
          size="sm"
          aria-label={t("Dataset")}
          value={state.dataset ?? ""}
          onChange={(e) => update((s) => pickDataset(s, e.target.value))}
          className="mb-1"
        >
          <option value="" disabled>
            {t("Pick a dataset…")}
          </option>
          {datasets.map((d) => (
            <option key={d.id} value={d.id}>
              {d.name}
            </option>
          ))}
        </Form.Select>
        <div className="small mb-2 d-flex gap-2">
          {dataset && (
            <a href={`#/datasets/${encodeURIComponent(dataset.id)}`}>
              <T text="Edit dataset" />
            </a>
          )}
          <a href="#/datasets/new">
            <T text="New dataset" />
          </a>
        </div>
        {dataset?.error && <Alert variant="warning" className="small p-2">{dataset.error}</Alert>}
        {datasets.length === 0 && (
          <p className="small text-secondary">
            <T text="There are no datasets yet. Create one to explore it." />
          </p>
        )}
        {columns.map((c) => (
          <div
            key={c.name}
            className="an-column"
            draggable
            onDragStart={(e) => e.dataTransfer.setData(DRAG_TYPE, JSON.stringify({ field: c.name }))}
            title={c.key ? t("{type}, a key of {table}", { type: c.type, table: c.key.table }) : c.type}
          >
            <span className="an-column-type">{typeBadge(c)}</span>
            {c.name}
          </div>
        ))}
      </aside>

      <section className="an-explorer-main">
        <div className="an-gallery" role="toolbar" aria-label={t("Gallery")}>
          {gallery.map((g) => (
            <Button
              key={g.preset}
              size="sm"
              variant={state.preset === g.preset ? "primary" : "outline-secondary"}
              disabled={!g.available || !state.dataset}
              title={g.available ? undefined : t("Arrives in {milestone}", { milestone: g.arrives_in ?? "" })}
              onClick={() => void pickPreset(g)}
            >
              {presetName(g.preset, t)}
              {!g.available && <span className="ms-1 small">({g.arrives_in})</span>}
            </Button>
          ))}
        </div>

        <div className="an-zones">
          {ZONES.map((zone) => (
            <DropZone
              key={zone}
              zone={zone}
              state={state}
              columns={columns}
              onChange={update}
            />
          ))}
        </div>

        <div className="d-flex flex-wrap align-items-center gap-2 mb-2">
          <ButtonGroup size="sm" aria-label={t("Plot or table")}>
            <Button
              variant={state.view === "plot" ? "secondary" : "outline-secondary"}
              onClick={() => update((s) => ({ ...s, view: "plot" }))}
            >
              <T text="Plot" />
            </Button>
            <Button
              variant={state.view === "table" ? "secondary" : "outline-secondary"}
              onClick={() => update((s) => ({ ...s, view: "table" }))}
            >
              <T text="Summary table" />
            </Button>
          </ButtonGroup>
          {state.view === "plot" ? (
            <MarkPalette
              chosen={state.mark}
              drawn={drawn?.spec.layers[0]?.mark}
              onPick={(m) => update((s) => pickMark(s, m))}
            />
          ) : (
            <TableControls
              fn={state.table.function}
              totals={state.table.totals}
              onChange={(table) => update((s) => ({ ...s, table }))}
            />
          )}
          <div className="ms-auto d-flex gap-2">
            <Button
              size="sm"
              variant={state.tests.show ? "secondary" : "outline-secondary"}
              aria-pressed={state.tests.show}
              onClick={() => update((s) => setTests(s, { show: !s.tests.show }))}
            >
              <T text="Tests" />
            </Button>
            {state.view === "plot" && (
              <Button size="sm" variant={layersOpen ? "secondary" : "outline-secondary"} onClick={() => setLayersOpen((o) => !o)}>
                <T text="Layers" />
                {state.extras.layers.length > 0 && (
                  <Badge bg="primary" className="ms-1">
                    {state.extras.layers.length}
                  </Badge>
                )}
              </Button>
            )}
            <Button size="sm" variant="outline-secondary" onClick={() => update(clear)} disabled={!state.dataset}>
              <T text="Clear" />
            </Button>
          </div>
        </div>

        {presetError && (
          <Alert variant="warning" className="py-2" dismissible onClose={() => setPresetError(null)}>
            {presetError}
          </Alert>
        )}

        <div className="an-panel">
          <div className="an-output">
            {!state.dataset ? (
              <p className="text-secondary p-3">
                <T text="Pick a dataset, then a plot from the gallery or drag columns onto the drop zones." />
              </p>
            ) : message ? (
              <Alert variant="info" className="m-2">
                {message}
              </Alert>
            ) : state.view === "plot" ? (
              drawn ? (
                <div className={drawing ? "an-plot-box an-stale" : "an-plot-box"}>
                  <PlotView spec={drawn.spec} data={drawn.data} theme={theme} categorical={categorical} />
                </div>
              ) : (
                <Spinner animation="border" size="sm" className="m-3" />
              )
            ) : table?.data ? (
              <SummaryTable data={table.data} />
            ) : (
              <Spinner animation="border" size="sm" className="m-3" />
            )}
          </div>
          {state.tests.show && state.dataset && (
            <TestResults
              analysis={tests?.analysis ?? null}
              error={tests?.error ?? null}
              loading={testing}
              settings={state.tests}
              yCount={(state.assignment.y ?? []).length}
              onChange={(change) => update((s) => setTests(s, change))}
              onOpenAsModel={canOpenAsModel ? () => void openAsModel() : undefined}
            />
          )}
        </div>
        {notes.length > 0 && !message && (
          <ul className="an-notes">
            {notes.map((n) => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        )}
      </section>

      {layersOpen && state.view === "plot" && (
        <LayersPanel
          spec={spec}
          extras={state.extras}
          onChange={(extras) => update((s) => ({ ...s, extras }))}
          onClose={() => setLayersOpen(false)}
        />
      )}
    </div>
  );
}

/** One drop zone: its columns as chips (each binned or removed), a drop
 * target for the column list and for other zones' chips, and a menu for
 * whoever would rather not drag. */
function DropZone({
  zone,
  state,
  columns,
  onChange,
}: {
  zone: Zone;
  state: ExplorerState;
  columns: StageColumn[];
  onChange: (change: (s: ExplorerState) => ExplorerState) => void;
}) {
  const { t } = useT();
  const [over, setOver] = useState(false);
  const fields = onZone(state.assignment, zone);
  const numeric = (name: string) => {
    const c = columns.find((col) => col.name === name);
    return Boolean(c && !c.key && (c.type === "int" || c.type === "float" || c.type === "decimal"));
  };
  const accept = (e: DragEvent) => {
    if (e.dataTransfer.types.includes(DRAG_TYPE)) {
      e.preventDefault();
      setOver(true);
    }
  };
  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    setOver(false);
    try {
      const { field, from } = JSON.parse(e.dataTransfer.getData(DRAG_TYPE)) as { field: string; from?: Zone };
      if (from === zone) return;
      // Shift adds a column to Y rather than replacing it.
      onChange((s) => drop(from ? remove(s, from, field) : s, zone, field, e.shiftKey));
    } catch {
      // Not a column.
    }
  };
  return (
    <div
      className={over ? "an-zone over" : "an-zone"}
      onDragOver={accept}
      onDragEnter={accept}
      onDragLeave={() => setOver(false)}
      onDrop={onDrop}
      aria-label={t("Drop zone {zone}", { zone: zoneName(zone, t) })}
    >
      <div className="an-zone-name">
        {zoneName(zone, t)}
        <Dropdown className="ms-auto">
          <Dropdown.Toggle
            size="sm"
            variant="link"
            className="p-0 an-zone-add"
            disabled={columns.length === 0}
            aria-label={t("Put a column on {zone}", { zone: zoneName(zone, t) })}
            title={zone === "y" ? t("Add a column; several on Y are compared as one variable (or Shift-drop)") : undefined}
          >
            +
          </Dropdown.Toggle>
          <Dropdown.Menu className="an-zone-menu">
            {columns.map((c) => (
              <Dropdown.Item key={c.name} onClick={() => onChange((s) => drop(s, zone, c.name, true))}>
                {c.name}
              </Dropdown.Item>
            ))}
          </Dropdown.Menu>
        </Dropdown>
      </div>
      {fields.map((f) => (
        <span
          key={f.field}
          className="an-chip"
          draggable
          onDragStart={(e) => e.dataTransfer.setData(DRAG_TYPE, JSON.stringify({ field: f.field, from: zone }))}
        >
          {f.field}
          {numeric(f.field) && (
            <button
              type="button"
              className={f.bin ? "an-chip-bin on" : "an-chip-bin"}
              title={f.bin ? t("Binned; click to use the values as they are") : t("Bin the values")}
              aria-pressed={Boolean(f.bin)}
              onClick={() => onChange((s) => toggleBin(s, zone, f.field))}
            >
              {t("bins")}
            </button>
          )}
          <button
            type="button"
            className="an-chip-remove"
            aria-label={t("Take {column} off {zone}", { column: f.field, zone: zoneName(zone, t) })}
            onClick={() => onChange((s) => remove(s, zone, f.field))}
          >
            ×
          </button>
        </span>
      ))}
    </div>
  );
}

/** The mark palette: let the explorer choose, or draw the drop zones as one
 * mark. */
function MarkPalette({
  chosen,
  drawn,
  onPick,
}: {
  chosen: Mark | undefined;
  drawn: Mark | undefined;
  onPick: (mark: Mark | undefined) => void;
}) {
  const { t } = useT();
  return (
    <ButtonGroup size="sm" aria-label={t("Mark")} className="flex-wrap">
      <Button variant={chosen === undefined ? "primary" : "outline-primary"} onClick={() => onPick(undefined)}>
        <T text="Auto" />
      </Button>
      {MARKS.map((m) => (
        <Button
          key={m}
          variant={chosen === m ? "primary" : "outline-primary"}
          className={chosen === undefined && drawn === m ? "an-mark-auto" : undefined}
          onClick={() => onPick(m)}
        >
          {markName(m, t)}
        </Button>
      ))}
    </ButtonGroup>
  );
}

function TableControls({
  fn,
  totals,
  onChange,
}: {
  fn: AggregateFn;
  totals: boolean;
  onChange: (table: { function: AggregateFn; totals: boolean }) => void;
}) {
  const { t } = useT();
  return (
    <div className="d-flex align-items-center gap-2">
      <Form.Select
        size="sm"
        style={{ width: "auto" }}
        aria-label={t("Cells")}
        value={fn}
        onChange={(e) => onChange({ function: e.target.value as AggregateFn, totals })}
      >
        {AGGREGATES.map((f) => (
          <option key={f} value={f}>
            {summaryName(f, t)}
          </option>
        ))}
      </Form.Select>
      <Form.Check
        type="switch"
        id="table-totals"
        label={t("Totals")}
        checked={totals}
        onChange={(e) => onChange({ function: fn, totals: e.target.checked })}
      />
    </div>
  );
}
