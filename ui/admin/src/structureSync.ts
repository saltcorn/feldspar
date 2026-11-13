// Source control's model (APP_SYNC_PROPOSAL §4, §5, §7.5): Status grouped by
// class, what can be committed or discarded, the link form's body, the deploy
// and discard reviews' ticks and typed confirmation, the ledger's lines and
// "restore this version", and the registry editor's diffs against the base and
// the branch tip.
//
// `screens/SourceControlPage.tsx` draws it. Everything here can be asserted
// without a browser. (A git *file store*'s working copy is another thing:
// `sourceControl.ts`.)

import type {
  GetSyncEntityDiffResponse,
  GetSyncLinkResponse,
  GetSyncStatusResponse,
  ListDeploymentsResponse,
  PlanSyncDeployResponse,
  SetSyncLinkRequest,
  SyncDeployResponse,
} from "./client";
import type { Translator } from "./i18n";
import { confirmationWord, type PlannedOp } from "./registry";

export type SyncStatus = GetSyncStatusResponse;
export type StatusEntry = GetSyncStatusResponse["entries"][number];
export type SyncLinkInfo = GetSyncLinkResponse;
export type DeployPlan = PlanSyncDeployResponse;
export type Policy = PlanSyncDeployResponse["policy"];
export type Receipt = SyncDeployResponse;
export type Deployment = ListDeploymentsResponse["deployments"][number];
export type EntityDiff = GetSyncEntityDiffResponse;
export type SideDiff = GetSyncEntityDiffResponse["local"];

// --- Status ------------------------------------------------------------------

/** Status's groups, in the order the screen shows them: what needs a decision
 * first, then what this instance would commit, then what Deploy would apply. */
export const STATUS_GROUPS = ["conflict", "local", "incoming", "incoming_delete"] as const;
export type StatusGroup = (typeof STATUS_GROUPS)[number];

/** The group an entry's class is shown under. */
export function statusGroup(cls: string): StatusGroup {
  switch (cls) {
    case "conflict":
      return "conflict";
    case "local_change":
    case "local_delete":
    case "local_new":
    case "pin_moved":
      return "local";
    case "incoming_delete":
      return "incoming_delete";
    default:
      return "incoming";
  }
}

export function groupLabel(group: StatusGroup, t: Translator["t"]): string {
  switch (group) {
    case "conflict":
      return t("Conflict");
    case "local":
      return t("Local");
    case "incoming":
      return t("Incoming");
    case "incoming_delete":
      return t("Incoming delete");
  }
}

/** The finer word beside an entry: what kind of local or incoming change. */
export function classNote(cls: string, t: Translator["t"]): string {
  switch (cls) {
    case "local_new":
    case "incoming_new":
      return t("new");
    case "local_delete":
      return t("deleted");
    case "local_change":
    case "incoming":
      return t("changed");
    case "pin_moved":
      return t("pinned commit moved");
    case "incoming_delete":
      return t("deleted on the branch");
    case "conflict":
      return t("changed here and on the branch");
    default:
      return cls;
  }
}

/** Status's entries under their groups, in order, leaving out empty groups. */
export function groupStatus(
  entries: readonly StatusEntry[],
): { group: StatusGroup; entries: StatusEntry[] }[] {
  return STATUS_GROUPS.map((group) => ({
    group,
    entries: entries.filter((e) => statusGroup(e.class) === group),
  })).filter((g) => g.entries.length > 0);
}

/** Whether an entry can be ticked for Commit and Discard: a local change, or a
 * conflict (ticked, it is *keep mine* for a commit and *discard mine* for a
 * discard). An incoming change is Deploy's. */
export function selectable(entry: Pick<StatusEntry, "class">): boolean {
  const group = statusGroup(entry.class);
  return group === "local" || group === "conflict";
}

/** What an entry is called on the screen: `helpdesk / list` for a view. */
export function entryTitle(entry: Pick<StatusEntry, "name" | "application">): string {
  return entry.application ? `${entry.application} / ${entry.name}` : entry.name;
}

/** The ticks Status starts with: every local change, and no conflict — a
 * conflict is the admin's to decide. */
export function defaultSelection(entries: readonly StatusEntry[]): Set<string> {
  return new Set(entries.filter((e) => statusGroup(e.class) === "local").map((e) => e.key));
}

/** The body of a commit of `selected`: all of them by key, with each ticked
 * conflict kept as mine. `null` when nothing is ticked. */
export function commitBody(
  message: string,
  entries: readonly StatusEntry[],
  selected: ReadonlySet<string>,
): { message: string; keys: string[]; keep_mine: string[] } | null {
  const chosen = entries.filter((e) => selectable(e) && selected.has(e.key));
  if (chosen.length === 0) return null;
  return {
    message: message.trim(),
    keys: chosen.map((e) => e.key),
    keep_mine: chosen.filter((e) => e.class === "conflict").map((e) => e.key),
  };
}

/** Why Deploy cannot be opened, or `null` when it can: the local changes and
 * conflicts it is refused over (D5), in a sentence. */
export function deployRefusal(status: Pick<SyncStatus, "deploy_blockers">, t: Translator["t"]): string | null {
  if (status.deploy_blockers.length === 0) return null;
  return t(
    "Deploy is refused until what changed here is committed or discarded: {keys}",
    { keys: status.deploy_blockers.join(", ") },
  );
}

/** A commit as the screen shows it: its first ten characters. */
export function shortCommit(commit: string | null | undefined): string {
  return commit ? commit.slice(0, 10) : "—";
}

// --- One entity's diff ----------------------------------------------------------

/** A JSON change as one line: `fields/1/name: "status" → "state"`. */
export function diffLine(change: SideDiff["changes"][number]): string {
  const show = (v: unknown) => (v === undefined || v === null ? "—" : JSON.stringify(v));
  return `${change.path || "(whole)"}: ${show(change.before)} → ${show(change.after)}`;
}

/** Whether a side changed anything. */
export function sideChanged(side: SideDiff): boolean {
  return side.changes.length > 0 || Boolean(side.text) || side.binary;
}

// --- The link ------------------------------------------------------------------

/** What the link form holds. */
export interface LinkForm {
  url: string;
  branch: string;
  subdirectory: string;
  /** `none`, `deploy_key` or `https_token`. */
  credential: string;
  username: string;
  /** A new token; empty keeps the stored one. */
  token: string;
  destructive: string;
  fullBackup: string;
}

/** The form, from what is stored. */
export function linkForm(info: SyncLinkInfo | null): LinkForm {
  const link = info?.link ?? null;
  const credential = link?.credential ?? info?.credential ?? { kind: "none" };
  return {
    url: link?.url ?? "",
    branch: link?.branch ?? "main",
    subdirectory: link?.subdirectory ?? "",
    credential: credential.kind,
    username: credential.username ?? "",
    token: "",
    destructive: link?.policy.destructive ?? "confirm",
    fullBackup: link?.policy.full_backup ?? "off",
  };
}

/** The `setSyncLink` / `testSyncLink` body the form asks for. A token is
 * sent only when one was typed, so an untouched form keeps the stored one. */
export function linkBody(form: LinkForm): SetSyncLinkRequest {
  const token = form.token.trim();
  return {
    url: form.url.trim(),
    branch: form.branch.trim(),
    subdirectory: form.subdirectory.trim() || null,
    credential: {
      kind: form.credential,
      username: form.credential === "https_token" ? form.username.trim() || null : null,
      token: form.credential === "https_token" && token ? token : null,
    },
    policy: { destructive: form.destructive, full_backup: form.fullBackup },
  };
}

/** Why the form cannot be saved yet, or `null`. */
export function linkProblem(form: LinkForm, info: SyncLinkInfo | null, t: Translator["t"]): string | null {
  if (!form.url.trim()) return t("A link needs the repository's URL.");
  if (!form.branch.trim()) return t("A link needs a branch.");
  if (form.credential === "deploy_key" && info?.credential.kind !== "deploy_key")
    return t("Generate a deploy key first, and give the repository its public key.");
  const storedToken = info?.credential.kind === "https_token";
  if (form.credential === "https_token" && !storedToken && !form.token.trim())
    return t("Type the HTTPS token.");
  return null;
}

// --- The deploy and discard reviews -----------------------------------------------

/** Whether an operation's tick can be changed under `policy`: an unsupported
 * one never is, and with `destructive: never` nothing that loses data can be
 * ticked here (only `feldspar sync deploy --allow-destructive` can). */
export function reviewTickable(op: PlannedOp, policy: Pick<Policy, "destructive"> | null): boolean {
  if (op.class === "unsupported") return false;
  if (op.class === "data_destructive" && policy?.destructive === "never") return false;
  return true;
}

/** The ticks a review starts with: each operation's default, less what the
 * policy forbids. */
export function reviewTicks(ops: readonly PlannedOp[], policy: Pick<Policy, "destructive"> | null): Set<string> {
  return new Set(ops.filter((op) => op.ticked && reviewTickable(op, policy)).map((op) => op.id));
}

/** Whether the review's confirm button may be pressed: no blocker, and the
 * word typed when anything ticked loses data. */
export function reviewCanConfirm(
  plan: Pick<DeployPlan, "operations" | "blockers">,
  ticked: ReadonlySet<string>,
  typed: string,
  word: string,
): boolean {
  if (plan.blockers.length > 0) return false;
  const asked = confirmationWord(plan.operations, ticked, word);
  return asked === null || typed.trim() === asked;
}

/** The counts a review's heading shows, by class. */
export function classCounts(ops: readonly PlannedOp[]): Record<string, number> {
  const out: Record<string, number> = {};
  for (const op of ops) out[op.class] = (out[op.class] ?? 0) + 1;
  return out;
}

/** What a receipt says in its headline. */
export function receiptTone(receipt: Pick<Receipt, "result">): "success" | "warning" | "danger" {
  switch (receipt.result) {
    case "converged":
      return "success";
    case "drifted":
      return "warning";
    default:
      return "danger";
  }
}

// --- History ---------------------------------------------------------------------

export function ledgerKindLabel(kind: string, t: Translator["t"]): string {
  switch (kind) {
    case "deploy":
      return t("Deploy");
    case "discard":
      return t("Discard");
    case "commit":
      return t("Commit");
    case "edit":
      return t("Registry save");
    default:
      return kind;
  }
}

/** What a ledger row was about, in one line. */
export function ledgerSummary(row: Deployment, t: Translator["t"]): string {
  const summary = (row.summary ?? {}) as Record<string, unknown>;
  if (row.kind === "commit") {
    const entities = Array.isArray(summary.entities) ? summary.entities.length : 0;
    return t("{message} · entities: {count}", {
      message: typeof summary.message === "string" ? summary.message : "",
      count: entities,
    });
  }
  const parts: string[] = [];
  if (row.kind === "edit" && typeof summary.path === "string") parts.push(summary.path);
  const applied = Array.isArray(summary.applied) ? summary.applied.length : 0;
  parts.push(t("applied: {count}", { count: applied }));
  if (row.deferred.length > 0) parts.push(t("deferred: {count}", { count: row.deferred.length }));
  const drift = Array.isArray(summary.drift) ? summary.drift.length : 0;
  if (drift > 0) parts.push(t("differences after: {count}", { count: drift }));
  return parts.join(" · ");
}

/** What "restore this version" saves for a registry save: the file and its
 * JSON before. `null` for anything else, or a save that created the file. */
export function restorable(row: Deployment): { path: string; value: unknown } | null {
  if (row.kind !== "edit") return null;
  const path = (row.summary as Record<string, unknown> | null)?.path;
  if (typeof path !== "string" || row.before === null || row.before === undefined) return null;
  return { path, value: row.before };
}

// --- The registry editor's diff tabs -------------------------------------------------

/** `value` without the keys at `pointers` (JSON pointers, `/a/b`): a registry
 * file as the portable tree holds it, so a diff against the base or the tip
 * does not show this instance's own values as changes. */
export function withoutPointers(value: unknown, pointers: readonly string[]): unknown {
  if (pointers.length === 0) return value;
  const copy = JSON.parse(JSON.stringify(value ?? null)) as unknown;
  for (const pointer of pointers) {
    const parts = pointer
      .split("/")
      .slice(1)
      .map((p) => p.replace(/~1/g, "/").replace(/~0/g, "~"));
    let at: unknown = copy;
    for (let i = 0; i < parts.length - 1 && at !== null && typeof at === "object"; i++) {
      at = (at as Record<string, unknown>)[parts[i]];
    }
    const last = parts[parts.length - 1];
    if (at !== null && typeof at === "object" && last !== undefined) {
      if (Array.isArray(at)) at.splice(Number(last), 1);
      else delete (at as Record<string, unknown>)[last];
    }
  }
  return copy;
}

/** Every changed value between two JSON values, in path order: what the
 * editor's base and tip tabs list. Arrays of objects with a `name` are matched
 * by name, so a field moved in the list is not every field changed. */
export function jsonChanges(before: unknown, after: unknown): SideDiff["changes"] {
  const out: SideDiff["changes"] = [];
  walk("", before, after, out);
  return out;
}

function walk(path: string, a: unknown, b: unknown, out: SideDiff["changes"]) {
  if (JSON.stringify(a) === JSON.stringify(b)) return;
  const isObject = (v: unknown) => v !== null && typeof v === "object" && !Array.isArray(v);
  if (isObject(a) && isObject(b)) {
    const ao = a as Record<string, unknown>;
    const bo = b as Record<string, unknown>;
    for (const key of [...new Set([...Object.keys(ao), ...Object.keys(bo)])].sort()) {
      walk(path ? `${path}/${key}` : key, ao[key], bo[key], out);
    }
    return;
  }
  if (Array.isArray(a) && Array.isArray(b)) {
    const named = (xs: unknown[]) =>
      xs.every((x) => isObject(x) && typeof (x as Record<string, unknown>).name === "string");
    if (named(a) && named(b)) {
      const byName = (xs: unknown[]) =>
        new Map(xs.map((x) => [(x as Record<string, unknown>).name as string, x]));
      const am = byName(a);
      const bm = byName(b);
      for (const name of [...new Set([...am.keys(), ...bm.keys()])]) {
        walk(`${path}[${name}]`, am.get(name), bm.get(name), out);
      }
      return;
    }
  }
  out.push({ path, before: a === undefined ? null : a, after: b === undefined ? null : b });
}
