// The pure part of a provider's Models list and the agent form's model
// pick-list (TODO §3a): what a model row shows, what the pick-list offers, and
// what a save sends. Kept apart from the screens so it can be tested without a
// browser.

import type { ListLlmModelsResponse } from "./client";

export type ModelItem = ListLlmModelsResponse[number];

/** What a model's settings resolve to, as the server reports it. */
export type Capabilities = {
  parallel_tool_calls: boolean;
  parallel_tool_calls_default: boolean;
  native_apply_patch: boolean;
  reasoning_replay: boolean;
  prompt_caching: string;
  edit_format: string;
  context_window: number;
  working_budget: number;
  vision: boolean;
};

/** A model's prices per million tokens (or per image, or per million
 * characters). `null` or absent is unknown, never zero. */
export type Prices = {
  input: number | null;
  cached_input: number | null;
  cache_write: number | null;
  output: number | null;
  per_image?: number | null;
  per_million_characters?: number | null;
};

/** What a model is called for (TODO-agent-skills AS3.1). */
export type ModelKind = "chat" | "embedding" | "image" | "speech";

/** Every kind, in the order the Models card lists them. */
export const MODEL_KINDS: ModelKind[] = ["chat", "embedding", "image", "speech"];

/** The heading a kind's models are listed under. */
export const KIND_HEADINGS: Record<ModelKind, string> = {
  chat: "Chat models",
  embedding: "Embedding models",
  image: "Image models",
  speech: "Speech models",
};

/** A model's kind; a row from before kinds is chat. */
export function kindOf(m: Pick<ModelItem, "kind">): ModelKind {
  const kind = m.kind ?? "chat";
  return (MODEL_KINDS as string[]).includes(kind) ? (kind as ModelKind) : "chat";
}

/** The kind a model's name suggests, for a name *Fetch models* offers:
 * "text-embedding-3-small" is an embedding model, "tts-1" a speech one,
 * "dall-e-3" and "gpt-image-1" image ones. Anything else is chat. */
export function guessKind(name: string): ModelKind {
  const n = name.toLowerCase();
  if (n.includes("embed")) return "embedding";
  if (n.includes("tts") || n.includes("speech")) return "speech";
  if (n.includes("dall-e") || n.includes("image")) return "image";
  return "chat";
}

/** The kinds a backend serves: Anthropic serves chat only. */
export function kindsServedBy(backend: string): ModelKind[] {
  return backend === "anthropic" ? ["chat"] : MODEL_KINDS;
}

/** The models grouped by kind, in [`MODEL_KINDS`] order, leaving out kinds
 * with no models. */
export function groupByKind(models: ModelItem[]): Array<{ kind: ModelKind; models: ModelItem[] }> {
  return MODEL_KINDS.map((kind) => ({
    kind,
    models: models.filter((m) => kindOf(m) === kind),
  })).filter((g) => g.models.length > 0);
}

/** A model's prices as its row shows them, by kind. */
export function priceSummary(kind: ModelKind, prices: Prices): string {
  switch (kind) {
    case "embedding":
      return `${formatPrice(prices.input)} input`;
    case "image":
      return prices.per_image !== null && prices.per_image !== undefined
        ? `${prices.per_image} per image`
        : `${formatPrice(prices.input)} / ${formatPrice(prices.output)}`;
    case "speech":
      return `${formatPrice(prices.per_million_characters)} characters`;
    default:
      return `${formatPrice(prices.input)} / ${formatPrice(prices.output)}`;
  }
}

/** One option of the agent form's model select. `value` is what is saved:
 * the empty string is "the provider's default". */
export type ModelOption = { value: string; label: string };

/**
 * The agent form's model options for the chosen provider's chat `models`,
 * given the agent's current `model`.
 *
 * - The first option is the provider's default, naming which model that is,
 *   or saying there is none — an agent saved that way would not validate, and
 *   the admin should see why before saving.
 * - Each model follows by name, the default marked.
 * - A model the agent names that has no row (deleted, or of another provider)
 *   is kept and marked missing, so saving the form does not silently repoint
 *   the agent.
 */
export function modelOptions(models: ModelItem[], current: string): ModelOption[] {
  // An agent converses: only a chat model can be its model.
  const byName = models.filter((m) => kindOf(m) === "chat").sort((a, b) => a.name.localeCompare(b.name));
  const fallback = byName.find((m) => m.is_default);
  const options: ModelOption[] = [
    {
      value: "",
      label: fallback ? `Provider default (${fallback.name})` : "Provider default (none set)",
    },
  ];
  for (const m of byName) {
    options.push({
      value: m.name,
      label: m.is_default ? `${m.name} (default)` : m.name,
    });
  }
  if (current !== "" && byName.every((m) => m.name !== current)) {
    options.push({ value: current, label: `${current} (missing)` });
  }
  return options;
}

/** A price as the list shows it: per million, or "unknown" for a blank one. */
export function formatPrice(price: number | null | undefined): string {
  return price === null || price === undefined ? "unknown" : `${price}/M`;
}

/** A token count as the list shows it: 200000 → "200k", 1000000 → "1M". */
export function formatTokens(tokens: number): string {
  if (tokens >= 1_000_000 && tokens % 1_000_000 === 0) return `${tokens / 1_000_000}M`;
  if (tokens >= 1_000) return `${Math.round(tokens / 1_000)}k`;
  return String(tokens);
}

/** The short capability summary a model row shows. */
export function capabilitySummary(caps: Capabilities): string[] {
  const out = [
    `${formatTokens(caps.context_window)} window`,
    `${formatTokens(caps.working_budget)} budget`,
    caps.edit_format,
  ];
  if (caps.prompt_caching !== "none") out.push(`${caps.prompt_caching} caching`);
  if (caps.vision) out.push("vision");
  if (caps.reasoning_replay) out.push("reasoning replay");
  if (caps.native_apply_patch) out.push("native apply_patch");
  return out;
}

/** The names *Fetch models* offers that are not yet rows, in order. The server
 * already leaves out existing rows; this also drops any added since the fetch. */
export function namesToOffer(fetched: string[], models: ModelItem[]): string[] {
  const existing = new Set(models.map((m) => m.name));
  return fetched.filter((n) => !existing.has(n));
}
