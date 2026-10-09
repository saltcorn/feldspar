// The source-control panel's model: what a store's working copy is, which rows
// the panel draws, and which button sits beside the commit message.
//
// The panel is laid out like VS Code's Source Control view, deliberately — an
// admin who knows what `U` means there, or where Stage All is, should not have
// to learn a second vocabulary here. This file is the part of that which can be
// asserted without a browser; `screens/SourceControl.tsx` draws it.
//
// **Which stores get the panel** is decided by what a backend *answers*, not by
// its name: a store whose automatic status operation returns a payload this
// parses (`parseScmStatus`) is a working copy, and the panel drives it through
// the operations named in `SCM_OPERATIONS`. The git backend is the one that
// does today; the file-store form still contains no `backend === "git"`.

import { format, type Translator } from "./i18n";

/** One changed path, as `git status --porcelain` reports it. */
export interface ScmChange {
  /** The two-character `XY` code — index column, then working-tree column. */
  readonly status: string;
  /** The path, relative to the store root. */
  readonly path: string;
}

/** What the working copy is right now — the status operation's `data`. */
export interface ScmStatus {
  /** Whether there is a working copy at all. */
  readonly cloned: boolean;
  /** Whether cloning may write here: no clone, and a missing or empty directory. */
  readonly canClone: boolean;
  /** The checked-out branch, or `""` on a detached head or an empty repository. */
  readonly branch: string;
  /** Every branch that can be checked out. */
  readonly branches: readonly string[];
  /** Whether the branch has an upstream to push to. */
  readonly upstream: boolean;
  /** Commits to push. */
  readonly ahead: number;
  /** Commits to pull. */
  readonly behind: number;
  /** The last commit as one line, or `""` when there is none. */
  readonly lastCommit: string;
  /** Every uncommitted change. */
  readonly changes: readonly ScmChange[];
}

/** The operations the panel calls, by the names the backend declares them under. */
export const SCM_OPERATIONS = [
  "status",
  "clone",
  "pull",
  "push",
  "stage",
  "unstage",
  "discard",
  "commit",
  "checkout",
] as const;

export type ScmOperation = (typeof SCM_OPERATIONS)[number];

/** Whether an operation is one the panel draws, so the generic form skips it. */
export function isScmOperation(name: string): boolean {
  return (SCM_OPERATIONS as readonly string[]).includes(name);
}

/**
 * Read a status payload, or `null` when `data` is not one.
 *
 * A parser rather than a cast: an operation's `data` is optional and
 * backend-shaped, and anything unrecognisable means *this is not a working
 * copy* — which is exactly the question the form asks it.
 */
export function parseScmStatus(data: unknown): ScmStatus | null {
  if (data == null || typeof data !== "object") return null;
  const raw = data as Record<string, unknown>;
  if (typeof raw.cloned !== "boolean") return null;
  return {
    cloned: raw.cloned,
    canClone: raw.can_clone === true,
    branch: typeof raw.branch === "string" ? raw.branch : "",
    branches: Array.isArray(raw.branches)
      ? raw.branches.filter((b): b is string => typeof b === "string")
      : [],
    upstream: raw.upstream === true,
    ahead: count(raw.ahead),
    behind: count(raw.behind),
    lastCommit: typeof raw.last_commit === "string" ? raw.last_commit : "",
    changes: changeList(raw.changes),
  };
}

/** The groups of the list, in the order they are drawn. */
export type ChangeGroup = "merge" | "staged" | "unstaged";

/** One row of the list: a change, in one group. */
export interface ChangeRow {
  readonly group: ChangeGroup;
  readonly path: string;
  /** VS Code's letter: `M`, `A`, `D`, `R`, `C`, `T`, `U` (untracked), `!` (conflict). */
  readonly letter: string;
  /** What the letter means, for the tooltip. */
  readonly label: string;
  /** Whether the file is gone, so the row is struck through. */
  readonly deleted: boolean;
  /** Whether the file is untracked, so discarding it deletes it. */
  readonly untracked: boolean;
}

/**
 * Every row, in the order git listed the changes. A file staged and then edited
 * again is **two** rows — one in each group — because a commit takes one and
 * not the other.
 */
export function changeRows(status: ScmStatus, t: Translator["t"] = format): ChangeRow[] {
  return status.changes.flatMap((change) => rowsFor(change, t));
}

/** Only the rows of one group. */
export function rowsIn(rows: readonly ChangeRow[], group: ChangeGroup): ChangeRow[] {
  return rows.filter((row) => row.group === group);
}

function rowsFor(change: ScmChange, t: Translator["t"]): ChangeRow[] {
  const code = `${change.status}  `.slice(0, 2);
  const index = code[0] ?? " ";
  const worktree = code[1] ?? " ";
  const row = (group: ChangeGroup, letter: string, label: string): ChangeRow => ({
    group,
    path: change.path,
    letter,
    label,
    deleted: letter === "D",
    untracked: code === "??",
  });

  if (code === "??") return [row("unstaged", "U", t("Untracked"))];
  if (CONFLICTS.includes(code)) return [row("merge", "!", t("Conflict"))];

  const rows: ChangeRow[] = [];
  if (index !== " ") rows.push(row("staged", letterFor(index), nameFor(index, t)));
  if (worktree !== " ") rows.push(row("unstaged", letterFor(worktree), nameFor(worktree, t)));
  return rows.length > 0 ? rows : [row("unstaged", "M", t("Modified"))];
}

/** The `XY` codes git uses for an unresolved merge. */
const CONFLICTS = ["DD", "AU", "UD", "UA", "DU", "AA", "UU"];

function letterFor(column: string): string {
  return "MADRCT".includes(column) ? column : "M";
}

function nameFor(column: string, t: Translator["t"]): string {
  switch (column) {
    case "A":
      return t("Added");
    case "D":
      return t("Deleted");
    case "R":
      return t("Renamed");
    case "C":
      return t("Copied");
    case "T":
      return t("Type changed");
    default:
      return t("Modified");
  }
}

/** The file's own name, and the directory it is in (`""` at the root). */
export function splitPath(path: string): { name: string; dir: string } {
  const slash = path.lastIndexOf("/");
  return slash < 0
    ? { name: path, dir: "" }
    : { name: path.slice(slash + 1), dir: path.slice(0, slash) };
}

/** What the button beside the message box does. */
export type PrimaryKind = "commit" | "push" | "publish" | "pull";

export interface PrimaryAction {
  readonly kind: PrimaryKind;
  /** Why it cannot be pressed yet, or `null` when it can. */
  readonly blocked: string | null;
}

/**
 * The one button beside the message box, which — as in VS Code — becomes Push
 * once there is nothing left to commit and something to send.
 *
 * - Uncommitted changes: **Commit**, until a message is typed.
 * - Clean, behind: **Pull** first, since a push would be refused anyway.
 * - Clean, ahead: **Push**.
 * - Clean, on a branch with commits but no upstream: **Publish branch** —
 *   the push that sets the upstream.
 * - Otherwise: Commit, with nothing to commit.
 */
export function primaryAction(
  status: ScmStatus,
  message: string,
  t: Translator["t"] = format,
): PrimaryAction {
  if (status.changes.length > 0) {
    return {
      kind: "commit",
      blocked: message.trim() === "" ? t("Type a commit message first.") : null,
    };
  }
  if (status.upstream && status.behind > 0) {
    return { kind: "pull", blocked: null };
  }
  if (status.upstream && status.ahead > 0) {
    return { kind: "push", blocked: null };
  }
  if (!status.upstream && status.branch !== "" && status.lastCommit !== "") {
    return { kind: "publish", blocked: null };
  }
  return { kind: "commit", blocked: t("There are no changes to commit.") };
}

/**
 * The commit operation's input. What is staged is committed when anything is;
 * with nothing staged, every change is — VS Code's "smart commit", which is what
 * an admin who never touched the stage buttons means by pressing Commit.
 */
export function commitInput(
  status: ScmStatus,
  message: string,
): { message: string; staged_only: boolean } {
  const staged = changeRows(status).some((row) => row.group === "staged");
  return { message: message.trim(), staged_only: staged };
}

/** The confirmation a discard asks for — it cannot be undone. */
export function discardConfirmation(rows: readonly ChangeRow[]): string {
  if (rows.length === 1) {
    const [row] = rows;
    return row.untracked
      ? `Delete ${row.path}? It is untracked, so this is permanent.`
      : `Discard the changes to ${row.path}? This cannot be undone.`;
  }
  const untracked = rows.filter((row) => row.untracked).length;
  const deleted = untracked > 0 ? ` ${untracked} untracked file(s) will be deleted.` : "";
  return `Discard all ${rows.length} changes?${deleted} This cannot be undone.`;
}

/** The option value in the branch selector that asks for a new branch. */
export const NEW_BRANCH = "\u0000new-branch";

/**
 * Why a new branch name will not do, or `null` when it will: git's rules,
 * narrowed to the ones someone can trip over in a text box. Everything else git
 * refuses for itself with its own message.
 */
export function branchNameProblem(
  name: string,
  existing: readonly string[],
  t: Translator["t"] = format,
): string | null {
  const trimmed = name.trim();
  if (trimmed === "") return t("A branch needs a name.");
  if (/[\s~^:?*[\\]/.test(trimmed)) {
    return t("A branch name cannot contain spaces or any of ~^:?*[\\");
  }
  if (trimmed.startsWith("-") || trimmed.startsWith("/") || trimmed.endsWith("/")) {
    return t("A branch name cannot start with - or /, or end with /.");
  }
  if (trimmed.includes("..") || trimmed.endsWith(".lock")) {
    return t("A branch name cannot contain .. or end with .lock.");
  }
  if (existing.includes(trimmed)) return t("{name} already exists.", { name: trimmed });
  return null;
}

function count(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function changeList(value: unknown): ScmChange[] {
  if (!Array.isArray(value)) return [];
  const changes: ScmChange[] = [];
  for (const item of value) {
    if (item == null || typeof item !== "object") continue;
    const raw = item as Record<string, unknown>;
    if (typeof raw.path !== "string" || raw.path === "") continue;
    changes.push({ status: typeof raw.status === "string" ? raw.status : "", path: raw.path });
  }
  return changes;
}
