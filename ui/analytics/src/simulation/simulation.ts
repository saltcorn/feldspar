// The Simulation workspace's state and what is made from it (analytics TODO
// A7.6–A7.8): a fitted model's inputs set by hand, the prediction and the
// profile curves the server answers for them (`profileModel`), named
// scenarios kept in the state and compared by their predicted distributions
// (`compareScenarios`), scoring a dataset, and the two panels a simulation is
// dragged as.
//
// The state is `sc_analytics::simulate::SimulationState`: `{ model, fit?,
// level?, values, scenarios }`, which the server checks when it is saved (at
// most 20 scenarios, each with its own non-empty name). `values` holds only
// the inputs that were moved; every other input is at its typical value,
// which the server works out from the training data.

import type { DatasetDef, Operation, StageColumn, Translate } from "../datasets/ops";
import { uniqueName } from "../datasets/ops";
import { makePanel, type Panel } from "../panels/panel";

/** The most scenarios a simulation keeps (`simulate::MAX_SCENARIOS`). */
export const MAX_SCENARIOS = 20;

/** The interval's level when none is chosen (`sc_model::DEFAULT_LEVEL`). */
export const DEFAULT_LEVEL = 0.9;

/** The levels offered. */
export const LEVELS = [0.5, 0.8, 0.9, 0.95, 0.99];

/** A named set of input values. Inputs it does not name are typical. */
export type Scenario = { name: string; values: Record<string, unknown> };

/** A Simulation workspace's state. */
export type SimulationState = {
  /** The model, by id. */
  model?: string;
  /** A fixed fit; the model's active one when absent. */
  fit?: string;
  /** The interval's level. */
  level?: number;
  /** The inputs moved from their typical values. */
  values: Record<string, unknown>;
  /** The saved scenarios, in the order they were saved. */
  scenarios: Scenario[];
};

/** What kind of control an input is. */
export type InputKind = "number" | "integer" | "date" | "category" | "boolean";

/** One input of the model, as the server describes it from the training data. */
export type Input = {
  name: string;
  kind: InputKind;
  typical: unknown;
  min?: unknown;
  max?: unknown;
  levels?: unknown[];
  more_levels?: boolean;
};

/** A prediction, with how sure it is as the provider gives it. */
export type Prediction = {
  value: unknown;
  sd?: number;
  lower?: number;
  upper?: number;
  level?: number;
  probability?: number;
  probabilities?: Record<string, number>;
};

/** How the prediction changes along one input. */
export type Curve = { input: string; points: { x: unknown; prediction: Prediction }[] };

/** The fit a simulation used. */
export type FitInfo = { id: string; name: string; active: boolean; dataset_changed?: boolean };

/** `profileModel`'s answer. */
export type Profile = {
  model: string;
  fit: FitInfo;
  outcome: string;
  uncertainty: string;
  level: number;
  inputs: Input[];
  values: Record<string, unknown>;
  prediction: Prediction;
  curves: Curve[];
};

/** How a scenario's prediction is distributed. */
export type Spread =
  | { kind: "draws"; draws: number[]; total: number; density: number[] }
  | { kind: "interval"; df: number | null; density: number[] }
  | { kind: "classes"; probabilities: Record<string, number> }
  | { kind: "point" };

/** One scenario's prediction. */
export type ScenarioResult = { name: string; values: Record<string, unknown>; prediction: Prediction; spread: Spread };

/** `compareScenarios`'s answer. */
export type Comparison = {
  model: string;
  fit: FitInfo;
  outcome: string;
  uncertainty: string;
  level: number;
  inputs: Input[];
  grid: number[];
  scenarios: ScenarioResult[];
};

function isObject(v: unknown): v is Record<string, unknown> {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}

/** The state read from anything stored, with what is missing filled in. */
export function readSimState(raw: unknown): SimulationState {
  const s = isObject(raw) ? raw : {};
  const out: SimulationState = {
    values: isObject(s.values) ? { ...s.values } : {},
    scenarios: Array.isArray(s.scenarios)
      ? s.scenarios
          .filter((x): x is Record<string, unknown> => isObject(x) && typeof x.name === "string" && x.name.trim() !== "")
          .map((x) => ({ name: (x.name as string).trim(), values: isObject(x.values) ? { ...x.values } : {} }))
      : [],
  };
  if (typeof s.model === "string" && s.model !== "") out.model = s.model;
  if (typeof s.fit === "string" && s.fit !== "") out.fit = s.fit;
  if (typeof s.level === "number" && s.level > 0 && s.level < 1) out.level = s.level;
  return out;
}

/** Simulate another model: its inputs are not this one's, so the values and
 * scenarios go. */
export function pickModel(state: SimulationState, model: string): SimulationState {
  if (state.model === model) return state;
  return { values: {}, scenarios: [], model, ...(state.level ? { level: state.level } : {}) };
}

/** Set one input; `undefined` puts it back at its typical value. */
export function setValue(state: SimulationState, input: string, value: unknown): SimulationState {
  const values = { ...state.values };
  if (value === undefined) delete values[input];
  else values[input] = value;
  return { ...state, values };
}

/** Every input back at its typical value. */
export function resetValues(state: SimulationState): SimulationState {
  return { ...state, values: {} };
}

/** Save the current inputs as `name`. An existing scenario of that name
 * takes the new values in its place; a new one goes at the end. Refused, with
 * the sentence, for an empty name or one more than the most a simulation
 * keeps. */
export function saveScenario(
  state: SimulationState,
  name: string,
  t: Translate,
): { state: SimulationState; error?: string } {
  const trimmed = name.trim();
  if (trimmed === "") return { state, error: t("Give the scenario a name.") };
  const values = { ...state.values };
  const at = state.scenarios.findIndex((s) => s.name === trimmed);
  if (at >= 0) {
    const scenarios = state.scenarios.map((s, i) => (i === at ? { name: trimmed, values } : s));
    return { state: { ...state, scenarios } };
  }
  if (state.scenarios.length >= MAX_SCENARIOS) {
    return { state, error: t("A simulation keeps at most {n} scenarios: delete one first.", { n: MAX_SCENARIOS }) };
  }
  return { state: { ...state, scenarios: [...state.scenarios, { name: trimmed, values }] } };
}

/** Forget a scenario. */
export function removeScenario(state: SimulationState, name: string): SimulationState {
  return { ...state, scenarios: state.scenarios.filter((s) => s.name !== name) };
}

/** Set the inputs to a scenario's. */
export function applyScenario(state: SimulationState, name: string): SimulationState {
  const found = state.scenarios.find((s) => s.name === name);
  return found ? { ...state, values: { ...found.values } } : state;
}

/** A name no saved scenario has: "Scenario 1", "Scenario 2"… */
export function nextScenarioName(state: SimulationState, t: Translate): string {
  const taken = new Set(state.scenarios.map((s) => s.name));
  for (let n = 1; ; n += 1) {
    const name = t("Scenario {n}", { n });
    if (!taken.has(name)) return name;
  }
}

/** The part of a listed model the picker reads. */
type ListedModel = {
  id: string;
  name: string;
  active_instance?: { status: string } | null;
  last_fit?: { status: string } | null;
};

/** The models that can be simulated — those with a fit to predict with —
 * by name. */
export function simulatable<M extends ListedModel>(models: M[]): M[] {
  return models
    .filter((m) => m.active_instance?.status === "fitted" || m.last_fit?.status === "fitted")
    .sort((a, b) => a.name.localeCompare(b.name));
}

// --- the inputs ----------------------------------------------------------------

/** An input's current value: the one set, or its typical value. */
export function currentValue(input: Input, values: Record<string, unknown>): unknown {
  const v = values[input.name];
  return v === undefined || v === null ? input.typical : v;
}

/** Whether an input has been moved from its typical value. */
export function isMoved(input: Input, values: Record<string, unknown>): boolean {
  const v = values[input.name];
  return v !== undefined && v !== null && JSON.stringify(v) !== JSON.stringify(input.typical);
}

/** A slider's step for a number input: about a hundredth of its range,
 * rounded to one significant figure. One for an integer. */
export function sliderStep(input: Input): number {
  if (input.kind === "integer") return 1;
  const lo = Number(input.min);
  const hi = Number(input.max);
  const span = hi - lo;
  if (!Number.isFinite(span) || span <= 0) return 1;
  const raw = span / 100;
  const magnitude = 10 ** Math.floor(Math.log10(raw));
  return Math.round(raw / magnitude) * magnitude;
}

/** A level as a control shows it; a level is a value of the column. */
export function levelKey(level: unknown): string {
  return typeof level === "string" ? level : JSON.stringify(level);
}

/** The value a control's text stands for, as the input's kind is sent:
 * `undefined` when it is not one (an empty box, a word in a number). */
export function parseControl(input: Input, raw: string): unknown {
  switch (input.kind) {
    case "number": {
      const n = Number(raw);
      return raw.trim() === "" || !Number.isFinite(n) ? undefined : n;
    }
    case "integer": {
      const n = Number(raw);
      return raw.trim() === "" || !Number.isFinite(n) ? undefined : Math.round(n);
    }
    case "date":
      return /^\d{4}-\d{2}-\d{2}/.test(raw) ? raw : undefined;
    case "boolean":
      return raw === "true" ? true : raw === "false" ? false : undefined;
    case "category":
      return (input.levels ?? []).find((l) => levelKey(l) === raw);
  }
}

// --- words for numbers ------------------------------------------------------------

/** A number with a sensible number of digits: four significant figures, and
 * thousands separated. */
export function formatNumber(n: number, locale?: string): string {
  if (!Number.isFinite(n)) return String(n);
  const abs = Math.abs(n);
  const digits = abs >= 1000 ? 0 : abs >= 1 ? Math.max(0, 3 - Math.floor(Math.log10(abs))) : 4;
  return new Intl.NumberFormat(locale, { maximumFractionDigits: digits }).format(n);
}

/** Any value of an input or a prediction, in words. */
export function formatValue(v: unknown, locale?: string): string {
  if (typeof v === "number") return formatNumber(v, locale);
  if (typeof v === "boolean") return v ? "true" : "false";
  if (v === null || v === undefined) return "—";
  return typeof v === "string" ? v : JSON.stringify(v);
}

/** The interval of a prediction, in words, when it has one. */
export function intervalText(p: Prediction, t: Translate, locale?: string): string | null {
  if (typeof p.lower !== "number" || typeof p.upper !== "number") return null;
  const level = Math.round((p.level ?? DEFAULT_LEVEL) * 100);
  return t("{level} % interval {lower} to {upper}", {
    level,
    lower: formatNumber(p.lower, locale),
    upper: formatNumber(p.upper, locale),
  });
}

/** A classifier's probability of the class it predicts, in words. */
export function probabilityText(p: Prediction, t: Translate): string | null {
  const prob = typeof p.probability === "number" ? p.probability : classProbability(p);
  if (prob === null) return null;
  return t("probability {p} %", { p: Math.round(prob * 1000) / 10 });
}

function classProbability(p: Prediction): number | null {
  if (!p.probabilities) return null;
  const v = p.probabilities[String(p.value)];
  return typeof v === "number" ? v : null;
}

// --- scoring (A7.7) ---------------------------------------------------------------

/** The dataset Score makes: one based on `source`, adding the model's
 * prediction (and its interval) to every row with a Model predictions
 * operation, its column named so that it is not one `source` has. */
export function scoreDefinition(
  source: { id: string; name: string; columns: Pick<StageColumn, "name">[] },
  model: { name: string },
  options: { fit?: string; level?: number },
  t: Translate,
): Omit<DatasetDef, "id" | "description"> {
  const taken = source.columns.map((c) => c.name);
  const params: Record<string, unknown> = {
    model: model.name,
    name: uniqueName("predicted", taken),
    interval: true,
  };
  if (options.fit) params.fit = options.fit;
  if (options.level !== undefined && options.level !== DEFAULT_LEVEL) params.level = options.level;
  const op: Operation = { id: "score", enabled: true, kind: "model_predictions", params };
  return {
    name: t("{dataset} scored by {model}", { dataset: source.name, model: model.name }),
    base: { kind: "dataset", dataset: source.id },
    operations: [op],
  };
}

/** `name`, or `name (2)`, `name (3)`… — whichever no dataset is called. */
export function freeName(name: string, taken: string[]): string {
  if (!taken.includes(name)) return name;
  let n = 2;
  while (taken.includes(`${name} (${n})`)) n += 1;
  return `${name} (${n})`;
}

// --- panels (A7.8) ----------------------------------------------------------------

/** What a simulation panel is made from: the model, fit and level, and the
 * inputs and scenarios as they are when it is dragged. */
export type SimulationContent = {
  model: string;
  fit?: string;
  level?: number;
  values: Record<string, unknown>;
  scenarios: Scenario[];
};

function contentOf(state: SimulationState): SimulationContent | null {
  if (!state.model) return null;
  const content: SimulationContent = {
    model: state.model,
    values: JSON.parse(JSON.stringify(state.values)) as Record<string, unknown>,
    scenarios: JSON.parse(JSON.stringify(state.scenarios)) as Scenario[],
  };
  if (state.fit) content.fit = state.fit;
  if (state.level !== undefined) content.level = state.level;
  return content;
}

/** The profiler as a panel: on a dashboard its inputs can be moved; in a
 * report it shows the curves at these inputs and the saved scenarios. `null`
 * with no model chosen. */
export function profilerPanel(state: SimulationState, model: string | undefined, t: Translate): Panel | null {
  const content = contentOf(state);
  if (!content) return null;
  return makePanel({ kind: "profiler", content }, model ? t("Profiler — {model}", { model }) : t("Profiler"));
}

/** The scenarios compared, as a panel. `null` with no model or no scenario. */
export function scenariosPanel(state: SimulationState, model: string | undefined, t: Translate): Panel | null {
  const content = contentOf(state);
  if (!content || content.scenarios.length === 0) return null;
  return makePanel(
    { kind: "scenarios", content },
    model ? t("Scenarios — {model}", { model }) : t("Scenarios"),
  );
}

/** A panel's content read back as a state, for drawing it. */
export function stateOfContent(content: SimulationContent): SimulationState {
  return readSimState(content);
}

/** The scenarios a comparison sends: each saved one, and — when it differs
 * from all of them — the current inputs as "Current". */
export function scenariosToCompare(state: SimulationState, t: Translate): Scenario[] {
  const current = JSON.stringify(state.values);
  const saved = state.scenarios.map((s) => ({ name: s.name, values: s.values }));
  if (saved.length >= MAX_SCENARIOS || saved.some((s) => JSON.stringify(s.values) === current)) return saved;
  const name = freeName(t("Current"), saved.map((s) => s.name));
  return [...saved, { name, values: { ...state.values } }];
}
