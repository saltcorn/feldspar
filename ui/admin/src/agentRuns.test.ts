import { describe, expect, it } from "vitest";

import { prettyContext, runUserLabel } from "./agentRuns";
import type { ListRunsResponse, ListUsersResponse } from "./client";

const run = (user: string | null): ListRunsResponse[number] => ({
  id: "r1",
  kind: "agent",
  subject: "helper",
  description: "",
  state: "done",
  user,
  created_at: "2026-10-25T10:00:00Z",
  updated_at: "2026-10-25T10:01:00Z",
});

const users: ListUsersResponse = [
  { id: "u1", email: "ann@example.com", role: 1, disabled: false, extra: {} },
];

describe("runUserLabel", () => {
  it("names a known user by email", () => {
    expect(runUserLabel(run("u1"), users)).toBe("ann@example.com");
  });
  it("falls back to the id of a deleted user", () => {
    expect(runUserLabel(run("u9"), users)).toBe("u9");
  });
  it("calls a run with no user anonymous", () => {
    expect(runUserLabel(run(null), users)).toBe("anonymous");
  });
});

describe("prettyContext", () => {
  it("indents the context JSON", () => {
    expect(prettyContext({ a: [1] })).toBe('{\n  "a": [\n    1\n  ]\n}');
  });
  it("prints a missing context as null", () => {
    expect(prettyContext(undefined)).toBe("null");
  });
});
