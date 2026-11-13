// The registry editor's model (APP_SYNC_PROPOSAL §7.6): the instance tree's
// files grouped into the kind / name tree the screen lists, the filter, the
// path a new entry is written at, and the save review — which operations are
// shown under which heading, and when a typed confirmation is asked for.
//
// `screens/SourceControlPage.tsx` draws it. Everything here can be asserted
// without a browser.

import type {
  GetRegistryEntityResponse,
  ListRegistryResponse,
  PlanRegistrySaveResponse,
  RegistrySaveResponse,
} from "./client";
import type { Translator } from "./i18n";

export type RegistryEntry = ListRegistryResponse["entries"][number];
export type RegistryEntity = GetRegistryEntityResponse;
export type PlannedOp = PlanRegistrySaveResponse["operations"][number];
export type RegistryReceipt = RegistrySaveResponse;

/** The kinds, in the order the tree lists them: this instance's own first,
 * then the structure in the order a deploy builds it (§6 step 8). */
export const KIND_ORDER = [
  "setting",
  "build_host",
  "module",
  "role",
  "users",
  "table",
  "db_connection",
  "file_store",
  "code_library",
  "llm_provider",
  "mcp_server",
  "trigger",
  "stream",
  "agent",
  "dataset",
  "model",
  "workspace",
  "application",
  "view",
  "page",
  "library_item",
  "translation",
  "checklist",
] as const;

/** A kind's heading in the reader's language. Spelled out so the extractor
 * sees each literal. */
export function kindLabel(kind: string, t: Translator["t"]): string {
  switch (kind) {
    case "setting":
      return t("Settings");
    case "build_host":
      return t("Build hosts");
    case "module":
      return t("Modules");
    case "role":
      return t("Roles");
    case "users":
      return t("Users table");
    case "table":
      return t("Tables");
    case "db_connection":
      return t("Database connections");
    case "file_store":
      return t("File stores");
    case "code_library":
      return t("Code libraries");
    case "llm_provider":
      return t("LLM providers");
    case "mcp_server":
      return t("MCP servers");
    case "trigger":
      return t("Triggers");
    case "stream":
      return t("Streams");
    case "agent":
      return t("Agents");
    case "dataset":
      return t("Datasets");
    case "model":
      return t("Models");
    case "workspace":
      return t("Workspaces");
    case "application":
      return t("Applications");
    case "view":
      return t("Views");
    case "page":
      return t("Pages");
    case "library_item":
      return t("Library");
    case "translation":
      return t("Translations");
    case "checklist":
      return t("Checklists");
    default:
      return kind;
  }
}

/** One heading of the tree and the entries under it. */
export interface KindGroup {
  readonly kind: string;
  readonly entries: readonly RegistryEntry[];
  /** Every entry under it is this instance's alone. */
  readonly instanceOnly: boolean;
}

/** What an entry is called in the tree: a view with its application. */
export function entryLabel(entry: RegistryEntry): string {
  return entry.application ? `${entry.application} / ${entry.name}` : entry.name;
}

/** Whether all of an entry is this instance's alone. */
export function wholeInstanceOnly(entry: Pick<RegistryEntry, "instance_only">): boolean {
  return entry.instance_only?.scope === "whole";
}

/** The values of an entry that are this instance's alone, as JSON pointers;
 * empty when all of it is portable or all of it is not. */
export function instanceOnlyKeys(entry: Pick<RegistryEntry, "instance_only">): readonly string[] {
  return entry.instance_only?.scope === "keys" ? (entry.instance_only.keys ?? []) : [];
}

/** The entries matching `filter` (by name, application, path or kind
 * heading, ignoring case), grouped by kind in tree order. */
export function groupEntries(
  entries: readonly RegistryEntry[],
  filter: string,
  t: Translator["t"],
): KindGroup[] {
  const needle = filter.trim().toLowerCase();
  const matches = (e: RegistryEntry) =>
    needle === "" ||
    [entryLabel(e), e.path, kindLabel(e.kind, t)].some((s) => s.toLowerCase().includes(needle));
  const rank = (kind: string) => {
    const i = (KIND_ORDER as readonly string[]).indexOf(kind);
    return i < 0 ? KIND_ORDER.length : i;
  };
  const byKind = new Map<string, RegistryEntry[]>();
  for (const e of entries.filter(matches)) {
    const list = byKind.get(e.kind) ?? [];
    list.push(e);
    byKind.set(e.kind, list);
  }
  return [...byKind.entries()]
    .sort(([a], [b]) => rank(a) - rank(b) || a.localeCompare(b))
    .map(([kind, list]) => ({
      kind,
      entries: [...list].sort((a, b) => entryLabel(a).localeCompare(entryLabel(b))),
      instanceOnly: list.every(wholeInstanceOnly),
    }));
}

/** The kinds New can make, with where their files go. */
export interface NewKind {
  readonly kind: string;
  /** The directory under the tree, or under an application for a scoped
   * kind. */
  readonly dir: string;
  /** Inside an application. */
  readonly scoped: boolean;
}

export const NEW_KINDS: readonly NewKind[] = [
  { kind: "table", dir: "tables", scoped: false },
  { kind: "db_connection", dir: "db-connections", scoped: false },
  { kind: "file_store", dir: "file-stores", scoped: false },
  { kind: "code_library", dir: "code-libraries", scoped: false },
  { kind: "llm_provider", dir: "llm-providers", scoped: false },
  { kind: "mcp_server", dir: "mcp-servers", scoped: false },
  { kind: "trigger", dir: "triggers", scoped: false },
  { kind: "stream", dir: "streams", scoped: false },
  { kind: "agent", dir: "agents", scoped: false },
  { kind: "dataset", dir: "analytics/datasets", scoped: false },
  { kind: "workspace", dir: "analytics/workspaces", scoped: false },
  { kind: "checklist", dir: "checklists", scoped: false },
  { kind: "application", dir: "applications", scoped: false },
  { kind: "view", dir: "views", scoped: true },
  { kind: "page", dir: "pages", scoped: true },
  { kind: "library_item", dir: "library", scoped: true },
];

/** An entity's name as one segment of a tree path: the server's
 * `path_segment`, which writes `/`, `\`, `%`, `:`, `*`, `?`, `"`, `<`, `>`,
 * `|`, a control character and a leading `.` as `%XX`. */
export function pathSegment(name: string): string {
  let out = "";
  let i = 0;
  for (const ch of name) {
    const code = ch.codePointAt(0) ?? 0;
    const escape =
      '/\\%:*?"<>|'.includes(ch) || code < 0x20 || (code >= 0x7f && code < 0xa0) || (i === 0 && ch === ".");
    if (escape) {
      for (const byte of new TextEncoder().encode(ch)) {
        out += `%${byte.toString(16).toUpperCase().padStart(2, "0")}`;
      }
    } else {
      out += ch;
    }
    i += 1;
  }
  return out === "" ? "%00" : out;
}

/** Where a new entity of `kind` called `name` goes (in `application`, for a
 * kind inside one), or `null` while something it needs is missing. */
export function newEntryPath(kind: NewKind, name: string, application: string): string | null {
  const trimmed = name.trim();
  if (trimmed === "") return null;
  if (kind.kind === "application") return `applications/${pathSegment(trimmed)}/application.json`;
  if (kind.scoped) {
    if (application.trim() === "") return null;
    return `applications/${pathSegment(application.trim())}/${kind.dir}/${pathSegment(trimmed)}.json`;
  }
  return `${kind.dir}/${pathSegment(trimmed)}.json`;
}

/** The JSON a new entity starts from: its name, which is all every kind
 * shares; the save's review says what else it needs. */
export function newEntryValue(kind: NewKind, name: string): Record<string, unknown> {
  const trimmed = name.trim();
  return kind.kind === "application" ? { name: trimmed, subdomain: trimmed } : { name: trimmed };
}

/** The editor's text for a value: two-space JSON, as the tree renders it. */
export function draftText(value: unknown): string {
  return `${JSON.stringify(value, null, 2)}\n`;
}

/** What the editor holds, parsed. */
export type ParsedDraft = { ok: true; value: unknown } | { ok: false; error: string };

export function parseDraft(text: string): ParsedDraft {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/** The review's headings, most dangerous first. */
export const CLASS_ORDER = [
  "unsupported",
  "data_destructive",
  "can_fail",
  "metadata_delete",
  "safe",
] as const;

export function classLabel(cls: string, t: Translator["t"]): string {
  switch (cls) {
    case "unsupported":
      return t("Not possible");
    case "data_destructive":
      return t("Loses data");
    case "can_fail":
      return t("Checked against the data");
    case "metadata_delete":
      return t("Deletes a definition");
    case "safe":
      return t("Safe");
    default:
      return cls;
  }
}

/** The operations of a plan under their headings, in order, leaving out
 * empty headings. */
export function groupOps(ops: readonly PlannedOp[]): { cls: string; ops: PlannedOp[] }[] {
  return CLASS_ORDER.map((cls) => ({ cls, ops: ops.filter((op) => op.class === cls) })).filter(
    (g) => g.ops.length > 0,
  );
}

/** Whether an operation's tick can be changed: an unsupported one is never
 * applied, and everything else is the admin's choice. */
export function tickable(op: PlannedOp): boolean {
  return op.class !== "unsupported";
}

/** The ticks a plan is offered with: each operation's default. */
export function defaultTicks(ops: readonly PlannedOp[]): Set<string> {
  return new Set(ops.filter((op) => op.ticked).map((op) => op.id));
}

/** The word the admin types to confirm a save that loses data — the entry's
 * name — or `null` when nothing ticked loses data. */
export function confirmationWord(
  ops: readonly PlannedOp[],
  ticked: ReadonlySet<string>,
  name: string,
): string | null {
  return ops.some((op) => op.class === "data_destructive" && ticked.has(op.id)) ? name : null;
}

/** Whether the review's Save may be pressed. */
export function canConfirm(
  plan: Pick<PlanRegistrySaveResponse, "operations" | "blockers">,
  ticked: ReadonlySet<string>,
  typed: string,
  name: string,
): boolean {
  if (plan.blockers.length > 0) return false;
  const word = confirmationWord(plan.operations, ticked, name);
  return word === null || typed.trim() === word;
}

/** A difference between what was typed and what is stored, as one line. */
export function changeLine(change: RegistryReceipt["changes"][number]): string {
  const show = (v: unknown) => (v === undefined || v === null ? "—" : JSON.stringify(v));
  return `${change.path}: ${show(change.before)} → ${show(change.after)}`;
}
