import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { AgentMemory, memoryUserLabel } from "./AgentMemory";

describe("AgentMemory", () => {
  const html = renderToStaticMarkup(<AgentMemory agent="help desk" />);

  it("titles the page with the agent", () => {
    expect(html).toContain('<div class="page-pretitle">Agent</div>');
    expect(html).toContain('<h2 class="page-title">help desk</h2>');
  });

  it("shows the agent's tabs, with this screen's open", () => {
    expect(html).toContain('href="#/agents/help%20desk/chat"');
    expect(html).toMatch(/class="nav-link active" aria-current="page" href="#\/agents\/help%20desk\/memory"/);
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
