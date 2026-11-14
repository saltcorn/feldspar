import { describe, expect, it } from "vitest";

import { AGENT_AREA_TABS, AGENT_SECTIONS, agentSectionHref } from "./agentNav";

describe("the agents area's tabs", () => {
  it("are agents, LLM providers and MCP servers, each its own list", () => {
    expect(AGENT_AREA_TABS.map((tab) => tab.href)).toEqual([
      "#/agents",
      "#/llm-providers",
      "#/mcp-servers",
    ]);
  });
});

describe("an agent's screens", () => {
  it("are chat, runs, memory and settings, in that order", () => {
    expect(AGENT_SECTIONS.map((s) => s.id)).toEqual(["chat", "runs", "memory", "settings"]);
  });

  it("address chat, runs and memory by the agent's name", () => {
    expect(agentSectionHref("chat", "help desk", "id-1")).toBe("#/agents/help%20desk/chat");
    expect(agentSectionHref("runs", "help desk", undefined)).toBe("#/agents/help%20desk/runs");
    expect(agentSectionHref("memory", "a/b", undefined)).toBe("#/agents/a%2Fb/memory");
  });

  it("address the settings by the agent's id, and have no link without one", () => {
    expect(agentSectionHref("settings", "help desk", "9ef4")).toBe("#/agents/9ef4/edit");
    expect(agentSectionHref("settings", "help desk", undefined)).toBeNull();
  });
});
