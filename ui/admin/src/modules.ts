// The Modules tab's model: what an admin may install, and how an installed
// module reads.
//
// Kept out of the component for the reason `backup.ts` is: the arithmetic of a
// form — what counts as a filled-in specifier, what the census sentence says,
// whether a module is usable — is testable without a browser, and the component
// is then only flow.
//
// **Two languages, one tab** (§8). A module is a module to an admin, so what the
// language changes here is small and entirely local: which registry the package
// comes from, which toolchain has to be on the server to install it, and whether
// the permissions form means anything at all — which for Python it does not
// (§10), and saying so is this screen's obligation rather than an omission.

import type { ListModulesResponse } from "./client";
import { format, type Translator } from "./i18n";

/** One installed module, as `listModules` describes it. */
export type Module = ListModulesResponse["modules"][number];

/** One module this server ships with, as the same call describes it.
 *
 * A different type from `Module` and not a sparse one: an entry that has not
 * been installed has no row, no version and supplies nothing yet — what it has
 * is a card. */
export type BundledModule = ListModulesResponse["bundled"][number];

/** Which language a module is written in, and therefore which host loads it. */
export type ModuleLanguage = "javascript" | "python";

/** Where a module's package comes from. The three the server accepts: one
 * registry per language, and the local directory both share. */
export type ModuleSource = "npm" | "pypi" | "local";

/** What an admin fills in to install one. */
export type InstallForm = {
  language: ModuleLanguage;
  source: ModuleSource;
  location: string;
};

/** What this server can install *with*, as `listModules` reports it.
 *
 * Four booleans rather than two, because the two languages fail differently: an
 * interpreter with no `pip` is a different repair from no interpreter at all,
 * and `node` without `npm` is worth saying because a module *runs* without
 * either. And one field that is not a boolean, for the toolchain that is
 * present and still cannot do the job. */
export type Toolchains = {
  npm: boolean;
  /** The npm that is here and the npm that would work, when the one on the
   * server is too old to install anything — and `null` when it is not.
   *
   * Its own field rather than `npm: false`, because the two are different
   * repairs: no npm is "install Node.js", and an npm from Debian's or Ubuntu's
   * own packages is "install a newer one", which is not a conclusion an admin
   * would reach from a message saying npm is missing. */
  npmTooOld: { version: string; minimum: string } | null;
  node: boolean;
  python: boolean;
  pip: boolean;
};

/** Everything present, which is what a form assumes until `listModules` has
 * answered — the alternative is a form that is briefly and wrongly disabled. */
export const ALL_TOOLCHAINS: Toolchains = {
  npm: true,
  npmTooOld: null,
  node: true,
  python: true,
  pip: true,
};

/** A blank install form. JavaScript from npm first, because that is where a
 * module normally comes from; a local directory is the developer's case, and
 * Python is the newer half of §8. */
export const EMPTY_INSTALL: InstallForm = { language: "javascript", source: "npm", location: "" };

/** One entry in the Type select: a language and the registry it installs from.
 *
 * The two are one control because they are one decision — a Python module does
 * not come from npm and the server refuses the combination — so offering them as
 * two selects would be offering two invalid pairs. */
export type InstallChoice = {
  language: ModuleLanguage;
  source: ModuleSource;
  label: string;
};

/** The four combinations, in the order the select offers them. */
export const INSTALL_CHOICES: InstallChoice[] = [
  { language: "javascript", source: "npm", label: "JavaScript — npm package" },
  { language: "javascript", source: "local", label: "JavaScript — local directory" },
  { language: "python", source: "pypi", label: "Python — PyPI distribution" },
  { language: "python", source: "local", label: "Python — local directory" },
];

/** The `<option>` value for a choice: the pair, because neither half of it
 * identifies one on its own — `local` is two of the four. */
export function choiceValue(pick: { language: ModuleLanguage; source: ModuleSource }): string {
  return `${pick.language}:${pick.source}`;
}

/** The choice a select's value names, falling back to the first — a value that
 * is not one of the four cannot come from this select, and a form that refused
 * to change would be a worse answer than the default one. */
export function choiceFor(value: string): InstallChoice {
  return INSTALL_CHOICES.find((choice) => choiceValue(choice) === value) ?? INSTALL_CHOICES[0];
}

/** A language in the words the screen uses for it. */
export function languageLabel(language: string): string {
  return language === "python" ? "Python" : "JavaScript";
}

/** The label above the specifier box, which is a different thing for each
 * source: two are names in a registry, the third is a path on the *server*. */
export function locationLabel(form: { language: ModuleLanguage; source: ModuleSource }): string {
  if (form.source === "npm") return "Package name";
  if (form.source === "pypi") return "Distribution name";
  return "Directory on the server";
}

/** The placeholder in it. */
export function locationPlaceholder(form: {
  language: ModuleLanguage;
  source: ModuleSource;
}): string {
  if (form.source === "npm") return "@saltcorn/mqtt";
  if (form.source === "pypi") return "saltcorn-mqtt>=0.2";
  return form.language === "python" ? "/srv/checkouts/saltcorn-mqtt" : "/srv/checkouts/mqtt";
}

/** The distribution a PyPI specifier names — `httpx[http2]>=0.27` is `httpx` —
 * or `null` when it names none.
 *
 * The server's own rule (`pypi_spec_name`), applied here so that `==1.0` is
 * refused in front of the form rather than by pip a minute later. */
export function distributionName(spec: string): string | null {
  const trimmed = spec.trim();
  const end = trimmed.search(/[^A-Za-z0-9._-]/);
  const name = (end === -1 ? trimmed : trimmed.slice(0, end)).trim();
  return name === "" ? null : name;
}

/** Why this server cannot install a module in `language`, or `null` when it
 * can.
 *
 * The reason an admin cannot fix from this screen, said before they type a
 * package name rather than after a failed install — and one per language,
 * because a server with one toolchain and not the other is ordinary. */
export function toolchainMissing(language: ModuleLanguage, tools: Toolchains): string | null {
  if (language === "javascript") {
    if (!tools.npm)
      return "This server has no npm on its PATH, so it cannot install a JavaScript module. Install Node.js and restart Saltcorn.";
    // An npm too old to resolve the modules directory's own dependencies fails
    // every install, whatever is being installed, with a semver error about a
    // `file:` path. Said here, in the two versions an admin can act on.
    if (tools.npmTooOld)
      return `This server's npm is ${tools.npmTooOld.version}, which cannot install a module: npm ${tools.npmTooOld.minimum} or newer is needed. Debian and Ubuntu package npm 9.2.0, so this is what apt install npm gives — install Node.js from NodeSource, or upgrade npm alone with sudo npm install -g npm@latest.`;
    return null;
  }
  if (!tools.python) {
    return "This server has no Python interpreter on its PATH, so it cannot install a Python module. Install Python 3.11 or newer — or name one with --python-bin — and restart Saltcorn.";
  }
  if (!tools.pip) {
    return "This server's Python interpreter has no pip, so it cannot install a Python module. Add pip to it (python3 -m ensurepip) and restart Saltcorn.";
  }
  return null;
}

/** What this server can install, in one sentence, whichever half is missing. */
export function toolchainSentence(tools: Toolchains): string {
  const js = !tools.npm
    ? "there is no npm, so no JavaScript module can be installed"
    : tools.npmTooOld
      ? `npm is ${tools.npmTooOld.version}, which is too old to install a JavaScript module (${tools.npmTooOld.minimum} or newer is needed)`
      : "npm installs a JavaScript module";
  const py = !tools.python
    ? "there is no Python interpreter, so no Python module can be installed"
    : tools.pip
      ? "pip installs a Python one"
      : "the Python interpreter has no pip, so no Python module can be installed";
  return `On this server, ${js}; ${py}.`;
}

/** Why the Install button is disabled, or `null` when it is not.
 *
 * A sentence rather than a boolean because every reason is worth saying: a form
 * with nothing in it, a specifier that names no package, and a server without
 * the toolchain for the language that was picked.
 */
export function installBlocked(form: InstallForm, tools: Toolchains): string | null {
  const missing = toolchainMissing(form.language, tools);
  if (missing) return missing;
  const location = form.location.trim();
  if (location === "") {
    if (form.source === "npm") return "Type the name of an npm package.";
    if (form.source === "pypi") return "Type the name of a distribution on PyPI.";
    return "Type the path of a directory on this server.";
  }
  if (form.source === "local" && !location.startsWith("/")) {
    return "A local module is installed from an absolute path, so that it means the same thing wherever the server was started from.";
  }
  if (form.source === "pypi" && distributionName(location) === null) {
    return `${location} does not start with a distribution's name — write httpx, or httpx>=0.27 to ask for a version.`;
  }
  return null;
}

/** The version line under a module's name: what is installed, which language it
 * is written in, and where it came from. */
export function moduleSubtitle(module: Module): string {
  const version = module.version ? `v${module.version}` : "not installed";
  const from = module.source === "local" ? module.location : module.source;
  return `${version} · ${languageLabel(module.language)} · ${from}`;
}

/** The names of the actions a module supplies, in the order it declared them. */
export function actionNames(module: Module): string[] {
  return module.actions.map((action) => action.name);
}

/** The sentence about what a module supplies that this version does not load.
 *
 * `null` when there is nothing to say, so the component renders nothing rather
 * than an empty note. */
export function unsupportedSentence(module: Module): string | null {
  if (module.unsupported.length === 0) return null;
  const parts = module.unsupported.map((entity) => {
    const count = entity.count ?? null;
    return count === null ? entity.key : `${count} × ${entity.key}`;
  });
  return `Also supplies ${parts.join(", ")}, which this version of Saltcorn does not load yet.`;
}

/** How a module's state reads in a badge: what it is, and the tone to use.
 *
 * A module may supply actions, table providers, or both, and the badge counts
 * what it actually has: `@saltcorn/rss` is a plugin with no actions at all, and
 * "0 actions" would read as "this module does nothing" about a module that
 * serves a table. */
export function moduleStatus(module: Module): {
  label: string;
  tone: "green" | "yellow" | "red";
} {
  if (!module.loaded) return { label: "Not loaded", tone: "red" };
  if (module.issues.length > 0) return { label: "Loaded with issues", tone: "yellow" };
  // A service that is not running is the one thing on a loaded module that can
  // go wrong after it loaded (TODO-imap M5.2), so it wins over the counts.
  if (module.services.some((service) => service.state !== "running")) {
    return { label: "A service is not running", tone: "yellow" };
  }
  const parts: string[] = [];
  const actions = module.actions.length;
  const providers = module.table_providers.length;
  const models = module.model_providers.length;
  const streams = module.stream_providers.length;
  const patterns = module.view_patterns.length;
  if (actions > 0) parts.push(actions === 1 ? "1 action" : `${actions} actions`);
  if (providers > 0) {
    parts.push(providers === 1 ? "1 table provider" : `${providers} table providers`);
  }
  if (models > 0) {
    parts.push(models === 1 ? "1 model provider" : `${models} model providers`);
  }
  if (streams > 0) {
    parts.push(streams === 1 ? "1 stream provider" : `${streams} stream providers`);
  }
  if (patterns > 0) {
    parts.push(patterns === 1 ? "1 view pattern" : `${patterns} view patterns`);
  }
  return {
    label: parts.length === 0 ? "Loaded" : parts.join(", "),
    tone: "green",
  };
}

/** What a module supplies, counted, for the sentence that follows an install.
 *
 * Every kind rather than the badge's two, because the badge answers "is this
 * thing working" and this answers "what did I just get" — and a Python module
 * whose whole purpose is one function would otherwise be reported as having
 * installed "0 actions". */
export function suppliedSummary(module: Module): string {
  const counts: [number, string, string][] = [
    [module.actions.length, "action", "actions"],
    [module.functions.length, "function", "functions"],
    [module.table_providers.length, "table provider", "table providers"],
    [module.tables.length, "table", "tables"],
    [module.services.length, "service", "services"],
    [module.model_providers.length, "model provider", "model providers"],
    [module.stream_providers.length, "stream provider", "stream providers"],
    [module.view_patterns.length, "view pattern", "view patterns"],
  ];
  const parts = counts
    .filter(([count]) => count > 0)
    .map(([count, one, many]) => `${count} ${count === 1 ? one : many}`);
  if (parts.length === 0) return "nothing this version of Saltcorn loads";
  if (parts.length === 1) return parts[0];
  return `${parts.slice(0, -1).join(", ")} and ${parts[parts.length - 1]}`;
}

/** One of a module's services, as its card shows it: running, restarting after
 * a failure (how many times, and why), or stopped (TODO-imap M5.2). */
export function serviceState(
  service: Module["services"][number],
  t: Translator["t"] = format,
): {
  label: string;
  tone: "green" | "yellow" | "red";
  detail: string | null;
} {
  const restarts =
    service.restarts === 0
      ? null
      : service.restarts === 1
        ? "restarted once"
        : `restarted ${service.restarts} times`;
  switch (service.state) {
    case "running":
      return {
        label: t("Running"),
        tone: "green",
        detail: restarts && service.last_error ? `${restarts}; last: ${service.last_error}` : restarts,
      };
    case "restarting":
      return {
        label: t("Restarting"),
        tone: "yellow",
        detail: service.last_error ? `it failed: ${service.last_error}` : restarts,
      };
    default:
      return { label: t("Stopped"), tone: "red", detail: service.last_error ?? null };
  }
}

/** Whether a module has settings of its own to configure. */
export function isConfigurable(module: Module): boolean {
  return module.config_spec.length > 0;
}

/** The stored configuration as the form's string values expect it.
 *
 * `configuration` crosses the wire as an opaque JSON object (it is whatever the
 * module declared), so it is narrowed here rather than in the component. */
export function configValues(module: Module): Record<string, string> {
  const config = module.configuration;
  if (typeof config !== "object" || config === null || Array.isArray(config)) return {};
  const values: Record<string, string> = {};
  for (const [key, value] of Object.entries(config as Record<string, unknown>)) {
    if (value === null || value === undefined) continue;
    values[key] = typeof value === "string" ? value : JSON.stringify(value);
  }
  return values;
}

// --- permissions -----------------------------------------------------------
// What a module's worker may reach. Four allow-lists, closed by default, and
// every empty one means *nothing* rather than *everything* — which is the one
// place this screen could mislead, so the summary sentence says "nothing"
// out loud rather than rendering four empty boxes and leaving it to be inferred.
//
// **And all of it is JavaScript's** (§10). A Deno worker has a permission set;
// the embedded interpreter has nothing of the kind, and neither
// `RestrictedPython` nor an import gate is one. So a Python module gets no form
// here and a sentence instead — the one thing this screen owes an admin, because
// showing the form for one language is exactly what would let somebody infer a
// permission model for the other.

/** Whether the permissions on this screen mean anything for a module: they do
 * for JavaScript, and there is nothing of the kind for Python (§10).
 *
 * Read off the module rather than off a flag, so a language this SPA has not
 * heard of is treated as the sandboxed one — the reading that cannot promise
 * more isolation than there is. */
export function hasPermissions(module: Module): boolean {
  return module.language !== "python";
}

/** What a Python module can reach, which is everything this server can.
 *
 * Said in the place the other language's allow-lists are, because the absence of
 * a form is not a sentence anybody reads. */
export const NO_SANDBOX =
  "A Python module runs inside this server with the server's own privileges. There is no sandbox for one and nothing to grant: it can reach any host, any file and any environment variable this server can, so install only Python modules you trust.";

/** What a reload of a Python module does and does not do (§11).
 *
 * Beside the Reload button's effect rather than in the documentation, because
 * the failure it warns about — an upgraded distribution whose old classes are
 * still live — looks exactly like the module having ignored the upgrade. */
export const PYTHON_RELOAD =
  "Reloading re-imports the package where it can. A distribution with a compiled extension in it, and any version change, takes full effect only when this server restarts.";

/** The five things a module can be granted. The keys are the server's. The
 * first four are what its worker may reach; `roles` is the most privileged role
 * it may grant users (TODO-payments P3.3), checked by the server when it asks. */
export type PermissionKind = "net" | "read" | "write" | "env" | "roles";

/** A module's permission set as the API sends it. */
export type ModulePermissionSet = Record<PermissionKind, string[]>;

/** Nothing granted. */
export const CLOSED_PERMISSIONS: ModulePermissionSet = {
  net: [],
  read: [],
  write: [],
  env: [],
  roles: [],
};

/** One kind, as the form asks for it. */
export type PermissionKindSpec = {
  key: PermissionKind;
  label: string;
  help: string;
  placeholder: string;
};

/** The four, in the order the form shows them: the one a real module actually
 * needs first, and the filesystem below it. */
export const PERMISSION_KINDS: PermissionKindSpec[] = [
  {
    key: "net",
    label: "Hosts it may connect to",
    help: "A host, or a host and a port — broker.example or broker.example:1883. One per line. A single * means any host, which is what a module whose addresses are configured per table needs.",
    placeholder: "broker.example:1883",
  },
  {
    key: "read",
    label: "Paths it may read",
    help: "Absolute paths; a directory allows everything under it. The module can always read its own package.",
    placeholder: "/srv/data",
  },
  {
    key: "write",
    label: "Paths it may write",
    help: "Absolute paths. One per line.",
    placeholder: "/srv/data/out",
  },
  {
    key: "env",
    label: "Environment variables it may read",
    help: "Variable names. One per line. A variable that is not listed reads as undefined rather than failing.",
    placeholder: "MQTT_PASSWORD",
  },
  {
    key: "roles",
    label: "Roles it may grant",
    help: "The most privileged role it may give users, as a number: 70 lets it grant roles 70 to 99, for as long as a subscription lasts, say. Blank grants none. No module may grant the admin role.",
    placeholder: "70",
  },
];

/** A module's stored permissions, narrowed from the wire's opaque JSON.
 *
 * Anything unreadable is the **closed** set, never an open one: a screen that
 * could not parse what it was sent must not draw a module as more restricted
 * than it is — and closed is the direction that cannot mislead. */
export function modulePermissions(module: Module): ModulePermissionSet {
  return permissionSet(module.permissions);
}

/** One permission set out of the opaque JSON the wire carries it as.
 *
 * Shared by an installed module's granted set and a bundled entry's requested
 * one, which are the same object and must read the same way — the card and the
 * form are showing an admin the same four lists. */
export function permissionSet(raw: unknown): ModulePermissionSet {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return CLOSED_PERMISSIONS;
  const source = raw as Record<string, unknown>;
  const set: ModulePermissionSet = { net: [], read: [], write: [], env: [], roles: [] };
  for (const { key } of PERMISSION_KINDS) {
    const list = source[key];
    if (Array.isArray(list)) {
      // A role ceiling may arrive as a number from a manifest's request; every
      // other list is strings only.
      set[key] = list
        .map((entry) => (key === "roles" && typeof entry === "number" ? String(entry) : entry))
        .filter((entry): entry is string => typeof entry === "string");
    }
  }
  return set;
}

/** Whether nothing at all is granted. */
export function isClosed(set: ModulePermissionSet): boolean {
  return PERMISSION_KINDS.every(({ key }) => set[key].length === 0);
}

/** The `net` entry that means *any host* — the server's `ANY_HOST`. */
export const ANY_HOST = "*";

/** Whether the net list grants any host at all, rather than a list of them. */
export function anyHost(set: ModulePermissionSet): boolean {
  return set.net.includes(ANY_HOST);
}

/** The one-line summary above the form: what this module can reach, in words. */
export function permissionSummary(set: ModulePermissionSet): string {
  if (isClosed(set)) {
    return "Reaches nothing: no host, no file, no environment variable.";
  }
  const parts: string[] = [];
  if (anyHost(set)) parts.push("connects to any host");
  else if (set.net.length > 0) parts.push(`connects to ${set.net.join(", ")}`);
  if (set.read.length > 0) parts.push(`reads ${set.read.join(", ")}`);
  if (set.write.length > 0) parts.push(`writes ${set.write.join(", ")}`);
  if (set.env.length > 0) parts.push(`reads ${set.env.join(", ")}`);
  if ((set.roles ?? []).length > 0) parts.push(`grants users roles ${set.roles[0]} to 99`);
  return `Reaches only these: ${parts.join("; ")}.`;
}

/** A textarea's text for one list, and the list back out of it. One entry per
 * line, because the entries are paths and hosts and both may contain a comma. */
export function permissionText(list: string[]): string {
  return list.join("\n");
}

/** The list an admin typed: trimmed, with blank lines dropped. */
export function parsePermissionText(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");
}

/** Why an entry will be refused, or `null` when it will not.
 *
 * The same rules the server applies, said before the save rather than after it.
 * The server remains the authority — this is the form being helpful, not the
 * check. */
export function permissionProblem(kind: PermissionKind, entry: string): string | null {
  if (kind === "net") {
    if (entry.includes("://") || entry.includes("/")) {
      return `${entry} is a URL — write a host or a host:port, like broker.example:1883.`;
    }
    const colon = entry.lastIndexOf(":");
    if (colon > 0 && !entry.endsWith("]")) {
      const port = Number(entry.slice(colon + 1));
      if (!Number.isInteger(port) || port < 1 || port > 65535) {
        return `${entry.slice(colon + 1)} in ${entry} is not a port number.`;
      }
    }
    return null;
  }
  if (kind === "env") {
    return entry.includes("=") ? `${entry} is not a variable name.` : null;
  }
  if (kind === "roles") {
    const role = Number(entry);
    if (!Number.isInteger(role)) return `${entry} is not a role number.`;
    if (role <= 1) return "No module may grant the admin role (1).";
    if (role >= 100) return `Role ${role} cannot be granted: the public role is everybody's already.`;
    return null;
  }
  return entry.startsWith("/")
    ? null
    : `${entry} is not an absolute path, so it would mean a different directory depending on where the server was started.`;
}

/** Every problem in a set, so the Save button can say why it is disabled. */
export function permissionProblems(set: ModulePermissionSet): string[] {
  const problems: string[] = [];
  for (const { key } of PERMISSION_KINDS) {
    for (const entry of set[key]) {
      const problem = permissionProblem(key, entry);
      if (problem !== null) problems.push(problem);
    }
  }
  if (set.roles.length > 1) {
    problems.push(
      "Roles it may grant is one role, the most privileged; it may then grant every less privileged one too.",
    );
  }
  return problems;
}

// --- the bundled catalog ---------------------------------------------------
// The modules this server ships with. They are still modules — nothing is
// loaded until somebody installs one — and what installing does that a package
// name in the form above does not is: the directory is the server's own, the
// language is the catalog's answer rather than a select, and the permissions
// are the ones printed on the card. One click, and everything the click does is
// on the card before it is clicked.
//
// The dependencies are the honest part. `plugins/rss` ships without
// `rss-parser`, so the click reaches npm; a card that promised otherwise would
// be promising an offline install that is not there.

/** Whether an entry can be installed on this server, or why not.
 *
 * The same toolchain question the form asks, asked per card because a card is
 * per language: on a server with npm and no pip, the JavaScript entries have
 * buttons and the Python ones say why they do not. */
export function bundledBlocked(entry: BundledModule, tools: Toolchains): string | null {
  return toolchainMissing(entry.language === "python" ? "python" : "javascript", tools);
}

/** The line under a bundled entry's heading: its language, and its package. */
export function bundledSubtitle(entry: BundledModule): string {
  return `${languageLabel(entry.language)} · ${entry.name}`;
}

/** What installing an entry downloads, in a sentence — or `null` when it needs
 * nothing, in which case there is nothing to warn about.
 *
 * Named as a *download* because that is the part that can fail and the part
 * that leaves this machine: the module itself is already here. */
export function installsSentence(entry: BundledModule): string | null {
  if (entry.installs.length === 0) return null;
  const manager = entry.language === "python" ? "pip" : "npm";
  const list = entry.installs.join(", ");
  const plural = entry.installs.length === 1 ? "it" : "them";
  return `Installing downloads ${list} — ${manager} fetches ${plural} now; ${
    entry.installs.length === 1 ? "it is" : "they are"
  } not shipped with Saltcorn.`;
}

/** What installing an entry grants it, in a sentence — or `null` when there is
 * nothing to grant.
 *
 * Printed **beside the button**, which is the whole basis on which a one-click
 * install is allowed to grant anything at all: the admin who clicks has read
 * what they are granting. */
export function grantSentence(entry: BundledModule): string | null {
  const set = bundledPermissions(entry);
  if (isClosed(set)) return null;
  const parts: string[] = [];
  if (anyHost(set)) parts.push("connect to any host");
  else if (set.net.length > 0) parts.push(`connect to ${set.net.join(", ")}`);
  if (set.read.length > 0) parts.push(`read ${set.read.join(", ")}`);
  if (set.write.length > 0) parts.push(`write ${set.write.join(", ")}`);
  if (set.env.length > 0) parts.push(`read ${set.env.join(", ")}`);
  return `Installing lets it ${parts.join("; ")}. You can change that afterwards.`;
}

/** An entry's requested permissions, narrowed from the wire's opaque JSON.
 *
 * The same narrowing an installed module's set gets, and unreadable is the
 * **closed** set for the same reason: a card that could not parse what it was
 * sent must under-report a grant rather than over-report one. */
export function bundledPermissions(entry: BundledModule): ModulePermissionSet {
  return permissionSet(entry.permissions);
}

/** The catalog with the installed entries last, so what a server can add is at
 * the top of the list rather than under what it already has. */
export function catalogOrder(entries: BundledModule[]): BundledModule[] {
  return [...entries].sort((a, b) => Number(a.installed) - Number(b.installed));
}
