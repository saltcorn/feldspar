// What the home page's entities list knows, apart from how it is drawn: which
// kinds there are, where each entity is configured and what its Run link does,
// which rows a search and the type filter keep, and which are the recently
// edited ones. Here rather than in the screen so it can be tested without a
// browser (`entities.test.ts`).

import { appUrl } from "./adminAddress";
import type { ListEntitiesResponse } from "./client";

export type Entity = ListEntitiesResponse[number];

/** The kinds the server lists, in the order the type filter offers them. */
export const ENTITY_KINDS = [
  "table",
  "file_store",
  "trigger",
  "agent",
  "application",
] as const;

export type EntityKind = (typeof ENTITY_KINDS)[number];

/** A kind's name, singular for a row's badge and plural for the filter. */
export const KIND_LABELS: Record<EntityKind, { one: string; many: string }> = {
  table: { one: "Table", many: "Tables" },
  file_store: { one: "File store", many: "File stores" },
  trigger: { one: "Trigger", many: "Triggers" },
  agent: { one: "Agent", many: "Agents" },
  application: { one: "Application", many: "Applications" },
};

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
 * where an application is served, given its subdomain. */
export function runAction(
  entity: Entity,
  openApp: (subdomain: string) => string = (subdomain) => appUrl(subdomain),
): RunAction | null {
  if (!entity.runnable) return null;
  const name = encodeURIComponent(entity.name);
  switch (entity.kind) {
    case "table":
      return { type: "link", label: "Data", href: `#/tables/${name}/data`, external: false };
    case "file_store":
      return { type: "link", label: "Browse", href: `#/files/${name}`, external: false };
    case "trigger":
      return { type: "test-run", label: "Test run" };
    case "agent":
      return { type: "link", label: "Chat", href: `#/agents/${name}/chat`, external: false };
    case "application":
      return entity.subdomain
        ? { type: "link", label: "Open", href: openApp(entity.subdomain), external: true }
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
  const label = KIND_LABELS[entity.kind as EntityKind];
  const haystack = [
    entity.name,
    entity.description,
    entity.kind,
    label?.one ?? "",
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
