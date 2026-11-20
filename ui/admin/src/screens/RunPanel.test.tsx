// An agent run's own page, rendered from a stored run.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import type { GetRunResponse } from "../client";
import { AgentRunDetail, ToolCallsDetail } from "./RunPanel";

function run(state: string): GetRunResponse {
  return {
    id: "r1",
    kind: "agent",
    subject: "construct-shop",
    description: "Construct: tables `books` and `authors`",
    state,
    created_at: "2026-01-01T10:00:00Z",
    updated_at: "2026-01-01T10:03:05Z",
    context: {
      messages: [
        { role: "user", content: "Create the tables `books` and `authors`." },
        {
          role: "assistant",
          content: "Checking the schema first. Then the batch.",
          tool_calls: [
            { id: "c1", name: "edit_schema", arguments: {} },
            { id: "c2", name: "edit_schema", arguments: {} },
          ],
        },
        { role: "tool_result", tool_call_id: "c1", content: "error: refused" },
        { role: "tool_result", tool_call_id: "c2", content: "ok" },
        { role: "assistant", content: "Done. Both tables exist." },
      ],
      ledger: {
        steps: [
          {
            cost: null,
            usage: {
              input_tokens: 20_000,
              cached_input_tokens: 15_000,
              output_tokens: 70,
            },
          },
          {
            cost: null,
            usage: {
              input_tokens: 30_000,
              cached_input_tokens: 25_000,
              output_tokens: 900,
            },
          },
        ],
      },
    },
    attributes: {},
    trace: [],
  };
}

describe("an agent run's page", () => {
  it("shows its figures as tiles, its backticked names as code, and folds the context", () => {
    const html = renderToStaticMarkup(
      <AgentRunDetail run={run("done")} onRefresh={() => {}} />,
    );
    expect(html).toContain("<code>books</code>");
    expect(html).toMatch(/Model calls<\/div><div class="run-stat-value">2</);
    expect(html).toMatch(/Input tokens<\/div><div class="run-stat-value">50k</);
    expect(html).toContain("80% cached");
    expect(html).toContain("A model has no price");
    expect(html).toMatch(
      /Duration<\/div><div class="run-stat-value">3 min 5 s</,
    );
    // Folded until opened.
    expect(html).toMatch(/<details class="card run-context">/);
    expect(html).not.toMatch(/<details[^>]*open/);
  });

  it("retells the transcript: what it was asked, each step with its tools, and its answer", () => {
    const html = renderToStaticMarkup(
      <AgentRunDetail run={run("done")} onRefresh={() => {}} />,
    );
    expect(html).toContain("What it was asked");
    expect(html).toContain("<code>books</code>");
    // The step: its first sentence, and the two calls folded into one chip,
    // marked as having a failure.
    expect(html).toContain("Checking the schema first.");
    // Why it failed, under the step's tools.
    // The retry worked, so the chip is not red: only its count of failures is.
    expect(html).toMatch(
      /class="story-tool"[^>]*>edit_schema<span class="story-tool-count">×2/,
    );
    expect(html).toContain('<span class="text-danger">1 failed</span>');
    // The failure is a row: retried, the tool, why.
    expect(html).toContain(
      '<li class="story-failure-retried"><span class="story-failure-status">retried</span><code>edit_schema</code><span class="story-failure-why" title="refused">refused</span></li>',
    );
    expect(html).toMatch(/What it answered.*Done\. Both tables exist\./s);
    expect(html).toMatch(
      /Tool calls<\/div><div class="run-stat-value">2<\/div><div class="run-stat-note">1 failed/,
    );
    // The full transcript is still there, folded.
    expect(html).toMatch(
      /<details class="card run-context mb-3"><summary[^>]*>.*Full transcript/s,
    );
  });

  it("has no duration while the run is still going", () => {
    const html = renderToStaticMarkup(
      <AgentRunDetail run={run("running")} onRefresh={() => {}} />,
    );
    expect(html).not.toContain("Duration");
  });
});

describe("a tool chip's calls", () => {
  it("shows each call's arguments and result, numbered, with the failed one marked", () => {
    const call = (id: string, result: string, isError: boolean) => ({
      kind: "tool" as const,
      id,
      name: "search_files",
      args: { pattern: "todo", dir: id === "c1" ? "README.md" : "src" },
      result,
      isError,
    });
    const html = renderToStaticMarkup(
      <ToolCallsDetail
        tool={{
          name: "search_files",
          count: 2,
          errors: 1,
          failures: ["`dir` must be a directory"],
          lastFailed: false,
          calls: [
            call("c1", "error: `dir` must be a directory", true),
            call("c2", "src/a.ts:1: todo", false),
          ],
        }}
      />,
    );
    // A tab per call, saying which call it is and how it went; the first shown.
    expect(html).toMatch(
      /class="story-call-tab is-current"><span class="story-call-dot is-failed"[^>]*><\/span>Call 1<span class="story-call-tab-outcome">failed<\/span>/,
    );
    expect(html).toMatch(
      /Call 2<span class="story-call-tab-outcome">ok<\/span>/,
    );
    // Only the current call is laid out: arguments as named values, the
    // error without its prefix and labelled as one.
    expect(html).toMatch(
      /<dt>dir<\/dt><dd><div class="story-call-text"[^>]*><span>README.md<\/span><\/div><\/dd>/,
    );
    expect(html).toContain(
      '<div class="story-call-label">Error</div><pre>`dir` must be a directory</pre>',
    );
    expect(html).not.toContain("src/a.ts:1: todo");
  });

  it("folds a list of objects to a line each, naming it by its id and title", () => {
    const html = renderToStaticMarkup(
      <ToolCallsDetail
        tool={{
          name: "save_plan",
          count: 1,
          errors: 0,
          failures: [],
          lastFailed: false,
          calls: [
            {
              kind: "tool" as const,
              id: "c1",
              name: "save_plan",
              args: {
                features: [
                  {
                    id: "progress-overview",
                    title: "Progress tab",
                    description: "Long.",
                  },
                  {
                    id: "pr-badge",
                    title: "New PR badge",
                    acceptance: ["a", "b", "c", "d", "e", "f"],
                  },
                ],
              },
              result: "plan saved.",
              isError: false,
            },
          ],
        }}
      />,
    );
    expect(html).not.toContain("story-call-tabs");
    expect(html).toMatch(
      /aria-expanded="false"><span class="story-call-n">1<\/span><code>progress-overview<\/code><span class="story-call-object-label">Progress tab<\/span>/,
    );
    expect(html).toContain("<code>pr-badge</code>");
    // Folded: the items' own fields are not laid out until opened.
    expect(html).not.toContain("<dt>description</dt>");
  });
});
