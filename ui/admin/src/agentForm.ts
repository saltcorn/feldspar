// The agent form's attributes, as data: the model roles, the budgets, and
// where the `coding` trait's shell settings go.
//
// An agent's attributes are sparse (§9): a key that is absent means the
// default, and some keys this form does not show are still the agent's — the
// loop-control limits, an attribute a later version adds. So saving the form
// **changes only the keys it shows** and keeps every other one, which is what
// stops an edit to the prompt from quietly resetting an agent's strong model.

import type { FieldSpec } from "./settings";

/** The two optional model roles (`sc_agent::ATTR_STRONG` / `ATTR_CHEAP`). The
 * executor is the agent's own provider and model. */
export const ROLES = [
  {
    key: "strong",
    label: "Strong model",
    help: "Plans, reviews and takes escalations. Blank uses the agent's own model.",
  },
  {
    key: "cheap",
    label: "Cheap model",
    help: "Summarises, explores and writes commit messages. Blank uses the agent's own model.",
  },
] as const;

export type RoleKey = (typeof ROLES)[number]["key"];

/** A role's model as the form holds it: an empty provider is "same as the agent". */
export type RoleChoice = { provider: string; model: string };

/** The numeric attributes, in the order the form shows them. */
export const NUMBER_ATTRIBUTES = [
  {
    key: "temperature",
    label: "Temperature",
    help: "Blank uses the provider's own default.",
  },
  {
    key: "max_tokens",
    label: "Max tokens per answer",
    help: "Blank uses the provider's own default.",
  },
  {
    key: "max_steps",
    label: "Max steps per run",
    help: "How many times one run may go round the loop. Blank means 250.",
  },
] as const;

/** A run's budgets (`sc_agent::ATTR_MAX_COST` and the rest). */
export const BUDGETS = [
  {
    key: "max_cost",
    label: "Cost per run",
    help: "In the models' priced currency, sessions included. Every model the agent uses needs a price.",
  },
  {
    key: "max_wall_seconds",
    label: "Working time per run (seconds)",
    help: "Time spent calling models and tools; time waiting for you is not counted.",
  },
  {
    key: "context_budget",
    label: "Context per request (tokens)",
    help: "The context is compacted at 80% of this. Blank uses the model's own working budget.",
  },
  {
    key: "max_images",
    label: "Screenshots kept per run",
    help: "Older ones are replaced by a note. Blank means 20.",
  },
] as const;

/** What the agent charges the person using it in LLM credits
 * (`sc_agent::ATTR_CREDITS_PER_DOLLAR` and the rest, TODO-payments P6.3). No
 * rate set is not metered; an admin is never charged. */
export const CREDITS = [
  {
    key: "credits_per_dollar",
    label: "Credits per dollar",
    help: "Per unit of the models' priced cost. Every model the agent uses needs a price, unless a rate per token is set too.",
  },
  {
    key: "credits_per_1k_input",
    label: "Credits per 1,000 input tokens",
    help: "Used when a call's cost is unknown, or alone.",
  },
  {
    key: "credits_per_1k_output",
    label: "Credits per 1,000 output tokens",
    help: "Used when a call's cost is unknown, or alone.",
  },
  {
    key: "credits_min_balance",
    label: "Balance needed to start",
    help: "A run with less does not start. Blank is anything above 0.",
  },
] as const;

const NUMBER_KEYS: string[] = [...NUMBER_ATTRIBUTES, ...BUDGETS, ...CREDITS].map((a) => a.key);

/** The role choices stored in an agent's attributes. */
export function readRoles(attributes: unknown): Record<RoleKey, RoleChoice> {
  const stored = (attributes ?? {}) as Record<string, unknown>;
  const read = (key: RoleKey): RoleChoice => {
    const value = stored[key] as { provider?: unknown; model?: unknown } | null | undefined;
    return {
      provider: typeof value?.provider === "string" ? value.provider : "",
      model: typeof value?.model === "string" ? value.model : "",
    };
  };
  return { strong: read("strong"), cheap: read("cheap") };
}

/** The numeric attributes as the boxes edit them: text, blank for absent. */
export function readNumbers(attributes: unknown): Record<string, string> {
  const stored = (attributes ?? {}) as Record<string, unknown>;
  const out: Record<string, string> = {};
  for (const key of NUMBER_KEYS) {
    const value = stored[key];
    if (typeof value === "number") out[key] = String(value);
  }
  return out;
}

/** The reasoning effort levels (`sc_llm::ReasoningEffort`), in order. Blank is
 * "the model's own setting". */
export const REASONING_EFFORTS = ["none", "minimal", "low", "medium", "high", "xhigh", "max"] as const;

/** The stored `reasoning_effort` (`sc_agent::ATTR_REASONING_EFFORT`), blank
 * for absent. */
export function readEffort(attributes: unknown): string {
  const value = ((attributes ?? {}) as Record<string, unknown>).reasoning_effort;
  return typeof value === "string" ? value : "";
}

/** The attributes to save: every stored key the form does not show, then the
 * numbers that were typed and the roles that were chosen. An empty box is not
 * a zero, and an unchosen role is not a role. `effort` is the reasoning effort
 * picked; left out, the stored one is kept, and blank removes it. */
export function agentAttributes(
  stored: unknown,
  numbers: Record<string, string>,
  roles: Record<RoleKey, RoleChoice>,
  effort?: string,
): Record<string, unknown> {
  const shown = new Set<string>([...NUMBER_KEYS, ...ROLES.map((r) => r.key)]);
  if (effort !== undefined) shown.add("reasoning_effort");
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries((stored ?? {}) as Record<string, unknown>)) {
    if (!shown.has(key)) out[key] = value;
  }
  for (const key of NUMBER_KEYS) {
    const text = (numbers[key] ?? "").trim();
    if (text !== "") out[key] = Number(text);
  }
  if (effort !== undefined && effort.trim() !== "") out.reasoning_effort = effort.trim();
  for (const { key } of ROLES) {
    const choice = roles[key];
    if (choice.provider.trim() === "") continue;
    out[key] =
      choice.model.trim() === ""
        ? { provider: choice.provider.trim() }
        : { provider: choice.provider.trim(), model: choice.model.trim() };
  }
  return out;
}

/** The `coding` setting that grants the shell, and the first of the group of
 * settings that only matter under it (`sc_core_traits::CFG_MAY_USE_SHELL`). */
const MAY_USE_SHELL = "may_use_shell";
/** Where the shell's commands run (`CFG_SHELL_SANDBOX`). */
const SHELL_SANDBOX = "shell_sandbox";

/** A trait's settings, split into its own and the shell's: for `coding`, the
 * grant and every setting declared after it, which the trait declares last and
 * together. Any other trait's settings are all its own. */
export function splitShellSettings(
  trait: string,
  spec: FieldSpec[],
): { own: FieldSpec[]; shell: FieldSpec[] } {
  const at = trait === "coding" ? spec.findIndex((f) => f.name === MAY_USE_SHELL) : -1;
  if (at < 0) return { own: spec, shell: [] };
  return { own: spec.slice(0, at), shell: spec.slice(at) };
}

/** The warning a `coding` trait with the shell on and no sandbox needs, or
 * `null`. `values` are the form's strings; an untouched sandbox is its
 * default, which is `none`. */
export function shellWarning(trait: string, values: Record<string, string>): string | null {
  if (trait !== "coding" || values[MAY_USE_SHELL] !== "true") return null;
  const sandbox = (values[SHELL_SANDBOX] ?? "").trim();
  if (sandbox !== "" && sandbox !== "none") return null;
  return (
    "The shell runs with no sandbox: commands run on this server as the server's own user. " +
    "They can read and change any file that user can, including other applications' source " +
    "and the server's configuration, and reach the network. Only admins can use it, but " +
    "what the model does with it is not reviewed. Choose the container sandbox unless this " +
    "server is itself disposable."
  );
}
