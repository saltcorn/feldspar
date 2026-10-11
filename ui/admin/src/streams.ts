// The streams screens' model: the Observe socket's frame types, and the pure
// functions the list, the form and the Observe screen would otherwise each
// invent (TODO "Streams", task 7.2).
//
// Three of the stream endpoints' fields are declared `json` in the endpoint set
// and therefore arrive as `unknown` in the generated client — the element type,
// the status and the counters. That is right for the *wire*, for the reason it
// is right of a model's outcome: an element type is a provider's vocabulary and
// a status is the supervisor's, neither of which the endpoint set should be
// declaring the shape of. It is wrong for a screen, which has to render them.
// So the shapes are written out here once, as the discriminated unions their
// Rust originals serialise to, and the screens read them through the narrowing
// functions below rather than casting at each use.
//
// The socket's frames are written here too, beside the chat's in `agentChat.ts`
// and for the same reason: `sc-api` generates the typed *request/response*
// client, and a socket has no shape in it (§10). So the four frames of §9 are
// declared by hand, and a Rust test pins the route this file names against the
// one `sc-server` mounts — a page connecting to the wrong path looks exactly
// like a stream that never publishes.
//
// The rest is arithmetic with an opinion:
//
//   - a **status** is four cases and a colour, and `failed` is *yellow* rather
//     than red, because the supervisor is retrying and a stream that will be
//     back in thirty seconds is not the same news as one that cannot be saved;
//   - an **element renders against its declared type**, which is the whole
//     point of declaring one: a table of the declared keys for `json`, the text
//     for `text`, and a hex head for `binary` — never bytes pretended to be
//     characters;
//   - and the **tail is bounded**, because an Observe screen left open on a
//     busy stream would otherwise grow a DOM until the tab dies.

import type { ListStreamProvidersResponse, ListStreamsResponse } from "./client";
import type { FieldSpec } from "./settings";

/** One stream, as the list and the form see it. */
export type StreamItem = ListStreamsResponse[number];

/** One provider the picker offers, with the settings it declares. */
export type StreamProviderInfo = ListStreamProvidersResponse["providers"][number];

// --- the element type --------------------------------------------------------

/** One declared key of a `json` element (`sc_stream::ElementField`).
 *
 * `type` is the basic type's *name* — `"float"`, not `"float8"` — because this
 * declaration is read by this screen and by a module in JavaScript, and neither
 * speaks Postgres. */
export type ElementField = {
  name: string;
  type: string;
  /** Absent means optional: the Rust side skips it when false. */
  required?: boolean;
};

/** What the elements of a stream are (`sc_stream::ElementType`), tagged `kind`. */
export type ElementType =
  | { kind: "json"; keys: ElementField[] }
  | { kind: "text"; encoding: string }
  | { kind: "binary" };

/** Read an `unknown` element type off the wire, or nothing.
 *
 * Nothing is a real answer and not an error: a stream whose provider could not
 * be resolved has no element type, and it is still a stream the screen shows
 * the status of and offers the Edit that repairs it. */
export function readElementType(raw: unknown): ElementType | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as { kind?: unknown; keys?: unknown; encoding?: unknown };
  if (value.kind === "json") {
    const keys = Array.isArray(value.keys) ? value.keys : [];
    return {
      kind: "json",
      keys: keys
        .filter((k): k is ElementField => Boolean(k) && typeof k === "object")
        .map((k) => ({
          name: String(k.name ?? ""),
          type: String(k.type ?? ""),
          required: Boolean(k.required),
        })),
    };
  }
  if (value.kind === "text") {
    return { kind: "text", encoding: String(value.encoding ?? "utf8") };
  }
  if (value.kind === "binary") return { kind: "binary" };
  return null;
}

/** A one-line description of an element type, for the list and the form. */
export function elementTypeSummary(type: ElementType | null): string {
  if (!type) return "—";
  if (type.kind === "text") return `Text (${type.encoding})`;
  if (type.kind === "binary") return "Binary";
  if (type.keys.length === 0) return "JSON";
  return `JSON: ${type.keys.map((k) => `${k.name} (${k.type})`).join(", ")}`;
}

// --- the envelope ------------------------------------------------------------

/** One element as it arrives (§4). A wire contract: the same JSON a trigger's
 * `only_if` reads and an application's generated client is typed from. */
export type Envelope = {
  stream: string;
  /** The topic it was published on, for a stream with topics — a user's id or
   * a row's key (TODO.md "Live updates" §2). Absent on a stream without. */
  topic?: string;
  value: unknown;
  received_at: string;
  /** The provider's own metadata — MQTT's topic, QoS and retain flag. Absent
   * for a provider that has none. */
  source?: unknown;
};

/** Read an `unknown` envelope off a socket frame, or nothing. */
export function readEnvelope(raw: unknown): Envelope | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as Partial<Envelope>;
  if (typeof value.received_at !== "string") return null;
  return {
    stream: typeof value.stream === "string" ? value.stream : "",
    ...(typeof value.topic === "string" ? { topic: value.topic } : {}),
    value: value.value,
    received_at: value.received_at,
    ...(value.source === undefined || value.source === null ? {} : { source: value.source }),
  };
}

/** How a stream's elements are split into topics (`sc_stream::TopicSpec`),
 * tagged `kind`. */
export type TopicKind = "single" | "user" | "row" | "per_element_row";

/** Read a stream's `topics` off the wire: `single` for anything unreadable,
 * which is what every stream without topics is. */
export function readTopics(raw: unknown): { kind: TopicKind; table?: string } {
  if (!raw || typeof raw !== "object") return { kind: "single" };
  const value = raw as { kind?: unknown; table?: unknown };
  switch (value.kind) {
    case "user":
      return { kind: "user" };
    case "row":
    case "per_element_row":
      return { kind: value.kind, ...(typeof value.table === "string" ? { table: value.table } : {}) };
    default:
      return { kind: "single" };
  }
}

/** What the Observe screen calls a stream's topics, for its column heading.
 * `t` is the screen's translator, called here with each message written out —
 * the message id is the English text, so it has to be a literal at the call. */
export function topicLabel(
  t: (text: string, args?: Record<string, string>) => string,
  topics: { kind: TopicKind; table?: string },
): string {
  switch (topics.kind) {
    case "user":
      return t("Topic (user)");
    case "row":
    case "per_element_row":
      return topics.table ? t("Topic ({table} row)", { table: topics.table }) : t("Topic (row)");
    default:
      return t("Topic");
  }
}

/** The envelopes on topics matching `query`: a substring, so a user's id can
 * be found by its first few characters. A blank query is every envelope. */
export function filterByTopic(tail: Envelope[], query: string): Envelope[] {
  const wanted = query.trim();
  if (wanted === "") return tail;
  return tail.filter((envelope) => (envelope.topic ?? "").includes(wanted));
}

// --- the status and the counters --------------------------------------------

/** How a running stream is going (`sc_stream::StreamStatus`), tagged `status`. */
export type StreamStatusValue =
  | { status: "starting" }
  | { status: "running"; since: string }
  | { status: "failed"; error: string; since: string; attempt: number }
  | { status: "stopped" };

/** Read an `unknown` status off the wire, or nothing — which is the answer for
 * a stream this process holds no running copy of at all. */
export function readStatus(raw: unknown): StreamStatusValue | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as { status?: unknown; since?: unknown; error?: unknown; attempt?: unknown };
  switch (value.status) {
    case "starting":
      return { status: "starting" };
    case "stopped":
      return { status: "stopped" };
    case "running":
      return { status: "running", since: String(value.since ?? "") };
    case "failed":
      return {
        status: "failed",
        error: String(value.error ?? ""),
        since: String(value.since ?? ""),
        attempt: Number(value.attempt ?? 0),
      };
    default:
      return null;
  }
}

/** The colours a status badge comes in — Tabler's, as `layout.tsx` names them. */
export type StatusTone = "green" | "red" | "yellow" | "blue" | "secondary";

/** What a status reads as, and what colour it is.
 *
 * `failed` is **yellow**, not red: the supervisor is retrying with backoff
 * (§6), so a broker that bounced is a stream that will be back — a different
 * piece of news from a stream whose provider is gone, which is the `error` on
 * the row and is red. A stream nobody has started here is `secondary`, because
 * "not running on this server" is a fact rather than a fault (§6: one process,
 * one subscription).
 *
 * `disabled` is passed in rather than read from the status, because the two are
 * different facts: `enabled` is on the row and the status is the supervisor's,
 * and a disabled stream that this process has never held has no status at all. */
export function statusLabel(
  status: StreamStatusValue | null,
  enabled: boolean,
): { label: string; tone: StatusTone; title?: string } {
  if (!enabled) return { label: "Disabled", tone: "secondary" };
  if (!status) return { label: "Not running here", tone: "secondary" };
  switch (status.status) {
    case "starting":
      return { label: "Starting", tone: "blue" };
    case "running":
      return { label: "Running", tone: "green", title: `Connected ${formatTimestamp(status.since)}` };
    case "failed":
      return {
        label: `Retrying (${status.attempt})`,
        tone: "yellow",
        title: status.error,
      };
    case "stopped":
      return { label: "Stopped", tone: "secondary" };
  }
}

/** What a running stream has done since this server started (§7). */
export type Counters = {
  elements: number;
  dropped_for_triggers: number;
  dropped_for_rate: number;
  malformed: number;
  last_element_at: string | null;
};

const NO_COUNTERS: Counters = {
  elements: 0,
  dropped_for_triggers: 0,
  dropped_for_rate: 0,
  malformed: 0,
  last_element_at: null,
};

/** Read an `unknown` counter block, defaulting every count to zero.
 *
 * Zeros rather than nothing, so the list has one column shape for every row: a
 * stream that has delivered nothing and a stream this process is not running
 * are told apart by the status beside them, not by a missing cell. */
export function readCounters(raw: unknown): Counters {
  if (!raw || typeof raw !== "object") return { ...NO_COUNTERS };
  const value = raw as Record<string, unknown>;
  const count = (key: string): number => {
    const n = Number(value[key]);
    return Number.isFinite(n) ? n : 0;
  };
  const at = value.last_element_at;
  return {
    elements: count("elements"),
    dropped_for_triggers: count("dropped_for_triggers"),
    dropped_for_rate: count("dropped_for_rate"),
    malformed: count("malformed"),
    last_element_at: typeof at === "string" ? at : null,
  };
}

/** The counters worth a line under the element count, and only when they are
 * not zero.
 *
 * §7's point is that "a stream that is dropping is a thing you can see", and
 * the way to keep that visible is to say nothing at all about the three drop
 * counters while they are zero — a row of `0 dropped · 0 malformed` on every
 * healthy stream is how a reader learns to stop reading that column. */
export function counterNotes(counters: Counters): string[] {
  const notes: string[] = [];
  if (counters.dropped_for_triggers > 0) {
    notes.push(`${formatCount(counters.dropped_for_triggers)} dropped for triggers`);
  }
  if (counters.dropped_for_rate > 0) {
    notes.push(`${formatCount(counters.dropped_for_rate)} over the rate cap`);
  }
  if (counters.malformed > 0) notes.push(`${formatCount(counters.malformed)} malformed`);
  return notes;
}

/** What a `table_changes` stream's per-element checks decided since the server
 * started (`sc_live::ChangeCounts`, TODO.md "Live updates" L3.9): every element
 * × subscription pair checked, how many were sent something, how many were
 * withheld by the read rule or fell outside the subscriber's filter, and the
 * time spent deciding readability. */
export type ChangeCounts = {
  checked: number;
  delivered: number;
  suppressed_access: number;
  suppressed_filter: number;
  evaluator_micros: number;
};

/** The `changes` field of a stream row or status, or `null` for a stream that
 * has none (any stream but `table_changes`, and one nobody has subscribed to on
 * this server). */
export function readChanges(raw: unknown): ChangeCounts | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as Record<string, unknown>;
  const count = (key: string): number => {
    const n = Number(value[key]);
    return Number.isFinite(n) ? n : 0;
  };
  return {
    checked: count("checked"),
    delivered: count("delivered"),
    suppressed_access: count("suppressed_access"),
    suppressed_filter: count("suppressed_filter"),
    evaluator_micros: count("evaluator_micros"),
  };
}

/** The line under a `table_changes` stream's subscriber count: what was sent,
 * what the read rule withheld, what the filters left out, and the evaluator's
 * time — the last only once it is worth a millisecond. Empty before anything
 * was checked. */
export function changeNotes(changes: ChangeCounts | null): string[] {
  if (!changes || changes.checked === 0) return [];
  const notes = [
    `${formatCount(changes.delivered)} of ${formatCount(changes.checked)} sent`,
  ];
  if (changes.suppressed_access > 0) {
    notes.push(`${formatCount(changes.suppressed_access)} withheld by access`);
  }
  if (changes.suppressed_filter > 0) {
    notes.push(`${formatCount(changes.suppressed_filter)} outside filters`);
  }
  const ms = Math.round(changes.evaluator_micros / 1000);
  if (ms > 0) notes.push(`${formatCount(ms)} ms evaluating`);
  return notes;
}

/** A count, grouped — `1,024`. A stream's element count is the one number on
 * this screen that reaches seven digits in an afternoon. */
export function formatCount(n: number): string {
  return Number.isFinite(n) ? Math.trunc(n).toLocaleString() : "—";
}

/** A timestamp as the local reading of it, or the raw text when it will not
 * parse (which is a thing to show, not to hide). */
export function formatTimestamp(value: string | null): string {
  if (!value) return "—";
  const when = new Date(value);
  return Number.isNaN(when.getTime()) ? value : when.toLocaleString();
}

/** Just the clock part of a timestamp, for a tail where every row is today. */
export function formatTime(value: string): string {
  const when = new Date(value);
  return Number.isNaN(when.getTime()) ? value : when.toLocaleTimeString();
}

// --- the form ----------------------------------------------------------------

/** The settings spec of the picked provider, or nothing to render.
 *
 * The form knows no provider: it renders whatever `config_spec` the picked one
 * declares (§3), which is what makes a provider supplied by a module get a
 * working form with no change to this bundle. */
export function providerSpec(
  providers: StreamProviderInfo[] | null,
  name: string,
): FieldSpec[] {
  const picked = providers?.find((p) => p.name === name);
  return (picked?.config_spec ?? []) as FieldSpec[];
}

/** Whether a name is one the server will take.
 *
 * A stream's name becomes a socket path segment and a trigger's channel (§5),
 * so it is an identifier rather than a title. Checked here only to say so
 * *while the admin is typing it*; the server checks the same rule on save and
 * its message is the authority. */
export function nameProblem(name: string): string | null {
  const trimmed = name.trim();
  if (trimmed === "") return "A stream needs a name.";
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(trimmed)) {
    return "A stream's name becomes part of a URL and a trigger's channel: letters, digits and underscores, not starting with a digit.";
  }
  return null;
}

// --- rendering an element ----------------------------------------------------

/** The columns an element table has, for a `json` stream.
 *
 * The declared keys, in the order they were declared, and nothing else: a
 * publisher that adds a field does not get a column (§4 carries unknown keys
 * through, it does not promote them), because a table whose columns move when
 * somebody else deploys is not a table anybody can read. The extra keys are
 * still in the row's JSON, which is what the expander shows. */
export function elementColumns(type: ElementType | null): string[] {
  return type?.kind === "json" ? type.keys.map((k) => k.name) : [];
}

/** One `json` element as the cells of its declared columns.
 *
 * A declared key that is absent is `—`, which is §4's "absent is null" said in
 * a table cell. A key whose value is an object or an array is printed as
 * compact JSON rather than `[object Object]`. */
export function elementCells(envelope: Envelope, type: ElementType | null): string[] {
  const value =
    envelope.value && typeof envelope.value === "object" && !Array.isArray(envelope.value)
      ? (envelope.value as Record<string, unknown>)
      : {};
  return elementColumns(type).map((key) => cellText(value[key]));
}

/** Keys an element carried that the type did not declare — carried through
 * (§4), and worth saying so the admin can add them to the declaration. */
export function extraKeys(envelope: Envelope, type: ElementType | null): string[] {
  if (type?.kind !== "json") return [];
  if (!envelope.value || typeof envelope.value !== "object" || Array.isArray(envelope.value)) {
    return [];
  }
  const declared = new Set(type.keys.map((k) => k.name));
  return Object.keys(envelope.value as Record<string, unknown>).filter((k) => !declared.has(k));
}

/** One value as a cell. */
export function cellText(value: unknown): string {
  if (value === undefined || value === null) return "—";
  if (typeof value === "string") return value;
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

/** A `text` element's text, or "" for one that is not a string. */
export function elementText(envelope: Envelope): string {
  return typeof envelope.value === "string" ? envelope.value : cellText(envelope.value);
}

/** The head of a `binary` element as hex, with its length.
 *
 * A hex head rather than the text it is not (§4). `bytes` bounds what is shown,
 * because a megabyte of hex in a live tail is a frozen tab. */
export function hexHead(base64: string, bytes = 16): { hex: string; length: number | null } {
  const raw = decodeBase64(base64);
  if (raw === null) return { hex: "", length: null };
  const head = Array.from(raw.slice(0, bytes))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join(" ");
  return { hex: raw.length > bytes ? `${head} …` : head, length: raw.length };
}

/** Base64 → bytes, or nothing for something that is not base64.
 *
 * `atob` is used where it exists (a browser, and jsdom) and the arithmetic is
 * done by hand where it does not, so this function is testable in node without
 * a DOM — which is the only reason it is written out. */
function decodeBase64(value: string): Uint8Array | null {
  if (typeof value !== "string") return null;
  const cleaned = value.trim();
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(cleaned) || cleaned.length % 4 !== 0) return null;
  const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const out: number[] = [];
  let buffer = 0;
  let bits = 0;
  for (const char of cleaned) {
    if (char === "=") break;
    buffer = (buffer << 6) | ALPHABET.indexOf(char);
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push((buffer >> bits) & 0xff);
    }
  }
  return Uint8Array.from(out);
}

// --- the Observe socket ------------------------------------------------------

/** The route the server serves an Observe socket on (matches
 * `STREAM_OBSERVE_ROUTE`; a Rust test asserts the two spellings agree). */
export const STREAM_OBSERVE_ROUTE = "/api/streams/{id}/observe";

/** Where the Observe socket for `id` is, for a page served from `location`.
 *
 * `wss:` wherever the page itself is over TLS: a `ws:` socket opened from an
 * `https:` page is mixed content and the browser blocks it, which would present
 * as a stream that never publishes on exactly the deployments that are
 * configured correctly. (The chat socket's rule, for the chat socket's
 * reason.) */
export function streamObserveUrl(
  location: { protocol: string; host: string },
  id: string,
): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const path = STREAM_OBSERVE_ROUTE.replace("{id}", encodeURIComponent(id));
  return `${scheme}//${location.host}${path}`;
}

/** One frame the server sends down an Observe socket (§9).
 *
 * Nothing goes *up*: Pause is client-side, because pausing the server would
 * mean either dropping the elements or queueing them, and §7 says which of
 * those a stream does. */
export type ObserveFrame =
  | {
      type: "ready";
      stream: string;
      element_type: unknown;
      status: unknown;
      counters: unknown;
      /** How many of the elements that follow are the ring's history rather
       * than live arrivals — "since this server started", and there is no
       * more history than that (§4). */
      replayed: number;
    }
  | { type: "element"; envelope: unknown }
  | { type: "lagged"; dropped: number }
  | { type: "status"; status: unknown; counters: unknown };

/** Parse one text frame, or nothing for anything this version does not know.
 *
 * Unknown frames are dropped rather than rendered, which is what lets a newer
 * server add one without breaking a tab somebody left open across a deploy. */
export function parseFrame(data: string): ObserveFrame | null {
  try {
    const value: unknown = JSON.parse(data);
    if (!value || typeof value !== "object") return null;
    const type = (value as { type?: unknown }).type;
    if (type === "ready" || type === "element" || type === "lagged" || type === "status") {
      return value as ObserveFrame;
    }
  } catch {
    // Not JSON: not a frame. A malformed frame is the server's bug and there is
    // nothing the screen can do about it but keep reading the next one.
  }
  return null;
}

/** How many elements the Observe screen keeps. The server's ring is 100 (§9);
 * a live tail holds more than the replay so that a screen left open for a
 * minute shows more than a screen just opened. */
export const TAIL_LIMIT = 500;

/** What the Observe screen is showing. */
export type ObserveState = {
  /** Newest first — the order the tail is read in. */
  tail: Envelope[];
  elementType: ElementType | null;
  status: StreamStatusValue | null;
  counters: Counters;
  /** How many of `tail` are replay rather than live, so the screen can label
   * them. Counted down as live elements push them along. */
  replayed: number;
  /** Elements this socket was too slow to take, summed (§7). Told, never
   * hidden: a gap the screen names beats a gap it shows. */
  lagged: number;
  /** Whether the `ready` frame has arrived. */
  ready: boolean;
  /** A note the screen shows instead of the tail — a closed socket, mostly. */
  error: string | null;
};

/** The state an Observe screen starts in. */
export function emptyObserve(): ObserveState {
  return {
    tail: [],
    elementType: null,
    status: null,
    counters: { ...NO_COUNTERS },
    replayed: 0,
    lagged: 0,
    ready: false,
    error: null,
  };
}

/** Apply one frame. A reducer, so the part that has to be right — elements in
 * order, the replay labelled, a lag counted rather than swallowed — is testable
 * without a browser or a server. */
export function applyFrame(
  state: ObserveState,
  frame: ObserveFrame,
  limit = TAIL_LIMIT,
): ObserveState {
  switch (frame.type) {
    case "ready":
      return {
        ...state,
        ready: true,
        error: null,
        elementType: readElementType(frame.element_type),
        status: readStatus(frame.status),
        counters: readCounters(frame.counters),
        replayed: frame.replayed,
      };
    case "status":
      return {
        ...state,
        status: readStatus(frame.status),
        counters: readCounters(frame.counters),
      };
    case "lagged":
      return { ...state, lagged: state.lagged + frame.dropped };
    case "element": {
      const envelope = readEnvelope(frame.envelope);
      if (!envelope) return state;
      const tail = [envelope, ...state.tail].slice(0, limit);
      // The replay is the *oldest* n of the tail, so what shrinks it is the
      // truncation at the far end and nothing else: a live arrival on a tail
      // with room leaves every remembered element exactly where it was.
      const lost = state.tail.length + 1 - tail.length;
      return { ...state, tail, replayed: Math.max(state.replayed - lost, 0) };
    }
  }
}

/** Whether the envelope at `index` of the tail is replayed history rather than
 * something that arrived while the screen was open. */
export function isReplayed(state: ObserveState, index: number): boolean {
  return index >= state.tail.length - state.replayed;
}
