// The agent form's attributes, as data: the model roles, how the models answer,
// a run's limits, the LLM credits, and where the `coding` trait's shell
// settings go. Each numeric attribute is in exactly one of the form's groups.
//
// An agent's attributes are sparse (§9): a key that is absent means the
// default, and some keys this form does not show are still the agent's — the
// loop-control limits, an attribute a later version adds. So saving the form
// **changes only the keys it shows** and keeps every other one, which is what
// stops an edit to the prompt from quietly resetting an agent's strong model.

import type { FieldSpec } from "./settings";

/** A translator: the tables' labels are functions of one, so each English
 * string is written inside its own `t("…")`, where the extractor reads it. */
export type Tr = (text: string) => string;

/** The two optional model roles (`sc_agent::ATTR_STRONG` / `ATTR_CHEAP`). The
 * executor is the agent's own provider and model. */
export const ROLES = [
  {
    key: "strong",
    label: (t: Tr) => t("Strong model"),
    help: (t: Tr) => t("Plans, reviews and takes escalations."),
  },
  {
    key: "cheap",
    label: (t: Tr) => t("Cheap model"),
    help: (t: Tr) => t("Summarises, explores and writes commit messages."),
  },
] as const;

export type RoleKey = (typeof ROLES)[number]["key"];

/** A role's model as the form holds it: an empty provider is "same as the agent". */
export type RoleChoice = { provider: string; model: string };

/** How the models answer, shown under them. */
export const ANSWER_SETTINGS = [
  {
    key: "temperature",
    label: (t: Tr) => t("Temperature"),
    help: (t: Tr) => t("Blank uses the provider's own default."),
  },
  {
    key: "max_tokens",
    label: (t: Tr) => t("Max tokens per answer"),
    help: (t: Tr) => t("Blank uses the provider's own default."),
  },
] as const;

/** A run's limits (`sc_agent::ATTR_MAX_STEPS`, `ATTR_MAX_COST` and the rest). */
export const LIMITS = [
  {
    key: "max_steps",
    label: (t: Tr) => t("Max steps per run"),
    help: (t: Tr) => t("How many times one run may go round the loop. Blank means 250."),
  },
  {
    key: "max_cost",
    label: (t: Tr) => t("Cost per run"),
    help: (t: Tr) => t("In the models' priced currency, sessions included. Every model the agent uses needs a price."),
  },
  {
    key: "max_wall_seconds",
    label: (t: Tr) => t("Working time per run (seconds)"),
    help: (t: Tr) => t("Time spent calling models and tools; time waiting for you is not counted."),
  },
  {
    key: "context_budget",
    label: (t: Tr) => t("Context per request (tokens)"),
    help: (t: Tr) => t("The context is compacted at 80% of this. Blank uses the model's own working budget."),
  },
  {
    key: "max_images",
    label: (t: Tr) => t("Screenshots kept per run"),
    help: (t: Tr) => t("Older ones are replaced by a note. Blank means 20."),
  },
] as const;

/** What the agent charges the person using it in LLM credits
 * (`sc_agent::ATTR_CREDITS_PER_DOLLAR` and the rest, TODO-payments P6.3). No
 * rate set is not metered; an admin is never charged. */
export const CREDITS = [
  {
    key: "credits_per_dollar",
    label: (t: Tr) => t("Credits per dollar"),
    help: (t: Tr) => t("Per unit of the models' priced cost. Every model the agent uses needs a price, unless a rate per token is set too."),
  },
  {
    key: "credits_per_1k_input",
    label: (t: Tr) => t("Credits per 1,000 input tokens"),
    help: (t: Tr) => t("Used when a call's cost is unknown, or alone."),
  },
  {
    key: "credits_per_1k_output",
    label: (t: Tr) => t("Credits per 1,000 output tokens"),
    help: (t: Tr) => t("Used when a call's cost is unknown, or alone."),
  },
  {
    key: "credits_min_balance",
    label: (t: Tr) => t("Balance needed to start"),
    help: (t: Tr) => t("A run with less does not start. Blank is anything above 0."),
  },
] as const;

const NUMBER_KEYS: string[] = [...ANSWER_SETTINGS, ...LIMITS, ...CREDITS].map((a) => a.key);

/** Whether any LLM credit rate or minimum is filled in: the form opens the
 * credits group for an agent that charges, and keeps it shut for the many
 * that do not. */
export function chargesCredits(numbers: Record<string, string>): boolean {
  return CREDITS.some(({ key }) => (numbers[key] ?? "").trim() !== "");
}

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
 * settings that only matter under it (`sc_agent_builtins::CFG_MAY_USE_SHELL`). */
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

/** The form's tabs, in order. The first is where a new agent starts and where
 * the required fields are. */
export const AGENT_TABS = [
  { id: "general", label: (t: Tr) => t("General") },
  { id: "instructions", label: (t: Tr) => t("Instructions") },
  { id: "traits", label: (t: Tr) => t("What it can do") },
  { id: "limits", label: (t: Tr) => t("Limits") },
] as const;

export type AgentTab = (typeof AGENT_TABS)[number]["id"];

/** The tab a URL names, or the first for none or one that does not exist. */
export function agentTab(raw: string | undefined): AgentTab {
  return AGENT_TABS.find((tab) => tab.id === raw)?.id ?? "general";
}

/** The URL of `tab` of the agent `agentId`, or of the new agent's form. The
 * first tab is the form's own URL. */
export function agentTabHref(agentId: string | undefined, tab: AgentTab): string {
  const base = agentId ? `#/agents/${encodeURIComponent(agentId)}/edit` : "#/agents/new";
  return tab === "general" ? base : `${base}/${tab}`;
}

/** What is missing before the form can be saved, with the tab it is on, or
 * `null`. The browser cannot point at an empty box on a tab that is not shown,
 * so the form says so itself. */
export function missingRequired(
  name: string,
  provider: string,
): { tab: AgentTab; message: (t: Tr) => string } | null {
  if (name.trim() === "") {
    return { tab: "general", message: (t: Tr) => t("Give the agent a name.") };
  }
  if (provider.trim() === "") {
    return { tab: "general", message: (t: Tr) => t("Choose an LLM provider for the main model.") };
  }
  return null;
}

/** The tab a save's refusal is about, when it says: the server names a trait
 * or a tool when one of them is the problem ("its tool `query_books` has the
 * same name as one from trait 1"). Anything else is shown above every tab. */
export function refusalTab(message: string): AgentTab | null {
  return /\btraits?\b|\btools?\b/i.test(message) ? "traits" : null;
}

/** One line of what a trait is set to, for its card while folded: each value
 * that is set, by its label less any bracketed explanation ("Table: books ·
 * Writable"). Secrets and JSON are left out, a ticked box is its label alone,
 * and long values are cut. */
export function traitSummary(spec: FieldSpec[], values: Record<string, string>): string {
  const parts: string[] = [];
  for (const field of spec) {
    if (field.secret || field.type === "json") continue;
    const raw = (values[field.name] ?? "").trim();
    if (raw === "" || raw === "false") continue;
    // A label's explanation in brackets is for the form, not the line.
    const label = field.label.replace(/\s*\([^)]*\)/g, "").trim() || field.label;
    if (field.type === "bool") {
      parts.push(label);
      continue;
    }
    const firstLine = raw.split("\n")[0];
    const shown = firstLine.length > 40 ? `${firstLine.slice(0, 39)}…` : firstLine;
    parts.push(`${label}: ${shown}${firstLine !== raw && shown === firstLine ? "…" : ""}`);
  }
  return parts.join(" · ");
}

/** Whether a trait's card starts open: when something on it needs the admin —
 * a required setting with no value, or `coding`'s shell without a sandbox. */
export function traitNeedsAttention(
  trait: string,
  spec: FieldSpec[],
  values: Record<string, string>,
): boolean {
  if (shellWarning(trait, values)) return true;
  return spec.some(
    (field) => field.required && field.type !== "bool" && (values[field.name] ?? "").trim() === "",
  );
}
