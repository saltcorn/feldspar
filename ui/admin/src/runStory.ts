// A run's transcript, retold for a person scanning it (a run's own page):
// what the agent was asked, what it did step by step, and what it answered.
//
// The transcript is every word and every call; this is the outline of it. A
// step is one thing the agent said and the calls it made after saying it, read
// as the first sentence of the one and the names of the other. The full
// transcript stays on the page, folded, for when the outline is not enough.

import type { Entry } from "./agentChat";

/** The calls to one tool within a step, with why each failed one failed, and
 * whether the last of them failed (`false` after a retry that worked). */
export type StepTool = {
  name: string;
  count: number;
  errors: number;
  failures: string[];
  lastFailed: boolean;
  /** The calls themselves, in order, for their details. */
  calls: ToolCallEntry[];
};

/** One tool call of a transcript. */
export type ToolCallEntry = Extract<Entry, { kind: "tool" }>;

/** A failed call's error, the loop's `error: ` prefix dropped, as one line:
 * its first, which is where the loop puts the reason, and the line after when
 * the first only introduces it ("… do not match its schema:"). */
export function failureLine(result: unknown): string {
  const text = typeof result === "string" ? result : JSON.stringify(result ?? "");
  const lines = text
    .replace(/^error: /, "")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");
  const [first = "", next] = lines;
  return first.endsWith(":") && next ? `${first} ${next.replace(/^- /, "")}` : first;
}

/** One step: what the agent said (`null` when it called tools without a word)
 * and the tools it called before it next spoke. */
export type StoryStep =
  | { kind: "agent"; line: string | null; text: string; tools: StepTool[] }
  /** A person's message after the first: a follow-up in the conversation. */
  | { kind: "person"; line: string; text: string }
  /** A failure, or the loop stopping on its own. */
  | { kind: "problem"; line: string };

export type RunStory = {
  /** The first message: what the run was asked to do. */
  task: string | null;
  steps: StoryStep[];
  /** The agent's last words, when the run ended on them. */
  outcome: string | null;
  toolCalls: number;
  toolErrors: number;
};

/** The first sentence of `text`, or its first line, at most `max` characters:
 * enough to tell one step from the next. Markdown's line markers (`#`, `-`,
 * `**`) are dropped, since they are noise in one line. Punctuation inside a
 * backticked name does not end the sentence, and a first sentence of a word
 * or two ("Done.") runs on into the next, since it says nothing alone. */
export function firstSentence(text: string, max = 160): string {
  const line =
    text
      .split("\n")
      .map((l) => l.replace(/^\s*(#+|[-*]|\d+\.)\s+/, "").replace(/\*\*/g, "").trim())
      .find((l) => l !== "") ?? "";
  let end = line.length;
  let inCode = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (c === "`") inCode = !inCode;
    else if (!inCode && ".!?:".includes(c) && (i + 1 === line.length || line[i + 1] === " ")) {
      const words = line.slice(0, i).trim().split(/\s+/).length;
      if (words <= 2 && i + 1 < line.length) continue;
      end = i + 1;
      break;
    }
  }
  const sentence = line.slice(0, end);
  return sentence.length > max ? `${sentence.slice(0, max - 1).trimEnd()}…` : sentence;
}

export function runStory(entries: Entry[]): RunStory {
  const story: RunStory = { task: null, steps: [], outcome: null, toolCalls: 0, toolErrors: 0 };
  let current: Extract<StoryStep, { kind: "agent" }> | null = null;
  for (const entry of entries) {
    switch (entry.kind) {
      case "user":
        current = null;
        if (story.task === null) story.task = entry.text;
        else story.steps.push({ kind: "person", line: firstSentence(entry.text), text: entry.text });
        break;
      case "assistant": {
        if (entry.text.trim() === "") break;
        current = { kind: "agent", line: firstSentence(entry.text), text: entry.text, tools: [] };
        story.steps.push(current);
        break;
      }
      case "tool": {
        if (current === null) {
          current = { kind: "agent", line: null, text: "", tools: [] };
          story.steps.push(current);
        }
        story.toolCalls += 1;
        if (entry.isError) story.toolErrors += 1;
        let last = current.tools[current.tools.length - 1];
        if (last && last.name === entry.name) {
          last.count += 1;
        } else {
          last = { name: entry.name, count: 1, errors: 0, failures: [], lastFailed: false, calls: [] };
          current.tools.push(last);
        }
        last.calls.push(entry);
        last.lastFailed = entry.isError;
        if (entry.isError) {
          last.errors += 1;
          last.failures.push(failureLine(entry.result));
        }
        break;
      }
      case "error":
      case "notice":
        current = null;
        story.steps.push({ kind: "problem", line: entry.message });
        break;
      default:
        break;
    }
  }
  // A run that ended on the agent's words answered with them: they are the
  // outcome, not a step.
  const last = story.steps[story.steps.length - 1];
  if (last?.kind === "agent" && last.tools.length === 0) {
    story.outcome = last.text;
    story.steps.pop();
  }
  return story;
}
