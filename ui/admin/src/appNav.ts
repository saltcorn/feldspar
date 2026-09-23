// The Applications section of the sidebar, as data.
//
// The sidebar has two halves. **Data Layer** is the installation's shared
// furniture — tables, triggers, files, models, agents, users, settings — and is
// the same for everyone. **Applications** is about *one* application at a time:
// the admin picks the current one in the sidebar, and under the picker come the
// links for working on it, which depend on its framework the way the actions on
// its row in the applications list do. This module decides which links those are
// and when one is "here"; `AppSidebar.tsx` draws them.

import { ideUrl } from "./App";
import type { ListAgentsResponse, ListApplicationsResponse } from "./client";

type AppItem = ListApplicationsResponse[number];
type AgentItem = ListAgentsResponse[number];

/** The trait a builder agent carries, and its key naming the subdomain it checks
 * and builds (`sc_app::TRAIT_CODING` / `TRAIT_CFG_APPLICATION`). */
const BUILD_TRAIT = "coding";
const BUILD_TRAIT_APPLICATION = "application";

/** One link under the application picker. */
export type AppNavLink = {
  id:
  | "edit-code"
  | "update-client"
  | "build"
  | "chat"
  | "views"
  | "pages"
  | "library"
  | "translations"
  | "settings";
  label: string;
  /** Where it goes. Absent for the two links that *do* something (build, update
   * client) rather than go somewhere. */
  href?: string;
  /** Opens in a new tab (the IDE is a separate page). */
  external?: boolean;
  /** Route prefixes that light it up. */
  matches: string[];
};

/** The coding agent that builds `app`, by name, if there is one.
 *
 * Found the way the server finds it when it deletes one: by a `coding` trait
 * naming the application, not by the name. An agent an admin made under the
 * conventional name for another job is not this application's builder, and one
 * they renamed still is. */
export function builderAgentFor(
  app: Pick<AppItem, "subdomain">,
  agents: Pick<AgentItem, "name" | "traits">[],
): string | null {
  const subdomain = app.subdomain.trim();
  const agent = agents.find((a) =>
    a.traits.some((t) => {
      if (t.trait !== BUILD_TRAIT) return false;
      const config = t.config as Record<string, unknown> | null;
      const target = config?.[BUILD_TRAIT_APPLICATION];
      return typeof target === "string" && target.trim() === subdomain;
    }),
  );
  return agent?.name ?? null;
}

/**
 * The links for working on `app`, in sidebar order.
 *
 * - An application that is **built** from a source tree (React, code) gets the
 *   loop an admin works in: edit the code, update the generated client, build.
 * - One that is **constructed** from views and pages (Saltcorn UI) gets those,
 *   and the library its views and pages place.
 * - Either gets a new chat with its coding agent, when it has one.
 * - **Settings is always there, and always last**; deleting is not a sidebar
 *   action at all — it stays on the applications list, beside the rest of the
 *   rows, where it cannot be clicked on the way to something else.
 */
export function appNavLinks(
  app: Pick<AppItem, "id" | "builds" | "has_views" | "source">,
  agent: string | null,
): AppNavLink[] {
  const base = `/applications/${encodeURIComponent(app.id)}`;
  const links: AppNavLink[] = [];
  if (app.builds) {
    if (app.source) {
      links.push({
        id: "edit-code",
        label: "Edit code",
        href: ideUrl(app.source.store),
        external: true,
        matches: [],
      });
    }
    links.push({ id: "update-client", label: "Update client", matches: [] });
    links.push({ id: "build", label: "Build", matches: [] });
  }
  if (app.has_views) {
    links.push({ id: "views", label: "Views", href: `#${base}/views`, matches: [`${base}/views`] });
    links.push({ id: "pages", label: "Pages", href: `#${base}/pages`, matches: [`${base}/pages`] });
    links.push({
      id: "library",
      label: "Library",
      href: `#${base}/library`,
      matches: [`${base}/library`],
    });
  }
  if (agent) {
    links.push({
      id: "chat",
      label: "Coding agent",
      href: `#/agents/${encodeURIComponent(agent)}/chat`,
      // A new chat is a thing to start, not a place to be: the chat screen
      // belongs to Agents, which is what lights up once it is open.
      matches: [],
    });
  }
  links.push({
    id: "settings",
    label: "Settings",
    href: `#${base}/edit`,
    matches: [`${base}/edit`, `${base}/app-settings`],
  });
  return links;
}

/** Whether a link is the current screen. A prefix must end at a path segment, so
 * `…/views` lights up for a view's editor but not for a sibling that merely
 * starts with the same letters. */
export function linkActive(link: Pick<AppNavLink, "matches">, path: string): boolean {
  return link.matches.some((prefix) => path === prefix || path.startsWith(`${prefix}/`));
}

/** The application a route is about, if it is about one — which makes it the
 * current application, so following a link to an app's screen from anywhere
 * (the list, a notice) brings the sidebar along with it. */
export function appIdFromRoute(path: string): string | null {
  const match = path.match(/^\/applications\/([^/]+)\//);
  return match ? decodeURIComponent(match[1]) : null;
}

/** Whether a route is the applications list itself (or creating one), which is
 * what the picker's own "All applications" entry stands for. */
export function onApplicationsList(path: string): boolean {
  return path === "/applications" || path === "/applications/new";
}
