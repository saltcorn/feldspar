// What the home page's entities list knows, apart from how it is drawn: which
// kinds there are, where each entity is configured and what its Run link does,
// which rows a search and the type filter keep, and which are the recently
// edited ones. Here rather than in the screen so it can be tested without a
// browser (`entities.test.ts`).

import { appUrl } from "./adminAddress";
import type { ListEntitiesResponse } from "./client";
import { format, type Translator } from "./i18n";

export type Entity = ListEntitiesResponse[number];

/** The kinds the server lists, in the order the type filter offers them. */
export const ENTITY_KINDS = [
  "table",
  "file_store",
  "trigger",
  "stream",
  "agent",
  "application",
] as const;

export type EntityKind = (typeof ENTITY_KINDS)[number];

/** The kinds the relationship diagram draws, and so the only type buttons it
 * offers: tables with their keys, the triggers on them, and the streams that
 * fire triggers. */
export const DIAGRAM_KINDS: readonly EntityKind[] = ["table", "trigger", "stream"];

/** A kind's name, for a row's badge, in the screen's language when it passes
 * its `t`. Each label is a `t("…")` of its own so the extractor can read it. */
export function kindLabel(kind: string, t: Translator["t"] = format): string {
  switch (kind) {
    case "table":
      return t("Table");
    case "file_store":
      return t("File store");
    case "trigger":
      return t("Trigger");
    case "stream":
      return t("Stream");
    case "agent":
      return t("Agent");
    case "application":
      return t("Application");
    default:
      return kind;
  }
}

/** A kind's plural name, for the type filter, in the screen's language when it
 * passes its `t`. */
export function kindLabelPlural(kind: string, t: Translator["t"] = format): string {
  switch (kind) {
    case "table":
      return t("Tables");
    case "file_store":
      return t("File stores");
    case "trigger":
      return t("Triggers");
    case "stream":
      return t("Streams");
    case "agent":
      return t("Agents");
    case "application":
      return t("Applications");
    default:
      return kind;
  }
}

/** How many entities the "Recently edited" section shows. */
export const RECENT_COUNT = 5;

/** Where an entity's own configuration screen is. */
export function configHref(entity: Entity): string {
  const id = encodeURIComponent(entity.id);
  switch (entity.kind) {
    case "table":
      return `#/tables/${id}`;
    case "file_store":
      return `#/file-stores/${id}/edit`;
    case "trigger":
      return `#/triggers/${id}/edit`;
    case "stream":
      return `#/streams/${id}/edit`;
    case "agent":
      return `#/agents/${id}/edit`;
    case "application":
      return `#/applications/${id}/edit`;
    default:
      return "#/entities";
  }
}

/** What an entity's Run link does: go somewhere, or test run a trigger here. */
export type RunAction =
  | { type: "link"; label: string; href: string; external: boolean }
  | { type: "test-run"; label: string };

/** The Run link for an entity: an instance of it, wherever that means something
 * — a table's rows, a store's files, a trigger's test run, a chat with an
 * agent, the running application. `null` for one that cannot be run now (a
 * trigger switched off or broken; an agent that is not usable). `openApp` is
 * where an application is served, given its subdomain. The label is in the
 * screen's language when it passes its `t`. */
export function runAction(
  entity: Entity,
  openApp: (subdomain: string) => string = (subdomain) => appUrl(subdomain),
  t: Translator["t"] = format,
): RunAction | null {
  if (!entity.runnable) return null;
  const name = encodeURIComponent(entity.name);
  switch (entity.kind) {
    case "table":
      return { type: "link", label: t("Data"), href: `#/tables/${name}/data`, external: false };
    case "file_store":
      return { type: "link", label: t("Browse"), href: `#/files/${name}`, external: false };
    case "trigger":
      return { type: "test-run", label: t("Test run") };
    case "stream":
      return {
        type: "link",
        label: t("Observe"),
        href: `#/streams/${encodeURIComponent(entity.id)}/observe`,
        external: false,
      };
    case "agent":
      return { type: "link", label: t("Chat"), href: `#/agents/${name}/chat`, external: false };
    case "application":
      return entity.subdomain
        ? { type: "link", label: t("Open"), href: openApp(entity.subdomain), external: true }
        : null;
    default:
      return null;
  }
}

/** What the search and the type filter are set to. */
export type EntityFilter = {
  query: string;
  /** Kinds switched on in the filter; empty means every kind. */
  kinds: EntityKind[];
  /** Search each entity's whole definition, not only its name, description and
   * kind. Needs a listing fetched with `deep`. */
  deep: boolean;
};

/** Whether `entity` is kept by `filter`. Every word of the query must appear
 * (case-insensitively) somewhere in what is searched. */
export function matchesFilter(entity: Entity, filter: EntityFilter): boolean {
  if (filter.kinds.length > 0 && !filter.kinds.includes(entity.kind as EntityKind)) {
    return false;
  }
  const words = filter.query.toLowerCase().split(/\s+/).filter((w) => w !== "");
  if (words.length === 0) return true;

  const haystack = [
    entity.name,
    entity.description,
    entity.kind,
    kindLabel(entity.kind),
    ...(filter.deep ? [entity.definition ?? ""] : []),
  ]
    .join("\n")
    .toLowerCase();
  return words.every((word) => haystack.includes(word));
}

/** Toggle one kind in the filter, keeping the filter's order stable. */
export function toggleKind(kinds: EntityKind[], kind: EntityKind): EntityKind[] {
  return kinds.includes(kind)
    ? kinds.filter((k) => k !== kind)
    : ENTITY_KINDS.filter((k) => k === kind || kinds.includes(k));
}

/** The most recently edited entities, newest first: those with an edit time. */
export function recentlyEdited(entities: Entity[], count = RECENT_COUNT): Entity[] {
  return entities
    .filter((e) => e.updated_at)
    .sort((a, b) => Date.parse(b.updated_at ?? "") - Date.parse(a.updated_at ?? ""))
    .slice(0, count);
}

/** Every entity alphabetically by name, as Saltcorn 1 listed them, with the
 * kind (in filter order) between two of one name. */
export function sortEntities(entities: Entity[]): Entity[] {
  const rank = (kind: string) => {
    const i = ENTITY_KINDS.indexOf(kind as EntityKind);
    return i < 0 ? ENTITY_KINDS.length : i;
  };
  return [...entities].sort(
    (a, b) =>
      a.name.localeCompare(b.name, undefined, { sensitivity: "base" }) ||
      rank(a.kind) - rank(b.kind),
  );
}

/** An edit time as "3h ago", the way the recently edited rows show it. */
export function timeAgo(iso: string, now: number = Date.now()): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return iso;
  const seconds = Math.max(0, Math.round((now - then) / 1000));
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return `${months}mo ago`;
  return `${Math.floor(days / 365)}y ago`;
}
