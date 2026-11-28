// Screens modules add to the admin (TODO-app-constructor AC12.4).
//
// A module declares a screen with the route of its own that serves the
// screen's script: an ES module exporting `mount(element, host)`. The admin
// imports it and mounts it in its own page, under its own header and sidebar.
// An application's screen is a link in the sidebar of the applications on the
// frameworks it names, at `#/applications/<id>/m/<segment>/<name>`; the admin's
// is opened from the module's card, at `#/m/<segment>/<name>`. Whatever follows
// the name is the screen's own (`/phases/<id>`), handed to it as `host.path`.

import type { ListModulesResponse } from "./client";

/** One screen, with the module that adds it. */
export type ModuleScreenRef = ListModulesResponse["modules"][number]["admin_screens"][number] & {
  module: string;
};

/** Every module's screens. */
export function moduleScreensOf(listing: ListModulesResponse): ModuleScreenRef[] {
  return listing.modules.flatMap((m) =>
    (m.admin_screens ?? []).map((screen) => ({ ...screen, module: m.name })),
  );
}

/** The screens in the sidebar of an application on `framework`. */
export function applicationScreens(
  screens: ModuleScreenRef[],
  framework: string,
): ModuleScreenRef[] {
  return screens.filter(
    (s) =>
      s.scope === "application" && (s.frameworks.length === 0 || s.frameworks.includes(framework)),
  );
}

/** The route of a screen: an application's when `appId` is given. */
export function moduleScreenPath(appId: string | null, segment: string, name: string): string {
  const screen = `/m/${encodeURIComponent(segment)}/${encodeURIComponent(name)}`;
  return appId ? `/applications/${encodeURIComponent(appId)}${screen}` : screen;
}

/** Where a route is, if it is a module's screen. */
export type ModuleScreenRoute = {
  appId: string | null;
  segment: string;
  name: string;
  /** What follows the screen's name: `""` or `/phases/…`. */
  rest: string;
};

/** The module screen `path` is, or `null`. */
export function moduleScreenRoute(path: string): ModuleScreenRoute | null {
  const match = path.match(/^(?:\/applications\/([^/]+))?\/m\/([^/]+)\/([^/]+)(\/.*)?$/);
  if (!match) return null;
  return {
    appId: match[1] ? decodeURIComponent(match[1]) : null,
    segment: decodeURIComponent(match[2]),
    name: decodeURIComponent(match[3]),
    rest: match[4] ?? "",
  };
}

/** The screen `route` names among `screens`, if a module still adds it. */
export function findScreen(
  screens: ModuleScreenRef[],
  route: ModuleScreenRoute,
): ModuleScreenRef | null {
  const scope = route.appId ? "application" : "admin";
  return (
    screens.find((s) => s.segment === route.segment && s.name === route.name && s.scope === scope) ??
    null
  );
}

/** Where the routes of the module whose segment is `segment` are served. */
export function moduleRoutes(segment: string): string {
  return `/_feldspar/routes/${encodeURIComponent(segment)}`;
}

const CSRF_COOKIES = ["__Host-sc_csrf", "sc_csrf"];

function csrfToken(): string | null {
  for (const name of CSRF_COOKIES) {
    const found = document.cookie
      .split("; ")
      .find((c) => c.startsWith(`${name}=`));
    if (found) return decodeURIComponent(found.slice(name.length + 1));
  }
  return null;
}

/** A request to the route `path` of the module served at `routes`, with the
 * session and the CSRF header a POST needs. */
export function routeFetch(routes: string, path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  const csrf = csrfToken();
  if (csrf) headers.set("x-csrf-token", csrf);
  if (init.body !== undefined && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  return fetch(`${routes}${path}`, { ...init, headers, credentials: "same-origin" });
}

/** The same, answering the body as JSON and throwing the route's `error` when
 * it answers one. */
export async function routeJson(routes: string, path: string, init?: RequestInit): Promise<unknown> {
  const response = await routeFetch(routes, path, init);
  const text = await response.text();
  const body = text ? (JSON.parse(text) as unknown) : null;
  if (!response.ok) {
    const said =
      body && typeof body === "object" && "error" in body ? String((body as { error: unknown }).error) : "";
    throw new Error(said || `${response.status} ${response.statusText}`);
  }
  return body;
}
