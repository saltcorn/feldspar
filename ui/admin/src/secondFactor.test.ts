import { describe, expect, it } from "vitest";

import { codeError, normaliseCode } from "./secondFactor";

const t = (text: string) => text;

describe("normaliseCode", () => {
  it("takes out the space an app shows in the middle of a code", () => {
    expect(normaliseCode(" 123 456 ")).toBe("123456");
  });

  it("leaves a recovery code's dash for the server to ignore", () => {
    expect(normaliseCode("abcde-fghjk")).toBe("abcde-fghjk");
  });
});

describe("codeError", () => {
  it("says a wrong code can be tried again", () => {
    expect(codeError(401, t)).toMatch(/not right/);
  });

  it("says to wait when there were too many", () => {
    expect(codeError(429, t)).toMatch(/Too many/);
  });

  it("has something to say for anything else", () => {
    expect(codeError(null, t)).toMatch(/went wrong/);
  });
});
