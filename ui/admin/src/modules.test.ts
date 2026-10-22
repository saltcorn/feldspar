/**
 * The Modules tab's model.
 *
 * Three things here are worth pinning without a browser. The **install form**,
 * because its four combinations take different things and the failure modes are
 * quiet ones: a relative path installs something the server will resolve against
 * its own modules directory rather than against wherever the admin was standing,
 * a `==1.0` names no distribution, and a server without the toolchain for the
 * language that was picked cannot install anything at all — which an admin
 * should be told before filling in a form, not by a failed install. The two
 * languages fail *separately*, and a server that has npm and no pip must say so
 * about Python and not about JavaScript.
 *
 * The **reading of a module**, because everything interesting about one is
 * something that went wrong: it did not load, it loaded but an action of its
 * name was taken, it supplies other entity types this version ignores. Each of
 * those is a sentence the admin acts on.
 *
 * And **what a module may reach**, where the two languages differ in kind rather
 * than in degree (§10): a JavaScript module has an allow-list, and a Python one
 * has no sandbox at all. A screen that rendered the same four empty boxes for
 * both would be claiming a permission model that does not exist, so the model
 * says which module has one.
 *
 * The **bundled catalog** is the fourth, and it is here for one sentence: a card
 * whose button grants a permission has to say what it grants, beside the button,
 * before it is pressed. That is the entire basis on which a one-click install is
 * allowed to widen anything, so the sentence is asserted rather than reviewed.
 */

import { describe, expect, it } from "vitest";

import {
  ALL_TOOLCHAINS,
  CLOSED_PERMISSIONS,
  EMPTY_INSTALL,
  INSTALL_CHOICES,
  NO_SANDBOX,
  actionNames,
  anyHost,
  bundledBlocked,
  bundledPermissions,
  bundledSubtitle,
  catalogOrder,
  grantSentence,
  installsSentence,
  choiceFor,
  choiceValue,
  configValues,
  distributionName,
  hasPermissions,
  installBlocked,
  isConfigurable,
  languageLabel,
  locationLabel,
  locationPlaceholder,
  moduleStatus,
  moduleSubtitle,
  isClosed,
  modulePermissions,
  parsePermissionText,
  permissionProblem,
  permissionProblems,
  permissionSummary,
  permissionText,
  serviceState,
  suppliedSummary,
  toolchainMissing,
  toolchainSentence,
  unsupportedSentence,
  type BundledModule,
  type Module,
  type Toolchains,
} from "./modules";

/** A loaded module with one action, as `listModules` sends it. */
function module_(overrides: Partial<Module> = {}): Module {
  return {
    id: "3f0d2c1e-0000-4000-8000-000000000001",
    name: "@saltcorn/mqtt",
    language: "javascript",
    source: "npm",
    location: "@saltcorn/mqtt",
    version: "0.2.0",
    configuration: { broker_url: "mqtt://localhost", password: "•••••" },
    permissions: { net: [], read: [], write: [], env: [] },
    config_spec: [],
    actions: [{ name: "mqtt_publish", description: "Publish a message", config_spec: [] }],
    functions: [],
    table_providers: [],
    tables: [],
    services: [],
    webhooks: [],
    csp_sources: [],
    model_providers: [],
    stream_providers: [],
    view_patterns: [],
    unsupported: [],
    issues: [],
    loaded: true,
    api_version: 1,
    ...overrides,
  };
}

/** The same, written in the other language: a distribution from PyPI, with the
 * permission column the server still stores and nothing enforces (§10). */
function pythonModule(overrides: Partial<Module> = {}): Module {
  return module_({
    name: "saltcorn-mqtt",
    language: "python",
    source: "pypi",
    location: "saltcorn-mqtt",
    ...overrides,
  });
}

/** One entry of the bundled catalog, as `listModules` sends it. */
function bundled(overrides: Partial<BundledModule> = {}): BundledModule {
  return {
    id: "rss",
    name: "@feldspar/rss",
    language: "javascript",
    title: "RSS feeds",
    description: "Read an RSS or Atom feed as a table.",
    supplies: ['A table provider, "RSS feed".'],
    installs: ["rss-parser"],
    tables: [],
    services: [],
    webhooks: [],
    permissions: { net: ["*"], read: [], write: [], env: [] },
    installed: false,
    ...overrides,
  };
}

/** A server with one toolchain and not the other. */
function tools(overrides: Partial<Toolchains> = {}): Toolchains {
  return { ...ALL_TOOLCHAINS, ...overrides };
}

describe("the install form", () => {
  it("offers one entry per language-and-registry pair, and no invalid one", () => {
    // `local` is two of the four, so neither half of a choice identifies it and
    // the select's value is the pair.
    expect(INSTALL_CHOICES.map(choiceValue)).toEqual([
      "javascript:npm",
      "javascript:local",
      "python:pypi",
      "python:local",
    ]);
    // What the server refuses — a Python module from npm — is not offered.
    expect(INSTALL_CHOICES.some((c) => c.language === "python" && c.source === "npm")).toBe(false);
    expect(choiceFor("python:pypi")).toEqual({
      language: "python",
      source: "pypi",
      label: "Python — PyPI distribution",
    });
    // A value that cannot come from this select falls back rather than sticking.
    expect(choiceFor("elixir:hex")).toBe(INSTALL_CHOICES[0]);
  });

  it("asks for the thing the chosen source actually takes", () => {
    expect(locationLabel({ language: "javascript", source: "npm" })).toMatch(/package/i);
    expect(locationPlaceholder({ language: "javascript", source: "npm" })).toBe("@saltcorn/mqtt");
    expect(locationLabel({ language: "python", source: "pypi" })).toMatch(/distribution/i);
    expect(locationPlaceholder({ language: "python", source: "pypi" })).toMatch(/saltcorn-mqtt/);
    // A directory is a directory in either language, and it is on the *server*.
    expect(locationLabel({ language: "python", source: "local" })).toMatch(/directory/i);
    expect(locationPlaceholder({ language: "python", source: "local" })).toMatch(/^\//);
    expect(locationPlaceholder({ language: "javascript", source: "local" })).toMatch(/^\//);
  });

  it("starts empty, on JavaScript from npm", () => {
    expect(EMPTY_INSTALL).toEqual({ language: "javascript", source: "npm", location: "" });
    expect(installBlocked(EMPTY_INSTALL, ALL_TOOLCHAINS)).toMatch(/npm package/i);
    expect(
      installBlocked({ language: "python", source: "pypi", location: "" }, ALL_TOOLCHAINS),
    ).toMatch(/PyPI/);
  });

  it("refuses a relative local path in either language, because the server would resolve it elsewhere", () => {
    for (const language of ["javascript", "python"] as const) {
      expect(
        installBlocked({ language, source: "local", location: "../mqtt" }, ALL_TOOLCHAINS),
      ).toMatch(/absolute path/i);
      expect(
        installBlocked({ language, source: "local", location: "/srv/mqtt" }, ALL_TOOLCHAINS),
      ).toBeNull();
    }
  });

  it("reads the distribution a specifier names, and refuses one that names none", () => {
    // pip's own grammar, applied in front of the form: everything from the first
    // character that cannot be in a name is a version, an extra or a marker.
    expect(distributionName("httpx")).toBe("httpx");
    expect(distributionName("httpx>=0.27")).toBe("httpx");
    expect(distributionName("httpx[http2]>=0.27")).toBe("httpx");
    expect(distributionName("saltcorn-mqtt==0.2.0")).toBe("saltcorn-mqtt");
    expect(distributionName("==1.0")).toBeNull();
    expect(
      installBlocked({ language: "python", source: "pypi", location: "==1.0" }, ALL_TOOLCHAINS),
    ).toMatch(/does not start with a distribution/);
    expect(
      installBlocked(
        { language: "python", source: "pypi", location: "saltcorn-mqtt>=0.2" },
        ALL_TOOLCHAINS,
      ),
    ).toBeNull();
  });

  it("blocks each language on its own toolchain, and neither on the other's", () => {
    // No npm: JavaScript is blocked whatever is typed, and Python is not.
    const noNpm = tools({ npm: false, node: false });
    expect(
      installBlocked(
        { language: "javascript", source: "npm", location: "@saltcorn/mqtt" },
        noNpm,
      ),
    ).toMatch(/no npm/i);
    expect(installBlocked(EMPTY_INSTALL, noNpm)).toMatch(/no npm/i);
    expect(
      installBlocked({ language: "python", source: "pypi", location: "saltcorn-mqtt" }, noNpm),
    ).toBeNull();

    // No interpreter, and an interpreter without pip, are different repairs and
    // are said differently.
    const noPython = tools({ python: false, pip: false });
    expect(toolchainMissing("python", noPython)).toMatch(/no Python interpreter/);
    expect(toolchainMissing("python", noPython)).toMatch(/--python-bin/);
    expect(toolchainMissing("python", tools({ pip: false }))).toMatch(/no pip/);
    expect(toolchainMissing("javascript", noPython)).toBeNull();
    expect(
      installBlocked({ language: "python", source: "pypi", location: "httpx" }, noPython),
    ).toMatch(/no Python interpreter/);
    // …and the toolchain's absence wins over an empty box, because it is the one
    // the admin cannot fix from this screen.
    expect(installBlocked({ language: "python", source: "local", location: "" }, noPython)).toMatch(
      /no Python interpreter/,
    );
  });

  it("blocks JavaScript on an npm that is present and too old, and says which", () => {
    // A different repair from no npm at all: the toolchain is there, and the
    // sentence has to say that a *newer* one is what is wanted — an admin told
    // to install Node.js would find it already installed.
    const old = tools({ npmTooOld: { version: "9.2.0", minimum: "9.3.0" } });
    const blocked = installBlocked(
      { language: "javascript", source: "npm", location: "@saltcorn/mqtt" },
      old,
    );
    expect(blocked).toMatch(/9\.2\.0/);
    expect(blocked).toMatch(/9\.3\.0/);
    expect(blocked).toMatch(/NodeSource/);
    expect(blocked).not.toMatch(/no npm/i);
    // Only JavaScript: pip is unaffected by npm's age.
    expect(toolchainMissing("python", old)).toBeNull();
    expect(toolchainSentence(old)).toMatch(/too old/);
    // And the bundled cards, which are the click this was reported from: the
    // RSS card cannot be installed either, for the same stated reason.
    expect(bundledBlocked(bundled(), old)).toMatch(/9\.3\.0/);
    expect(bundledBlocked(bundled({ language: "python" }), old)).toBeNull();
  });

  it("says what this server can install with, before a name is typed", () => {
    expect(toolchainSentence(ALL_TOOLCHAINS)).toBe(
      "On this server, npm installs a JavaScript module; pip installs a Python one.",
    );
    expect(toolchainSentence(tools({ npm: false }))).toMatch(/no npm/);
    expect(toolchainSentence(tools({ pip: false }))).toMatch(/no pip/);
    expect(toolchainSentence(tools({ python: false, pip: false }))).toMatch(
      /no Python interpreter/,
    );
  });

  it("lets a filled-in package through in either language", () => {
    expect(
      installBlocked(
        { language: "javascript", source: "npm", location: "@saltcorn/mqtt@0.2.0" },
        ALL_TOOLCHAINS,
      ),
    ).toBeNull();
    expect(
      installBlocked(
        { language: "python", source: "pypi", location: "saltcorn-mqtt" },
        ALL_TOOLCHAINS,
      ),
    ).toBeNull();
  });
});

describe("how an installed module reads", () => {
  it("summarises what is installed, which language it is, and where it came from", () => {
    expect(moduleSubtitle(module_())).toBe("v0.2.0 · JavaScript · npm");
    expect(
      moduleSubtitle(module_({ source: "local", location: "/srv/checkouts/mqtt" })),
    ).toBe("v0.2.0 · JavaScript · /srv/checkouts/mqtt");
    // The language is on every card, because which host loads a module decides
    // how it is installed, reloaded and — below — what it may reach.
    expect(moduleSubtitle(pythonModule())).toBe("v0.2.0 · Python · pypi");
    expect(moduleSubtitle(module_({ version: null }))).toMatch(/not installed/);
    // A language this SPA has not heard of reads as the sandboxed one rather
    // than as a blank.
    expect(languageLabel("rust")).toBe("JavaScript");
  });

  it("counts the actions it supplies", () => {
    expect(moduleStatus(module_())).toEqual({ label: "1 action", tone: "green" });
    expect(actionNames(module_())).toEqual(["mqtt_publish"]);
    const two = module_({
      actions: [
        { name: "a", description: "", config_spec: [] },
        { name: "b", description: "", config_spec: [] },
      ],
    });
    expect(moduleStatus(two).label).toBe("2 actions");
  });

  it("counts the table providers it supplies, and a module that only has those", () => {
    // `@saltcorn/rss` has no actions at all, and "0 actions" would read as "this
    // module does nothing" about a module that serves a whole table.
    const rss = module_({ actions: [], table_providers: ["RSS feed"] });
    expect(moduleStatus(rss)).toEqual({ label: "1 table provider", tone: "green" });
    const both = module_({ table_providers: ["RSS feed", "Atom feed"] });
    expect(moduleStatus(both).label).toBe("1 action, 2 table providers");
    // And a module that loaded and supplies nothing this version reads still
    // says it loaded, rather than claiming a count of nothing.
    expect(moduleStatus(module_({ actions: [] })).label).toBe("Loaded");
  });

  it("counts the model providers it supplies", () => {
    // `feldspar-sklearn` is five estimators and nothing else, and the tab is
    // where an admin finds out that installing it got them a longer list on the
    // model form.
    const sklearn = module_({
      actions: [],
      model_providers: ["sklearn_ridge", "sklearn_dbscan"],
    });
    expect(moduleStatus(sklearn)).toEqual({ label: "2 model providers", tone: "green" });
    expect(
      suppliedSummary(module_({ actions: [], model_providers: ["sklearn_ridge"] })),
    ).toBe("1 model provider");
  });

  it("counts the stream providers it supplies", () => {
    // `plugins/rss` is one polled feed and nothing else, and the tab is where
    // an admin finds out that installing it got them a longer list on the
    // stream form.
    const rss = module_({ actions: [], stream_providers: ["rss_feed"] });
    expect(moduleStatus(rss)).toEqual({ label: "1 stream provider", tone: "green" });
    expect(
      suppliedSummary(module_({ actions: [], stream_providers: ["rss_feed", "atom_feed"] })),
    ).toBe("2 stream providers");
  });

  it("counts the view patterns it supplies", () => {
    // `@saltcorn/kanban` is two view patterns and nothing else.
    const kanban = module_({ actions: [], view_patterns: ["Kanban", "KanbanAllocator"] });
    expect(moduleStatus(kanban)).toEqual({ label: "2 view patterns", tone: "green" });
    expect(suppliedSummary(module_({ view_patterns: ["Mind map"] }))).toBe(
      "1 action and 1 view pattern",
    );
  });

  it("counts the tables it declares (TODO-imap M4)", () => {
    expect(
      suppliedSummary(module_({ tables: ["mail_accounts", "mail_messages"] })),
    ).toBe("1 action and 2 tables");
  });

  it("marks a module that did not load, and one that loaded with a complaint", () => {
    expect(moduleStatus(module_({ loaded: false, issues: ["its package is not installed"] })))
      .toEqual({ label: "Not loaded", tone: "red" });
    expect(
      moduleStatus(module_({ issues: ["its action `insert_row` is not available"] })).tone,
    ).toBe("yellow");
  });

  it("says what it also supplies and this version ignores", () => {
    expect(unsupportedSentence(module_())).toBeNull();
    const sentence = unsupportedSentence(
      module_({
        unsupported: [
          { key: "viewtemplates", count: 2 },
          { key: "eventTypes", count: null },
        ],
      }),
    );
    expect(sentence).toMatch(/2 × viewtemplates/);
    expect(sentence).toMatch(/eventTypes/);
    expect(sentence).toMatch(/does not load yet/);
  });

  it("says what an install just added, in every kind a module can supply", () => {
    expect(suppliedSummary(module_())).toBe("1 action");
    // A Python plugin whose whole purpose is one function must not be reported
    // as having installed "0 actions".
    expect(
      suppliedSummary(
        pythonModule({
          actions: [],
          functions: [{ name: "score", description: "", is_async: false, arguments: [] }],
        }),
      ),
    ).toBe("1 function");
    expect(
      suppliedSummary(
        module_({
          functions: [{ name: "score", description: "", is_async: false, arguments: [] }],
          table_providers: ["RSS feed", "Atom feed"],
        }),
      ),
    ).toBe("1 action, 1 function and 2 table providers");
    expect(suppliedSummary(module_({ actions: [] }))).toMatch(/nothing this version/);
  });

  it("knows whether there is anything to configure", () => {
    expect(isConfigurable(module_())).toBe(false);
    expect(
      isConfigurable(
        module_({
          config_spec: [
            {
              name: "broker_url",
              label: "Broker URL",
              type: "text",
              required: true,
              default: null,
              options: [],
              multiline: false,
              secret: false,
              create_only: false,
              show_if: [],
              code_language: null,
            },
          ],
        }),
      ),
    ).toBe(true);
  });

  it("reads the stored configuration as form values, whatever shape it is", () => {
    expect(configValues(module_())).toEqual({
      broker_url: "mqtt://localhost",
      password: "•••••",
    });
    // A module's settings are whatever it declared, so a number or a flag is
    // rendered as the text a form control holds.
    expect(configValues(module_({ configuration: { port: 1883, tls: true } }))).toEqual({
      port: "1883",
      tls: "true",
    });
    // Nothing configured, or something that is not an object at all.
    expect(configValues(module_({ configuration: {} }))).toEqual({});
    expect(configValues(module_({ configuration: null }))).toEqual({});
  });
});

describe("a module's permissions", () => {
  it("reads a set off the wire, and reads anything it cannot understand as closed", () => {
    expect(modulePermissions(module_({ permissions: { net: ["broker:1883"] } }))).toEqual({
      net: ["broker:1883"],
      read: [],
      write: [],
      env: [],
    });
    // The direction that cannot mislead: a screen that could not parse what it
    // was sent must not draw a module as *less* able to reach things than a
    // reader would then assume. Closed is what it shows, and closed is what the
    // server defaults to.
    expect(modulePermissions(module_({ permissions: null }))).toEqual(CLOSED_PERMISSIONS);
    expect(modulePermissions(module_({ permissions: "everything" }))).toEqual(CLOSED_PERMISSIONS);
    expect(modulePermissions(module_({ permissions: { net: [1, "a"] } })).net).toEqual(["a"]);
  });

  it("says out loud that an empty set means nothing rather than everything", () => {
    expect(isClosed(CLOSED_PERMISSIONS)).toBe(true);
    expect(permissionSummary(CLOSED_PERMISSIONS)).toMatch(/reaches nothing/i);
    expect(
      permissionSummary({ net: ["broker:1883"], read: ["/srv/data"], write: [], env: [] }),
    ).toBe("Reaches only these: connects to broker:1883; reads /srv/data.");
  });

  it("round-trips a list through the textarea an admin types it in", () => {
    expect(permissionText(["a", "b"])).toBe("a\nb");
    // Blank lines and stray spaces are what typing produces, and neither is an
    // entry.
    expect(parsePermissionText("  a \n\n b\n")).toEqual(["a", "b"]);
    expect(parsePermissionText("")).toEqual([]);
  });

  it("has none at all for a Python module, and says so rather than showing an empty form", () => {
    // §10: the Deno worker's allow-list has no counterpart in the embedded
    // interpreter. Rendering the same four empty boxes would claim a permission
    // model that does not exist — and "Reaches nothing" would be the *opposite*
    // of true about a module running with the server's own privileges.
    expect(hasPermissions(module_())).toBe(true);
    expect(hasPermissions(pythonModule())).toBe(false);
    expect(NO_SANDBOX).toMatch(/no sandbox/i);
    expect(NO_SANDBOX).toMatch(/server's own privileges/);
    // Even when the row carries a set — the column is shared, and a set stored
    // against a Python module enforces nothing.
    expect(hasPermissions(pythonModule({ permissions: { net: ["broker:1883"] } }))).toBe(false);
    // A language this SPA has not heard of is read as the sandboxed one: the
    // direction that cannot promise more isolation than there is.
    expect(hasPermissions(module_({ language: "rust" }))).toBe(true);
  });

  it("names what the server would refuse, before the save rather than after it", () => {
    expect(permissionProblem("net", "broker.example")).toBeNull();
    expect(permissionProblem("net", "broker.example:1883")).toBeNull();
    expect(permissionProblem("net", "https://broker.example")).toMatch(/URL/);
    expect(permissionProblem("net", "broker:eighteen")).toMatch(/port/);
    expect(permissionProblem("read", "/srv/data")).toBeNull();
    expect(permissionProblem("read", "data")).toMatch(/absolute/);
    expect(permissionProblem("env", "MQTT_PASSWORD")).toBeNull();
    expect(permissionProblem("env", "A=B")).toMatch(/variable name/);
    expect(
      permissionProblems({ net: ["https://x"], read: ["rel"], write: [], env: [] }),
    ).toHaveLength(2);
    expect(permissionProblems(CLOSED_PERMISSIONS)).toEqual([]);
  });
});

describe("the bundled catalog", () => {
  it("says what installing downloads, because that is the part that leaves the machine", () => {
    // The module is already here; its dependencies are not, and a card that
    // implied an offline install would be promising something the click cannot
    // do.
    expect(installsSentence(bundled())).toMatch(/rss-parser/);
    expect(installsSentence(bundled())).toMatch(/npm/);
    expect(installsSentence(bundled())).toMatch(/not shipped/);
    // Python's fetches with the other tool, and is named as such.
    const python = bundled({ language: "python", installs: ["markdown"] });
    expect(installsSentence(python)).toMatch(/pip/);
    // Nothing to download is nothing to say.
    expect(installsSentence(bundled({ installs: [] }))).toBeNull();
  });

  it("says what installing grants, beside the button that grants it", () => {
    // The whole basis of a one-click grant: a package may not grant itself a
    // permission, and an admin who pressed a button with this sentence next to
    // it granted one.
    const sentence = grantSentence(bundled());
    expect(sentence).toMatch(/any host/);
    expect(sentence).toMatch(/change that afterwards/);
    // A named host reads as itself rather than as a wildcard.
    expect(grantSentence(bundled({ permissions: { net: ["a.example"], read: [], write: [], env: [] } }))).toMatch(
      /connect to a.example/,
    );
    // And an entry that asks for nothing says nothing — which is every Python
    // entry, where there is nothing to grant at all.
    expect(grantSentence(bundled({ permissions: { net: [], read: [], write: [], env: [] } }))).toBeNull();
    expect(grantSentence(bundled({ permissions: null }))).toBeNull();
  });

  it("reads a requested permission set the same way a granted one is read", () => {
    expect(anyHost(bundledPermissions(bundled()))).toBe(true);
    // Unreadable is the *closed* set, never an open one: a card that could not
    // parse what it was sent must under-report a grant.
    expect(bundledPermissions(bundled({ permissions: "everything" }))).toEqual(CLOSED_PERMISSIONS);
    expect(anyHost(CLOSED_PERMISSIONS)).toBe(false);
  });

  it("blocks the card its language's toolchain is missing, and only that one", () => {
    const js = bundled();
    const py = bundled({ id: "markdown", language: "python" });
    expect(bundledBlocked(js, tools())).toBeNull();
    expect(bundledBlocked(py, tools())).toBeNull();
    // A server with npm and no pip installs one of the two, and the card that
    // cannot be installed says which repair it needs.
    expect(bundledBlocked(js, tools({ pip: false }))).toBeNull();
    expect(bundledBlocked(py, tools({ pip: false }))).toMatch(/pip/);
    expect(bundledBlocked(js, tools({ npm: false }))).toMatch(/npm/);
  });

  it("puts what a server can still add above what it already has", () => {
    const order = catalogOrder([
      bundled({ id: "a", installed: true }),
      bundled({ id: "b" }),
      bundled({ id: "c", installed: true }),
      bundled({ id: "d" }),
    ]);
    expect(order.map((entry) => entry.id)).toEqual(["b", "d", "a", "c"]);
  });

  it("names the language and the package under the heading", () => {
    expect(bundledSubtitle(bundled())).toBe("JavaScript · @feldspar/rss");
    expect(bundledSubtitle(bundled({ language: "python", name: "feldspar-markdown" }))).toBe(
      "Python · feldspar-markdown",
    );
  });
});

describe("any host", () => {
  it("reads as one phrase rather than as a host called *", () => {
    // The wildcard exists for a module whose addresses are configured per table
    // — an RSS feed's host is on the table, not the module — and "connects to *"
    // would be the one rendering that reads as *less* than it is.
    expect(permissionSummary({ net: ["*"], read: [], write: [], env: [] })).toMatch(
      /connects to any host/,
    );
    expect(permissionSummary({ net: ["a.example"], read: [], write: [], env: [] })).toMatch(
      /connects to a.example/,
    );
    // It is a grant, so it is not the closed set.
    expect(isClosed({ net: ["*"], read: [], write: [], env: [] })).toBe(false);
    // And the form does not refuse it on the way in.
    expect(permissionProblem("net", "*")).toBeNull();
  });
});

describe("a module's services (TODO-imap M5.2)", () => {
  const service = (overrides: Partial<Module["services"][number]> = {}) => ({
    name: "sync",
    description: "Keeps the mailboxes in step",
    state: "running",
    restarts: 0,
    last_error: null,
    ...overrides,
  });

  it("shows a running service green, and a failing one with why", () => {
    expect(serviceState(service())).toEqual({ label: "Running", tone: "green", detail: null });
    expect(
      serviceState(service({ state: "restarting", restarts: 2, last_error: "login refused" })),
    ).toEqual({ label: "Restarting", tone: "yellow", detail: "it failed: login refused" });
    expect(serviceState(service({ restarts: 1, last_error: "timeout" })).detail).toBe(
      "restarted once; last: timeout",
    );
    expect(serviceState(service({ state: "stopped" })).tone).toBe("red");
  });

  it("makes a module whose service is not running yellow, and counts services", () => {
    const running = module_({ services: [service()] });
    expect(moduleStatus(running).tone).toBe("green");
    expect(suppliedSummary(running)).toBe("1 action and 1 service");
    const failing = module_({ services: [service({ state: "restarting" })] });
    expect(moduleStatus(failing)).toEqual({ label: "A service is not running", tone: "yellow" });
  });
});
