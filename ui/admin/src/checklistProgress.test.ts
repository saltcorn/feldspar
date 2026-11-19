/**
 * The checklist run's progress toast: what it says from what the server
 * pushes, and the run socket it reads that from.
 */

import { describe, expect, it, vi } from "vitest";

import type { ChecklistRun } from "./checklists";
import {
  RECENT_STEPS,
  RunError,
  progressView,
  readFrame,
  runLive,
  runSocketUrl,
  type ChecklistProgress,
  type SocketLike,
} from "./checklistProgress";

const base: ChecklistProgress = {
  checklist: "Orders work",
  checklists_done: 0,
  checklists_total: null,
  total: 3,
  step: 2,
  label: "Open the list",
  type: "browser.goto",
  steps: [{ label: "Sign in", type: "browser.login", outcome: "pass" }],
};

describe("progressView", () => {
  it("is empty before the first poll answers", () => {
    const v = progressView(null);
    expect(v.checklist).toBeNull();
    expect(v.step).toBeNull();
    expect(v.percent).toBe(0);
  });

  it("names the running step and counts the finished ones", () => {
    const v = progressView(base);
    expect(v.checklist).toBe("Orders work");
    expect(v.step).toEqual({ number: 2, label: "Open the list" });
    expect(v.total).toBe(3);
    expect(v.done).toBe(1);
    expect(v.percent).toBe(33);
    expect(v.recent).toEqual([{ label: "Sign in", outcome: "pass" }]);
    expect(v.checklistCount).toBeNull();
  });

  it("falls back to a step's type when it has no label", () => {
    const v = progressView({
      ...base,
      label: "",
      steps: [{ label: "", type: "browser.login", outcome: "fail" }],
    });
    expect(v.step?.label).toBe("browser.goto");
    expect(v.recent[0].label).toBe("browser.login");
  });

  it("counts the checklists when a request runs several", () => {
    const v = progressView({ ...base, checklists_total: 4, checklists_done: 1 });
    expect(v.checklistCount).toEqual({ index: 2, count: 4 });
    // Once the last has finished the count does not run past the end.
    expect(progressView({ ...base, checklists_total: 4, checklists_done: 4 }).checklistCount)
      .toEqual({ index: 4, count: 4 });
  });

  it("lists only the latest finished steps", () => {
    const steps = Array.from({ length: RECENT_STEPS + 3 }, (_, i) => ({
      label: `Step ${i + 1}`,
      type: "core.manual",
      outcome: "pass",
    }));
    const v = progressView({ ...base, total: steps.length, step: null, steps });
    expect(v.recent).toHaveLength(RECENT_STEPS);
    expect(v.recent[RECENT_STEPS - 1].label).toBe(`Step ${steps.length}`);
    expect(v.percent).toBe(100);
  });
});

describe("runSocketUrl", () => {
  it("runs one checklist, or an application's, on this host", () => {
    expect(runSocketUrl({ protocol: "https:", host: "example.com" }, { checklist: "abc" })).toBe(
      "wss://example.com/api/checklists/abc/run/live",
    );
    expect(runSocketUrl({ protocol: "http:", host: "localhost:3000" }, { application: "none" })).toBe(
      "ws://localhost:3000/api/checklist-runs/live?application=none",
    );
  });
});

describe("readFrame", () => {
  it("reads the three frames and ignores anything else", () => {
    expect(readFrame(JSON.stringify({ type: "progress", progress: base }))).toEqual({
      type: "progress",
      progress: base,
    });
    expect(readFrame(JSON.stringify({ type: "finished", runs: [] }))).toEqual({
      type: "finished",
      runs: [],
    });
    expect(readFrame(JSON.stringify({ type: "failed", error: "no such checklist" }))).toEqual({
      type: "failed",
      error: "no such checklist",
    });
    expect(readFrame("not json")).toBeNull();
    expect(readFrame(JSON.stringify({ type: "later" }))).toBeNull();
  });
});

/** A socket the test drives: `open`, `say` a frame, `drop` the connection. */
function scripted() {
  const socket: SocketLike & { closed: boolean } = {
    onopen: null,
    onmessage: null,
    onclose: null,
    closed: false,
    close() {
      this.closed = true;
    },
  };
  return {
    socket,
    open: () => socket.onopen?.(),
    say: (frame: unknown) => socket.onmessage?.({ data: JSON.stringify(frame) }),
    drop: () => socket.onclose?.(),
  };
}

const aRun = { id: "r1" } as unknown as ChecklistRun;

describe("runLive", () => {
  it("passes each progress frame on and resolves to the runs", async () => {
    const s = scripted();
    const seen: ChecklistProgress[] = [];
    const fallback = vi.fn(async () => [] as ChecklistRun[]);
    const done = runLive(() => s.socket, fallback, (p) => seen.push(p), "lost");
    s.open();
    s.say({ type: "progress", progress: base });
    s.say({ type: "progress", progress: { ...base, step: 3 } });
    s.say({ type: "finished", runs: [aRun] });
    s.drop();
    await expect(done).resolves.toEqual([aRun]);
    expect(seen.map((p) => p.step)).toEqual([2, 3]);
    expect(fallback).not.toHaveBeenCalled();
    expect(s.socket.closed).toBe(true);
  });

  it("rejects with the server's reason when the run could not be made", async () => {
    const s = scripted();
    const done = runLive(() => s.socket, async () => [], () => {}, "lost");
    s.open();
    s.say({ type: "failed", error: "there is no checklist x" });
    await expect(done).rejects.toEqual(new RunError("there is no checklist x"));
  });

  it("falls back to the POST when the socket never opens", async () => {
    const s = scripted();
    const fallback = vi.fn(async () => [aRun]);
    const done = runLive(() => s.socket, fallback, () => {}, "lost");
    s.drop();
    await expect(done).resolves.toEqual([aRun]);
    expect(fallback).toHaveBeenCalledOnce();

    const thrown = runLive(() => { throw new Error("no sockets here"); }, fallback, () => {}, "lost");
    await expect(thrown).resolves.toEqual([aRun]);
  });

  it("does not run again when the socket goes after the run began", async () => {
    const s = scripted();
    const fallback = vi.fn(async () => [aRun]);
    const done = runLive(() => s.socket, fallback, () => {}, "the run carries on");
    s.open();
    s.say({ type: "progress", progress: base });
    s.drop();
    await expect(done).rejects.toEqual(new RunError("the run carries on"));
    expect(fallback).not.toHaveBeenCalled();
  });
});
