// The agents area of the admin: the agents themselves, the LLM providers they
// talk to, and the MCP servers whose tools they may be given. One sidebar
// entry, three lists, and the tabs across the top of each.

import type { Translator } from "./i18n";

type Translate = Translator["t"];

/** The area's lists, in the order their tabs show. */
export const AGENT_AREA_TABS = [
  { id: "agents", label: (t: Translate) => t("Agents"), href: "#/agents" },
  { id: "llm-providers", label: (t: Translate) => t("LLM providers"), href: "#/llm-providers" },
  { id: "mcp-servers", label: (t: Translate) => t("MCP servers"), href: "#/mcp-servers" },
] as const;

export type AgentAreaTab = (typeof AGENT_AREA_TABS)[number]["id"];

/** One agent's screens — its chat, its runs, what it remembers, and its
 * settings — in the order their tabs show. Three are addressed by the agent's
 * name (what runs and memories are keyed by) and the settings by its id (what
 * the form loads and saves by). */
export const AGENT_SECTIONS = [
  { id: "chat", label: (t: Translate) => t("Chat") },
  { id: "runs", label: (t: Translate) => t("Runs") },
  { id: "memory", label: (t: Translate) => t("Memories") },
  { id: "settings", label: (t: Translate) => t("Settings") },
] as const;

export type AgentSection = (typeof AGENT_SECTIONS)[number]["id"];

/** The URL of one of the agent's screens, or `null` for the settings while
 * the agent's id is not known. */
export function agentSectionHref(
  section: AgentSection,
  name: string,
  id: string | undefined,
): string | null {
  if (section === "settings") {
    return id ? `#/agents/${encodeURIComponent(id)}/edit` : null;
  }
  return `#/agents/${encodeURIComponent(name)}/${section}`;
}
