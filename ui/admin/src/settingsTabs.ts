// An application's settings tabs (TODO UX2.1).
//
// The framework declares them and the server completes them (`sc_app::
// SettingsTab`), so this screen knows no framework's arrangement: it is sent a
// list of tabs, each naming the parts ("groups") of the application form it
// holds, and renders whichever tab is open. What is here is the arithmetic of
// that, kept out of the components so it can be tested without a browser:
//
// - **Where each tab goes.** A tab holding a view screen (Views, Pages,
//   Library) is that screen's route; a tab holding only the translations
//   editor is its route; every other tab is the application form at
//   `edit/<id>`, and the first one is plain `edit`, which is what every link
//   to an application's settings has always said.
// - **Which tab a screen is on**, for highlighting: the tab whose id it is, or
//   failing that the tab holding it as a group — the Translations editor is on
//   Saltcorn UI's Advanced tab.
// - **Which of the framework's own settings a tab shows**: those naming it as
//   their `tab`, and on the tab holding the framework choice, those naming no
//   tab (or one there is not) — where every setting was before tabs.

/** One settings tab, as the server sends it. */
export type SettingsTab = { id: string; label: string; groups: string[] };

/** The screens that are a route of their own, each alone on its tab. */
export const VIEW_SCREENS = ["views", "pages", "library"] as const;

/** The group holding the framework choice: its tab is the one a framework
 * setting naming no tab is shown on. */
export const GROUP_FRAMEWORK = "framework";

/** Where the framework's own settings for a tab go. */
export const GROUP_SETTINGS = "settings";

/** The Translations editor. */
export const GROUP_TRANSLATIONS = "translations";

/** The view screen a tab is, if it is one. */
export function screenOf(tab: SettingsTab): string | undefined {
  return tab.groups.find((g) => (VIEW_SCREENS as readonly string[]).includes(g));
}

/** Whether a tab is the application form (rather than a screen's route). */
export function isFormTab(tab: SettingsTab): boolean {
  if (screenOf(tab)) return false;
  return !(tab.groups.length === 1 && tab.groups[0] === GROUP_TRANSLATIONS);
}

/** The link each tab opens, in order. */
export function settingsTabLinks(
  appId: string,
  tabs: SettingsTab[],
): { id: string; label: string; href: string }[] {
  const base = `#/applications/${encodeURIComponent(appId)}`;
  return tabs.map((tab, i) => {
    const screen = screenOf(tab);
    let href: string;
    if (screen) href = `${base}/${screen}`;
    else if (!isFormTab(tab)) href = `${base}/translations`;
    else if (i === 0) href = `${base}/edit`;
    else href = `${base}/edit/${encodeURIComponent(tab.id)}`;
    return { id: tab.id, label: tab.label, href };
  });
}

/** The tab to highlight for `active` — a tab id, or the group a screen shows. */
export function activeTabId(tabs: SettingsTab[], active: string): string | undefined {
  return (
    tabs.find((t) => t.id === active)?.id ??
    tabs.find((t) => t.groups.includes(active))?.id
  );
}

/** The form tab the form shows for the route's `requested` id: that one, or
 * the first tab when there is none of that id or it is a screen's. */
export function formTab(tabs: SettingsTab[], requested?: string): SettingsTab | undefined {
  const asked = tabs.find((t) => t.id === requested);
  if (asked && isFormTab(asked)) return asked;
  return tabs.find(isFormTab);
}

/** The tab a framework setting is shown on. */
export function tabOfField(field: { tab?: string | null }, tabs: SettingsTab[]): string | undefined {
  const named = tabs.find((t) => t.id === field.tab);
  if (named && isFormTab(named)) return named.id;
  return (tabs.find((t) => t.groups.includes(GROUP_FRAMEWORK)) ?? tabs.find(isFormTab))?.id;
}

/** The framework settings shown on tab `tabId`, in the spec's order. */
export function fieldsOnTab<F extends { tab?: string | null }>(
  spec: F[],
  tabs: SettingsTab[],
  tabId: string,
): F[] {
  return spec.filter((f) => tabOfField(f, tabs) === tabId);
}

/** A tab's groups with the framework settings' place made explicit: a tab
 * that has settings to show but does not say where shows them last. */
export function groupsToRender(tab: SettingsTab, hasSettings: boolean): string[] {
  if (!hasSettings || tab.groups.includes(GROUP_SETTINGS)) return tab.groups;
  return [...tab.groups, GROUP_SETTINGS];
}
