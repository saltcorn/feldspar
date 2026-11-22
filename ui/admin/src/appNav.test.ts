/**
 * The Applications section of the sidebar: which links the current application
 * gets, and which of them is "here".
 */

import { describe, expect, it } from "vitest";

import {
  appIdFromRoute,
  appNavLinks,
  builderAgentFor,
  codingAgentTarget,
  linkActive,
  selectedCodingAgent,
  storedCodingAgent,
  withCodingAgent,
  linkKey,
  onApplicationsList,
} from "./appNav";

/** A Claude Code session can start for it. */
const claudeOk = { available: true, reason: null };

const react = {
  id: "a1",
  name: "todo",
  subdomain: "todo",
  framework: { name: "react" },
  builds: true,
  has_views: false,
  source: { store: "code", path: "todo" },
  targets: [],
  claude: claudeOk,
  attributes: {},
};

const saltcornUi = {
  id: "a2",
  name: "crm",
  subdomain: "crm",
  framework: { name: "saltcorn" },
  builds: false,
  has_views: true,
  source: null,
  targets: [],
  claude: { available: false, reason: "crm has no source directory" },
  attributes: {},
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
      "Plan & build",
      "Edit code",
      "Build",
      "Coding agent",
      "Checklists",
      "Application settings",
    ]);
    const edit = links[2];
    expect(edit.href).toBe("/ide/?store=code");
    expect(edit.external).toBe(true);
    // Build does something rather than going somewhere, and it brings the
    // generated client up to date itself — there is no separate link for that.
    expect(links[3].href).toBeUndefined();
    expect(links.map((l) => l.id)).not.toContain("update-client");
  });

  it("offers Plan & build right after the application link, for React only", () => {
    const links = appNavLinks(react, null, admin);
    expect(links[1].id).toBe("construct");
    expect(links[1].href).toBe("#/applications/a1/construct");
    expect(linkActive(links[1], "/applications/a1/construct")).toBe(true);
    expect(appNavLinks(saltcornUi, null, admin).map((l) => l.id)).not.toContain("construct");
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

  /** One link, whichever coding agent it opens: Claude Code and the built-in
   * agent used to be two rows of a sidebar that is already tall. */
  it("has one Coding agent link, whatever it opens", () => {
    for (const app of [react, saltcornUi]) {
      for (const agent of [null, "build-todo"]) {
        const coding = appNavLinks(app, agent, admin).filter((l) => l.label === "Coding agent");
        expect(coding.length).toBeLessThanOrEqual(1);
      }
    }
    expect(appNavLinks(react, "build-todo", admin).map((l) => l.label)).toEqual([
      "Application link",
      "Plan & build",
      "Edit code",
      "Build",
      "Coding agent",
      "Checklists",
      "Application settings",
    ]);
  });

  it("opens the built-in coding agent's chat when that is chosen", () => {
    const chosen = { ...react, attributes: { coding_agent: "agent" } };
    const link = appNavLinks(chosen, "build-todo", admin).find((l) => l.label === "Coding agent")!;
    expect(link.id).toBe("chat");
    expect(link.href).toBe("#/agents/build-todo/chat");
    expect(link.external).toBeUndefined();
  });

  it("opens the built-in agent for an application Claude Code cannot work on", () => {
    // No directory, so no Claude Code; its agent is all there is.
    const link = appNavLinks(saltcornUi, "build-crm", admin).find(
      (l) => l.label === "Coding agent",
    )!;
    expect(link.href).toBe("#/agents/build-crm/chat");
  });

  it("gives an application with no framework edit code but nothing to build", () => {
    const none = {
      id: "a3",
      name: "landing",
      subdomain: "landing",
      framework: { name: "none" },
      builds: false,
      has_views: false,
      source: { store: "site", path: "public" },
      targets: [],
      claude: claudeOk,
      attributes: {},
    };
    const links = appNavLinks(none, null, admin);
    expect(links.map((l) => l.id)).toEqual([
      "app-link",
      "edit-code",
      "claude",
      "checklists",
      "settings",
    ]);
    expect(links[1].href).toBe("/ide/?store=site");
  });

  it("opens Claude Code in the IDE, in the application's directory", () => {
    const claude = appNavLinks(react, null, admin).find((l) => l.id === "claude")!;
    expect(claude.href).toBe("/ide/?store=code&claude=1&app=todo");
    expect(claude.external).toBe(true);
    expect(claude.unavailable).toBeUndefined();
  });

  it("shows the Claude Code link disabled, with the server's reason, when it cannot start", () => {
    const off = {
      ...react,
      claude: {
        available: false,
        reason: "the IDE terminal is turned off (Settings → Development)",
      },
    };
    const claude = appNavLinks(off, null, admin).find((l) => l.id === "claude")!;
    expect(claude.unavailable).toBe("the IDE terminal is turned off (Settings → Development)");
    // A name that needs escaping in the URL.
    const spaced = { ...react, name: "my shop" };
    expect(appNavLinks(spaced, null, admin).find((l) => l.id === "claude")!.href).toBe(
      "/ide/?store=code&claude=1&app=my%20shop",
    );
  });

  it("leaves a Saltcorn UI application with no coding agent without the link", () => {
    expect(appNavLinks(saltcornUi, null, admin).map((l) => l.id)).not.toContain("claude");
    expect(appNavLinks(saltcornUi, null, admin).map((l) => l.id)).not.toContain("chat");
  });

  it("gives a Saltcorn UI application views, pages and library, then settings", () => {
    expect(appNavLinks(saltcornUi, null, admin).map((l) => l.label)).toEqual([
      "Application link",
      "Views",
      "Pages",
      "Library",
      "Checklists",
      "Application settings",
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

describe("which coding agent an application's link opens", () => {
  const off = { ...react, claude: { available: false, reason: "turned off" } };
  const choose = <A extends object>(app: A, choice: string) => ({
    ...app,
    attributes: { coding_agent: choice },
  });

  it("follows the application's choice when it can", () => {
    expect(codingAgentTarget(choose(react, "claude"), "build-todo")).toBe("claude");
    expect(codingAgentTarget(choose(react, "agent"), "build-todo")).toBe("agent");
  });

  it("prefers Claude Code where it can start, when nothing is chosen", () => {
    expect(codingAgentTarget(react, "build-todo")).toBe("claude");
    expect(codingAgentTarget(off, "build-todo")).toBe("agent");
  });

  /** A choice that cannot be followed is not a dead link: Claude Code
   * switched off, or the agent deleted, falls back to the other. */
  it("falls back to the other when the chosen one is not there", () => {
    expect(codingAgentTarget(choose(off, "claude"), "build-todo")).toBe("agent");
    expect(codingAgentTarget(choose(react, "agent"), null)).toBe("claude");
  });

  it("ignores a stored value it does not know", () => {
    expect(codingAgentTarget(choose(off, "copilot"), "build-todo")).toBe("agent");
  });

  it("keeps the disabled Claude Code link only when there is nothing else", () => {
    expect(codingAgentTarget(off, null)).toBe("claude");
    expect(codingAgentTarget(saltcornUi, null)).toBeNull();
  });
});

describe("the stored coding-agent choice", () => {
  it("is written into the attributes, keeping the rest", () => {
    const out = withCodingAgent({ check_after_build: true }, "agent");
    expect(out).toEqual({ check_after_build: true, coding_agent: "agent" });
    expect(storedCodingAgent(out)).toBe("agent");
  });

  it("replaces an earlier choice", () => {
    const out = withCodingAgent({ coding_agent: "claude" }, "agent");
    expect(storedCodingAgent(out)).toBe("agent");
  });

  it("is left as it was when there was nothing to choose from", () => {
    expect(withCodingAgent({ coding_agent: "claude" }, null)).toEqual({ coding_agent: "claude" });
  });

  /** The selector names an agent, never "Default": with nothing stored it
   * starts on Claude Code where Claude Code can start. */
  it("starts the selector on the agent the link opens", () => {
    const off = { ...react, claude: { available: false, reason: "off" } };
    expect(selectedCodingAgent(react, "build-todo")).toBe("claude");
    expect(selectedCodingAgent(off, "build-todo")).toBe("agent");
    expect(selectedCodingAgent({ ...react, attributes: { coding_agent: "agent" } }, "build-todo")).toBe(
      "agent",
    );
    // Neither can be chosen: no selector at all.
    expect(selectedCodingAgent(off, null)).toBeNull();
    expect(selectedCodingAgent(saltcornUi, null)).toBeNull();
  });

  it("reads nothing out of attributes that are not an object", () => {
    expect(storedCodingAgent(null)).toBeNull();
    expect(storedCodingAgent(["claude"])).toBeNull();
  });
});

