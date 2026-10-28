// The chat model: the socket protocol of §11.4, and the transcript it builds.
//
// Deliberately **not** a React component. The screen renders what is here, and
// what is here is a reducer plus a thin wrapper over one socket — so the part
// that has to be right (deltas appended in order, a tool call paired with its
// result, an error rendered rather than swallowed, an abort leaving the
// transcript intact) is testable without a browser or a server, which is what
// `agentChat.test.ts` does against a stubbed socket.
//
// The protocol constant below is the same string `sc-server` mounts the route
// on; a Rust test asserts the two spellings agree, because a page connecting to
// the wrong path looks exactly like an agent that never answers.

import { appHost, servedBaseDomain } from "./adminAddress";

/** The route the server serves the chat socket on (matches `AGENT_CHAT_ROUTE`). */
export const AGENT_CHAT_ROUTE = "/admin/agent-chat";

/** Where the chat socket is, for a page served from `location`.
 *
 * `wss:` wherever the page itself is over TLS: a `ws:` socket opened from an
 * `https:` page is mixed content and the browser blocks it, which would present
 * as an agent that never answers on exactly the deployments that are configured
 * correctly. */
export function agentChatUrl(location: { protocol: string; host: string }): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}${AGENT_CHAT_ROUTE}`;
}

/* --------------------------------------------------------------------------
 * The preview pane (TODO "The preview pane")
 *
 * An agent may carry the `preview_pane` trait, which contributes no tool: it
 * says that this agent's work can be looked at, and at which URL. The chat
 * screen reads it off the agent's stored traits and offers the split view — the
 * conversation in a column, the page beside it.
 *
 * The resolution lives here rather than in the component because it is the part
 * that can be wrong: a stored URL is a template (`//todo.{base}`) so one agent
 * follows the deployment from `localhost:3000` to the production domain, and it
 * ends up in an `iframe src`, so a scheme that would execute in the admin's own
 * origin must not reach the DOM. The trait refuses those on save; this refuses
 * them again on the way in, because a stored agent is not a trusted input.
 * ------------------------------------------------------------------------ */

/** The trait that puts a page beside the conversation. */
export const PREVIEW_PANE_TRAIT = "preview_pane";

/** What that trait was configured with. */
export type PreviewPane = {
  /** The URL as stored, `{base}`, `{host}` and all. */
  url: string;
  /** Reload the pane when the agent finishes a turn. */
  reloadOnTurn: boolean;
};

/** One enabled trait, as `listAgents` serves it. */
type EnabledTrait = { trait: string; config: unknown };

/** The preview pane `traits` declares, or `null` for an agent that has none.
 *
 * The first one wins: a second pane would be a second screen, and the button
 * that opens it can only open one. */
export function previewPaneOf(traits: EnabledTrait[] | undefined): PreviewPane | null {
  for (const enabled of traits ?? []) {
    if (enabled.trait !== PREVIEW_PANE_TRAIT) continue;
    const config = (enabled.config ?? {}) as Record<string, unknown>;
    const url = typeof config.url === "string" ? config.url.trim() : "";
    if (!url) continue;
    return { url, reloadOnTurn: config.reload_on_turn !== false };
  }
  return null;
}

/** The stored URL as a browser at `location` should load it, or `null` when it
 * is not one this pane will open.
 *
 * `{base}` becomes the base domain applications are served under, as
 * {@link appHost} has it for the application on `@` — so `//todo.{base}` is the
 * application's own subdomain on this deployment, and `//{base}` the one on the
 * base domain, wherever the admin UI itself is served. `{host}` becomes the
 * host the admin is being read from. What is
 * accepted is an absolute `http(s)` URL, a protocol-relative `//host/path`, or
 * a path on the admin's own origin — the same allow-list the trait validates
 * against, kept here too because this is the side that writes `src`. */
export function resolvePaneUrl(
  url: string,
  location: { protocol: string; host: string },
  base: string | null = servedBaseDomain(),
): string | null {
  const resolved = url
    .trim()
    .split("{base}")
    .join(appHost("@", location, base))
    .split("{host}")
    .join(location.host);
  if (resolved.startsWith("//")) {
    return resolved.length > 2 ? `${location.protocol}${resolved}` : null;
  }
  if (resolved.startsWith("/")) return resolved;
  if (resolved.startsWith("http://") || resolved.startsWith("https://")) return resolved;
  return null;
}

/** A width the pane can be looked at, in the order the buttons sit in.
 *
 * Three, because the question a width answers is "does this layout hold up on a
 * phone?" and not "what is it at 1180px": `full` is the pane itself, and the
 * other two are the CSS widths of the two devices whose breakpoints a layout
 * actually has to survive. */
export type PaneWidth = { name: string; label: string; px: number | null };

export const PANE_WIDTHS: PaneWidth[] = [
  { name: "full", label: "Full width", px: null },
  { name: "tablet", label: "Tablet width", px: 820 },
  { name: "phone", label: "Phone width", px: 390 },
];

/** One event the server sends. The six of §11.4, plus `controls` (below). */
export type ServerEvent =
  | { type: "text"; delta: string }
  | { type: "reasoning"; delta: string }
  | { type: "tool_call"; id: string; name: string; arguments: unknown }
  | {
      type: "tool_result";
      id: string;
      name: string;
      content: string;
      is_error: boolean;
      /** Images the tool returned — a `view_app` screenshot (TODO §7b). */
      images?: ImagePart[];
      /** What the tool showed the person and not the model (AS6.3). */
      display?: DisplayRef[];
    }
  | {
      type: "done";
      run: string | null;
      state: string;
      answer: string;
      /** How the loop concluded, when it did: `answered`, `max_steps`,
       * `aborted`, `over_budget` (with `budget`), `stuck` (with `reason`) or
       * `awaiting_user` (with `request`). */
      conclusion?: string;
      budget?: string;
      reason?: string;
      request?: UserRequest;
    }
  | { type: "error"; message: string }
  /** What the final answer showed the person: a spoken answer (AS11.2). */
  | { type: "display"; display: DisplayRef[] }
  | { type: "controls"; controls: unknown }
  /** After `start` on a run waiting for an answer (AS6.1). */
  | { type: "awaiting"; request: UserRequest }
  /** The loop compacted the context before its next model call (TODO §9). */
  | {
      type: "compaction";
      step: number;
      elided: number;
      before_tokens: number;
      after_tokens: number;
      summary?: string;
    };

/** Something a tool showed the person and not the model (TODO-agent-skills
 * AS6.3), fetched with `getRunPart`. */
export interface DisplayRef {
  id: string;
  kind: "rows" | "image" | "audio" | string;
  mime_type: string;
  summary: string;
}

/** One option of a question. */
export interface RequestOption {
  value: string;
  label?: string;
  description?: string;
}

/** A question a tool put to the person; the run waits for the answer
 * (AS6.1). */
export type UserRequest = { id: string; prompt: string } & (
  | { kind: "choice"; options: RequestOption[]; multiple?: boolean }
  | { kind: "text"; placeholder?: string }
  | { kind: "approve"; label: string; payload?: unknown }
  | { kind: "button"; label: string; payload?: unknown }
);

/** What an answer to `request` reads as in the transcript. */
export function answerLabel(request: UserRequest, value: unknown): string {
  switch (request.kind) {
    case "choice": {
      const label = (v: unknown) => {
        const option = request.options.find((o) => o.value === v);
        return option ? option.label || option.value : String(v);
      };
      return Array.isArray(value) ? value.map(label).join(", ") : label(value);
    }
    case "approve":
      return value === true ? "Approved" : "Declined";
    case "button":
      return request.label;
    default:
      return String(value);
  }
}

/** An image in a tool result, as the loop stores and streams it. */
export interface ImagePart {
  media_type: string;
  /** Base64. */
  data: string;
}

/** An image part as a URL an `<img>` can show. Only image types are kept. */
export function imageUrl(image: ImagePart): string | null {
  if (!/^image\/(jpeg|png|gif|webp)$/.test(image.media_type)) return null;
  if (!/^[A-Za-z0-9+/=]*$/.test(image.data)) return null;
  return `data:${image.media_type};base64,${image.data}`;
}

/** The URLs of the images a result carried, or nothing to add to an entry. */
function imagesOf(images: ImagePart[] | undefined): { images?: string[] } {
  const urls = (images ?? []).map(imageUrl).filter((u): u is string => u !== null);
  return urls.length > 0 ? { images: urls } : {};
}

/** One control a trait puts in the composer, beside the send button.
 *
 * The composer is the only place a person can say anything to an agent, so it
 * is where a trait's *modes* belong: "search the web for this one", "which
 * branch am I working on", "run it after you write it". A dropdown of a store's
 * directories set once beats the same sentence typed into every message, and a
 * toggle is a thing the model should be told rather than guess.
 *
 * Nothing declares one yet: no built-in trait has a mode. The shape is here —
 * and rendered — because the alternative is a composer whose toolbar row does
 * not exist and has to be invented under a trait that needs it. A control is
 * therefore **data**: a trait declares it the way it declares a tool's schema,
 * the server forwards the declaration in a `controls` event, and the panel
 * renders whatever arrives without knowing which trait sent it — the same rule
 * §11.2 sets for the trait config form.
 *
 * The two kinds are the two the toolbar of every chat interface has: a toggle
 * (a mode that is on or off) and a select (one of several). Both carry a value
 * that travels with the *next* message, which is what makes them a modifier on
 * what the person is about to say rather than a button that does something on
 * its own. A control that acts immediately would be a client→server frame of
 * its own, and is deliberately not invented until a trait asks for one. */
export type ComposerControl =
  | { kind: "toggle"; name: string; label: string; title?: string; default?: boolean }
  /** Sends a message at once — its label — with this control set to `true`. */
  | { kind: "button"; name: string; label: string; title?: string }
  | {
      kind: "select";
      name: string;
      label?: string;
      title?: string;
      options: { value: string; label: string }[];
      default?: string;
    }
  /** A select of messages (AS10.5): choosing one sends its value at once as
   * the person's message, and the select goes back to empty. */
  | {
      kind: "send";
      name: string;
      label?: string;
      title?: string;
      options: { value: string; label: string }[];
    };

/** What a control is currently set to, as it travels with a message. */
export type ControlValue = string | boolean;

/** One entry in the transcript as the panel renders it. */
export type Entry =
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string; reasoning: string }
  | {
      kind: "tool";
      id: string;
      name: string;
      args: unknown;
      /** `null` until the result arrives — which is what renders as "running". */
      result: string | null;
      isError: boolean;
      /** Screenshots the result carried, as `data:` URLs, shown inline. */
      images?: string[];
    }
  /** What a tool showed the person (AS6.3): fetched and drawn by id. */
  | { kind: "display"; parts: DisplayRef[] }
  /** A question the agent asked (AS6.1). Answerable while it is the one the
   * run waits on. */
  | { kind: "request"; request: UserRequest }
  /** A failure, in the transcript where it happened. A chat window that
   * silently stops is unfixable by the person watching it (§11.4). */
  | { kind: "error"; message: string }
  /** Why the loop stopped on its own — a budget, or loop control deciding the
   * agent was stuck. Not a failure: the conversation can be continued. */
  | { kind: "notice"; message: string }
  /** Where the loop compacted what the model is sent. Everything above it is
   * still here — the transcript is whole — but from this point the model saw
   * old tool results as stubs and, with a summary, the older turns only as
   * that summary, which the admin can expand. */
  | {
      kind: "compaction";
      elided: number;
      beforeTokens: number;
      afterTokens: number;
      summary: string | null;
    };

/** The whole of what the panel draws. */
export type ChatState = {
  entries: Entry[];
  /** Whether a turn is in flight: what disables the composer and shows Stop. */
  running: boolean;
  /** The run this conversation is, once the server has said. */
  runId: string | null;
  /** How the last turn ended, for the line under the transcript. */
  lastState: string | null;
  /** What the agent's traits put in the composer's toolbar. Empty until one
   * declares something, which no built-in trait does yet. */
  controls: ComposerControl[];
  /** What each of those is set to, keyed by control name. */
  controlValues: Record<string, ControlValue>;
  /** The question the run waits on, if it does (AS6.1). */
  awaiting: UserRequest | null;
};

/** A conversation with nothing in it yet. */
export function emptyChat(): ChatState {
  return {
    entries: [],
    running: false,
    runId: null,
    lastState: null,
    controls: [],
    controlValues: {},
    awaiting: null,
  };
}

/** The controls in a `controls` event, with anything unreadable dropped.
 *
 * A declaration comes off the socket, so it is checked here rather than
 * trusted: a trait that ships a malformed control should cost the composer that
 * one button, not the whole toolbar and not the transcript. */
export function normalizeControls(value: unknown): ComposerControl[] {
  if (!Array.isArray(value)) return [];
  const controls: ComposerControl[] = [];
  for (const raw of value) {
    const control = raw as Partial<ComposerControl> & Record<string, unknown>;
    if (typeof control?.name !== "string" || control.name === "") continue;
    if (control.kind === "toggle" || control.kind === "button") {
      if (typeof control.label !== "string") continue;
      controls.push(
        control.kind === "toggle"
          ? {
              kind: "toggle",
              name: control.name,
              label: control.label,
              title: typeof control.title === "string" ? control.title : undefined,
              default: control.default === true,
            }
          : {
              kind: "button",
              name: control.name,
              label: control.label,
              title: typeof control.title === "string" ? control.title : undefined,
            },
      );
    } else if (control.kind === "select" || control.kind === "send") {
      // An option with no label of its own is read by its value.
      const options = Array.isArray(control.options)
        ? control.options
            .filter((option) => typeof (option as { value?: unknown })?.value === "string")
            .map((option) => {
              const { value, label } = option as { value: string; label?: unknown };
              return { value, label: typeof label === "string" && label !== "" ? label : value };
            })
        : [];
      // A select with nothing to select is a dead control, not a narrow one.
      if (options.length === 0) continue;
      const label = typeof control.label === "string" ? control.label : undefined;
      const title = typeof control.title === "string" ? control.title : undefined;
      controls.push(
        control.kind === "send"
          ? { kind: "send", name: control.name, label, title, options }
          : {
              kind: "select",
              name: control.name,
              label,
              title,
              options,
              default: typeof control.default === "string" ? control.default : undefined,
            },
      );
    }
  }
  return controls;
}

/** What the controls start at, keeping whatever the person had already chosen.
 *
 * The declaration can arrive again mid-conversation (a trait whose options
 * depend on what the last turn did), and a dropdown that reset itself every
 * time would lose a choice made two messages ago. A value is kept only while
 * the control is still offered *and* still accepts it. */
export function defaultControlValues(
  controls: ComposerControl[],
  previous: Record<string, ControlValue> = {},
): Record<string, ControlValue> {
  const values: Record<string, ControlValue> = {};
  for (const control of controls) {
    const had = previous[control.name];
    // A button has no standing value: it is pressed, and sends.
    if (control.kind === "button" || control.kind === "send") continue;
    if (control.kind === "toggle") {
      values[control.name] = typeof had === "boolean" ? had : control.default === true;
    } else {
      const keeps = typeof had === "string" && control.options.some((o) => o.value === had);
      values[control.name] = keeps
        ? had
        : (control.options.find((o) => o.value === control.default)?.value ??
          control.options[0].value);
    }
  }
  return values;
}

/** Add what the person just said, and mark the turn as running.
 *
 * Separate from `applyEvent` because it is the one entry the *client* knows
 * about before the server does: the server echoes nothing back, so a transcript
 * that only grew from events would show an answer to a question nobody asked. */
export function applyUserMessage(state: ChatState, text: string): ChatState {
  return {
    ...state,
    entries: [...state.entries, { kind: "user", text }],
    running: true,
    lastState: null,
    // Answered, or passed over by an ordinary message: either way the question
    // is no longer waiting.
    awaiting: null,
  };
}

/** Fold one server event into the transcript. */
export function applyEvent(state: ChatState, event: ServerEvent): ChatState {
  switch (event.type) {
    case "text":
      return { ...state, entries: appendText(state.entries, event.delta, "text") };
    case "reasoning":
      return { ...state, entries: appendText(state.entries, event.delta, "reasoning") };
    case "tool_call":
      return {
        ...state,
        entries: [
          ...state.entries,
          {
            kind: "tool",
            id: event.id,
            name: event.name,
            args: event.arguments,
            result: null,
            isError: false,
          },
        ],
      };
    case "tool_result": {
      const settled: ChatState = {
        ...state,
        entries: state.entries.map((entry) =>
          entry.kind === "tool" && entry.id === event.id && entry.result === null
            ? {
                ...entry,
                result: event.content,
                isError: event.is_error,
                ...imagesOf(event.images),
              }
            : entry,
        ),
      };
      if (event.display && event.display.length > 0) {
        settled.entries = [...settled.entries, { kind: "display", parts: event.display }];
      }
      return settled;
    }
    case "error":
      // Appended, never replacing: a failure after two paragraphs and a tool
      // call is read alongside them, not instead of them.
      return { ...state, entries: [...state.entries, { kind: "error", message: event.message }] };
    case "compaction":
      return { ...state, entries: [...state.entries, compactionEntry(event)] };
    case "display":
      return event.display.length > 0
        ? { ...state, entries: [...state.entries, { kind: "display", parts: event.display }] }
        : state;
    case "controls": {
      const controls = normalizeControls(event.controls);
      return {
        ...state,
        controls,
        controlValues: defaultControlValues(controls, state.controlValues),
      };
    }
    case "awaiting":
      return withRequest(state, event.request);
    case "done": {
      const notice = conclusionNotice(event);
      const ended: ChatState = {
        ...state,
        entries: notice ? [...state.entries, { kind: "notice", message: notice }] : state.entries,
        running: false,
        runId: event.run ?? state.runId,
        lastState: event.state,
      };
      return event.request ? withRequest(ended, event.request) : ended;
    }
  }
}

/** `state` waiting on `request`, which closes the transcript unless it is
 * there already. */
function withRequest(state: ChatState, request: UserRequest): ChatState {
  const shown = state.entries.some((e) => e.kind === "request" && e.request.id === request.id);
  return {
    ...state,
    entries: shown ? state.entries : [...state.entries, { kind: "request", request }],
    awaiting: request,
  };
}

/** A compaction as the server spells it: a socket event, or a stored run's
 * record. */
type CompactionRecord = {
  elided?: number;
  before_tokens?: number;
  after_tokens?: number;
  summary?: string | null;
};

/** A compaction, as a transcript entry. */
function compactionEntry(raw: CompactionRecord): Entry {
  return {
    kind: "compaction",
    elided: raw.elided ?? 0,
    beforeTokens: raw.before_tokens ?? 0,
    afterTokens: raw.after_tokens ?? 0,
    summary: typeof raw.summary === "string" ? raw.summary : null,
  };
}

/** The line a compaction marker reads as. */
export function compactionLabel(entry: Extract<Entry, { kind: "compaction" }>): string {
  const what: string[] = [];
  if (entry.elided > 0) {
    what.push(`${entry.elided} old tool result${entry.elided === 1 ? "" : "s"} cleared`);
  }
  if (entry.summary !== null) what.push("older turns summarised");
  const detail = what.length > 0 ? `: ${what.join(", ")}` : "";
  return `Context compacted from ${entry.beforeTokens} to ${entry.afterTokens} tokens${detail}`;
}

/** A tool call entry. */
export type ToolEntry = Extract<Entry, { kind: "tool" }>;

/** One thing the transcript draws: an entry, or a run of calls to one tool. */
export type TranscriptItem =
  | { kind: "entry"; entry: Entry; index: number }
  | { kind: "tools"; name: string; calls: ToolEntry[]; index: number };

/** Fold consecutive calls to the same tool into one item.
 *
 * An agent paging through records, or editing five files, calls one tool many
 * times in a row, and a badge per call buries what it said between them. A
 * run of two or more reads as one badge with a count, opening onto the calls.
 * `index` is where the item starts in `entries`, so a key stays put while the
 * run grows. */
export function groupTranscript(entries: Entry[]): TranscriptItem[] {
  const items: TranscriptItem[] = [];
  entries.forEach((entry, index) => {
    const last = items[items.length - 1];
    if (entry.kind === "tool" && last) {
      if (last.kind === "tools" && last.name === entry.name) {
        last.calls.push(entry);
        return;
      }
      if (last.kind === "entry" && last.entry.kind === "tool" && last.entry.name === entry.name) {
        items[items.length - 1] = {
          kind: "tools",
          name: entry.name,
          calls: [last.entry, entry],
          index: last.index,
        };
        return;
      }
    }
    items.push({ kind: "entry", entry, index });
  });
  return items;
}

/** A loop conclusion as the server spells it, on a `done` event or in a stored
 * run's context. */
export type Conclusion = { conclusion?: string; budget?: string; reason?: string };

/** What the person is told when the loop stopped without answering, or `null`
 * for an answer or a stop they pressed themselves. */
export function conclusionNotice(conclusion: Conclusion | null | undefined): string | null {
  switch (conclusion?.conclusion) {
    case "max_steps":
      return "The agent used all of its steps without finishing.";
    case "over_budget":
      // The caller's LLM credits are theirs, not the agent's (TODO-payments P6.3).
      if (conclusion.budget === "credits") {
        return "You are out of LLM credits. Top up to carry on.";
      }
      return `The agent ran out of its ${(conclusion.budget ?? "").replace("_", " ")} budget.`;
    case "stuck":
      return `The agent was stopped because it was going round in circles: ${conclusion.reason ?? ""}`;
    default:
      return null;
  }
}

/** A short label for a run list, beside its state: `stuck`, `over budget`,
 * `out of steps`, or `null` when the state says enough. */
export function conclusionLabel(conclusion: Conclusion | null | undefined): string | null {
  switch (conclusion?.conclusion) {
    case "max_steps":
      return "out of steps";
    case "over_budget":
      return conclusion.budget === "credits" ? "out of credits" : "over budget";
    case "stuck":
      return "stuck";
    case "awaiting_user":
      return "waiting for an answer";
    default:
      return null;
  }
}

/** Append a delta to the assistant entry being written, starting one if the
 * last thing in the transcript is not one.
 *
 * Starting a new entry after a tool is what makes a turn read as it happened:
 * "let me look", the tool, then the answer — rather than one paragraph with a
 * tool call spliced into the middle of it. */
function appendText(entries: Entry[], delta: string, into: "text" | "reasoning"): Entry[] {
  const last = entries[entries.length - 1];
  if (last?.kind === "assistant") {
    const updated: Entry = { ...last, [into]: last[into] + delta };
    return [...entries.slice(0, -1), updated];
  }
  return [
    ...entries,
    {
      kind: "assistant",
      text: into === "text" ? delta : "",
      reasoning: into === "reasoning" ? delta : "",
    },
  ];
}

/** Parse one frame off the socket, or `null` if it is not an event we know.
 *
 * An unreadable frame is dropped rather than thrown: the socket is a live
 * conversation, and a parse failure in the middle of one must not take the
 * transcript with it. */
export function parseEvent(data: string): ServerEvent | null {
  try {
    const value: unknown = JSON.parse(data);
    if (value && typeof value === "object" && typeof (value as ServerEvent).type === "string") {
      return value as ServerEvent;
    }
  } catch {
    // fall through
  }
  return null;
}

/** A run of prose, or a fenced code block, in what the agent said. */
export type Block =
  | { kind: "prose"; text: string }
  | { kind: "code"; language: string; text: string };

/** Split an answer on ``` fences.
 *
 * Not a Markdown renderer, and deliberately not the beginning of one: an agent
 * that reads and writes files answers with code, and code set in the body font
 * with its indentation collapsed is the one part of an answer that is unusable
 * rather than merely plain. Everything else — headings, lists, emphasis — is
 * still legible as it was typed, so it stays as typed.
 *
 * An unterminated fence is still a code block: a streaming answer is read while
 * the fence is still open, and waiting for the closing one would reformat the
 * paragraph under the reader every time a turn finished. */
export function splitCodeBlocks(text: string): Block[] {
  const blocks: Block[] = [];
  const fence = /^```([^\n`]*)\n?/gm;
  let at = 0;
  let match: RegExpExecArray | null;
  while ((match = fence.exec(text)) !== null) {
    const prose = text.slice(at, match.index);
    if (prose.trim() !== "") blocks.push({ kind: "prose", text: prose.replace(/\n+$/, "") });
    const start = match.index + match[0].length;
    const close = fence.exec(text);
    const end = close ? close.index : text.length;
    const code = text.slice(start, end).replace(/\n$/, "");
    if (code !== "") blocks.push({ kind: "code", language: match[1].trim(), text: code });
    if (!close) return blocks;
    // A `null` from `exec` has already rewound `lastIndex` to 0, so an
    // unterminated fence must leave the loop rather than search again from the
    // top — which is the same fence, for ever.
    at = close.index + close[0].length;
  }
  const rest = text.slice(at);
  if (rest.trim() !== "") blocks.push({ kind: "prose", text: rest.replace(/\n+$/, "") });
  return blocks;
}

/** The transcript of a **stored** run, rebuilt from `_fd_runs.context`.
 *
 * This is how the history reopens a conversation: the run's context is the
 * loop's own state (§11.2), whose `messages` are the same `LlmMessage`s the
 * socket streamed. Reading them here rather than asking the server for a second
 * rendering is what keeps one description of what a transcript is. */
export function transcriptFromRun(context: unknown): Entry[] {
  const messages = (context as { messages?: unknown })?.messages;
  if (!Array.isArray(messages)) return [];
  const entries: Entry[] = [];
  // Each compaction is marked where the transcript had reached when it
  // happened (`at`), in the loop's own state beside the messages.
  const records = (context as { context?: { compactions?: unknown } })?.context?.compactions;
  const compactions = (Array.isArray(records) ? records : [])
    .filter((c): c is CompactionRecord & { at: number } => typeof c?.at === "number")
    .sort((a, b) => a.at - b.at);
  let marked = 0;
  const markUpTo = (index: number) => {
    while (marked < compactions.length && compactions[marked].at <= index) {
      entries.push(compactionEntry(compactions[marked]));
      marked += 1;
    }
  };
  for (const [index, raw] of messages.entries()) {
    markUpTo(index);
    const message = raw as {
      role?: string;
      content?: string;
      tool_calls?: { id: string; name: string; arguments: unknown }[];
      tool_call_id?: string;
      name?: string;
      images?: ImagePart[];
    };
    if (message.role === "user") {
      entries.push({ kind: "user", text: message.content ?? "" });
    } else if (message.role === "assistant") {
      // A turn that was only tool calls said nothing, and an empty bubble would
      // be putting words in the model's mouth.
      if ((message.content ?? "") !== "") {
        entries.push({ kind: "assistant", text: message.content ?? "", reasoning: "" });
      }
      // What the final answer showed: a spoken answer (AS11.2).
      const spoken = displayOf(context, `answer:${index}`);
      if (spoken.length > 0) entries.push({ kind: "display", parts: spoken });
      for (const call of message.tool_calls ?? []) {
        entries.push({
          kind: "tool",
          id: call.id,
          name: call.name,
          args: call.arguments,
          result: null,
          isError: false,
        });
      }
    } else if (message.role === "tool_result") {
      const call = entries.find(
        (entry) => entry.kind === "tool" && entry.id === message.tool_call_id,
      );
      if (call && call.kind === "tool") {
        call.result = message.content ?? "";
        // The loop writes a failed tool's error as its result, prefixed so the
        // model can tell (§11.2). The same prefix is what tells the panel.
        call.isError = (message.content ?? "").startsWith("error: ");
        const shown = imagesOf(message.images);
        if (shown.images) call.images = shown.images;
      }
      const parts = displayOf(context, message.tool_call_id);
      if (parts.length > 0) entries.push({ kind: "display", parts });
    }
  }
  markUpTo(Number.POSITIVE_INFINITY);
  // How it ended, where the loop stopped on its own.
  const phase = (context as { phase?: { phase?: string; conclusion?: Conclusion } })?.phase;
  const notice = phase?.phase === "done" ? conclusionNotice(phase.conclusion) : null;
  if (notice) entries.push({ kind: "notice", message: notice });
  const request = pendingRequest(context);
  if (request) entries.push({ kind: "request", request });
  return entries;
}

/** What tool call `id` showed the person, from a stored run's loop state. */
function displayOf(context: unknown, id: string | undefined): DisplayRef[] {
  const display = (context as { display?: Record<string, unknown> })?.display;
  const parts = id && display ? display[id] : undefined;
  return Array.isArray(parts) ? (parts as DisplayRef[]) : [];
}

/** The question a stored run waits on, if it does. */
export function pendingRequest(context: unknown): UserRequest | null {
  const phase = (context as { phase?: { phase?: string; conclusion?: { conclusion?: string; request?: UserRequest } } })
    ?.phase;
  return phase?.phase === "done" && phase.conclusion?.conclusion === "awaiting_user"
    ? (phase.conclusion.request ?? null)
    : null;
}

/** The bit of `WebSocket` this model uses — so a test can be a plain object. */
export interface SocketLike {
  send(data: string): void;
  close(): void;
  onopen: (() => void) | null;
  onmessage: ((event: { data: string }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
}

/** One chat: a socket, the transcript it is building, and the three things a
 * person can do to it. */
export class ChatSession {
  private state: ChatState = emptyChat();
  private open = false;
  /** Message frames composed before the socket opened, sent in order once it
   * does — each already carrying the controls it was composed with. */
  private pending: string[] = [];

  constructor(
    private socket: SocketLike,
    private agent: string,
    private onChange: (state: ChatState) => void,
    initial?: { runId?: string | null; entries?: Entry[] },
  ) {
    if (initial?.entries?.length) {
      this.state = { ...this.state, entries: initial.entries };
    }
    if (initial?.runId) {
      this.state = { ...this.state, runId: initial.runId };
    }
    // A reopened conversation that ended on a question is still waiting.
    const last = initial?.entries?.[initial.entries.length - 1];
    if (last?.kind === "request") {
      this.state = { ...this.state, awaiting: last.request };
    }
    socket.onopen = () => {
      this.open = true;
      // `start` names the agent and, when reopening a conversation, the run to
      // carry on — which is what makes the history's Continue button possible.
      this.socket.send(
        JSON.stringify({ type: "start", agent: this.agent, run: this.state.runId }),
      );
      for (const frame of this.pending.splice(0)) {
        this.socket.send(frame);
      }
    };
    socket.onmessage = (event) => {
      const parsed = parseEvent(event.data);
      if (parsed) this.update(applyEvent(this.state, parsed));
    };
    socket.onerror = () => {
      this.update(
        applyEvent(this.state, {
          type: "error",
          message: "The connection to the server failed.",
        }),
      );
    };
    socket.onclose = () => {
      this.open = false;
      // A socket that closed mid-turn would otherwise leave the composer
      // disabled for ever, waiting for a `done` that cannot arrive.
      if (this.state.running) {
        this.update({
          ...applyEvent(this.state, {
            type: "error",
            message: "The connection closed before the agent finished.",
          }),
          running: false,
        });
      }
    };
  }

  /** The transcript as it stands. */
  current(): ChatState {
    return this.state;
  }

  /** Say something. Refused while a turn is running, as the loop itself refuses
   * it: a message spliced into a history the model is already answering is how a
   * question gets answered before it was asked. */
  send(text: string): void {
    if (this.state.running || text.trim() === "") return;
    this.update(applyUserMessage(this.state, text));
    const frame = this.messageFrame(text);
    if (this.open) {
      this.socket.send(frame);
    } else {
      // Frozen now rather than on send: the controls belong to the message that
      // was composed with them, not to whatever they say when the socket opens.
      this.pending.push(frame);
    }
  }

  /** Answer the question the run waits on (AS6.2): the answer is the next
   * turn, shown as what the person chose. */
  answer(request: UserRequest, value: unknown): void {
    if (this.state.running || this.state.awaiting?.id !== request.id) return;
    this.update(applyUserMessage(this.state, answerLabel(request, value)));
    this.sendFrame(
      JSON.stringify({
        type: "message",
        answers_request: request.id,
        value,
        ...this.controlsField(),
      }),
    );
  }

  /** Press a button control: a message — its label — sent at once with that
   * control `true`. */
  press(name: string): void {
    const control = this.state.controls.find((c) => c.name === name);
    if (this.state.running || control?.kind !== "button") return;
    this.update(applyUserMessage(this.state, control.label));
    this.sendFrame(
      JSON.stringify({
        type: "message",
        text: control.label,
        controls: { ...this.state.controlValues, [name]: true },
      }),
    );
  }

  private sendFrame(frame: string): void {
    if (this.open) {
      this.socket.send(frame);
    } else {
      this.pending.push(frame);
    }
  }

  private controlsField(): { controls?: Record<string, ControlValue> } {
    const values = this.state.controlValues;
    return Object.keys(values).length > 0 ? { controls: values } : {};
  }

  /** Set one composer control. Takes effect on the *next* message: a control is
   * a modifier on what is about to be said, and a turn already in flight was
   * sent with the values it was sent with. */
  setControl(name: string, value: ControlValue): void {
    const control = this.state.controls.find((c) => c.name === name);
    if (!control) return;
    // A select of messages sends the one chosen, at once (AS10.5).
    if (control.kind === "send") {
      if (typeof value === "string" && control.options.some((o) => o.value === value)) {
        this.send(value);
      }
      return;
    }
    this.update({
      ...this.state,
      controlValues: { ...this.state.controlValues, [name]: value },
    });
  }

  /** One `message` frame, carrying the controls only when there are any — so an
   * agent with no modes sends exactly the frame it sent before them. */
  private messageFrame(text: string): string {
    const values = this.state.controlValues;
    return JSON.stringify(
      Object.keys(values).length > 0
        ? { type: "message", text, controls: values }
        : { type: "message", text },
    );
  }

  /** Stop the turn. The transcript stays exactly as it is — the server ends the
   * run and answers with `done`, which is what clears `running`. */
  abort(): void {
    if (!this.state.running || !this.open) return;
    this.socket.send(JSON.stringify({ type: "abort" }));
  }

  /** Close the socket (leaving the page). */
  close(): void {
    this.socket.close();
  }

  private update(state: ChatState): void {
    this.state = state;
    this.onChange(state);
  }
}
