import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { AgentRuns } from "./AgentRuns";

describe("AgentRuns", () => {
  const html = renderToStaticMarkup(<AgentRuns agent="help desk" />);

  it("titles the page with the agent and links to its chat", () => {
    expect(html).toContain("help desk · runs");
    expect(html).toContain('href="#/agents/help%20desk/chat"');
  });

  it("lists each run on one line with who started it", () => {
    expect(html).toContain("<th>User</th>");
    expect(html).toContain("Actions");
  });
});
