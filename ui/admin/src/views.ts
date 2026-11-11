// The Views and Pages tabs of a Saltcorn UI application (TODO "Saltcorn UI" 9.2).
//
// Kept out of the component for the reason `modules.ts` and `backup.ts` are: the
// arithmetic of the list is testable without a browser, and the component is
// then only flow. What that arithmetic is:
//
// - **Which tabs an application has.** Only an application whose source is
//   views and pages has the two tabs, and the server says which ones are
//   (`has_views`) — this screen does not know a framework by name.
// - **The link that opens a view on the app's subdomain.** A v1 view name has
//   spaces in it (`List Books`), so the name is encoded into the path; and the
//   generated client interpolates path parameters as they are, so the same
//   encoding is what the API calls are made with (`nameParam`).
// - **A view whose pattern this server does not have** is shown with that fact
//   attached rather than as an ordinary row: it is a view that will not render.

import { appUrl } from "./adminAddress";
import type {
  CreateViewRequest,
  ListApplicationsResponse,
  ListPagesResponse,
  ListViewPatternsResponse,
  ListViewsResponse,
  PageReferencesResponse,
  SaveViewRequest,
  ViewConfigStepResponse,
  ViewReferencesResponse,
} from "./client";
import { roleLabel, type Roles } from "./roles";
import { settingsTabLinks } from "./settingsTabs";
import { buildConfig, initialValues, readConfig } from "./settings";

export type AppItem = ListApplicationsResponse[number];
export type ViewItem = ListViewsResponse[number];
export type PageItem = ListPagesResponse[number];
export type PatternItem = ListViewPatternsResponse[number];
export type StepItem = ViewConfigStepResponse;
export type References = ViewReferencesResponse;

export type PageReferences = PageReferencesResponse;

/** Which of an application's tabs a screen is: a settings tab's id, or the
 * screen it shows (`views`, `translations`), which highlights the tab holding
 * it (see `activeTabId`). */
export type AppTab = string;

/** Where the page is, as much of `window.location` as a link needs. */
export type Here = { protocol: string; host: string };

/** What an application with no views is told, because it is not obvious: its
 * subdomain serves nothing until it has one. */
export const NO_VIEWS =
  "This application has no views yet. A Saltcorn UI application serves nothing " +
  "until it has views: restore a Saltcorn 1 backup into it, or create one.";

/** And with no pages, which it can do without: `/` then lists the views. */
export const NO_PAGES =
  "This application has no pages. Its views are still served at /view/<name>; a " +
  "page places views on one screen and can be each role's home page.";

/** The tabs an application's screen has, in order: its framework's settings
 * tabs (TODO UX2.1), as the server sends them on the application. */
export function appTabs(
  app: Pick<AppItem, "id" | "settings_tabs">,
): { id: string; label: string; href: string }[] {
  return settingsTabLinks(app.id, app.settings_tabs);
}

/** A view or page name as a path parameter: the generated client puts it in
 * the URL as it is given. */
export function nameParam(name: string): string {
  return encodeURIComponent(name);
}

/** The app's own origin: `<subdomain>.<base domain>` ({@link appUrl}). */
export function appOrigin(subdomain: string, here: Here): string {
  return appUrl(subdomain, here);
}

/** Where a view is served on its application's subdomain. */
export function viewUrl(subdomain: string, name: string, here: Here): string {
  return `${appOrigin(subdomain, here)}/view/${encodeURIComponent(name)}`;
}

/** Where a page is served on its application's subdomain. */
export function pageUrl(subdomain: string, name: string, here: Here): string {
  return `${appOrigin(subdomain, here)}/page/${encodeURIComponent(name)}`;
}

/** One row of the Views tab. */
export type ViewRow = {
  name: string;
  description: string;
  pattern: string;
  /** Set when the pattern list has loaded and does not have this one. */
  patternMissing: boolean;
  table: string;
  role: string;
  url: string;
};

/** The Views tab's rows, in the server's order (by name). `patterns` is `null`
 * while the list is loading or when it could not be had, and then no view is
 * marked as missing its pattern. */
export function viewRows(
  views: ViewItem[],
  patterns: PatternItem[] | null,
  roles: Roles,
  subdomain: string,
  here: Here,
): ViewRow[] {
  return views.map((view) => {
    const pattern = patterns?.find((p) => p.name === view.viewpattern);
    return {
      name: view.name,
      description: view.description,
      pattern: pattern?.label || view.viewpattern,
      patternMissing: patterns !== null && !pattern,
      table: view.table_name || "—",
      role: roleLabel(view.min_role, roles),
      url: viewUrl(subdomain, view.name, here),
    };
  });
}

/** One row of the Pages tab. */
export type PageRow = {
  name: string;
  title: string;
  role: string;
  /** The roles this page is the home page of, from v1's `root_page_for_roles`;
   * empty when it is nobody's. */
  homeFor: string[];
  url: string;
};

/** The Pages tab's rows. */
export function pageRows(
  pages: PageItem[],
  roles: Roles,
  subdomain: string,
  here: Here,
): PageRow[] {
  return pages.map((page) => {
    const attributes = (page.attributes ?? {}) as Record<string, unknown>;
    const home = attributes.root_page_for_roles;
    return {
      name: page.name,
      title: page.title,
      role: roleLabel(page.min_role, roles),
      homeFor: Array.isArray(home)
        ? home
            .map(Number)
            .filter((r) => Number.isInteger(r))
            .map((r) => roleLabel(r, roles))
        : [],
      url: pageUrl(subdomain, page.name, here),
    };
  });
}

/** The confirmation a delete asks for. `references` are the sentences saying
 * what names it and what its layout places (`viewReferenceLines`,
 * `pageReferenceLines`), empty while they could not be had. */
export function deleteConfirmation(
  kind: "view" | "page",
  name: string,
  appName: string,
  references: string[] = [],
): string {
  const consequence =
    kind === "view"
      ? "Pages and views that show or link to it will no longer find it."
      : "A role whose home page it is will land on the list of views instead.";
  const found = references.length ? ` ${references.join(" ")}` : "";
  return `Delete the ${kind} "${name}" from ${appName}? ${consequence}${found} This cannot be undone.`;
}

// ---------------------------------------------------------------------------
// Configuring a view (TODO "Saltcorn UI" Phase 10)
// ---------------------------------------------------------------------------
//
// The configuration editor is v1's own wizard: the pattern's steps, one at a
// time, each a form the server builds over the configuration gathered so far.
// What is worked out here is where a step's answers go — at the top of the
// configuration, or under the step's `context_field` — so that stepping back
// and forth, and saving from any step, never loses or invents a setting.

/** A view's configuration, the object a wizard gathers into. */
export type Configuration = Record<string, unknown>;

/** What a step v1 leaves out says, when it is the one on screen. */
export const STEP_SKIPPED =
  "This step does not apply to this view as it is configured, so it has nothing to ask.";

/** The form values a step opens with, as the settings form edits them: what the
 * configuration already holds, else each field's default. */
export function stepFormValues(step: StepItem): Record<string, string> {
  return initialValues(step.fields, readConfig(step.values));
}

/** The configuration with one step's answers in it.
 *
 * The answers land where v1's `Workflow` puts them: under `context_field` when
 * the step has one, else at the top level. Every field of the step is stated —
 * a field left empty is **removed**, rather than keeping the value it had — and
 * nothing the step does not ask about is touched. A layout step and a skipped
 * one change nothing. */
export function applyStep(
  configuration: Configuration,
  step: StepItem,
  values: Record<string, string>,
): Configuration {
  if (step.builder || step.skip) return configuration;
  const built = buildConfig(step.fields, values);
  const key = step.context_field;
  const nested = key ? configuration[key] : undefined;
  const target: Configuration = key
    ? {
        ...(nested && typeof nested === "object" && !Array.isArray(nested)
          ? (nested as Configuration)
          : {}),
      }
    : { ...configuration };
  for (const field of step.fields) {
    if (field.name in built) target[field.name] = built[field.name];
    else delete target[field.name];
  }
  return key ? { ...configuration, [key]: target } : target;
}

/** The part of a configuration a layout step owns, as the JSON it is shown as. */
export function layoutJson(configuration: Configuration): string {
  const shown: Configuration = {};
  for (const key of ["layout", "columns"]) {
    if (configuration[key] !== undefined) shown[key] = configuration[key];
  }
  return JSON.stringify(shown, null, 2);
}

/** `Step 2 of 5: Default state`. */
export function stepTitle(
  step: Pick<StepItem, "index" | "count" | "name">,
): string {
  return `Step ${step.index + 1} of ${step.count}: ${step.name}`;
}

/** The body that saves `view` with `configuration`, and optionally a new name. */
export function saveViewBody(
  view: ViewItem,
  configuration: Configuration,
  name: string = view.name,
): SaveViewRequest {
  return {
    name,
    description: view.description,
    viewpattern: view.viewpattern,
    table_name: view.table_name ?? null,
    configuration,
    min_role: view.min_role,
    slug: view.slug ?? null,
    attributes: view.attributes ?? {},
  };
}

/** What the new-view dialog holds. */
export type NewViewForm = {
  name: string;
  description: string;
  viewpattern: string;
  table_name: string;
  min_role: number;
};

/** Why the new-view form cannot be submitted yet, or `null`. The server checks
 * everything again — the name's characters, the table's subset, the role — and
 * says so in its own words; this only stops the obviously incomplete. */
export function newViewError(
  form: NewViewForm,
  patterns: PatternItem[] | null,
): string | null {
  if (!form.name.trim()) return "A view needs a name.";
  if (!form.viewpattern) return "Choose a view pattern.";
  const pattern = patterns?.find((p) => p.name === form.viewpattern);
  if (pattern?.table_required !== false && !form.table_name) {
    return `A ${pattern?.label || form.viewpattern} view is over a table: choose one.`;
  }
  return null;
}

/** The `createView` body of a new-view form. */
export function createViewBody(form: NewViewForm): CreateViewRequest {
  return {
    name: form.name.trim(),
    description: form.description.trim() || null,
    viewpattern: form.viewpattern,
    table_name: form.table_name || null,
    min_role: form.min_role,
  };
}

/** Where a view's configuration editor is, opened at `step` when one is given
 * (the builder's way back, TODO "The builder" §9). */
export function viewEditorHref(appId: string, name: string, step?: number): string {
  const query = step === undefined ? "" : `?step=${step}`;
  return `#/applications/${encodeURIComponent(appId)}/views/${encodeURIComponent(name)}${query}`;
}

/** Where a page's properties form is. The builder's **Page properties** link
 * is this route (`sc-server/src/builder.rs`). */
export function pagePropertiesHref(appId: string, name: string): string {
  return `#/applications/${encodeURIComponent(appId)}/pages/${encodeURIComponent(name)}/properties`;
}

/** Where a new page's properties form is. */
export function newPageHref(appId: string): string {
  return `#/applications/${encodeURIComponent(appId)}/pages/new`;
}

/** The sentences saying what names a view: the views embedding or linking to
 * it, and the pages and library items showing it. Empty when nothing does. */
export function viewReferenceLines(references: References): string[] {
  const lines: string[] = [];
  if (references.embedded_in.length) {
    lines.push(`Embedded in ${named(references.embedded_in, "view")}.`);
  }
  if (references.linked_from.length) {
    lines.push(`Linked to from ${named(references.linked_from, "view")}.`);
  }
  if (references.pages.length) {
    lines.push(`Shown on ${named(references.pages, "page")}.`);
  }
  if (references.library.length) {
    lines.push(`Shown or linked to by ${named(references.library, "library item")}.`);
  }
  return lines;
}

/** The sentences saying what names a page: the menu entries opening it, the
 * roles whose home page it is, and the views, pages and library items showing
 * or linking to it. Empty when nothing does. */
export function pageReferenceLines(references: PageReferences): string[] {
  const lines: string[] = [];
  if (references.menu.length) {
    lines.push(
      `Opened by the menu ${references.menu.length === 1 ? "entry" : "entries"} ${quotedList(references.menu)}.`,
    );
  }
  if (references.home_page_for.length) {
    lines.push(`The home page of ${named(references.home_page_for, "role")}.`);
  }
  if (references.views.length) {
    lines.push(`Shown or linked to by ${named(references.views, "view")}.`);
  }
  if (references.pages.length) {
    lines.push(`Shown or linked to by ${named(references.pages, "page")}.`);
  }
  if (references.library.length) {
    lines.push(`Shown or linked to by ${named(references.library, "library item")}.`);
  }
  return lines;
}

/** The sentence saying which library items a layout places, which a rename or a
 * delete leaves as they are, or `null` when it places none. */
export function placesLine(places: string[]): string | null {
  if (!places.length) return null;
  const them = places.length === 1 ? "it stays" : "they stay";
  return `Its layout places ${named(places, "library item")}; ${them} in the library.`;
}

/** Everything a delete warning says about a view's or page's references. */
export function deleteReferenceLines(
  lines: string[],
  places: string[],
): string[] {
  const place = placesLine(places);
  return place ? [...lines, place] : lines;
}

/** What a rename will leave behind, one sentence per kind, said **before** the
 * rename: the views and pages that name the view keep the old name, and stop
 * finding it. Nothing is rewritten. */
export function referencesReport(
  name: string,
  references: References,
): string[] {
  return renameReport("view", name, viewReferenceLines(references), references.places);
}

/** The same for a page. */
export function pageReferencesReport(
  name: string,
  references: PageReferences,
): string[] {
  return renameReport("page", name, pageReferenceLines(references), references.places);
}

function renameReport(
  kind: "view" | "page",
  name: string,
  lines: string[],
  places: string[],
): string[] {
  const report = lines.length
    ? [
        ...lines,
        `These refer to the ${kind} as "${name}" and will not be updated: after the rename ` +
          "they will no longer find it until they are changed to the new name.",
      ]
    : [`Nothing in this application refers to "${name}" by name, so nothing is left behind.`];
  const place = placesLine(places);
  return place ? [...report, place] : report;
}

/** `"a", "b"`. */
export function quotedList(names: string[]): string {
  return names.map((n) => `"${n}"`).join(", ");
}

/** `the view "a"`, `the views "a", "b"`. */
function named(names: string[], noun: string): string {
  return `the ${noun}${names.length === 1 ? "" : "s"} ${quotedList(names)}`;
}
