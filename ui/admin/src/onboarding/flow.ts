// A small language for dialog sequences: the first-run onboarding is written in
// it (`script.ts`), and `Onboarding.tsx` is its one renderer.
//
// A flow is a record of named **steps** and the name of the first. Four kinds:
//
//   - `ask`  — a question with a choice per option, each going somewhere.
//   - `say`  — a message with buttons, each going somewhere (usually to finish).
//   - `form` — settings to fill in; `submit` acts on them, then the flow moves on.
//   - `run`  — no dialog of its own: does something (a lookup, a create) and
//              decides where to go from what it found.
//
// Where a step goes is a **Go**: another step's name, or `finish(…)`, which
// closes the dialogs and optionally takes the admin somewhere. Steps share a
// **context**: `vars`, short strings the text may name as `{provider}`, and
// `data`, anything else a later step needs (the backends a form offers).
//
// Text is an English template, translated and then filled from `vars` — the
// same `{name}` convention as `t()`, so a step's words go through the catalogue
// like every other sentence in the admin. A path in `finish` is filled the same
// way, with each value URL-encoded.
//
// The engine below is pure apart from the `run` and `submit` bodies it awaits,
// which reach the server only through the effects object the flow is given.
// So the whole graph — which choice leads where, what a lookup's answer does —
// is testable with fakes (`script.test.ts`), and `checkFlow` finds a misspelt
// step name before anybody clicks on it.

import type { FieldSpec } from "../settings";

/** What the steps of one run of a flow share. */
export type FlowContext = {
  /** Short strings, for the text's `{placeholders}` and `finish` paths. */
  vars: Record<string, string>;
  /** Anything else a later step needs. */
  data: Record<string, unknown>;
};

/** Where the dialogs end up when they close. */
export type Exit = {
  /** A hash route to open in the admin (`/tables`). */
  to?: string;
  /** An address to open in a new tab (the IDE is a page of its own). */
  href?: string;
};

/** Leave the flow. */
export type Finish = { finish: Exit | ((ctx: FlowContext) => Exit) };

/** Where a step leads: another step, by name, or out. */
export type Go = string | Finish;

/** What a `run` or `submit` body answers: where to go, and what to remember. */
export type Outcome = Go | { go: Go; set?: Record<string, string>; data?: Record<string, unknown> };

/** One choice of an `ask`, or one button of a `say`. */
export type Choice = {
  label: string;
  description?: string;
  go: Go;
  /** Offered only while this holds. */
  when?: (ctx: FlowContext) => boolean;
  /** A button's tone; the first choice of a `say` is primary unless it says. */
  variant?: "primary" | "secondary" | "link";
};

type Common = {
  title: string;
  /** The question or message, as an English template over `vars`. */
  text?: string;
  /** A note shown above everything else: how to do this without the dialogs. */
  note?: string;
};

export type AskStep = Common & { kind: "ask"; options: Choice[] };
export type SayStep = Common & { kind: "say"; buttons: Choice[] };
export type FormStep<Fx> = Common & {
  kind: "form";
  /** The fields to show, which may depend on what has been typed so far (a
   * backend's own settings follow the backend picked). */
  fields: (ctx: FlowContext, values: Record<string, string>) => FieldSpec[];
  /** What the form starts out holding; a field's own default otherwise. */
  initial?: (ctx: FlowContext) => Record<string, string>;
  submitLabel?: string;
  /** Acts on the values. A throw keeps the form open with the error shown. */
  submit: (values: Record<string, string>, ctx: FlowContext, fx: Fx) => Promise<Outcome>;
  /** Every step `submit` may go to, for `checkFlow`. */
  exits: string[];
};
export type RunStep<Fx> = {
  kind: "run";
  /** What the admin is told while it runs. */
  busy?: string;
  do: (ctx: FlowContext, fx: Fx) => Promise<Outcome>;
  /** Every step `do` may go to, for `checkFlow`. */
  exits: string[];
};

export type Step<Fx> = AskStep | SayStep | FormStep<Fx> | RunStep<Fx>;

export type Flow<Fx> = { start: string; steps: Record<string, Step<Fx>> };

// --- the vocabulary ----------------------------------------------------------

/** Mark English as a message without translating it — gettext's `N_`. A flow is
 * data, written before any locale is known, so its words are translated where
 * they are shown (`Onboarding.tsx`); the marker is named `t` so that
 * `feldspar i18n extract` collects them into the catalogue all the same. */
export const t = (english: string): string => english;

export const ask = (step: Omit<AskStep, "kind">): AskStep => ({ kind: "ask", ...step });
export const say = (step: Omit<SayStep, "kind">): SayStep => ({ kind: "say", ...step });
export const form = <Fx>(step: Omit<FormStep<Fx>, "kind">): FormStep<Fx> => ({
  kind: "form",
  ...step,
});
export const run = <Fx>(step: Omit<RunStep<Fx>, "kind">): RunStep<Fx> => ({
  kind: "run",
  ...step,
});
export const finish = (exit: Finish["finish"] = {}): Finish => ({ finish: exit });
export const flow = <Fx>(definition: Flow<Fx>): Flow<Fx> => definition;

// --- running one --------------------------------------------------------------

/** Where a run of a flow is: on a step, or finished. `back` is the dialogs
 * already shown, most recent last, each with the context it was shown with. */
export type FlowState =
  | { at: string; ctx: FlowContext; back: { at: string; ctx: FlowContext }[] }
  | { done: Exit; ctx: FlowContext };

export function startFlow<Fx>(definition: Flow<Fx>): FlowState {
  return { at: definition.start, ctx: { vars: {}, data: {} }, back: [] };
}

/** The step a state is on, or null once finished. */
export function currentStep<Fx>(definition: Flow<Fx>, state: FlowState): Step<Fx> | null {
  if ("done" in state) return null;
  const step = definition.steps[state.at];
  if (!step) throw new Error(`onboarding: no step named "${state.at}"`);
  return step;
}

/** Fill `{name}` placeholders from `vars`, leaving unknown ones as they are. */
export function fill(
  template: string,
  vars: Record<string, string>,
  encode: (value: string) => string = (v) => v,
): string {
  return template.replace(/\{(\w+)\}/g, (whole, name: string) =>
    Object.prototype.hasOwnProperty.call(vars, name) ? encode(vars[name]) : whole,
  );
}

/** Follow an outcome from the step `state` is on. The dialog left is
 * remembered for Back unless it was a `run`, which has nothing to go back to. */
export function follow<Fx>(definition: Flow<Fx>, state: FlowState, outcome: Outcome): FlowState {
  if ("done" in state) return state;
  const { go, set, data } =
    typeof outcome === "object" && "go" in outcome ? outcome : { go: outcome, set: {}, data: {} };
  const ctx: FlowContext = {
    vars: { ...state.ctx.vars, ...set },
    data: { ...state.ctx.data, ...data },
  };
  if (typeof go !== "string") {
    const exit = typeof go.finish === "function" ? go.finish(ctx) : go.finish;
    return {
      done: {
        to: exit.to && fill(exit.to, ctx.vars, encodeURIComponent),
        href: exit.href && fill(exit.href, ctx.vars, encodeURIComponent),
      },
      ctx,
    };
  }
  const left = definition.steps[state.at];
  const back =
    left && left.kind !== "run" ? [...state.back, { at: state.at, ctx: state.ctx }] : state.back;
  return { at: go, ctx, back };
}

/** Run `run` steps until the state is on a dialog (or finished). A throw is the
 * caller's to show; the state it was thrown from is not advanced. */
export async function settle<Fx>(definition: Flow<Fx>, state: FlowState, fx: Fx): Promise<FlowState> {
  let current = state;
  for (let hops = 0; hops < 50; hops++) {
    const step = currentStep(definition, current);
    if (!step || step.kind !== "run") return current;
    const outcome = await step.do(current.ctx, fx);
    checkExit(step, outcome);
    current = follow(definition, current, outcome);
  }
  throw new Error("onboarding: too many steps without a dialog");
}

/** Take the `index`th of the choices the current step offers. */
export function choose<Fx>(definition: Flow<Fx>, state: FlowState, index: number): FlowState {
  const step = currentStep(definition, state);
  if (!step || (step.kind !== "ask" && step.kind !== "say")) return state;
  const choice = offered(step, state.ctx)[index];
  return choice ? follow(definition, state, choice.go) : state;
}

/** The choices an `ask` or `say` offers in this context. */
export function offered(step: AskStep | SayStep, ctx: FlowContext): Choice[] {
  const all = step.kind === "ask" ? step.options : step.buttons;
  return all.filter((choice) => !choice.when || choice.when(ctx));
}

/** Submit the current form. */
export async function submit<Fx>(
  definition: Flow<Fx>,
  state: FlowState,
  values: Record<string, string>,
  fx: Fx,
): Promise<FlowState> {
  const step = currentStep(definition, state);
  if (!step || step.kind !== "form" || "done" in state) return state;
  const outcome = await step.submit(values, state.ctx, fx);
  checkExit(step, outcome);
  return follow(definition, state, outcome);
}

/** Back to the dialog before this one, with the context it had then. */
export function goBack(state: FlowState): FlowState {
  if ("done" in state || state.back.length === 0) return state;
  const previous = state.back[state.back.length - 1];
  return { at: previous.at, ctx: previous.ctx, back: state.back.slice(0, -1) };
}

/** Whether there is a dialog to go back to. */
export function canGoBack(state: FlowState): boolean {
  return !("done" in state) && state.back.length > 0;
}

// --- checking one -------------------------------------------------------------

/** What is wrong with a flow's graph: a Go naming no step, a step nothing
 * reaches. Empty for a sound flow. */
export function checkFlow<Fx>(definition: Flow<Fx>): string[] {
  const problems: string[] = [];
  const targets = (step: Step<Fx>): string[] => {
    switch (step.kind) {
      case "ask":
        return step.options.flatMap((c) => (typeof c.go === "string" ? [c.go] : []));
      case "say":
        return step.buttons.flatMap((c) => (typeof c.go === "string" ? [c.go] : []));
      case "form":
      case "run":
        return step.exits;
    }
  };
  if (!definition.steps[definition.start]) {
    problems.push(`the start step "${definition.start}" does not exist`);
  }
  const reached = new Set<string>([definition.start]);
  const queue = [definition.start];
  while (queue.length > 0) {
    const name = queue.shift() as string;
    const step = definition.steps[name];
    if (!step) continue;
    for (const target of targets(step)) {
      if (!definition.steps[target]) {
        problems.push(`step "${name}" goes to "${target}", which does not exist`);
      } else if (!reached.has(target)) {
        reached.add(target);
        queue.push(target);
      }
    }
  }
  for (const name of Object.keys(definition.steps)) {
    if (!reached.has(name)) problems.push(`step "${name}" is never reached`);
  }
  return problems;
}

/** Throw unless a run's outcome goes to a step it declared. Checked as the
 * flow runs, so an undeclared exit is a failing test rather than a dialog
 * that never opens. */
export function checkExit<Fx>(step: FormStep<Fx> | RunStep<Fx>, outcome: Outcome): void {
  const go = typeof outcome === "object" && "go" in outcome ? outcome.go : outcome;
  if (typeof go === "string" && !step.exits.includes(go)) {
    throw new Error(`onboarding: "${go}" is not one of this step's exits`);
  }
}
