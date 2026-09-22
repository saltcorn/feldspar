/**
 * What a chat turn *is*, decided without a workbench (design §12.1).
 *
 * The two halves of putting a Saltcorn agent in VS Code's chat panel are what to
 * offer (the store's agents, as the panel's models) and what to do with the
 * events one turn streams back (§11.4). Both are functions over data — an agent
 * listing, a socket event — so both are here and tested, and `chat.ts` is left
 * holding only the `vscode.*` calls that cannot be.
 */

import type { ServerEvent } from "./agentChat";
import type { StoreAgent } from "./codingAgents";

/** The extension the participants below belong to; their ids are scoped to it. */
export const CHAT_EXTENSION_ID = "saltcorn.saltcorn-agents";

/** The vendor the placeholder model is registered under.
 *
 * `copilot` because that is the vendor the embedded workbench treats as its
 * built-in one; a vendor of our own is registered but never resolved. It names
 * nothing about GitHub here — see [`storeModel`](storeModel) for what the model
 * actually is. */
export const MODEL_VENDOR = "copilot";

/** One participant, as a manifest contributes it. */
export interface ParticipantContribution {
  readonly id: string;
  readonly name: string;
  readonly fullName: string;
  readonly description: string;
  readonly isDefault?: boolean;
  readonly modes: string[];
  readonly locations: string[];
}

/**
 * The `contributes.chatParticipants` for `agents` — one each, the first of them
 * the default.
 *
 * Every agent gets a participant rather than one participant with a picker of
 * its own, because that is the vocabulary the chat panel already has: `@build-todo`
 * is how VS Code addresses one of several, it completes as it is typed, and the
 * transcript records which one answered. The first is marked default because a
 * store usually has exactly one agent, and typing its name to reach it would be
 * a ceremony with one possible outcome.
 *
 * (The model picker would have been the other candidate, and is not: it collapses
 * to "Auto" for a single model and buries the rest behind Manage Models, so it is
 * where a *model* is chosen and not where an agent can be.)
 */
export function participantContributions(
  agents: StoreAgent[],
): ParticipantContribution[] {
  const taken = new Set<string>();
  return agents.map((agent, index) => {
    const name = uniqueSlug(agent.name, taken);
    return {
      id: `${CHAT_EXTENSION_ID}.${name}`,
      name,
      fullName: agent.application ?? agent.name,
      description: describe(agent),
      ...(index === 0 ? { isDefault: true } : {}),
      // `panel` only: the chat view in the secondary side bar. Inline chat in an
      // editor is a different interaction — a selection, an edit applied to the
      // buffer — and this agent edits the store, not the buffer.
      modes: ["agent"],
      locations: ["panel"],
    };
  });
}

/**
 * A participant name: what an admin types after `@`.
 *
 * VS Code matches those against a restricted alphabet, and an agent's name is
 * whatever the admin called it — so it is slugged rather than trusted, and made
 * unique afterwards, because two agents that slug to the same thing would
 * otherwise be one participant answering for whichever was registered last.
 */
export function uniqueSlug(name: string, taken: Set<string>): string {
  const base =
    name
      .toLowerCase()
      .replace(/[^a-z0-9-]+/g, "-")
      .replace(/^-+|-+$/g, "") || "agent";
  let slug = base;
  for (let n = 2; taken.has(slug); n += 1) slug = `${base}-${n}`;
  taken.add(slug);
  return slug;
}

/** The line the panel prints under a participant: what it is and what it may do. */
export function describe(agent: StoreAgent): string {
  const where = agent.root === "" ? "the whole store" : `${agent.root}/`;
  const may = agent.mayEdit
    ? "reads and changes"
    : "reads (it may not change files)";
  return `${agent.description || agent.name} — ${may} ${where}`;
}

/** The one model the chat is given. */
export interface AgentModel {
  readonly id: string;
  readonly name: string;
  readonly family: string;
  readonly version: string;
  readonly detail: string;
  readonly maxInputTokens: number;
  readonly maxOutputTokens: number;
  readonly capabilities: { toolCalling: boolean };
  readonly isDefault: boolean;
  readonly isUserSelectable: boolean;
}

/**
 * A budget the picker could print. It is **not** a limit this IDE enforces: what
 * an agent's provider and model allow is the agent's business (§11.2), and a
 * number invented here would be a second, wrong answer to that question.
 */
const UNBOUNDED_TOKENS = 1_000_000;

/**
 * The single model the chat is offered — a placeholder, and honestly labelled as
 * one.
 *
 * VS Code will not send a chat request without a language model, so there has to
 * be one. There is nothing for it to *be*: which LLM answers is already the
 * agent's own `provider` and `model` (§11.2), chosen on the agent screen, so a
 * picker offering models here would be a second place to configure the same
 * thing — and the one that cannot see the agent's system prompt or its traits.
 *
 * `isUserSelectable: false` is therefore the point of it: the workbench has a
 * model to hand the request, and the composer stops offering a choice that would
 * not mean anything. Which *agent* answers is `@name`, above.
 */
export function storeModel(store: string): AgentModel {
  return {
    id: `saltcorn-${store}`,
    name: "Saltcorn agent",
    family: "saltcorn",
    version: "1",
    detail: "answered by this installation's agent, not by a model chosen here",
    maxInputTokens: UNBOUNDED_TOKENS,
    maxOutputTokens: UNBOUNDED_TOKENS,
    // The agent calls its own tools, server-side, over the socket (§11.4). What
    // this flag would offer is VS Code's own tool loop, which needs the model to
    // be an LLM this page can talk to — it is not, it is an agent.
    capabilities: { toolCalling: false },
    isDefault: true,
    isUserSelectable: false,
  };
}

/** The slice of `ChatResponseStream` a turn is relayed into. */
export interface ResponseStream {
  markdown(value: string): void;
  progress(value: string): void;
  /** The collapsible "thinking" section — a proposed API, absent when it is not enabled. */
  thinkingProgress?(delta: { text: string; id?: string }): void;
}

/** What one event says the agent changed in the store. */
export interface StoreChange {
  /** The scope-relative paths it wrote. */
  readonly paths: string[];
  /** Anything may have changed: a shell command ran, and nothing says what it touched. */
  readonly everything: boolean;
  /** A commit was made, so source control's view of the working copy is stale. */
  readonly committed: boolean;
}

/** An event that changed nothing. */
const UNCHANGED: StoreChange = {
  paths: [],
  everything: false,
  committed: false,
};

/** Several events' changes, as one. */
export function mergeChanges(changes: StoreChange[]): StoreChange {
  return {
    paths: [...new Set(changes.flatMap((change) => change.paths))],
    everything: changes.some((change) => change.everything),
    committed: changes.some((change) => change.committed),
  };
}

/** Whether the workbench has anything to be told. */
export function hasChanges(change: StoreChange): boolean {
  return change.paths.length > 0 || change.everything || change.committed;
}

/**
 * Fold one server event into the response stream, and say what it changed in
 * the store.
 *
 * A tool call is reported as progress and its *result* is not: the coding
 * trait's results are file contents and search hits, which the model is reading
 * on the admin's behalf, and pasting them into the answer would bury the answer.
 * A failed tool is the exception — that is the sentence explaining a turn which
 * then went sideways.
 *
 * What changed is read from the call for the tools that name their paths, and
 * from the **result** for `implement_feature`, whose session wrote files this
 * socket never saw a call for: its diffstat names them, and its `commit:` line
 * says whether source control moved.
 */
export function relayEvent(
  event: ServerEvent,
  stream: ResponseStream,
): StoreChange {
  switch (event.type) {
    case "text":
      stream.markdown(event.delta);
      return UNCHANGED;
    case "reasoning":
      // Rendered as the collapsible "thinking" section where the proposal is
      // live, and dropped where it is not: reasoning shown as the answer reads
      // as the answer.
      stream.thinkingProgress?.({ text: event.delta, id: "agent" });
      return UNCHANGED;
    case "tool_call":
      stream.progress(toolProgress(event.name, event.arguments));
      return {
        paths: changedPaths(event.name, event.arguments),
        // Checked at the call, not the result: a command that timed out or
        // failed may still have written half of what it meant to.
        everything: event.name.startsWith("shell_"),
        committed: false,
      };
    case "tool_result":
      if (event.is_error)
        stream.markdown(`\n\n\`${event.name}\` failed: ${event.content}\n\n`);
      if (event.name.startsWith("implement_feature_")) {
        return {
          paths: diffstatPaths(event.content),
          everything: false,
          committed: /^commit: [0-9a-f]{7,}/m.test(event.content),
        };
      }
      return UNCHANGED;
    case "error":
      // Appended, never replacing: a failure after two paragraphs and a tool
      // call is read alongside them, not instead of them.
      stream.markdown(`\n\n⚠️ ${event.message}\n\n`);
      return UNCHANGED;
    case "compaction":
      // The agent's context was cleared or summarised to fit its budget. Worth
      // a line — an agent that seems to have forgotten something has a reason
      // — but not the summary, which is the agent's notes, not its answer.
      stream.progress(
        event.summary === undefined
          ? "Clearing old tool output from the context"
          : "Summarising the conversation so far to fit the context",
      );
      return UNCHANGED;
    case "done":
    case "controls":
      return UNCHANGED;
  }
}

/**
 * The scope-relative paths an `implement_feature` result's diffstat names:
 * `M src/App.tsx | +3 -1`, and both sides of `R src/a.ts → src/b.ts`.
 */
export function diffstatPaths(content: string): string[] {
  const lines = content.split("\n");
  const start = lines.indexOf("diffstat:");
  if (start < 0) return [];
  const paths: string[] = [];
  for (const line of lines.slice(start + 1)) {
    const moved = line.match(/^R (.+) → (.+)$/);
    if (moved) {
      paths.push(moved[1], moved[2]);
      continue;
    }
    const file = line.match(/^[AMD] (.+?) \| /);
    if (file == null) break;
    paths.push(file[1]);
  }
  return [...new Set(paths)];
}

/** The coding trait's tools, by the prefix its scope suffix is added to. */
const VERBS: { prefix: string; label: string; writes?: true }[] = [
  { prefix: "read_file_", label: "Reading" },
  { prefix: "find_files_", label: "Finding" },
  { prefix: "search_files_", label: "Searching for" },
  { prefix: "write_file_", label: "Writing", writes: true },
  { prefix: "edit_file_", label: "Editing", writes: true },
  { prefix: "apply_patch_", label: "Patching", writes: true },
  { prefix: "run_script_", label: "Running" },
];

/**
 * The one-line "what is it doing" for a tool call.
 *
 * The tool's own name carries the store and directory it is scoped to
 * (`read_file_app_src_web`), which is noise in a panel already open on that
 * store — so the verb is kept, the scope dropped, and the argument that says
 * *what* shown beside it. A tool from some other trait the agent also has is
 * named as it is: this knows the coding trait's vocabulary and does not pretend
 * to know anyone else's.
 */
export function toolProgress(tool: string, args: unknown): string {
  // Looking at the application: where, when the call says (TODO §7b).
  if (tool.startsWith("view_app_")) {
    return `Looking at ${firstString(args, ["path"]) ?? "the application"}`;
  }
  if (tool.startsWith("check_")) return "Running the checks";
  if (tool.startsWith("shell_")) {
    const command = firstString(args, ["command"]);
    return command == null
      ? "Running a shell command"
      : `Running \`${command}\``;
  }
  if (tool.startsWith("process_")) {
    const action = firstString(args, ["action"]) ?? "process";
    const name = firstString(args, ["name"]);
    const command = firstString(args, ["command"]);
    const head =
      name == null ? `Process: ${action}` : `Process ${name}: ${action}`;
    return command == null ? head : `${head} \`${command}\``;
  }
  // The application's static directories (TODO "Static directories" §6): a
  // read over a store the panel is *not* open on, so there is no scope to drop.
  if (tool.startsWith("list_assets_")) {
    const pattern = firstString(args, ["pattern", "dir"]);
    return pattern == null
      ? "Listing the application's assets"
      : `Listing assets: ${pattern}`;
  }
  if (tool.startsWith("save_plan_")) return "Saving the plan";
  if (tool.startsWith("implement_feature_")) {
    const id = firstString(args, ["id"]);
    return id == null ? "Implementing a feature" : `Implementing feature ${id}`;
  }
  if (tool.startsWith("explore_")) {
    const question = firstString(args, ["question"]);
    return question == null ? "Exploring the code" : `Exploring: ${question}`;
  }
  const verb = VERBS.find((candidate) => tool.startsWith(candidate.prefix));
  const subject = tool.startsWith("apply_patch_")
    ? patchPaths(args).join(", ") || null
    : firstString(args, ["path", "pattern", "dir", "script"]);
  if (verb == null) return subject == null ? tool : `${tool}: ${subject}`;
  return subject == null ? verb.label : `${verb.label} ${subject}`;
}

/** The scope-relative paths a tool call writes: none for a read. */
export function changedPaths(tool: string, args: unknown): string[] {
  const writes = VERBS.some(
    (verb) => verb.writes === true && tool.startsWith(verb.prefix),
  );
  if (!writes) return [];
  if (tool.startsWith("apply_patch_")) return patchPaths(args);
  const path = firstString(args, ["path"]);
  return path == null ? [] : [path];
}

/** Every path a V4A patch names: added, updated, deleted and moved to. */
function patchPaths(args: unknown): string[] {
  const patch = firstString(args, ["patch"]);
  if (patch == null) return [];
  const headers = [
    "*** Add File: ",
    "*** Update File: ",
    "*** Delete File: ",
    "*** Move to: ",
  ];
  const paths: string[] = [];
  for (const line of patch.split("\n")) {
    const header = headers.find((h) => line.startsWith(h));
    if (header != null) paths.push(line.slice(header.length).trim());
  }
  return [...new Set(paths)];
}

/**
 * Where a path the agent wrote is in the workspace: the store is the folder,
 * and the agent's own root is inside it.
 *
 * The tool reports a path relative to the *agent's* scope, which is not the
 * workspace root whenever the application's project sits in a sub-directory —
 * so an editor open on `src/App.tsx` would be told that `src/App.tsx` changed
 * and be right by accident only for a store whose root is the project.
 */
export function workspacePath(
  store: string,
  root: string,
  path: string,
): string {
  return `/${[store, root, path].filter((part) => part !== "").join("/")}`;
}

/** The first of `keys` that is a non-empty string in `args`. */
function firstString(args: unknown, keys: string[]): string | null {
  if (args == null || typeof args !== "object") return null;
  for (const key of keys) {
    const value = (args as Record<string, unknown>)[key];
    if (typeof value === "string" && value !== "") return value;
  }
  return null;
}
