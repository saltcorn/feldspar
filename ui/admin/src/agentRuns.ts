// The admin's list of one agent's runs: every user's, newest first.
//
// Pure functions, so the screen's one piece of interpretation — who a run
// belongs to — is tested without a browser.

import type { ListRunsResponse, ListUsersResponse } from "./client";

type RunItem = ListRunsResponse[number];

/** Who started a run, as an admin reads it: the user's email where the user
 * still exists, the bare id where they have since been deleted, and — for a run
 * with no user — whether a trigger or an anonymous visitor started it is not in
 * the summary, so it says only that nobody signed in did. */
export function runUserLabel(run: RunItem, users: ListUsersResponse): string {
  if (!run.user) return "anonymous";
  return users.find((u) => u.id === run.user)?.email ?? run.user;
}

/** A run's context, pretty-printed for the run page. */
export function prettyContext(context: unknown): string {
  return JSON.stringify(context ?? null, null, 2);
}
