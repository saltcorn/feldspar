// The Checklists screens' interpretation (TODO-checklists C1.16): what a run's
// status looks like, who wrote a step, what is waiting for a person, and how a
// step type's `params` spec becomes a form and the form becomes `params` again.
//
// Pure functions, so the rules the screens depend on — an agent's step is not
// pending once a person accepts it, an empty box is not a parameter, a JSON box
// that does not parse is an error rather than a string — are tested without a
// browser.

import type {
  GetChecklistResponse,
  ListChecklistStepTypesResponse,
  ListChecklistUsersResponse,
} from "./client";
import type { Tone } from "./layout";

export type ChecklistDetail = GetChecklistResponse;
export type ChecklistStep = ChecklistDetail["steps"][number];
export type ChecklistProposal = ChecklistDetail["proposals"][number];
export type ChecklistRun = ChecklistDetail["runs"][number];
export type ChecklistRunStep = ChecklistRun["steps"][number];
export type StepType = ListChecklistStepTypesResponse[number];
export type ParamSpec = StepType["params"][number];
export type CheckUser = ListChecklistUsersResponse[number];

/** The tone of a run's status badge. `incomplete` is yellow, not green: steps
 * the host could not run were skipped, and a checklist never goes green by not
 * running (CHECKLISTS.md §1). */
export function runTone(status: string | null | undefined): Tone {
  switch (status) {
    case "passed":
      return "green";
    case "failed":
    case "error":
      return "red";
    case "incomplete":
      return "yellow";
    default:
      return "secondary";
  }
}

/** The tone of one step's outcome in a run. */
export function outcomeTone(outcome: string): Tone {
  switch (outcome) {
    case "pass":
      return "green";
    case "fail":
    case "error":
      return "red";
    case "skipped":
      return "yellow";
    default:
      return "secondary";
  }
}

/** Who wrote a step or a proposal, parsed from `human:<user id>` /
 * `agent:<run id>`. */
export type Author = { kind: "human" | "agent" | "other"; id: string };

export function parseAuthor(author: string): Author {
  const colon = author.indexOf(":");
  const kind = colon < 0 ? "" : author.slice(0, colon);
  const id = colon < 0 ? author : author.slice(colon + 1);
  if (kind === "human" || kind === "agent") return { kind, id };
  return { kind: "other", id: author };
}

/** An author as an admin reads it: a person by email where they can be looked
 * up, an agent by the start of the run that wrote it. */
export function authorLabel(
  author: string,
  users: Pick<CheckUser, "id" | "email">[],
): Author & { label: string } {
  const parsed = parseAuthor(author);
  if (parsed.kind === "human") {
    return { ...parsed, label: users.find((u) => u.id === parsed.id)?.email ?? parsed.id };
  }
  if (parsed.kind === "agent") return { ...parsed, label: parsed.id.slice(0, 8) };
  return { ...parsed, label: parsed.id };
}

/** The steps waiting for a person: an agent's steps nobody has accepted yet
 * (§4 rule 2). A person's own steps are accepted by being written. */
export function pendingSteps(steps: ChecklistStep[]): ChecklistStep[] {
  return steps.filter((s) => parseAuthor(s.author).kind === "agent" && !s.accepted);
}

/** How many things are waiting for a person on a checklist: its proposals and
 * its unaccepted agent steps — the same count the list's `pending` carries. */
export function pendingCount(detail: Pick<ChecklistDetail, "steps" | "proposals">): number {
  return pendingSteps(detail.steps).length + detail.proposals.length;
}

/** The steps in run order. */
export function orderedSteps(steps: ChecklistStep[]): ChecklistStep[] {
  return [...steps].sort((a, b) => a.position - b.position);
}

/** The step order after moving the one at `index` by `delta` places, or `null`
 * when that would move it off either end. */
export function moveStep(order: string[], index: number, delta: number): string[] | null {
  const to = index + delta;
  if (index < 0 || index >= order.length || to < 0 || to >= order.length) return null;
  const next = [...order];
  const [moved] = next.splice(index, 1);
  next.splice(to, 0, moved);
  return next;
}

/** The last run's result for one step, if the newest run reached it — what a
 * proposal is shown beside, so the admin sees the failure it claims to fix. */
export function lastResultFor(
  stepId: string | null | undefined,
  runs: ChecklistRun[],
): ChecklistRunStep | null {
  if (!stepId || runs.length === 0) return null;
  return runs[0].steps.find((s) => s.step === stepId) ?? null;
}

/** What a proposal would do, as the parts its one-line summary needs: which
 * step (1-based, where it still exists) and, for `run_as`, whom. The screen
 * words it, so each wording is its own translatable message. */
export type ProposalSummary =
  | { kind: "edit" | "insert_before"; step: number | null }
  | { kind: "reorder" }
  | { kind: "run_as"; user: string | null }
  | { kind: "other"; name: string };

export function proposalSummary(
  proposal: Pick<ChecklistProposal, "kind" | "step" | "change">,
  steps: ChecklistStep[],
  users: Pick<CheckUser, "id" | "email">[],
): ProposalSummary {
  const index = orderedSteps(steps).findIndex((s) => s.id === proposal.step);
  const step = index < 0 ? null : index + 1;
  const change = (proposal.change ?? {}) as Record<string, unknown>;
  switch (proposal.kind) {
    case "edit":
    case "insert_before":
      return { kind: proposal.kind, step };
    case "reorder":
      return { kind: "reorder" };
    case "run_as": {
      const who = typeof change.run_as === "string" && change.run_as !== "" ? change.run_as : null;
      return { kind: "run_as", user: who ? (users.find((u) => u.id === who)?.email ?? who) : null };
    }
    default:
      return { kind: "other", name: proposal.kind };
  }
}

/** The step a proposal would write, for an `edit` or an `insert_before`. */
export function proposedStep(
  proposal: Pick<ChecklistProposal, "change">,
): { type: string; params: unknown; label?: string; as?: string | null } | null {
  const change = (proposal.change ?? {}) as Record<string, unknown>;
  const step = change.step as Record<string, unknown> | undefined;
  if (!step || typeof step.type !== "string") return null;
  return {
    type: step.type,
    params: step.params ?? {},
    label: typeof step.label === "string" ? step.label : undefined,
    as: typeof step.as === "string" ? step.as : null,
  };
}

// --- the step form -------------------------------------------------------

/** One box's value: text for everything typed, a flag for a checkbox. */
export type FormValue = string | boolean;
export type FormValues = Record<string, FormValue>;

/** A static option as a `<select>` shows it. Options arrive as JSON values —
 * a bare value, or `{label, value}`. */
export type SelectOption = { value: string; label: string; raw: unknown };

export function selectOptions(spec: Pick<ParamSpec, "options">): SelectOption[] {
  return spec.options.map((option) => {
    if (option && typeof option === "object" && "value" in option) {
      const o = option as { value: unknown; label?: unknown };
      return {
        value: JSON.stringify(o.value),
        label: typeof o.label === "string" ? o.label : String(o.value),
        raw: o.value,
      };
    }
    return { value: JSON.stringify(option), label: String(option), raw: option };
  });
}

/** Which control a parameter gets. */
export type ControlKind = "select" | "checkbox" | "number" | "json" | "textarea" | "text";

export function controlKind(spec: Pick<ParamSpec, "type" | "options" | "multiline">): ControlKind {
  if (spec.options.length > 0) return "select";
  switch (spec.type) {
    case "Bool":
      return "checkbox";
    case "Int":
    case "Float":
    case "Decimal":
      return "number";
    case "Json":
      return "json";
    default:
      return spec.multiline ? "textarea" : "text";
  }
}

/** One parameter's box, from the step's current value or the spec's default. */
function initialValue(spec: ParamSpec, value: unknown): FormValue {
  const v = value === undefined || value === null ? spec.default : value;
  const kind = controlKind(spec);
  if (kind === "checkbox") return v === true;
  if (v === undefined || v === null) return "";
  if (kind === "select") return JSON.stringify(v);
  if (kind === "json") return JSON.stringify(v, null, 2);
  return typeof v === "string" ? v : String(v);
}

/** The form's boxes for a step type, filled from `params` where it has them. */
export function initialValues(specs: ParamSpec[], params: unknown): FormValues {
  const bag = (params && typeof params === "object" ? params : {}) as Record<string, unknown>;
  const values: FormValues = {};
  for (const spec of specs) values[spec.name] = initialValue(spec, bag[spec.name]);
  return values;
}

/** What the form becomes: `params`, or the first thing wrong with it.
 *
 * An empty box is left out rather than sent as `""`, so the step's type
 * applies its own default and a required parameter left empty is reported here
 * rather than saved as an empty string. A checkbox is always a value. */
export type BuiltParams =
  | { ok: true; params: Record<string, unknown> }
  | { ok: false; problem: ParamProblem; label: string; detail?: string };

/** What is wrong with one box; the screen words it. */
export type ParamProblem = "required" | "number" | "json";

export function buildParams(specs: ParamSpec[], values: FormValues): BuiltParams {
  const params: Record<string, unknown> = {};
  for (const spec of specs) {
    const value = values[spec.name];
    const label = spec.label || spec.name;
    const kind = controlKind(spec);
    if (kind === "checkbox") {
      params[spec.name] = value === true;
      continue;
    }
    const text = typeof value === "string" ? value.trim() : "";
    if (text === "") {
      if (spec.required) {
        return { ok: false, problem: "required", label };
      }
      continue;
    }
    if (kind === "select") {
      const option = selectOptions(spec).find((o) => o.value === text);
      params[spec.name] = option ? option.raw : text;
    } else if (kind === "number") {
      const n = Number(text);
      if (!Number.isFinite(n) || (spec.type === "Int" && !Number.isInteger(n))) {
        return { ok: false, problem: "number", label };
      }
      params[spec.name] = n;
    } else if (kind === "json") {
      try {
        params[spec.name] = JSON.parse(text);
      } catch (err) {
        return {
          ok: false,
          problem: "json",
          label,
          detail: err instanceof Error ? err.message : String(err),
        };
      }
    } else {
      // Text keeps its own whitespace: a template or an expected message may
      // mean its leading space.
      params[spec.name] = value;
    }
  }
  return { ok: true, params };
}

// --- the checklist's own settings ------------------------------------------

/** The settings an admin edits on a checklist, out of its sparse `attributes`. */
export type ChecklistSettings = {
  stopOnFailure: boolean;
  /** Seconds one step may take, as typed; empty is the server's default. */
  timeout: string;
  allowOnProduction: boolean;
};

export function settingsFrom(attributes: unknown): ChecklistSettings {
  const a = (attributes && typeof attributes === "object" ? attributes : {}) as Record<
    string,
    unknown
  >;
  return {
    stopOnFailure: a.stop_on_failure !== false,
    timeout: typeof a.timeout === "number" ? String(a.timeout) : "",
    allowOnProduction: a.allow_on_production === true,
  };
}

/** `attributes` with the settings written into it, keeping every other key
 * (tags, and whatever else the server keeps there). `null` when the timeout is
 * not a positive number. */
export function attributesWith(
  attributes: unknown,
  settings: ChecklistSettings,
): Record<string, unknown> | null {
  const a = {
    ...((attributes && typeof attributes === "object" ? attributes : {}) as Record<
      string,
      unknown
    >),
  };
  a.stop_on_failure = settings.stopOnFailure;
  a.allow_on_production = settings.allowOnProduction;
  const timeout = settings.timeout.trim();
  if (timeout === "") {
    delete a.timeout;
  } else {
    const n = Number(timeout);
    if (!Number.isFinite(n) || n <= 0) return null;
    a.timeout = n;
  }
  return a;
}

// --- where things are ------------------------------------------------------
//
// An application's checklists live under the application's own route, so the
// sidebar keeps that application current and lights up its Checklists link
// while the admin is in one of them; a data-only checklist lives under
// `/checklists`, which is the Data Layer's entry.

/** The list a checklist belongs in. */
export function checklistsPath(application?: string | null): string {
  return application
    ? `/applications/${encodeURIComponent(application)}/checklists`
    : "/checklists";
}

/** One checklist's screen. */
export function checklistPath(id: string, application?: string | null): string {
  return `${checklistsPath(application)}/${encodeURIComponent(id)}`;
}

/** One run's screen. */
export function checklistRunPath(runId: string, application?: string | null): string {
  return application
    ? `${checklistsPath(application)}/runs/${encodeURIComponent(runId)}`
    : `/checklist-runs/${encodeURIComponent(runId)}`;
}
