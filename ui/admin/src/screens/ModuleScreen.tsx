// A screen a module adds to the admin (TODO-app-constructor AC12.4).
//
// The module serves the screen's script from one of its routes: an ES module
// exporting `mount(element, host)`, which renders into `element` and may answer
// a function that unmounts it, or `{ unmount, update }`: `update(path)` is
// called when the URL moves within the screen (a tab, a phase's page), so the
// screen keeps its state rather than being mounted again. It runs in the admin's own page, under its
// header and sidebar, with the signed-in admin's session, which is what the
// module's `agents` and `apps` permissions and its routes are answered for.
//
// `host` is what the admin lends it:
// - `app`: the application, for an application's screen (else `null`);
// - `path`: what follows the screen's name in the URL (`""`, `/phases/…`);
// - `href(path)` and `navigate(path)`: a link to, and a move to, another path
//   of the same screen;
// - `routes`: where the module's routes are served;
// - `fetch(path, init)`: a request to one of them, with the session and the
//   CSRF header a POST needs;
// - `json(path, init)`: the same, answering the body as JSON and throwing the
//   route's `error` when it answers one.

import { useEffect, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { ListApplicationsResponse } from "../client";
import { useT } from "../i18n";
import { PageBody } from "../layout";
import {
  findScreen,
  moduleRoutes,
  moduleScreenPath,
  moduleScreensOf,
  routeFetch,
  routeJson,
  type ModuleScreenRef,
  type ModuleScreenRoute,
} from "../moduleScreens";

type AppItem = ListApplicationsResponse[number];

/** What a module's screen is lent (see the file's comment). */
export type ModuleScreenHost = {
  app: AppItem | null;
  path: string;
  href: (path: string) => string;
  navigate: (path: string) => void;
  routes: string;
  fetch: (path: string, init?: RequestInit) => Promise<Response>;
  json: (path: string, init?: RequestInit) => Promise<unknown>;
};

/** What `mount` may answer. */
type Mounted = void | (() => void) | { unmount?: () => void; update?: (path: string) => void };

/** A script's exports, as the admin calls them. */
type ScreenModule = {
  mount?: (element: HTMLElement, host: ModuleScreenHost) => Mounted;
};

/** The host a screen at `route` is lent. */
export function screenHost(
  screen: ModuleScreenRef,
  route: ModuleScreenRoute,
  app: AppItem | null,
): ModuleScreenHost {
  const base = moduleScreenPath(route.appId, screen.segment, screen.name);
  const routes = moduleRoutes(screen.segment);
  return {
    app,
    path: route.rest,
    href: (path) => `#${base}${path}`,
    navigate: (path) => {
      window.location.hash = `${base}${path}`;
    },
    routes,
    fetch: (path, init) => routeFetch(routes, path, init),
    json: (path, init) => routeJson(routes, path, init),
  };
}

export function ModuleScreen({ route }: { route: ModuleScreenRoute }) {
  const { t } = useT();
  const element = useRef<HTMLDivElement>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const mounted = useRef<Mounted>(undefined);

  useEffect(() => {
    let live = true;
    setProblem(null);
    setLoading(true);
    (async () => {
      const screens = moduleScreensOf(await api.listModules());
      const screen = findScreen(screens, route);
      if (!screen) {
        throw new Error(t("No installed module adds this screen."));
      }
      const app = route.appId
        ? ((await api.listApplications()).find((a) => a.id === route.appId) ?? null)
        : null;
      if (route.appId && !app) throw new Error(t("That application no longer exists."));
      const loaded = (await import(/* @vite-ignore */ screen.script_url)) as ScreenModule;
      if (typeof loaded.mount !== "function") {
        throw new Error(
          t("The module's screen script has no mount function: {url}", { url: screen.script_url }),
        );
      }
      if (!live || !element.current) return;
      setLoading(false);
      mounted.current = loaded.mount(element.current, screenHost(screen, route, app));
    })().catch((e: unknown) => {
      if (!live) return;
      setLoading(false);
      setProblem(errorMessage(e, t("The module's screen could not be shown.")));
    });
    return () => {
      live = false;
      const done = mounted.current;
      mounted.current = undefined;
      if (typeof done === "function") done();
      else if (done && typeof done.unmount === "function") done.unmount();
      if (element.current) element.current.replaceChildren();
    };
    // Mounted once per screen; a move within it is `update`'s, below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [route.appId, route.segment, route.name]);

  useEffect(() => {
    const current = mounted.current;
    if (current && typeof current === "object" && typeof current.update === "function") {
      current.update(route.rest);
    }
  }, [route.rest]);

  return (
    <PageBody>
      {problem && <Alert variant="danger">{problem}</Alert>}
      {loading && !problem && <Spinner animation="border" size="sm" />}
      <div ref={element} />
    </PageBody>
  );
}
