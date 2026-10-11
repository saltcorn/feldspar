/**
 * The streams screens' pure parts (TODO "Streams", task 7.2).
 *
 * Four things are pinned here, and each is a thing a screen would otherwise get
 * quietly wrong:
 *
 *   - the **socket frames**, because they are a wire contract with `sc-server`
 *     and a frame this file parses differently from the way that file writes it
 *     is a live tail that silently shows nothing;
 *   - the **replay boundary**, because "since this server started" is a claim
 *     the screen makes about particular rows and it has to keep being true as
 *     live elements arrive;
 *   - the **rendering per element type**, because §4's whole argument for
 *     declaring a type is that binary is not shown as text and a missing key is
 *     shown as missing;
 *   - and the **status colours**, because a stream the supervisor is retrying
 *     must not read like a stream that cannot be saved.
 */

import { describe, expect, it } from "vitest";

import {
  TAIL_LIMIT,
  applyFrame,
  cellText,
  changeNotes,
  counterNotes,
  elementCells,
  elementColumns,
  elementText,
  elementTypeSummary,
  emptyObserve,
  extraKeys,
  filterByTopic,
  formatCount,
  hexHead,
  isReplayed,
  nameProblem,
  parseFrame,
  providerSpec,
  readChanges,
  readCounters,
  readElementType,
  readEnvelope,
  readStatus,
  readTopics,
  statusLabel,
  streamObserveUrl,
  topicLabel,
  type ElementType,
  type Envelope,
  type ObserveFrame,
  type StreamProviderInfo,
} from "./streams";

/** An envelope as the server writes it (§4). */
function envelope(value: unknown, at = "2026-09-17T09:00:00.000Z"): Envelope {
  return { stream: "boiler", value, received_at: at, source: { topic: "house/boiler/temp" } };
}

const jsonType: ElementType = {
  kind: "json",
  keys: [
    { name: "temperature", type: "float", required: true },
    { name: "humidity", type: "float", required: false },
  ],
};

describe("reading an element type off the wire", () => {
  it("reads each of the three kinds", () => {
    expect(readElementType({ kind: "json", keys: [{ name: "t", type: "float" }] })).toEqual({
      kind: "json",
      keys: [{ name: "t", type: "float", required: false }],
    });
    expect(readElementType({ kind: "text", encoding: "utf8" })).toEqual({
      kind: "text",
      encoding: "utf8",
    });
    expect(readElementType({ kind: "binary" })).toEqual({ kind: "binary" });
  });

  it("carries `required`, which the Rust side omits when false", () => {
    const type = readElementType({
      kind: "json",
      keys: [{ name: "t", type: "float", required: true }, { name: "h", type: "float" }],
    });
    expect(type?.kind === "json" && type.keys.map((k) => k.required)).toEqual([true, false]);
  });

  it("is nothing for a stream whose provider could not be resolved", () => {
    // Not an error: such a stream is still listed, with its status, and Edit is
    // the repair.
    expect(readElementType(null)).toBeNull();
    expect(readElementType({ kind: "something-newer" })).toBeNull();
  });

  it("summarises one for a list cell", () => {
    expect(elementTypeSummary(jsonType)).toBe("JSON: temperature (float), humidity (float)");
    expect(elementTypeSummary({ kind: "text", encoding: "utf8" })).toBe("Text (utf8)");
    expect(elementTypeSummary({ kind: "binary" })).toBe("Binary");
    expect(elementTypeSummary(null)).toBe("—");
  });
});

describe("reading an envelope", () => {
  it("reads the wire contract's four fields", () => {
    const read = readEnvelope({
      stream: "boiler",
      value: { temperature: 31.2 },
      received_at: "2026-09-17T09:00:00.000Z",
      source: { topic: "house/boiler/temp", qos: 0, retain: false },
    });
    expect(read).toEqual({
      stream: "boiler",
      value: { temperature: 31.2 },
      received_at: "2026-09-17T09:00:00.000Z",
      source: { topic: "house/boiler/temp", qos: 0, retain: false },
    });
  });

  it("leaves `source` off for a provider that has none", () => {
    const read = readEnvelope({ stream: "s", value: "hi", received_at: "2026-09-17T09:00:00Z" });
    expect(read && "source" in read).toBe(false);
  });

  it("refuses something that is not one", () => {
    expect(readEnvelope({ stream: "s", value: 1 })).toBeNull();
    expect(readEnvelope("nope")).toBeNull();
  });

  it("carries a topic when the stream has one, and leaves it off when not", () => {
    const read = readEnvelope({
      stream: "job_status",
      topic: "6f9619ff-8b86-d011-b42d-00cf4fc964ff",
      value: { message: "Done" },
      received_at: "2026-09-17T09:00:00.000Z",
    });
    expect(read?.topic).toBe("6f9619ff-8b86-d011-b42d-00cf4fc964ff");
    const single = readEnvelope({ stream: "s", value: 1, received_at: "2026-09-17T09:00:00Z" });
    expect(single && "topic" in single).toBe(false);
  });
});

describe("topics on the Observe screen", () => {
  it("reads a stream's topic kind, and anything else as single", () => {
    expect(readTopics({ kind: "user" })).toEqual({ kind: "user" });
    expect(readTopics({ kind: "row", table: "boards" })).toEqual({ kind: "row", table: "boards" });
    expect(readTopics({ kind: "single" })).toEqual({ kind: "single" });
    expect(readTopics(null)).toEqual({ kind: "single" });
    expect(readTopics({ kind: "galaxy" })).toEqual({ kind: "single" });
  });

  it("names the column after what the topic is", () => {
    // A translator that shows what it was asked, so the message and its
    // arguments are both checked.
    const t = (text: string, args?: Record<string, string>) =>
      args ? `${text} ${JSON.stringify(args)}` : text;
    expect(topicLabel(t, { kind: "user" })).toBe("Topic (user)");
    expect(topicLabel(t, { kind: "row", table: "boards" })).toBe('Topic ({table} row) {"table":"boards"}');
    expect(topicLabel(t, { kind: "row" })).toBe("Topic (row)");
    expect(topicLabel(t, { kind: "single" })).toBe("Topic");
  });

  it("narrows the tail to the topics containing the query, keeping the order", () => {
    const on = (topic: string | undefined, n: number): Envelope => ({
      stream: "job_status",
      ...(topic === undefined ? {} : { topic }),
      value: { n },
      received_at: "2026-09-17T09:00:00.000Z",
    });
    const tail = [on("alice-1", 3), on("bob-2", 2), on("alice-1", 1), on(undefined, 0)];
    expect(filterByTopic(tail, "").length).toBe(4);
    expect(filterByTopic(tail, "  ").length).toBe(4);
    expect(filterByTopic(tail, "alice").map((e) => (e.value as { n: number }).n)).toEqual([3, 1]);
    expect(filterByTopic(tail, "carol")).toEqual([]);
  });
});

describe("status and counters", () => {
  it("reads the four cases", () => {
    expect(readStatus({ status: "starting" })).toEqual({ status: "starting" });
    expect(readStatus({ status: "stopped" })).toEqual({ status: "stopped" });
    expect(readStatus({ status: "running", since: "2026-09-17T09:00:00Z" })).toEqual({
      status: "running",
      since: "2026-09-17T09:00:00Z",
    });
    expect(
      readStatus({
        status: "failed",
        error: "connection refused",
        since: "2026-09-17T09:00:00Z",
        attempt: 4,
      }),
    ).toEqual({
      status: "failed",
      error: "connection refused",
      since: "2026-09-17T09:00:00Z",
      attempt: 4,
    });
    expect(readStatus(null)).toBeNull();
  });

  it("colours a retrying stream yellow, not red", () => {
    // The supervisor is backing off and will reconnect (§6). Red is for a
    // stream that cannot be saved at all, which is the row's `error`.
    const failed = statusLabel(
      { status: "failed", error: "connection refused", since: "x", attempt: 3 },
      true,
    );
    expect(failed.tone).toBe("yellow");
    expect(failed.label).toBe("Retrying (3)");
    expect(failed.title).toBe("connection refused");
  });

  it("tells disabled from not-running-here", () => {
    // Two different facts: `enabled` is on the row, the status is this
    // process's, and §6 says a subscription is process-local.
    expect(statusLabel(null, false).label).toBe("Disabled");
    expect(statusLabel({ status: "running", since: "x" }, false).label).toBe("Disabled");
    expect(statusLabel(null, true).label).toBe("Not running here");
    expect(statusLabel({ status: "running", since: "x" }, true).tone).toBe("green");
    expect(statusLabel({ status: "starting" }, true).tone).toBe("blue");
  });

  it("defaults every counter to zero so the list has one column shape", () => {
    expect(readCounters(undefined)).toEqual({
      elements: 0,
      dropped_for_triggers: 0,
      dropped_for_rate: 0,
      malformed: 0,
      last_element_at: null,
    });
    expect(readCounters({ elements: 7, last_element_at: "2026-09-17T09:00:00Z" })).toMatchObject({
      elements: 7,
      last_element_at: "2026-09-17T09:00:00Z",
    });
  });

  it("says nothing about the drop counters while they are zero", () => {
    expect(counterNotes(readCounters({ elements: 100 }))).toEqual([]);
    expect(
      counterNotes(readCounters({ dropped_for_triggers: 3, dropped_for_rate: 2, malformed: 1 })),
    ).toEqual(["3 dropped for triggers", "2 over the rate cap", "1 malformed"]);
  });

  it("reads a table-changes stream's checks and says what they decided", () => {
    expect(readChanges(null)).toBeNull();
    expect(changeNotes(readChanges(null))).toEqual([]);
    expect(changeNotes(readChanges({ checked: 0 }))).toEqual([]);
    const changes = readChanges({
      checked: 10,
      delivered: 6,
      suppressed_access: 3,
      suppressed_filter: 1,
      evaluator_micros: 2500,
    });
    expect(changes).toMatchObject({ checked: 10, delivered: 6 });
    expect(changeNotes(changes)).toEqual([
      "6 of 10 sent",
      "3 withheld by access",
      "1 outside filters",
      "3 ms evaluating",
    ]);
    // Zeroes say nothing, and the evaluator's time only once it is worth a
    // millisecond.
    expect(changeNotes(readChanges({ checked: 2, delivered: 2, evaluator_micros: 10 }))).toEqual([
      "2 of 2 sent",
    ]);
  });

  it("groups a count, because a busy stream reaches seven digits", () => {
    expect(formatCount(1024)).toBe((1024).toLocaleString());
    expect(formatCount(Number.NaN)).toBe("—");
  });
});

describe("the form", () => {
  it("renders whatever the picked provider declares, and knows no provider", () => {
    const providers = [
      { name: "mqtt", config_spec: [{ name: "host" }, { name: "topic" }] },
      { name: "scripted", config_spec: [{ name: "elements" }] },
    ] as unknown as StreamProviderInfo[];
    expect(providerSpec(providers, "mqtt").map((f) => f.name)).toEqual(["host", "topic"]);
    expect(providerSpec(providers, "scripted").map((f) => f.name)).toEqual(["elements"]);
    expect(providerSpec(providers, "gone")).toEqual([]);
    expect(providerSpec(null, "mqtt")).toEqual([]);
  });

  it("holds the name to an identifier, because it becomes a URL segment", () => {
    expect(nameProblem("boiler")).toBeNull();
    expect(nameProblem("boiler_2")).toBeNull();
    expect(nameProblem("")).toMatch(/needs a name/);
    expect(nameProblem("house/boiler")).toMatch(/letters, digits/);
    expect(nameProblem("2boilers")).toMatch(/letters, digits/);
  });
});

describe("rendering an element against its declared type", () => {
  it("gives a json element the declared columns, in the declared order", () => {
    expect(elementColumns(jsonType)).toEqual(["temperature", "humidity"]);
    expect(elementColumns({ kind: "binary" })).toEqual([]);
    expect(elementColumns(null)).toEqual([]);
  });

  it("shows a declared key that is absent as absent", () => {
    // §4: a declared key that is missing is null, and a null in a table cell is
    // a dash rather than a blank that reads like an empty string.
    expect(elementCells(envelope({ temperature: 31.2 }), jsonType)).toEqual(["31.2", "—"]);
  });

  it("prints a nested value as compact JSON rather than [object Object]", () => {
    expect(cellText({ a: 1 })).toBe('{"a":1}');
    expect(cellText([1, 2])).toBe("[1,2]");
    expect(cellText(false)).toBe("false");
  });

  it("names the keys an element carried that the type did not declare", () => {
    // Carried through rather than dropped (§4) — and worth telling the admin,
    // because adding them to the declaration is what puts them in a column.
    expect(extraKeys(envelope({ temperature: 1, pressure: 9 }), jsonType)).toEqual(["pressure"]);
    expect(extraKeys(envelope("text"), jsonType)).toEqual([]);
    expect(extraKeys(envelope({ a: 1 }), { kind: "binary" })).toEqual([]);
  });

  it("shows a text element as its text", () => {
    expect(elementText(envelope("31.2 C"))).toBe("31.2 C");
  });

  it("shows a binary element as a hex head, never as characters", () => {
    // "hello" — which would render as five plausible letters if this pretended
    // bytes were text, and is exactly the failure §4 refuses.
    expect(hexHead("aGVsbG8=")).toEqual({ hex: "68 65 6c 6c 6f", length: 5 });
  });

  it("bounds the hex head and says there is more", () => {
    // Four bytes shown of eight ("AAAAAAAAAAA=" is 8 × 0x00).
    const head = hexHead("AAAAAAAAAAA=", 4);
    expect(head.length).toBe(8);
    expect(head.hex).toBe("00 00 00 00 …");
  });

  it("refuses something that is not base64 rather than inventing bytes", () => {
    expect(hexHead("not base64!")).toEqual({ hex: "", length: null });
  });
});

describe("the observe socket", () => {
  it("opens wss from an https page, so it is not blocked as mixed content", () => {
    expect(streamObserveUrl({ protocol: "https:", host: "example.com" }, "abc")).toBe(
      "wss://example.com/api/streams/abc/observe",
    );
    expect(streamObserveUrl({ protocol: "http:", host: "localhost:3000" }, "abc")).toBe(
      "ws://localhost:3000/api/streams/abc/observe",
    );
  });

  it("parses the four frames and drops one it does not know", () => {
    expect(parseFrame('{"type":"lagged","dropped":4}')).toEqual({ type: "lagged", dropped: 4 });
    expect(parseFrame('{"type":"ready","replayed":0}')).toMatchObject({ type: "ready" });
    // A newer server's frame must not break a tab left open across a deploy.
    expect(parseFrame('{"type":"something-newer"}')).toBeNull();
    expect(parseFrame("not json")).toBeNull();
  });

  it("takes the element type, status and replay count from `ready`", () => {
    const ready: ObserveFrame = {
      type: "ready",
      stream: "boiler",
      element_type: jsonType,
      status: { status: "running", since: "2026-09-17T09:00:00Z" },
      counters: { elements: 3 },
      replayed: 2,
    };
    const state = applyFrame(emptyObserve(), ready);
    expect(state.ready).toBe(true);
    expect(state.elementType).toEqual(jsonType);
    expect(state.status).toEqual({ status: "running", since: "2026-09-17T09:00:00Z" });
    expect(state.counters.elements).toBe(3);
    expect(state.replayed).toBe(2);
  });

  it("puts the newest element first and keeps the replay labelled", () => {
    let state = applyFrame(emptyObserve(), {
      type: "ready",
      stream: "boiler",
      element_type: jsonType,
      status: null,
      counters: null,
      replayed: 0,
    });
    // Two replayed, then one live: the two history elements stay labelled.
    for (const at of ["09:00:00", "09:00:01"]) {
      state = applyFrame(state, {
        type: "element",
        envelope: envelope({ temperature: 1 }, `2026-09-17T${at}.000Z`),
      });
    }
    state = { ...state, replayed: 2 };
    state = applyFrame(state, {
      type: "element",
      envelope: envelope({ temperature: 2 }, "2026-09-17T09:00:02.000Z"),
    });
    expect(state.tail.map((e) => e.received_at)).toEqual([
      "2026-09-17T09:00:02.000Z",
      "2026-09-17T09:00:01.000Z",
      "2026-09-17T09:00:00.000Z",
    ]);
    expect(state.replayed).toBe(2);
    expect(isReplayed(state, 0)).toBe(false);
    expect(isReplayed(state, 1)).toBe(true);
    expect(isReplayed(state, 2)).toBe(true);
  });

  it("bounds the tail, and the replay shrinks with it", () => {
    // Three replayed, then two live arrivals against a tail that holds three.
    let state = {
      ...emptyObserve(),
      tail: [envelope({ t: 1 }), envelope({ t: 2 }), envelope({ t: 3 })],
      replayed: 3,
    };
    for (let i = 0; i < 2; i += 1) {
      state = applyFrame(state, { type: "element", envelope: envelope({ t: i }) }, 3);
    }
    expect(state.tail).toHaveLength(3);
    // Only one of the three remembered elements is still on screen: two fell
    // off the end, and the label must not claim a row that is gone.
    expect(state.replayed).toBe(1);
    expect(TAIL_LIMIT).toBeGreaterThan(100);
  });

  it("counts a lag rather than swallowing it", () => {
    // §7: a consumer that cannot keep up is told. A gap the screen names beats
    // a gap it shows.
    let state = emptyObserve();
    state = applyFrame(state, { type: "lagged", dropped: 4 });
    state = applyFrame(state, { type: "lagged", dropped: 6 });
    expect(state.lagged).toBe(10);
  });

  it("keeps the element type across a status frame", () => {
    let state = applyFrame(emptyObserve(), {
      type: "ready",
      stream: "boiler",
      element_type: jsonType,
      status: { status: "running", since: "x" },
      counters: null,
      replayed: 0,
    });
    state = applyFrame(state, {
      type: "status",
      status: { status: "failed", error: "gone", since: "y", attempt: 1 },
      counters: { elements: 9 },
    });
    expect(state.elementType).toEqual(jsonType);
    expect(state.status).toMatchObject({ status: "failed", attempt: 1 });
    expect(state.counters.elements).toBe(9);
  });

  it("ignores an element frame whose envelope is not one", () => {
    const state = applyFrame(emptyObserve(), { type: "element", envelope: { nope: true } });
    expect(state.tail).toEqual([]);
  });
});
