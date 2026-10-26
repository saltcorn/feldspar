import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { AgentMemory, memoryUserLabel } from "./AgentMemory";

describe("AgentMemory", () => {
  const html = renderToStaticMarkup(<AgentMemory agent="help desk" />);

  it("titles the page with the agent and links back to its runs", () => {
    expect(html).toContain("help desk · memory");
    expect(html).toContain('href="#/agents/help%20desk/runs"');
  });

  it("can be searched, and lists who each memory is about", () => {
    expect(html).toContain("Search memories");
    expect(html).toContain("<th>User</th>");
  });

  it("names a memory's user by email when it can, by id when it cannot", () => {
    const memory = {
      id: 1,
      agent: "help desk",
      user_id: "u1",
      run_id: null,
      written_at: "2026-01-01T00:00:00Z",
      kind: "memory",
      personal: true,
      topic: "",
      contents: "likes tea",
    };
    const users = [{ id: "u1", email: "ada@example.com" }] as never;
    expect(memoryUserLabel(memory, users)).toBe("ada@example.com");
    expect(memoryUserLabel({ ...memory, user_id: "u2" }, users)).toBe("u2");
    expect(memoryUserLabel({ ...memory, user_id: null }, users)).toBe("—");
  });
});
