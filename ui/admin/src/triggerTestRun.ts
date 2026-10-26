// What a **Test run** of a trigger says afterwards.
//
// The screen's part of a test run is two clicks and a toast; everything between
// them is arithmetic over one response, and it is here so it can be read and
// tested without a browser.
//
// Three things it decides, each of which the toast would otherwise get wrong:
//
//   - **A failure is news, not an error.** `testRunTrigger` answers 200 with
//     `ok: false` when the action failed, because the message *is* the result an
//     admin asked for. Only a request that never reached the action — the server
//     down, the trigger deleted in another tab — is a thrown error, and the two
//     read the same way in the toast.
//   - **The transcript belongs to the failure.** A `run_js_code` that threw on
//     its last line printed everything before it, and that is what says why. So
//     the console lines are shown whichever way the run went, and never
//     collapsed into the result.
//   - **Which row it ran on is part of the answer.** A trigger on `insert`,
//     `update` or `delete` is tested against a row picked at random from its
//     table, so a result that looks wrong is only traceable if the screen says
//     which row produced it — and an empty table is said out loud rather than
//     left to look like a trigger that ignored its row.

import type { TestRunTriggerResponse } from "./client";

/** One `console.*` call a code body made, as the server collected it. */
export type ConsoleLine = { level: string; text: string };

/** What the toast shows after a test run. */
export type TestRunOutcome = {
  /** Whether the action ran to completion. */
  ok: boolean;
  /** The toast's heading: the trigger's name and what happened to it. */
  title: string;
  /** The result as JSON, or the failure's message — the toast's main text. */
  text: string;
  /** What the body printed, in order. Empty for an action that printed nothing
   * and for every action that is not a code body. */
  console: ConsoleLine[];
  /** The row the run was given, described in one line, or `null` where the
   * event has no row at all. */
  row: string | null;
};

/** The response's console lines, defensively: the field is generated as an
 * array of objects, and a screen that trusted it blindly would render
 * `undefined` for a body that printed nothing. */
function consoleLines(response: TestRunTriggerResponse): ConsoleLine[] {
  const lines = response.console;
  return Array.isArray(lines) ? lines : [];
}

/** The row a table trigger was run against, in one line.
 *
 * The **id** where the row has one, because that is what an admin looks a row
 * up by; the whole object otherwise, bounded, because a table with no `id` is
 * still a table whose row is worth naming. */
export function rowSummary(row: unknown): string | null {
  if (row === null || row === undefined) return null;
  if (typeof row !== "object") return String(row);
  const fields = row as Record<string, unknown>;
  const id = fields.id;
  if (id !== undefined && id !== null) return `id ${String(id)}`;
  const text = JSON.stringify(row);
  return text.length > 120 ? `${text.slice(0, 120)}…` : text;
}

/** Whether this trigger's event carries a row, and so whether "no row" is a
 * thing worth saying about it. Mirrors `EventKind::is_table_event`. */
export function isTableEvent(when: string): boolean {
  return ["insert", "update", "insert_or_update", "delete"].includes(when);
}

/** The outcome of a test run that reached the action, either way. */
export function testRunOutcome(
  trigger: { name: string; when: string },
  response: TestRunTriggerResponse,
): TestRunOutcome {
  const row = rowSummary(response.row);
  return {
    ok: response.ok,
    title: response.ok ? `${trigger.name} ran` : `${trigger.name} failed`,
    text: response.ok
      ? JSON.stringify(response.result ?? null, null, 2)
      : (response.error ?? "The action failed, and said nothing about why."),
    console: consoleLines(response),
    // Only a table event has a row to have been given one, so this is silent
    // about every other kind rather than reporting "no row" about a `none`
    // trigger, which never had one to miss.
    row: isTableEvent(trigger.when)
      ? (row ?? "no row — the table is empty")
      : null,
  };
}

/** The outcome of a test run that never reached the action: the request itself
 * failed. Shown in the same toast, because from where the admin is sitting the
 * question was the same one. */
export function testRunFailure(
  trigger: { name: string },
  message: string,
): TestRunOutcome {
  return {
    ok: false,
    title: `${trigger.name} could not be run`,
    text: message,
    console: [],
    row: null,
  };
}
