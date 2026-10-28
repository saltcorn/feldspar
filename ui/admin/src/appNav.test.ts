/**
 * The Applications section of the sidebar: which links the current application
 * gets, and which of them is "here".
 */

import { describe, expect, it } from "vitest";

import {
  appIdFromRoute,
  appNavLinks,
  builderAgentFor,
  linkActive,
  linkKey,
  onApplicationsList,
} from "./appNav";

const react = {
  id: "a1",
  subdomain: "todo",
  builds: true,
  has_views: false,
  source: { store: "code", path: "todo" },
  targets: [],
};

const saltcornUi = {
  id: "a2",
  subdomain: "crm",
  builds: false,
  has_views: true,
  source: null,
  targets: [],
};

/** Where the admin is served; applications sit on subdomains of it. */
const admin = { protocol: "https:", host: "example.com:3032" };

const builder = {
  name: "build-todo",
  traits: [{ trait: "coding", config: { store: "code", root: "todo", application: "todo" } }],
};

describe("the current application's sidebar links", () => {
  it("offers each of the framework's build targets right after Build", () => {
    const android = {
      name: "android",
      label: "Android APK",
      readiness: { ready: true, missing: [], from_environment: [] },
    };
    const native = { ...react, targets: [android] };
    const links = appNavLinks(native, null, admin);
    const build = links.findIndex((l) => l.id === "build");
    const target = links[build + 1];
    expect(target.id).toBe("target");
    expect(target.label).toBe("Build Android APK");
    expect(target.target).toEqual(android);
    // Something to do, not somewhere to go.
    expect(target.href).toBeUndefined();
    // One key per target, so two targets are two links.
    expect(linkKey(target)).toBe("target:android");
    expect(linkKey(links[build])).toBe("build");
  });

  it("gives a built application edit code and build, then settings", () => {
    const links = appNavLinks(react, null, admin);
    expect(links.map((l) => l.label)).toEqual([
      "Application link",
      "Edit code",
      "Build",
      "Checklists",
      "Settings",
    ]);
    const edit = links[1];
    expect(edit.href).toBe("/ide/?store=code");
    expect(edit.external).toBe(true);
    // Build does something rather than going somewhere, and it brings the
    // generated client up to date itself — there is no separate link for that.
    expect(links[2].href).toBeUndefined();
    expect(links.map((l) => l.id)).not.toContain("update-client");
  });

  it("starts with a link out to the application on its own subdomain", () => {
    for (const app of [react, saltcornUi]) {
      for (const agent of [null, "x"]) {
        const first = appNavLinks(app, agent, admin)[0];
        expect(first.id).toBe("app-link");
        expect(first.label).toBe("Application link");
        expect(first.href).toBe(`https://${app.subdomain}.example.com:3032`);
        expect(first.external).toBe(true);
        expect(linkActive(first, `/applications/${app.id}/edit`)).toBe(false);
      }
    }
  });

  it("adds a link to the coding agent when the application has one", () => {
    const labels = appNavLinks(react, "build-todo", admin).map((l) => l.label);
    expect(labels).toEqual([
      "Application link",
      "Edit code",
      "Build",
      "Coding agent",
      "Checklists",
      "Settings",
    ]);
    expect(appNavLinks(react, "build-todo", admin)[3].href).toBe("#/agents/build-todo/chat");
  });

  it("gives an application with no framework edit code but nothing to build", () => {
    const none = {
      id: "a3",
      subdomain: "landing",
      builds: false,
      has_views: false,
      source: { store: "site", path: "public" },
      targets: [],
    };
    const links = appNavLinks(none, null, admin);
    expect(links.map((l) => l.id)).toEqual(["app-link", "edit-code", "checklists", "settings"]);
    expect(links[1].href).toBe("/ide/?store=site");
  });

  it("gives a Saltcorn UI application views, pages and library, then settings", () => {
    expect(appNavLinks(saltcornUi, null, admin).map((l) => l.label)).toEqual([
      "Application link",
      "Views",
      "Pages",
      "Library",
      "Checklists",
      "Settings",
    ]);
  });

  it("always ends with settings, and never offers a delete", () => {
    for (const app of [react, saltcornUi]) {
      for (const agent of [null, "x"]) {
        const links = appNavLinks(app, agent, admin);
        expect(links[links.length - 1].id).toBe("settings");
        expect(links.map((l) => l.label.toLowerCase())).not.toContain("delete");
      }
    }
  });

  it("lights up settings for both of an application's settings tabs", () => {
    const settings = appNavLinks(saltcornUi, null, admin).find((l) => l.id === "settings")!;
    expect(linkActive(settings, "/applications/a2/edit")).toBe(true);
    expect(linkActive(settings, "/applications/a2/app-settings")).toBe(true);
    expect(linkActive(settings, "/applications/a1/edit")).toBe(false);
  });

  it("gives every application its checklists, just before settings", () => {
    for (const app of [react, saltcornUi]) {
      const links = appNavLinks(app, null, admin);
      const checklists = links[links.length - 2];
      expect(checklists.id).toBe("checklists");
      expect(checklists.href).toBe(`#/applications/${app.id}/checklists`);
      expect(linkActive(checklists, `/applications/${app.id}/checklists`)).toBe(true);
      expect(linkActive(checklists, `/applications/${app.id}/edit`)).toBe(false);
    }
  });

  it("lights up views for a view's editor, but not pages", () => {
    const links = appNavLinks(saltcornUi, null, admin);
    const views = links.find((l) => l.id === "views")!;
    const pages = links.find((l) => l.id === "pages")!;
    expect(linkActive(views, "/applications/a2/views/orders_list")).toBe(true);
    expect(linkActive(pages, "/applications/a2/views/orders_list")).toBe(false);
    expect(linkActive(pages, "/applications/a2/pages/home/properties")).toBe(true);
  });
});

describe("finding an application's coding agent", () => {
  it("is the agent whose coding trait names the application's subdomain", () => {
    expect(builderAgentFor(react, [builder])).toBe("build-todo");
    expect(builderAgentFor(saltcornUi, [builder])).toBeNull();
    // A coding agent over the same source that builds nothing is not its builder,
    // and neither is the trait that used to build applications.
    const reader = {
      name: "reader",
      traits: [{ trait: "coding", config: { store: "code", root: "todo" } }],
    };
    const old = {
      name: "old",
      traits: [{ trait: "build_application", config: { application: "todo" } }],
    };
    expect(builderAgentFor(react, [reader, old])).toBeNull();
  });

  it("goes by the trait, not the conventional name", () => {
    const impostor = { name: "build-todo", traits: [{ trait: "subagent", config: {} }] };
    const renamed = { ...builder, name: "todo-coder" };
    expect(builderAgentFor(react, [impostor])).toBeNull();
    expect(builderAgentFor(react, [impostor, renamed])).toBe("todo-coder");
  });
});

describe("which application a route is about", () => {
  it("is the one in an application's own screens", () => {
    expect(appIdFromRoute("/applications/a1/edit")).toBe("a1");
    expect(appIdFromRoute("/applications/a%20b/views/x")).toBe("a b");
  });

  it("is none for the list or the new-application form", () => {
    expect(appIdFromRoute("/applications")).toBeNull();
    expect(appIdFromRoute("/applications/new")).toBeNull();
    expect(appIdFromRoute("/tables")).toBeNull();
    expect(onApplicationsList("/applications")).toBe(true);
    expect(onApplicationsList("/applications/new")).toBe(true);
    expect(onApplicationsList("/applications/a1/edit")).toBe(false);
  });
});
