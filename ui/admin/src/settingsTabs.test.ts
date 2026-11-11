import { describe, expect, it } from "vitest";

import {
  activeTabId,
  fieldsOnTab,
  formTab,
  groupsToRender,
  settingsTabLabel,
  settingsTabLinks,
  type SettingsTab,
} from "./settingsTabs";

/** The default layout, as the server sends it for a React application. */
const REACT: SettingsTab[] = [
  { id: "framework", label: "Framework", groups: ["identity", "framework", "settings", "targets"] },
  { id: "data", label: "Data", groups: ["tables", "file_stores"] },
  { id: "triggers", label: "Triggers & Streams", groups: ["triggers", "streams"] },
  { id: "api", label: "API", groups: ["apis", "row_api", "agents", "uploads", "static_dirs"] },
  { id: "security", label: "Security", groups: ["access", "guest_sessions", "csp"] },
  { id: "development", label: "Development", groups: ["coding_agent", "checklists"] },
  { id: "translations", label: "Translations", groups: ["translations"] },
];

/** Saltcorn UI's own layout. */
const SALTCORN_UI: SettingsTab[] = [
  { id: "site", label: "Site", groups: ["identity", "framework", "settings", "targets"] },
  { id: "views", label: "Views", groups: ["views"] },
  { id: "pages", label: "Pages", groups: ["pages"] },
  { id: "library", label: "Library", groups: ["library"] },
  { id: "data", label: "Data", groups: ["tables", "file_stores", "settings", "triggers", "streams"] },
  { id: "users", label: "Users & Security", groups: ["settings", "access", "guest_sessions", "csp"] },
  { id: "advanced", label: "Advanced", groups: ["apis", "row_api", "coding_agent", "translations"] },
];

describe("settingsTabLinks", () => {
  it("opens the form, a screen or the translations editor", () => {
    const links = settingsTabLinks("a 1", REACT);
    expect(links.map((l) => l.href)).toEqual([
      // The first tab is plain `edit`, as every older link says.
      "#/applications/a%201/edit",
      "#/applications/a%201/edit/data",
      "#/applications/a%201/edit/triggers",
      "#/applications/a%201/edit/api",
      "#/applications/a%201/edit/security",
      "#/applications/a%201/edit/development",
      "#/applications/a%201/translations",
    ]);
    const sc = settingsTabLinks("a1", SALTCORN_UI).map((l) => l.href);
    expect(sc.slice(0, 4)).toEqual([
      "#/applications/a1/edit",
      "#/applications/a1/views",
      "#/applications/a1/pages",
      "#/applications/a1/library",
    ]);
    // The translations editor shares Advanced, so that tab is the form.
    expect(sc[6]).toBe("#/applications/a1/edit/advanced");
  });
});

describe("activeTabId", () => {
  it("is the tab of that id, else the tab holding that screen", () => {
    expect(activeTabId(REACT, "translations")).toBe("translations");
    expect(activeTabId(SALTCORN_UI, "translations")).toBe("advanced");
    expect(activeTabId(SALTCORN_UI, "views")).toBe("views");
    expect(activeTabId(SALTCORN_UI, "users")).toBe("users");
    expect(activeTabId(REACT, "views")).toBeUndefined();
  });
});

describe("formTab", () => {
  it("is the tab asked for, or the first when that is a screen or unknown", () => {
    expect(formTab(REACT, "security")?.id).toBe("security");
    expect(formTab(REACT, undefined)?.id).toBe("framework");
    expect(formTab(REACT, "nope")?.id).toBe("framework");
    expect(formTab(REACT, "translations")?.id).toBe("framework");
    expect(formTab(SALTCORN_UI, "views")?.id).toBe("site");
    expect(formTab(SALTCORN_UI, "advanced")?.id).toBe("advanced");
  });
});

describe("fieldsOnTab", () => {
  const spec = [
    { name: "site_name", tab: "site" },
    { name: "login_form", tab: "users" },
    { name: "search", tab: "data" },
    { name: "runtime_key" },
    { name: "odd", tab: "nowhere" },
    { name: "screen", tab: "views" },
  ];
  it("places a setting by its tab, and one with no usable tab with the framework", () => {
    const names = (tab: string) => fieldsOnTab(spec, SALTCORN_UI, tab).map((f) => f.name);
    expect(names("site")).toEqual(["site_name", "runtime_key", "odd", "screen"]);
    expect(names("users")).toEqual(["login_form"]);
    expect(names("data")).toEqual(["search"]);
    expect(names("advanced")).toEqual([]);
  });

  it("shows a tab's settings last when the tab does not say where", () => {
    const mobile: SettingsTab = { id: "mobile", label: "Mobile", groups: ["targets"] };
    expect(groupsToRender(mobile, true)).toEqual(["targets", "settings"]);
    expect(groupsToRender(mobile, false)).toEqual(["targets"]);
    expect(groupsToRender(REACT[0], true)).toEqual(REACT[0].groups);
  });
});

describe("settingsTabLabel", () => {
  // A stand-in translator that marks what it was asked to translate.
  const t = (text: string) => `«${text}»`;

  it("translates the labels the server and the bundled frameworks declare", () => {
    for (const tab of [...REACT, ...SALTCORN_UI]) {
      expect(settingsTabLabel(tab.label, t)).toBe(`«${tab.label}»`);
    }
  });

  it("shows a framework's own label as declared", () => {
    expect(settingsTabLabel("Mobile apps", t)).toBe("Mobile apps");
  });
});
