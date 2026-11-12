import { describe, expect, it } from "vitest";

import { codeSpans } from "./inlineCode";

describe("backticked names", () => {
  it("sets backticked names as code and leaves the rest as written", () => {
    expect(codeSpans("Tables `machines` and `sets`, done.")).toEqual([
      { code: false, text: "Tables " },
      { code: true, text: "machines" },
      { code: false, text: " and " },
      { code: true, text: "sets" },
      { code: false, text: ", done." },
    ]);
  });

  it("leaves an unpaired backtick, and one across a line break, as text", () => {
    expect(codeSpans("a ` b")).toEqual([{ code: false, text: "a ` b" }]);
    expect(codeSpans("a `b\nc` d")).toEqual([{ code: false, text: "a `b\nc` d" }]);
  });
});
