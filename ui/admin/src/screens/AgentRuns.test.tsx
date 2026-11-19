import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { AgentRuns } from "./AgentRuns";

describe("AgentRuns", () => {
  const html = renderToStaticMarkup(<AgentRuns agent="help desk" />);

  it("titles the page with the agent, under the trail back to Agents", () => {
    expect(html).toContain('<li class="breadcrumb-item"><a href="#/agents">Agents</a></li>');
    expect(html).toContain('<h2 class="page-title">help desk</h2>');
  });

  it("shows the agent's tabs, with this screen's open", () => {
    expect(html).toContain('href="#/agents/help%20desk/chat"');
    expect(html).toMatch(/class="nav-link active" aria-current="page" href="#\/agents\/help%20desk\/runs"/);
  });

  it("lists each run on one line with who started it", () => {
    expect(html).toContain("<th>User</th>");
    expect(html).toContain("Actions");
  });
});
