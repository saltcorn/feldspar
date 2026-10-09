// The Map workspace (analytics TODO A5.8–A5.13): layers of datasets over a
// base map, styled, selected from and analysed with the toolbox (goals
// document, "Map workspace").
//
// On the left, the layers — top first, as a map's layer list is read — each
// shown or hidden, moved up or down (or dragged), with the settings of the one
// picked below them (`LayerSettings`), and the reference layers. On the right,
// the map, its selection tools, and the attribute table of the picked layer.
//
// "A layer is a dataset, a geometry source and a style. The map never
// computes anything itself": each layer's features and scales come from
// `renderMap`, read again only when what they are made of changes — its
// dataset, geometry, filter, encoding or style — and its name, visibility,
// opacity, legend and popup are the browser's. The toolbox makes datasets
// and adds them as layers; "Save selection as dataset" does too.
//
// The whole map is a panel (`DragHandle`): dragged into a report, it is the
// shown layers as they are styled, drawn there as an image.

import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState, type DragEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import ButtonGroup from "react-bootstrap/ButtonGroup";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import InputGroup from "react-bootstrap/InputGroup";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { ListDatasetsResponse } from "../client";
import { T, useT } from "../i18n";
import { mapPanel } from "../panels/panel";
import { DragHandle } from "../panels/PanelView";
import { useAnnounce, useChanges, usePane } from "../panes";
import { useDocumentTheme } from "../theme";
import type { WorkspaceProps, WorkspaceState } from "../workspaces/WorkspaceFrame";
import { AttributeTable } from "./AttributeTable";
import { LayerSettings } from "./LayerSettings";
import type { MapTool } from "./MapView";
import { ReferenceLayers } from "./ReferenceLayers";
import { layerRequest, type MapLayer, type RenderedLayer, type SourceChoice } from "./spec";
import type { LayerRows } from "./table";
import { ToolDialog, ToolboxMenu } from "./Toolbox";
import type { ToolItem } from "./toolForm";
import {
  addLayer,
  addReference,
  clickFeature,
  dataKey,
  layerById,
  moveLayer,
  placeLayer,
  readMapState,
  removeLayer,
  removeReference,
  selectFound,
  sortBy,
  sortOf,
  specOf,
  updateLayer,
  updateReference,
  type ColumnInfo,
  type MapState,
} from "./workspace";

const MapView = lazy(() => import("./MapView").then((m) => ({ default: m.MapView })));

type DatasetItem = ListDatasetsResponse[number];

/** A layer's data as last read: its key, and the answer or why there is none. */
type Drawn = { key: string; data?: RenderedLayer; loading: boolean; error?: string };

/** What a dragged layer of the layer list carries. */
const LAYER_DRAG = "application/x-feldspar-map-layer";

export function MapWorkspace({ state: raw, setState, name }: WorkspaceProps) {
  const { t } = useT();
  const pane = usePane();
  const changed = useAnnounce();
  const theme = useDocumentTheme();
  const state = useMemo(() => readMapState(raw), [raw]);
  const update = useCallback(
    (change: (s: MapState) => MapState) =>
      setState((current) => ({ ...current, ...change(readMapState(current)) }) as unknown as WorkspaceState),
    [setState],
  );

  const [datasets, setDatasets] = useState<DatasetItem[]>([]);
  const [tools, setTools] = useState<ToolItem[]>([]);
  const [hosts, setHosts] = useState<string[]>([]);
  const [problem, setProblem] = useState<string | null>(null);
  const [notice, setNotice] = useState<{ text: string; dataset?: string } | null>(null);
  // Bumped when a dataset changes on the other side of a split view: every
  // layer is read again (A4.1).
  const [version, setVersion] = useState(0);
  useChanges(["dataset"], () => setVersion((v) => v + 1));

  useEffect(() => {
    let live = true;
    api
      .listDatasets()
      .then((ds) => live && setDatasets(ds))
      .catch((err: unknown) => live && setProblem(errorMessage(err, t("Could not load the datasets."))));
    return () => {
      live = false;
    };
  }, [t, version]);
  useEffect(() => {
    let live = true;
    api
      .listMapTools()
      .then((ts) => live && setTools(ts))
      .catch(() => live && setTools([]));
    api
      .mapSettings()
      .then((s) => live && setHosts(s.hosts))
      .catch(() => live && setHosts([]));
    return () => {
      live = false;
    };
  }, []);
  const columnsOf = useCallback(
    (dataset: string): ColumnInfo[] => (datasets.find((d) => d.id === dataset)?.columns ?? []) as ColumnInfo[],
    [datasets],
  );
  const datasetName = (id: string) => datasets.find((d) => d.id === id)?.name;

  // --- each layer's data -------------------------------------------------------
  const [drawn, setDrawn] = useState<Record<string, Drawn>>({});
  const asked = useRef<Record<string, string>>({});
  const layersKey = JSON.stringify(state.layers);
  useEffect(() => {
    for (const layer of state.layers) {
      const id = layer.id ?? "";
      const key = `${dataKey(layer)}#${version}`;
      if (asked.current[id] === key) continue;
      asked.current[id] = key;
      setDrawn((d) => ({ ...d, [id]: { key, data: d[id]?.data, loading: true } }));
      // Its popup is the browser's to show, so it is not asked about.
      const { popup: _popup, ...drawnLayer } = layer;
      api
        .renderMap({ spec: { layers: [drawnLayer] } })
        .then((answer) => {
          const one = (answer.layers as RenderedLayer[])[0];
          setDrawn((d) => (asked.current[id] === key ? { ...d, [id]: { key, data: one, loading: false } } : d));
        })
        .catch((err: unknown) => {
          const error = errorMessage(err, t("Could not draw the layer."));
          setDrawn((d) => (asked.current[id] === key ? { ...d, [id]: { key, loading: false, error } } : d));
        });
    }
    // The layers are compared by value (`layersKey`).
  }, [layersKey, version, t]);

  // What the map draws: the layers whose data has arrived, in order.
  const referenceKey = JSON.stringify(state.reference);
  const shown = useMemo(() => {
    const layers: MapLayer[] = [];
    const data: RenderedLayer[] = [];
    for (const l of state.layers) {
      const d = drawn[l.id ?? ""]?.data;
      if (d) {
        layers.push(l);
        data.push(d);
      }
    }
    return { spec: { layers, reference: state.reference }, data: { layers: data } };
    // By value: a moved map changes the state, not what is drawn.
  }, [layersKey, referenceKey, drawn]);
  const selectionKey = JSON.stringify(state.selection);
  const selection = useMemo(() => {
    if (!state.selection) return null;
    const layer = shown.spec.layers.findIndex((l) => l.id === state.selection?.layer);
    return layer < 0 ? null : { layer, ids: state.selection.ids };
  }, [selectionKey, shown]);

  // --- the picked layer: its sources and its attribute table ----------------------
  const active = layerById(state, state.active);
  const [sources, setSources] = useState<Record<string, SourceChoice[]>>({});
  useEffect(() => {
    if (!active || sources[active.dataset]) return;
    let live = true;
    api
      .suggestMap({ dataset: active.dataset })
      .then((a) => live && setSources((s) => ({ ...s, [active.dataset]: a.sources as SourceChoice[] })))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, [active?.dataset, sources]);

  const [rows, setRows] = useState<{ key: string; answer?: LayerRows; error?: string } | null>(null);
  const [rowsLoading, setRowsLoading] = useState(false);
  const sort = sortOf(state, active?.id);
  const tableKey = active ? JSON.stringify({ request: layerRequest(active), sort: sort ?? null, version }) : "";
  useEffect(() => {
    if (!active || !state.table.open) return;
    let live = true;
    setRowsLoading(true);
    api
      .layerRows({ layer: layerRequest(active), sort: sort ? { formula: sort.column, descending: sort.descending } : null })
      .then((a) => {
        if (!live) return;
        setRows(a.error ? { key: tableKey, error: a.error } : { key: tableKey, answer: a as unknown as LayerRows });
      })
      .catch((err: unknown) => live && setRows({ key: tableKey, error: errorMessage(err, t("Could not read the rows.")) }))
      .finally(() => live && setRowsLoading(false));
    return () => {
      live = false;
    };
    // By value (`tableKey`).
  }, [tableKey, state.table.open, t]);

  // --- selecting ---------------------------------------------------------------------
  const [tool, setTool] = useState<MapTool>("pick");
  const [distance, setDistance] = useState("1000");
  const [condition, setCondition] = useState("");
  const [selecting, setSelecting] = useState(false);
  const [selectNote, setSelectNote] = useState<string | null>(null);
  const selectBy = async (layer: MapLayer | undefined, by: Record<string, unknown>) => {
    if (!layer?.id) return;
    setSelecting(true);
    setSelectNote(null);
    try {
      const found = await api.selectFeatures({ layer: layerRequest(layer), by });
      if (found.error) {
        setSelectNote(found.error);
        return;
      }
      const ids = found.ids ?? [];
      update((s) => selectFound(s, layer.id as string, ids, found.condition ?? undefined));
      if (found.truncated) {
        setSelectNote(
          t("{count} features match; the first {shown} are selected.", {
            count: (found.count ?? 0).toLocaleString(),
            shown: ids.length.toLocaleString(),
          }),
        );
      } else if (ids.length === 0) {
        setSelectNote(t("Nothing in {layer} matches.", { layer: layer.name ?? "" }));
      }
    } catch (err) {
      setSelectNote(errorMessage(err, t("Could not select.")));
    } finally {
      setSelecting(false);
    }
  };
  const metres = Number(distance);
  const goodDistance = Number.isFinite(metres) && metres > 0;
  const selectedLayer = layerById(state, state.selection?.layer);
  const canNear = Boolean(
    active && selectedLayer && selectedLayer.id !== active.id && goodDistance && state.selection?.ids.length,
  );

  // --- saving the selection ----------------------------------------------------------
  const [saving, setSaving] = useState<{ name: string; error?: string } | null>(null);
  const saveSelection = async () => {
    if (!saving || !selectedLayer || !state.selection) return;
    try {
      const made = await api.saveSelection({
        layer: layerRequest(selectedLayer),
        name: saving.name.trim(),
        ids: state.selection.ids,
        condition: state.selection.condition ?? null,
      });
      const dataset = made.dataset as { id: string; name: string };
      changed("dataset", dataset.id);
      const copy: MapLayer = { ...selectedLayer, dataset: dataset.id, name: dataset.name };
      delete copy.id;
      delete copy.filter;
      update((s) => addLayer(s, copy, dataset.name));
      setSaving(null);
      setNotice({ text: t("Saved the selection as the dataset {name}.", { name: dataset.name }), dataset: dataset.id });
      setVersion((v) => v + 1);
    } catch (err) {
      setSaving({ ...saving, error: errorMessage(err, t("Could not save the selection.")) });
    }
  };

  // --- adding layers -----------------------------------------------------------------
  const addDataset = async (dataset: DatasetItem) => {
    setProblem(null);
    try {
      const suggested = await api.suggestMap({ dataset: dataset.id });
      const layer = (suggested.spec as { layers: MapLayer[] } | null | undefined)?.layers?.[0];
      if (suggested.error || !layer) {
        setProblem(suggested.error ?? t("Could not make a layer of {name}.", { name: dataset.name }));
        return;
      }
      setSources((s) => ({ ...s, [dataset.id]: suggested.sources as SourceChoice[] }));
      update((s) => addLayer(s, layer, dataset.name));
    } catch (err) {
      setProblem(errorMessage(err, t("Could not make a layer of {name}.", { name: dataset.name })));
    }
  };
  const [runningTool, setRunningTool] = useState<ToolItem | null>(null);
  const runTool = async (tool: ToolItem, params: Record<string, unknown>, newName: string | null) => {
    const made = await api.runMapTool({ tool: tool.id, params, name: newName });
    const dataset = made.dataset as { id: string; name: string };
    changed("dataset", dataset.id);
    update((s) => addLayer(s, made.layer as MapLayer, dataset.name));
    setNotice({ text: t("{tool} made the dataset {name}.", { tool: t(tool.label), name: dataset.name }), dataset: dataset.id });
    setVersion((v) => v + 1);
  };

  // --- the layer list's drag and drop ----------------------------------------------------
  const [dragOver, setDragOver] = useState<string | null>(null);
  const onLayerDrop = (e: DragEvent, at: string) => {
    const id = e.dataTransfer.getData(LAYER_DRAG);
    setDragOver(null);
    if (!id) return;
    e.preventDefault();
    update((s) => placeLayer(s, id, at));
  };

  const top = [...state.layers].reverse();
  const activeColumns = active ? columnsOf(active.dataset) : [];

  return (
    <div className="an-mapws">
      <aside className="an-mapws-side" aria-label={t("Layers")}>
        <div className="d-flex align-items-center gap-2 mb-2">
          <strong className="me-auto">
            <T text="Layers" />
          </strong>
          <Dropdown>
            <Dropdown.Toggle size="sm" variant="outline-secondary">
              <T text="Add layer" />
            </Dropdown.Toggle>
            <Dropdown.Menu className="an-mapws-datasets">
              {datasets.length === 0 && (
                <Dropdown.ItemText className="small text-secondary">
                  <T text="There are no datasets yet." />
                </Dropdown.ItemText>
              )}
              {datasets.map((d) => (
                <Dropdown.Item key={d.id} onClick={() => void addDataset(d)}>
                  {d.name}
                </Dropdown.Item>
              ))}
            </Dropdown.Menu>
          </Dropdown>
          <ToolboxMenu tools={tools} disabled={state.layers.length === 0} onPick={setRunningTool} />
        </div>
        {problem && (
          <Alert variant="warning" className="small p-2" dismissible onClose={() => setProblem(null)}>
            {problem}
          </Alert>
        )}
        {notice && (
          <Alert variant="success" className="small p-2" dismissible onClose={() => setNotice(null)}>
            {notice.text}{" "}
            {notice.dataset && (
              <a href={pane.href({ name: "dataset", id: notice.dataset })}>
                <T text="Open it in the Dataset editor" />
              </a>
            )}
          </Alert>
        )}
        {state.layers.length === 0 && (
          <p className="small text-secondary">
            <T text="Add a dataset as a layer. Its geometry is found by itself: a geometry column, longitude and latitude columns, or a key to a table with geometry." />
          </p>
        )}
        <ul className="an-layer-list">
          {top.map((layer, i) => {
            const id = layer.id ?? "";
            const d = drawn[id];
            const refused = d?.data?.data.delivery === "none" ? d.data.data.error : d?.error;
            return (
              <li
                key={id}
                className={[
                  "an-layer-row",
                  id === state.active ? "active" : "",
                  dragOver === id ? "drop" : "",
                ].join(" ")}
                draggable
                onDragStart={(e) => {
                  e.dataTransfer.setData(LAYER_DRAG, id);
                  e.dataTransfer.effectAllowed = "move";
                }}
                onDragOver={(e) => {
                  if (!Array.from(e.dataTransfer.types).includes(LAYER_DRAG)) return;
                  e.preventDefault();
                  setDragOver(id);
                }}
                onDragLeave={() => setDragOver(null)}
                onDrop={(e) => onLayerDrop(e, id)}
              >
                <div className="d-flex align-items-center gap-1">
                  <span className="an-layer-grip" aria-hidden>
                    ⠿
                  </span>
                  <Form.Check
                    aria-label={t("Show {name}", { name: layer.name ?? "" })}
                    checked={layer.visible !== false}
                    onChange={(e) =>
                      update((s) =>
                        updateLayer(s, id, (l) => {
                          const next = { ...l };
                          if (e.target.checked) delete next.visible;
                          else next.visible = false;
                          return next;
                        }),
                      )
                    }
                  />
                  <button
                    type="button"
                    className="an-layer-name"
                    onClick={() => update((s) => ({ ...s, active: id }))}
                    aria-pressed={id === state.active}
                  >
                    {layer.name ?? datasetName(layer.dataset) ?? id}
                  </button>
                  {d?.loading && <Spinner animation="border" size="sm" />}
                  <ButtonGroup size="sm">
                    <Button
                      variant="link"
                      className="p-0 px-1"
                      disabled={i === 0}
                      onClick={() => update((s) => moveLayer(s, id, 1))}
                      aria-label={t("Move {name} up", { name: layer.name ?? "" })}
                    >
                      ▲
                    </Button>
                    <Button
                      variant="link"
                      className="p-0 px-1"
                      disabled={i === top.length - 1}
                      onClick={() => update((s) => moveLayer(s, id, -1))}
                      aria-label={t("Move {name} down", { name: layer.name ?? "" })}
                    >
                      ▼
                    </Button>
                  </ButtonGroup>
                  <Dropdown align="end">
                    <Dropdown.Toggle size="sm" variant="link" className="p-0" aria-label={t("More")} />
                    <Dropdown.Menu>
                      <Dropdown.Item href={pane.href({ name: "dataset", id: layer.dataset })}>
                        <T text="Edit the dataset" />
                      </Dropdown.Item>
                      <Dropdown.Item onClick={() => update((s) => ({ ...s, active: id, table: { ...s.table, open: true } }))}>
                        <T text="Attribute table" />
                      </Dropdown.Item>
                      <Dropdown.Divider />
                      <Dropdown.Item className="text-danger" onClick={() => update((s) => removeLayer(s, id))}>
                        <T text="Remove from the map" />
                      </Dropdown.Item>
                    </Dropdown.Menu>
                  </Dropdown>
                </div>
                {refused && <div className="an-op-error">{refused}</div>}
                {!refused && d?.data && d.data.data.delivery !== "none" && (
                  <div className="small text-secondary ps-4">
                    {d.data.data.count === 1
                      ? t("1 feature")
                      : t("{count} features", { count: d.data.data.count.toLocaleString() })}
                    {d.data.data.delivery === "tiles" && ` · ${t("as tiles")}`}
                  </div>
                )}
              </li>
            );
          })}
        </ul>
        {active && (
          <LayerSettings
            key={active.id}
            layer={active}
            columns={activeColumns}
            sources={sources[active.dataset] ?? []}
            onChange={(change) => update((s) => updateLayer(s, active.id as string, change))}
          />
        )}
        <div className="mt-3 mb-1">
          <strong>
            <T text="Reference layers" />
          </strong>
        </div>
        <ReferenceLayers
          layers={state.reference}
          hosts={hosts}
          onAdd={(ref) => update((s) => addReference(s, ref))}
          onChange={(id, change) => update((s) => updateReference(s, id, change))}
          onRemove={(id) => update((s) => removeReference(s, id))}
          onHostsChanged={setHosts}
        />
      </aside>

      <section className="an-mapws-main">
        <div className="an-mapws-toolbar">
          <ButtonGroup size="sm" aria-label={t("Selection tools")}>
            <Button variant={tool === "pick" ? "secondary" : "outline-secondary"} onClick={() => setTool("pick")}>
              <T text="Select" />
            </Button>
            <Button
              variant={tool === "lasso" ? "secondary" : "outline-secondary"}
              disabled={!active}
              onClick={() => setTool("lasso")}
              title={t("Draw around the features of the picked layer to select them")}
            >
              <T text="Lasso" />
            </Button>
            <Button
              variant={tool === "point" ? "secondary" : "outline-secondary"}
              disabled={!active || !goodDistance}
              onClick={() => setTool("point")}
              title={t("Click the map to select the features of the picked layer within the distance")}
            >
              <T text="Near a point" />
            </Button>
          </ButtonGroup>
          <InputGroup size="sm" style={{ width: "9rem" }}>
            <Form.Control
              aria-label={t("Distance in metres")}
              type="number"
              min={1}
              value={distance}
              onChange={(e) => setDistance(e.target.value)}
            />
            <InputGroup.Text>m</InputGroup.Text>
          </InputGroup>
          {canNear && selectedLayer && active && (
            <Button
              size="sm"
              variant="outline-secondary"
              onClick={() =>
                void selectBy(active, {
                  by: "near_features",
                  layer: layerRequest(selectedLayer),
                  ids: state.selection?.ids ?? [],
                  distance: metres,
                })
              }
            >
              {t("{layer} within {metres} m of the selected {other}", {
                layer: active.name ?? "",
                metres: metres.toLocaleString(),
                other: selectedLayer.name ?? "",
              })}
            </Button>
          )}
          <Form
            className="d-flex gap-1"
            onSubmit={(e) => {
              e.preventDefault();
              if (condition.trim() !== "") void selectBy(active, { by: "condition", formula: condition });
            }}
          >
            <Form.Control
              size="sm"
              className="font-monospace"
              style={{ width: "14rem" }}
              placeholder={t("Select where… (price > 100000)")}
              aria-label={t("Select by a condition")}
              value={condition}
              disabled={!active}
              onChange={(e) => setCondition(e.target.value)}
            />
            <Button size="sm" type="submit" variant="outline-secondary" disabled={!active || condition.trim() === ""}>
              <T text="Select" />
            </Button>
          </Form>
          {selecting && <Spinner animation="border" size="sm" />}
          <div className="ms-auto d-flex gap-2 align-items-center">
            {state.selection && selectedLayer && (
              <>
                <span className="small">
                  {t("{count} selected in {layer}", {
                    count: state.selection.ids.length.toLocaleString(),
                    layer: selectedLayer.name ?? "",
                  })}
                </span>
                <Button size="sm" variant="outline-secondary" onClick={() => update((s) => ({ ...s, selection: null }))}>
                  <T text="Clear" />
                </Button>
                <Button
                  size="sm"
                  variant="outline-primary"
                  onClick={() => setSaving({ name: t("{layer} (selection)", { layer: selectedLayer.name ?? "" }) })}
                >
                  <T text="Save selection as dataset" />
                </Button>
              </>
            )}
            <Button
              size="sm"
              variant={state.table.open ? "secondary" : "outline-secondary"}
              disabled={!active}
              aria-pressed={state.table.open}
              onClick={() => update((s) => ({ ...s, table: { ...s.table, open: !s.table.open } }))}
            >
              <T text="Table" />
            </Button>
            {state.layers.length > 0 && (
              <DragHandle make={() => mapPanel(specOf(state, "visible"), name)} label={t("Drag this map into a report or a dashboard")} />
            )}
          </div>
        </div>
        {selectNote && (
          <Alert variant="info" className="small py-1 px-2 mb-1" dismissible onClose={() => setSelectNote(null)}>
            {selectNote}
          </Alert>
        )}
        <div className="an-mapws-map">
          {state.layers.length === 0 && state.reference.length === 0 ? (
            <p className="text-secondary p-3">
              <T text="Add a layer to start: a dataset with geometry, or longitude and latitude columns, or a key to a table with geometry." />
            </p>
          ) : (
            <Suspense fallback={<Spinner animation="border" size="sm" className="m-3" />}>
              <MapView
                spec={shown.spec}
                data={shown.data}
                theme={theme}
                selection={selection}
                tool={tool}
                initialView={state.view}
                onView={(view) => update((s) => ({ ...s, view }))}
                onFeatureClick={(hit, add) => {
                  const id = hit ? shown.spec.layers[hit.layer]?.id : undefined;
                  if (!hit || !id) {
                    if (!add) update((s) => ({ ...s, selection: null }));
                    return;
                  }
                  update((s) => clickFeature(s, id, hit.id, add));
                }}
                onMapClick={(ll) =>
                  void selectBy(active, {
                    by: "near_point",
                    longitude: ll.lng,
                    latitude: ll.lat,
                    distance: metres,
                  })
                }
                onLasso={(geometry) => void selectBy(active, { by: "shape", geometry })}
              />
            </Suspense>
          )}
        </div>
        {state.table.open && active && (
          <div className="an-mapws-table">
            <AttributeTable
              answer={rows?.key === tableKey ? (rows.answer ?? null) : (rows?.answer ?? null)}
              error={rows?.error ?? null}
              loading={rowsLoading}
              selected={state.selection && state.selection.layer === active.id ? state.selection.ids : []}
              sort={sort}
              onSort={(column) => update((s) => sortBy(s, active.id as string, column))}
              onPick={(id, how) =>
                update((s) =>
                  how.range
                    ? selectFound(s, active.id as string, how.range)
                    : clickFeature(s, active.id as string, id, how.add),
                )
              }
            />
          </div>
        )}
      </section>

      {runningTool && (
        <ToolDialog
          tool={runningTool}
          layers={state.layers}
          active={state.active}
          columnsOf={columnsOf}
          onClose={() => setRunningTool(null)}
          onRun={(params, newName) => runTool(runningTool, params, newName)}
        />
      )}
      {saving && (
        <Modal show onHide={() => setSaving(null)}>
          <Modal.Header closeButton>
            <Modal.Title>
              <T text="Save selection as dataset" />
            </Modal.Title>
          </Modal.Header>
          <Modal.Body>
            <p className="small text-secondary">
              {state.selection?.condition
                ? t("A new dataset based on {dataset}, filtered by the condition the features were selected by.", {
                    dataset: datasetName(selectedLayer?.dataset ?? "") ?? "",
                  })
                : t("A new dataset based on {dataset}, filtered to the {count} selected rows.", {
                    dataset: datasetName(selectedLayer?.dataset ?? "") ?? "",
                    count: state.selection?.ids.length ?? 0,
                  })}
            </p>
            <Form.Control
              aria-label={t("Name")}
              value={saving.name}
              onChange={(e) => setSaving({ name: e.target.value })}
            />
            {saving.error && <Alert variant="warning" className="small mt-2 mb-0">{saving.error}</Alert>}
          </Modal.Body>
          <Modal.Footer>
            <Button variant="secondary" onClick={() => setSaving(null)}>
              <T text="Cancel" />
            </Button>
            <Button disabled={saving.name.trim() === ""} onClick={() => void saveSelection()}>
              <T text="Save" />
            </Button>
          </Modal.Footer>
        </Modal>
      )}
    </div>
  );
}
