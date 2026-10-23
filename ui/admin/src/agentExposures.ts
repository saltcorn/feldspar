// The application form's **exposed agents** (§11.7): the agents an
// application's client may talk to, at `{api mount}/_agents/{name}`.
//
// The form edits each exposure as one repeating row of short drop-downs and a
// text box, so this module is the translation between that and the record —
// and, like `uploadSlots.ts`, a decision testable without a browser
// (`agentExposures.test.ts`).

import type { CreateApplicationRequest } from "./client";

/** One exposed agent, as the application record carries it. */
export type AgentExposureItem = CreateApplicationRequest["agents"][number];

/** What the client may do with it: hold conversations, send single prompts,
 * or both. */
export type AgentUse = "both" | "chat" | "prompt";

/** One row of the exposed agents list, every cell as the form edits it. */
export type AgentRow = {
  name: string;
  use: AgentUse;
  anonymous: "no" | "yes";
  tools: "names" | "none" | "full";
  prompt_template: string;
};

/** An empty row — what "Add" produces: both uses, signed-in people only, and
 * tool names shown, which are the server's defaults. */
export function blankAgentRow(): AgentRow {
  return {
    name: "",
    use: "both",
    anonymous: "no",
    tools: "names",
    prompt_template: "",
  };
}

/** The stored exposures as rows. */
export function agentRowsFromApp(agents: AgentExposureItem[]): AgentRow[] {
  return agents.map((a) => ({
    name: a.name,
    use: a.chat && a.prompt ? "both" : a.chat ? "chat" : "prompt",
    anonymous: a.anonymous ? "yes" : "no",
    tools: a.tools === "none" || a.tools === "full" ? a.tools : "names",
    prompt_template: a.prompt_template ?? "",
  }));
}

/** The rows as the application record stores them. A row with no agent chosen
 * is one added and never filled in, and is dropped; everything else — that the
 * agent exists, that an anonymous one is public — is the server's to check. */
export function agentRowsToRequest(rows: AgentRow[]): AgentExposureItem[] {
  return rows
    .filter((r) => r.name.trim())
    .map((r) => ({
      name: r.name.trim(),
      chat: r.use !== "prompt",
      prompt: r.use !== "chat",
      anonymous: r.anonymous === "yes",
      tools: r.tools,
      prompt_template: r.prompt_template.trim() || null,
    }));
}

/** The agents an exposure row may name: every agent there is, plus a stored
 * name the list no longer has (shown, so the form never drops it silently). */
export function agentOptions(
  all: string[],
  value: string,
): Array<{ value: string; known: boolean }> {
  const options = all.map((name) => ({ value: name, known: true }));
  if (value && !all.includes(value)) options.push({ value, known: false });
  return options;
}
