import { describe, expect, it } from "vitest";

import type { Entry } from "./agentChat";
import { failureLine, firstSentence, runStory } from "./runStory";

const tool = (name: string, isError = false): Entry => ({
  kind: "tool",
  id: `${name}-${Math.random()}`,
  name,
  args: {},
  result: isError ? "error: `dir` must be a directory\nmore detail" : "ok",
  isError,
});
const said = (text: string): Entry => ({
  kind: "assistant",
  text,
  reasoning: "",
});

describe("a step's line", () => {
  it("is the first sentence, without markdown's line markers", () => {
    expect(firstSentence("Reading the schema first. Then the tables.")).toBe(
      "Reading the schema first.",
    );
    expect(firstSentence("\n## Tables `a` and `b`\n\nmore")).toBe(
      "Tables `a` and `b`",
    );
    // A word or two alone says nothing: the next sentence comes with it.
    expect(firstSentence("**Done.** Both tables exist. More.")).toBe(
      "Done. Both tables exist.",
    );
    expect(firstSentence("Done.")).toBe("Done.");
    // Punctuation inside a backticked name does not end the sentence.
    expect(
      firstSentence(
        "Saved all five, all `language: javascript` so they run. Next.",
      ),
    ).toBe("Saved all five, all `language: javascript` so they run.");
    expect(firstSentence("- id int, primary key")).toBe("id int, primary key");
    // A dot inside a name is not the end of a sentence.
    expect(firstSentence("Set attributes.options now. Next.")).toBe(
      "Set attributes.options now.",
    );
  });

  it("is cut to a length that fits one line", () => {
    expect(firstSentence("x".repeat(300), 20)).toBe(`${"x".repeat(19)}…`);
  });
});

describe("a failure's line", () => {
  it("is the error's first line, with the next when the first only introduces it", () => {
    expect(failureLine("error: listing `x` in store y\nat z")).toBe(
      "listing `x` in store y",
    );
    expect(
      failureLine(
        "error: the arguments for `save_plan` do not match its schema:\n- `/features/0/notes`: expected a string, got an array",
      ),
    ).toBe(
      "the arguments for `save_plan` do not match its schema: `/features/0/notes`: expected a string, got an array",
    );
  });
});

describe("a run's story", () => {
  it("is the task, each remark with the calls after it, and the last words as the outcome", () => {
    const story = runStory([
      { kind: "user", text: "Make the tables." },
      tool("describe_schema"),
      said("The dry run passes. Applying for real."),
      tool("edit_schema"),
      tool("edit_schema", true),
      tool("edit_schema"),
      tool("update_application"),
      said("Done. Both tables exist."),
    ]);
    expect(story.task).toBe("Make the tables.");
    expect(story.outcome).toBe("Done. Both tables exist.");
    expect(story.toolCalls).toBe(5);
    expect(story.toolErrors).toBe(1);
    expect(story.steps).toEqual([
      {
        kind: "agent",
        line: null,
        text: "",
        tools: [
          {
            name: "describe_schema",
            count: 1,
            errors: 0,
            failures: [],
            lastFailed: false,
            calls: [expect.objectContaining({ name: "describe_schema" })],
          },
        ],
      },
      {
        kind: "agent",
        line: "The dry run passes.",
        text: "The dry run passes. Applying for real.",
        tools: [
          // Why the failed call failed: its error's first line.
          {
            name: "edit_schema",
            count: 3,
            errors: 1,
            failures: ["`dir` must be a directory"],
            lastFailed: false,
            calls: expect.any(Array),
          },
          {
            name: "update_application",
            count: 1,
            errors: 0,
            failures: [],
            lastFailed: false,
            calls: expect.any(Array),
          },
        ],
      },
    ]);
  });

  it("keeps follow-ups and failures as steps, and has no outcome when the run did not end on words", () => {
    const story = runStory([
      { kind: "user", text: "Make the tables." },
      said("On it."),
      tool("edit_schema"),
      { kind: "user", text: "Also add an index. Thanks." },
      { kind: "error", message: "the model refused" },
    ]);
    expect(story.outcome).toBeNull();
    expect(story.steps.map((s) => s.kind)).toEqual([
      "agent",
      "person",
      "problem",
    ]);
    expect(story.steps[1]).toMatchObject({ line: "Also add an index." });
  });
});
