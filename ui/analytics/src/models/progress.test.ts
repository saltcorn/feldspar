import { describe, expect, it } from "vitest";

import { progressSocketUrl } from "../api";
import { readFrame, stageText } from "./progress";

describe("a fit's progress over its socket", () => {
  it("reads the server's two frames", () => {
    expect(
      readFrame(
        JSON.stringify({
          type: "progress",
          status: "fitting",
          progress: { stage: "sampling", chains: [{ chain: 2, iteration: 5, total: 10, phase: "warmup" }] },
          cancel_requested: false,
        }),
      ),
    ).toEqual({
      type: "progress",
      status: "fitting",
      progress: { stage: "sampling", chains: [{ chain: 2, iteration: 5, total: 10, phase: "warmup" }] },
      cancelRequested: false,
    });
    expect(readFrame('{"type":"progress","status":"fitting","progress":null,"cancel_requested":true}')).toEqual({
      type: "progress",
      status: "fitting",
      progress: null,
      cancelRequested: true,
    });
    expect(readFrame('{"type":"finished","status":"failed","error":"no rows"}')).toEqual({
      type: "finished",
      status: "failed",
      error: "no rows",
    });
  });

  it("ignores anything else", () => {
    expect(readFrame("not json")).toBeNull();
    expect(readFrame('{"type":"other"}')).toBeNull();
    expect(readFrame("[]")).toBeNull();
  });

  it("opens the socket on the page's own host, encrypted when the page is", () => {
    expect(progressSocketUrl("i 1", { protocol: "http:", host: "localhost:3000" })).toBe(
      "ws://localhost:3000/api/model-instances/i%201/progress",
    );
    expect(progressSocketUrl("i1", { protocol: "https:", host: "example.com" })).toBe(
      "wss://example.com/api/model-instances/i1/progress",
    );
  });

  it("names every stage a fit reports", () => {
    const t = (s: string) => s;
    for (const stage of ["queued", "compiling", "sampling", "summarising", "reading", "fitting", "scoring"]) {
      expect(stageText(t, stage)).not.toBe("");
    }
    expect(stageText(t, undefined)).toBe("");
  });
});
