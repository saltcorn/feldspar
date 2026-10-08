import { afterEach, describe, expect, it } from "vitest";

import { announce, concerns, listen, type Change } from "./changes";
import { DEFAULT_RATIO, MIN_RATIO, clampRatio, paneOf } from "./panes";
import { parseLayout } from "./router";

/** A browser's address bar, as much of it as a pane uses. */
function fakeWindow(hash: string) {
  const w = {
    location: { hash },
    history: {
      replaceState: (_s: unknown, _t: string, url: string) => {
        w.location.hash = url;
      },
    },
  };
  (globalThis as unknown as { window: unknown }).window = w;
  return w;
}

afterEach(() => {
  delete (globalThis as unknown as { window?: unknown }).window;
});

describe("a side of split view (A4.1)", () => {
  it("moves its own side and leaves the other", () => {
    const w = fakeWindow("#/w/left?side=%23%2Fw%2Fright");
    const right = paneOf("side", true);
    expect(parseLayout(right.href({ name: "model", id: "m" }))).toEqual({
      main: { name: "workspace", id: "left" },
      side: { name: "model", id: "m" },
    });
    right.go({ name: "dataset", id: "d" });
    expect(parseLayout(w.location.hash).side).toEqual({ name: "dataset", id: "d" });

    // It reads the address when it acts, so the left side moved by the right
    // is the right one moved since.
    const left = paneOf("main", true);
    left.go({ name: "home" });
    expect(parseLayout(w.location.hash)).toEqual({ main: { name: "home" }, side: { name: "dataset", id: "d" } });

    // A new model's address is replaced on its own side.
    right.replace({ name: "model", id: "new1" });
    expect(parseLayout(w.location.hash).side).toEqual({ name: "model", id: "new1" });
  });

  it("opens beside itself, splitting the screen, and closes", () => {
    const w = fakeWindow("#/w/explore");
    const only = paneOf("main", false);
    only.beside({ name: "model", id: "m" });
    expect(parseLayout(w.location.hash)).toEqual({
      main: { name: "workspace", id: "explore" },
      side: { name: "model", id: "m" },
    });
    paneOf("side", true).beside({ name: "workspace", id: "other" });
    expect(parseLayout(w.location.hash).main).toEqual({ name: "workspace", id: "other" });
    paneOf("main", true).close();
    expect(parseLayout(w.location.hash)).toEqual({ main: { name: "model", id: "m" }, side: null });
  });

  it("keeps the divider where both sides can be used", () => {
    expect(clampRatio(0.5)).toBe(0.5);
    expect(clampRatio(0.01)).toBe(MIN_RATIO);
    expect(clampRatio(0.99)).toBe(1 - MIN_RATIO);
    expect(clampRatio(Number.NaN)).toBe(DEFAULT_RATIO);
  });
});

describe("changes told to the other side (A4.1)", () => {
  it("reach the listeners on the other side, not the side that made them", () => {
    const heard: Change[] = [];
    const stop = listen((c) => {
      if (concerns(c, "side", ["dataset"])) heard.push(c);
    });
    announce({ kind: "dataset", id: "d1", from: "main" });
    announce({ kind: "dataset", id: "d2", from: "side" });
    announce({ kind: "model", id: "m1", from: "main" });
    stop();
    announce({ kind: "dataset", id: "d3", from: "main" });
    expect(heard.map((c) => c.id)).toEqual(["d1"]);
  });
});
