// The pure part of the MCP servers screen (TODO-agent-skills AS4.3): the form
// a definition is edited in, what a save sends, and the check a name gets
// before it is sent. Kept apart from the screen so it can be tested without a
// browser.

import type { ListMcpServersResponse, TestMcpServerResponse } from "./client";

export type McpServerItem = ListMcpServersResponse[number];
export type McpTestResult = TestMcpServerResponse;
export type McpToolItem = TestMcpServerResponse["tools"][number];

/** A header as the form holds it. A saved header's value is the sentinel,
 * which a save sends back to keep the stored value. */
export type HeaderRow = { name: string; value: string };

/** The form's state: `id` is null for a new server. */
export type McpServerForm = {
  id: string | null;
  name: string;
  description: string;
  url: string;
  transport: "streamable_http" | "sse";
  headers: HeaderRow[];
  allowPrivateNetwork: boolean;
  timeoutSecs: string;
};

/** A blank form, with one empty header row to type a credential into. */
export function emptyForm(): McpServerForm {
  return {
    id: null,
    name: "",
    description: "",
    url: "",
    transport: "streamable_http",
    headers: [{ name: "Authorization", value: "" }],
    allowPrivateNetwork: false,
    timeoutSecs: "",
  };
}

/** The form for a saved server. */
export function formFromServer(server: McpServerItem): McpServerForm {
  return {
    id: server.id,
    name: server.name,
    description: server.description,
    url: server.url,
    transport: server.transport === "sse" ? "sse" : "streamable_http",
    headers: server.headers.map((h) => ({ ...h })),
    allowPrivateNetwork: server.allow_private_network,
    timeoutSecs: server.timeout_secs ? String(server.timeout_secs) : "",
  };
}

/** What a save (or a test) sends: blank header rows dropped, a blank timeout
 * the default (0). */
export function bodyFromForm(form: McpServerForm) {
  const timeout = Number.parseInt(form.timeoutSecs, 10);
  return {
    name: form.name.trim(),
    description: form.description.trim(),
    url: form.url.trim(),
    transport: form.transport,
    headers: form.headers
      .filter((h) => h.name.trim() !== "")
      .map((h) => ({ name: h.name.trim(), value: h.value })),
    allow_private_network: form.allowPrivateNetwork,
    timeout_secs: Number.isFinite(timeout) && timeout > 0 ? timeout : 0,
  };
}

/** Why `name` will be refused, or null. The server checks the same, but the
 * admin should not have to save to learn the rule: a server's tools are
 * offered as `<name>__<tool>`. */
export function nameProblem(name: string): string | null {
  const n = name.trim();
  if (n === "") return "A name is required.";
  if (n.length > 24) return "At most 24 characters.";
  if (!/^[a-z0-9_-]+$/.test(n)) return "Lowercase letters, digits, - and _ only.";
  if (n.includes("__")) return "No double underscore: tools are named <server>__<tool>.";
  return null;
}

/** The tools as the list shows them, read-only ones marked. */
export function toolSummary(tools: McpToolItem[]): string {
  if (tools.length === 0) return "no tools";
  return tools.map((t) => (t.read_only ? `${t.name} (read-only)` : t.name)).join(", ");
}
