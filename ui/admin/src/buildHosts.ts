// Settings → Development → Build hosts: the machines that build an application's targets for
// this server, such as a Mac for iOS while the server runs on Linux
// (`TODO-build-hosts.md`). The pure half of the tab, so it is testable.
//
// A build host's agent asks this server for work; the server never calls it.
// It authenticates with the token shown once when the host is added, and the
// token is read from a file on the build host, never typed on its command line.

import type { ListBuildHostsResponse, ListFrameworksResponse } from "./client";

/** One build host as the admin API lists it. */
export type BuildHost = ListBuildHostsResponse[number];

/** Where the agent reads its token from on the build host. */
export const TOKEN_FILE = "~/.feldspar-build-token";

/** The badge beside a host's name: whether its agent is heard from. `t` is the
 * screen's translator, called here with each label written out. */
export function hostBadge(
  t: (text: string) => string,
  host: BuildHost,
): { label: string; tone: string } {
  if (refusal(host)) return { label: t("Needs update"), tone: "bg-red-lt" };
  if (host.online) return { label: t("Online"), tone: "bg-green-lt" };
  if (!host.last_seen_at) return { label: t("Never connected"), tone: "bg-secondary-lt" };
  return { label: t("Offline"), tone: "bg-yellow-lt" };
}

/** Why the server turned the host's agent away at its last heartbeat (an
 * agent too old for this server), if it did. */
export function refusal(host: BuildHost): string | null {
  const report = asObject(host.report);
  return typeof report?.refused === "string" ? report.refused : null;
}

/** What the agent reported about itself: `feldspar 0.1.0 on macos (aarch64)`. */
export function agentSummary(host: BuildHost): string | null {
  const report = asObject(host.report);
  if (!report || typeof report.version !== "string") return null;
  return `feldspar ${report.version} on ${String(report.os)} (${String(report.arch)})`;
}

/** Per target the host builds: what its machine lacks (empty: ready), or
 * `null` while the agent has not reported on that target yet. */
export function hostReadiness(host: BuildHost): Array<{ target: string; missing: string[] | null }> {
  const readiness = asObject(asObject(host.report)?.readiness);
  return host.targets.map((target) => {
    const entry = asObject(readiness?.[target]);
    const missing = Array.isArray(entry?.missing)
      ? entry.missing.filter((m): m is string => typeof m === "string")
      : null;
    return { target, missing };
  });
}

/** The targets a host can be given: every target an installed framework
 * declares, once, with its label. */
export function targetChoices(
  frameworks: ListFrameworksResponse,
): Array<{ name: string; label: string }> {
  const seen = new Map<string, string>();
  for (const framework of frameworks) {
    for (const target of framework.targets) {
      if (!seen.has(target.name)) seen.set(target.name, target.label);
    }
  }
  return [...seen].map(([name, label]) => ({ name, label }));
}

/** What is wrong with the form, or `null` when it can be saved. */
export function hostProblem(name: string, targets: string[]): string | null {
  if (name.trim() === "") return "Give the build host a name.";
  if (targets.length === 0) return "Choose at least one target it builds.";
  return null;
}

/** The command that starts the agent on the build host, once the token is in
 * {@link TOKEN_FILE}. */
export function agentCommand(origin: string): string {
  return `feldspar build-agent --server ${origin} --token-file ${TOKEN_FILE}`;
}

function asObject(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}
