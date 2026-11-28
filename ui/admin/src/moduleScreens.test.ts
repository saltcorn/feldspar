/**
 * Screens modules add to the admin (TODO-app-constructor AC12.4): their routes,
 * which application's sidebar has them, and what a screen's host sends.
 */

import { afterEach, describe, expect, it, vi } from "vitest";

import { appNavLinks, linkKey } from "./appNav";
import {
  applicationScreens,
  findScreen,
  moduleScreenPath,
  moduleScreenRoute,
  moduleScreensOf,
  type ModuleScreenRef,
} from "./moduleScreens";
import { screenHost } from "./screens/ModuleScreen";

const construct: ModuleScreenRef = {
  module: "@feldspar/app-constructor",
  name: "construct",
  label: "Plan & build",
  scope: "application",
  frameworks: ["react"],
  segment: "app-constructor",
  script_url: "/_feldspar/routes/app-constructor/ui.js",
};

const overview: ModuleScreenRef = {
  ...construct,
  name: "overview",
  label: "Constructions",
  scope: "admin",
  frameworks: [],
};

const app = {
  id: "a 1",
  name: "todo",
  subdomain: "todo",
  framework: { name: "react" },
  builds: false,
  has_views: false,
  source: null,
  targets: [],
  claude: { available: false, reason: null },
  attributes: {},
};

const admin = { protocol: "http:", host: "admin.example" };

describe("a module's screens", () => {
  it("are read off the module listing with the module that adds them", () => {
    const listing = {
      modules: [
        { name: "@feldspar/app-constructor", admin_screens: [construct, overview] },
        { name: "rss", admin_screens: [] },
      ],
    } as unknown as Parameters<typeof moduleScreensOf>[0];
    expect(moduleScreensOf(listing).map((s) => `${s.module}:${s.name}`)).toEqual([
      "@feldspar/app-constructor:construct",
      "@feldspar/app-constructor:overview",
    ]);
  });

  it("route an application's screen and the admin's, with what follows the name", () => {
    const path = moduleScreenPath("a 1", "app-constructor", "construct");
    expect(path).toBe("/applications/a%201/m/app-constructor/construct");
    expect(moduleScreenRoute(`${path}/phases/p1`)).toEqual({
      appId: "a 1",
      segment: "app-constructor",
      name: "construct",
      rest: "/phases/p1",
    });
    expect(moduleScreenRoute("/m/app-constructor/overview")).toEqual({
      appId: null,
      segment: "app-constructor",
      name: "overview",
      rest: "",
    });
    expect(moduleScreenRoute("/applications/a1/construct")).toBeNull();
    const route = moduleScreenRoute("/m/app-constructor/overview")!;
    expect(findScreen([construct, overview], route)).toBe(overview);
    // An application's screen is not found at the admin's route.
    expect(findScreen([construct], { ...route, name: "construct" })).toBeNull();
  });

  it("are in the sidebar of applications on the frameworks they name, after the application link", () => {
    expect(applicationScreens([construct, overview], "react")).toEqual([construct]);
    expect(applicationScreens([construct], "vue")).toEqual([]);
    expect(applicationScreens([{ ...construct, frameworks: [] }], "vue")).toHaveLength(1);
    const links = appNavLinks(app, null, admin, [construct, { ...construct, name: "more", label: "More" }]);
    const modules = links.filter((l) => l.id === "module");
    expect(links.findIndex((l) => l.id === "module")).toBe(1);
    expect(modules.map((l) => [l.label, l.href])).toEqual([
      ["Plan & build", "#/applications/a%201/m/app-constructor/construct"],
      ["More", "#/applications/a%201/m/app-constructor/more"],
    ]);
    // Two links of one kind, two keys.
    expect(new Set(modules.map(linkKey)).size).toBe(2);
    expect(appNavLinks({ ...app, framework: { name: "vue" } }, null, admin, [construct])
      .some((l) => l.id === "module")).toBe(false);
  });
});

describe("a module screen's host", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("links within the screen and asks the module's routes with the CSRF header", async () => {
    const sent: Array<[string, RequestInit]> = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit) => {
        sent.push([url, init]);
        return url.endsWith("/bad")
          ? new Response(JSON.stringify({ error: "the construction is running" }), { status: 400 })
          : new Response(JSON.stringify({ ok: true }), { status: 200 });
      }),
    );
    vi.stubGlobal("document", { cookie: "sc_csrf=tok%3D1" });
    const route = moduleScreenRoute("/applications/a1/m/app-constructor/construct/phases")!;
    const host = screenHost(construct, route, null);
    expect(host.path).toBe("/phases");
    expect(host.routes).toBe("/_feldspar/routes/app-constructor");
    expect(host.href("/feedback")).toBe("#/applications/a1/m/app-constructor/construct/feedback");
    expect(await host.json("/plan", { method: "POST", body: "{}" })).toEqual({ ok: true });
    const [url, init] = sent[0];
    expect(url).toBe("/_feldspar/routes/app-constructor/plan");
    const headers = new Headers(init.headers);
    expect(headers.get("x-csrf-token")).toBe("tok=1");
    expect(headers.get("content-type")).toBe("application/json");
    expect(init.credentials).toBe("same-origin");
    await expect(host.json("/bad")).rejects.toThrow("the construction is running");
  });
});
