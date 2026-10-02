// The Data explorer's state, and what is made of it (analytics TODO A2.9,
// A2.10, A2.14).
//
// What the workspace stores is what the person chose — the dataset, the
// columns on each drop zone, the mark palette's choice, the gallery preset
// that reshapes, plot or table, what the layers panel added, and the tests'
// settings (shown, paired, the value a mean is tested against) — never the
// spec: the spec is the server's answer to the drop zones (`suggestPlot`)
// with the layers panel's changes laid over it (`composeSpec`). So a dataset
// whose columns change still opens, and a later milestone's better rules
// apply to an old workspace.
//
// Every function here is pure: the screen calls them, and the tests do too.

import type { StageColumn, StageShape } from "../datasets/ops";
import type {
  AggregateFn,
  Cell,
  Channel,
  Coord,
  FieldDef,
  Layer,
  Mark,
  PlotSpec,
  Reference,
  Scale,
  Stat,
  TableSpec,
} from "../plot/spec";

/** A drop zone: a channel. */
export type Zone = Channel;

/** The drop zones, in the order the explorer shows them. */
export const ZONES: Zone[] = ["x", "y", "color", "size", "shape", "label", "row", "column", "wrap"];

/** The columns on the drop zones: several on Y, one on each other. */
export type Assignment = Partial<Record<Exclude<Zone, "y">, FieldDef>> & { y?: FieldDef[] };

/** A layer the layers panel added. Channels it does not set are taken from
 * the plot's first layer; one set to `null` is not taken. */
export type ExtraLayer = {
  mark: Mark;
  stat?: Stat;
  encoding?: Partial<Record<"x" | "y" | "color" | "size" | "shape" | "label", FieldDef | null>>;
};

/** What the layers panel changed. */
export type Extras = {
  /** The first layer's stat, instead of the one the explorer chose. */
  stat?: Stat;
  layers: ExtraLayer[];
  scales: Partial<Record<"x" | "y" | "color" | "size", Scale>>;
  references: Reference[];
  /** The coordinates, instead of the ones the explorer chose. */
  coord?: Coord;
};

/** The hypothesis tests beside the plot (A2.12–A2.14): whether they are
 * shown, whether two columns on Y are paired measurements, and the value a
 * single number's mean is tested against. */
export type TestsState = { show: boolean; paired: boolean; mu: number };

/** The explorer's whole state, as its workspace stores it. */
export type ExplorerState = {
  dataset?: string;
  assignment: Assignment;
  /** The mark palette's choice; the explorer chooses when absent. */
  mark?: Mark;
  /** A gallery preset that reshapes the data, read again on every drop. */
  preset?: string;
  view: "plot" | "table";
  table: { function: AggregateFn; totals: boolean };
  extras: Extras;
  tests: TestsState;
};

/** An empty layers panel. */
export function noExtras(): Extras {
  return { layers: [], scales: {}, references: [] };
}

function isObject(v: unknown): v is Record<string, unknown> {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}

function fieldDef(v: unknown): FieldDef | undefined {
  if (!isObject(v) || typeof v.field !== "string") return undefined;
  return isObject(v.bin) ? { field: v.field, bin: v.bin as FieldDef["bin"] } : { field: v.field };
}

/** The state a workspace stored, read tolerantly: anything missing or
 * malformed is the default. */
export function readState(raw: unknown): ExplorerState {
  const s = isObject(raw) ? raw : {};
  const a = isObject(s.assignment) ? s.assignment : {};
  const assignment: Assignment = {};
  for (const zone of ZONES) {
    if (zone === "y") {
      const ys = Array.isArray(a.y) ? a.y.map(fieldDef).filter((f): f is FieldDef => Boolean(f)) : [];
      if (ys.length > 0) assignment.y = ys;
    } else {
      const f = fieldDef(a[zone]);
      if (f) assignment[zone] = f;
    }
  }
  const table = isObject(s.table) ? s.table : {};
  const extras = isObject(s.extras) ? s.extras : {};
  const tests = isObject(s.tests) ? s.tests : {};
  return {
    dataset: typeof s.dataset === "string" ? s.dataset : undefined,
    assignment,
    mark: typeof s.mark === "string" ? (s.mark as Mark) : undefined,
    preset: typeof s.preset === "string" ? s.preset : undefined,
    view: s.view === "table" ? "table" : "plot",
    table: {
      function: typeof table.function === "string" ? (table.function as AggregateFn) : "mean",
      totals: table.totals !== false,
    },
    extras: {
      stat: isObject(extras.stat) ? (extras.stat as Stat) : undefined,
      layers: Array.isArray(extras.layers) ? (extras.layers.filter(isObject) as ExtraLayer[]) : [],
      scales: isObject(extras.scales) ? (extras.scales as Extras["scales"]) : {},
      references: Array.isArray(extras.references) ? (extras.references.filter(isObject) as Reference[]) : [],
      coord: typeof extras.coord === "string" ? (extras.coord as Coord) : undefined,
    },
    tests: {
      show: tests.show !== false,
      paired: tests.paired === true,
      mu: typeof tests.mu === "number" && Number.isFinite(tests.mu) ? tests.mu : 0,
    },
  };
}

/** The columns on a zone. */
export function onZone(a: Assignment, zone: Zone): FieldDef[] {
  if (zone === "y") return a.y ?? [];
  const f = a[zone];
  return f ? [f] : [];
}

/** Drop `field` on `zone`, in place of what is there — or, on Y with `add`,
 * beside it: several columns on Y are compared as one variable. */
export function drop(state: ExplorerState, zone: Zone, field: string, add = false): ExplorerState {
  const a = { ...state.assignment };
  if (zone === "y") {
    const ys = add ? (a.y ?? []) : [];
    if (ys.some((f) => f.field === field)) return state;
    a.y = [...ys, { field }];
  } else {
    a[zone] = { field };
  }
  return { ...state, assignment: a };
}

/** Take `field` off `zone`. */
export function remove(state: ExplorerState, zone: Zone, field: string): ExplorerState {
  const a = { ...state.assignment };
  if (zone === "y") {
    const ys = (a.y ?? []).filter((f) => f.field !== field);
    if (ys.length > 0) a.y = ys;
    else delete a.y;
  } else if (a[zone]?.field === field) {
    delete a[zone];
  }
  return { ...state, assignment: a };
}

/** Bin `field` on `zone`, or stop binning it. */
export function toggleBin(state: ExplorerState, zone: Zone, field: string): ExplorerState {
  const flip = (f: FieldDef): FieldDef => (f.field !== field ? f : f.bin ? { field: f.field } : { field: f.field, bin: {} });
  const a = { ...state.assignment };
  if (zone === "y") a.y = (a.y ?? []).map(flip);
  else if (a[zone]) a[zone] = flip(a[zone] as FieldDef);
  return { ...state, assignment: a };
}

/** Start again on the same dataset: nothing dropped, nothing chosen. */
export function clear(state: ExplorerState): ExplorerState {
  return {
    ...state,
    assignment: {},
    mark: undefined,
    preset: undefined,
    extras: noExtras(),
    tests: { ...state.tests, paired: false, mu: 0 },
  };
}

/** Change the tests' settings. */
export function setTests(state: ExplorerState, change: Partial<TestsState>): ExplorerState {
  return { ...state, tests: { ...state.tests, ...change } };
}

/** Explore another dataset: its columns are not this one's. */
export function pickDataset(state: ExplorerState, dataset: string): ExplorerState {
  if (state.dataset === dataset) return state;
  return { ...clear(state), dataset };
}

/** Choose a mark from the palette (`undefined`: let the explorer choose). A
 * reshaping preset gives way to it. */
export function pickMark(state: ExplorerState, mark: Mark | undefined): ExplorerState {
  return { ...state, mark, preset: undefined, extras: { ...state.extras, stat: undefined } };
}

/** Whether a column is a number to summarise rather than a value to group by. */
export function isMeasure(column: StageColumn | undefined): boolean {
  if (!column || column.key) return false;
  return column.type === "int" || column.type === "float" || column.type === "decimal";
}

function needsBin(column: StageColumn | undefined, f: FieldDef): FieldDef {
  if (!column || f.bin || column.key) return f;
  return column.type === "float" || column.type === "decimal" ? { ...f, bin: {} } : f;
}

/** The summary table of the same drop zones (A2.8): X, Facet rows and Wrap
 * are its rows; Color and Facet columns its columns; each number on Y a cell
 * (summarised by `function`), and a category on Y another column. A number
 * with many values on rows or columns is binned. */
export function tableSpecOf(state: ExplorerState, shape: StageShape | null): TableSpec | null {
  if (!state.dataset) return null;
  const a = state.assignment;
  const column = (f: FieldDef) => shape?.columns.find((c) => c.name === f.field);
  const dim = (f: FieldDef) => needsBin(column(f), f);
  const rows = [a.x, a.row, a.wrap].filter((f): f is FieldDef => Boolean(f)).map(dim);
  const columns = [a.color, a.column].filter((f): f is FieldDef => Boolean(f)).map(dim);
  const cells: Cell[] = [];
  for (const f of a.y ?? []) {
    if (isMeasure(column(f)) && !f.bin) {
      cells.push({ field: f.field, function: state.table.function });
    } else if (!columns.some((c) => c.field === f.field) && !rows.some((r) => r.field === f.field)) {
      columns.push(dim(f));
    }
  }
  return {
    data: { kind: "dataset", dataset: state.dataset },
    rows,
    columns,
    cells,
    totals: state.table.totals,
  };
}

// --- the layers panel ----------------------------------------------------------------

/** The channels a layer added in the panel takes from the first layer, by
 * its stat: a density or a count makes its own Y. */
function inherited(stat: Stat | undefined): ("x" | "y" | "color")[] {
  switch (stat?.kind ?? "identity") {
    case "density":
    case "count":
      return ["x", "color"];
    case "correlation":
      return [];
    default:
      return ["x", "y", "color"];
  }
}

/** The channels a stat reads as values, which therefore are not binned. */
function readsAsValues(stat: Stat | undefined): ("x" | "y")[] {
  switch (stat?.kind ?? "identity") {
    case "density":
      return ["x"];
    case "smooth":
      return ["x", "y"];
    case "summary":
    case "boxplot":
    case "quantiles":
    case "aggregate":
      return ["y"];
    default:
      return [];
  }
}

/** The plot: the explorer's spec with the layers panel's changes laid over
 * it. */
export function composeSpec(suggested: PlotSpec, extras: Extras): PlotSpec {
  const layers = suggested.layers.map((l, i) => (i === 0 && extras.stat ? { ...l, stat: extras.stat } : l));
  const first = layers[0];
  for (const extra of extras.layers) {
    const encoding: Layer["encoding"] = {};
    for (const c of inherited(extra.stat)) {
      const f = first?.encoding[c];
      if (f) encoding[c] = readsAsValues(extra.stat).includes(c as "x") ? { field: f.field } : f;
    }
    for (const [c, f] of Object.entries(extra.encoding ?? {})) {
      const key = c as keyof Layer["encoding"];
      if (f) encoding[key] = f;
      else delete encoding[key];
    }
    const layer: Layer = { mark: extra.mark, encoding };
    if (extra.stat && extra.stat.kind !== "identity") layer.stat = extra.stat;
    layers.push(layer);
  }
  const spec: PlotSpec = { ...suggested, layers };
  const scales = { ...(suggested.scales ?? {}) };
  for (const [c, s] of Object.entries(extras.scales)) {
    if (s) scales[c as "x"] = { ...(scales[c as "x"] ?? {}), ...s };
  }
  if (Object.keys(scales).length > 0) spec.scales = scales;
  if (extras.references.length > 0) spec.references = [...(suggested.references ?? []), ...extras.references];
  if (extras.coord) spec.coord = extras.coord;
  return spec;
}

/** The layers the panel offers to add, each a mark and a stat. */
export const LAYER_KINDS: { id: string; mark: Mark; stat: Stat }[] = [
  { id: "linear", mark: "line", stat: { kind: "smooth", method: "linear" } },
  { id: "loess", mark: "line", stat: { kind: "smooth", method: "loess" } },
  { id: "points", mark: "point", stat: { kind: "identity" } },
  { id: "line", mark: "line", stat: { kind: "identity" } },
  { id: "mean", mark: "errorbar", stat: { kind: "summary" } },
  { id: "density", mark: "line", stat: { kind: "density" } },
  { id: "counts", mark: "text", stat: { kind: "count" } },
];

/** Add one of the [`LAYER_KINDS`]. */
export function addLayer(extras: Extras, id: string): Extras {
  const kind = LAYER_KINDS.find((k) => k.id === id);
  if (!kind) return extras;
  return { ...extras, layers: [...extras.layers, { mark: kind.mark, stat: kind.stat }] };
}

/** Remove the added layer at `index`. */
export function removeLayer(extras: Extras, index: number): Extras {
  return { ...extras, layers: extras.layers.filter((_, i) => i !== index) };
}

/** Change the added layer at `index`. */
export function updateLayer(extras: Extras, index: number, change: Partial<ExtraLayer>): Extras {
  return { ...extras, layers: extras.layers.map((l, i) => (i === index ? { ...l, ...change } : l)) };
}

/** Set (or, with `undefined`, clear) a scale's settings. */
export function setScale(extras: Extras, channel: "x" | "y" | "color" | "size", scale: Scale | undefined): Extras {
  const scales = { ...extras.scales };
  if (scale && Object.keys(scale).length > 0) scales[channel] = scale;
  else delete scales[channel];
  return { ...extras, scales };
}

/** Add a reference line. A number typed as text is a number. */
export function addReference(extras: Extras, channel: "x" | "y", value: string, label?: string): Extras {
  const trimmed = value.trim();
  if (trimmed === "") return extras;
  const n = Number(trimmed);
  const ref: Reference = { channel, value: Number.isFinite(n) ? n : trimmed };
  if (label?.trim()) ref.label = label.trim();
  return { ...extras, references: [...extras.references, ref] };
}

/** Remove the reference line at `index`. */
export function removeReference(extras: Extras, index: number): Extras {
  return { ...extras, references: extras.references.filter((_, i) => i !== index) };
}

/** The stats the panel offers for a layer, by kind: what each needs is the
 * server's to check. */
export function defaultStat(kind: Stat["kind"]): Stat {
  switch (kind) {
    case "aggregate":
      return { kind: "aggregate", function: "mean" };
    case "quantiles":
      return { kind: "quantiles", probabilities: [0.1, 0.5, 0.9] };
    case "boxplot":
      return { kind: "boxplot" };
    case "summary":
      return { kind: "summary" };
    case "density":
      return { kind: "density" };
    case "smooth":
      return { kind: "smooth", method: "linear" };
    case "count":
      return { kind: "count" };
    default:
      return { kind: "identity" };
  }
}
