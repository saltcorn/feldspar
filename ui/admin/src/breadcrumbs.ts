// Where a screen is: the trail of screens above it, shown over its title.
//
// Every screen that is not the root of a sidebar section says how it was
// reached — `Applications › shop › Checklists` over a checklist run, `Tables ›
// books` over the books table's data — and every step of the trail is a link
// back to that screen. The trail holds the *ancestors* only: the screen itself
// is the title right under it, and naming it twice would push the title down a
// line for nothing.
//
// The trails are built here rather than in each screen so that two screens
// under one parent spell that parent the same way and link it to the same
// route, and so the shapes can be tested without a DOM. `PageHeader` draws
// them (`layout.tsx`, `crumbs`).

import { useEffect, useState } from "react";

import { api } from "./api";
import type { Translator } from "./i18n";

/** One step of the trail: what it is called and where it goes. A step with no
 * `href` is drawn as text — a parent that is not a screen of its own. */
export type Crumb = { label: string; href?: string };

type Translate = Translator["t"];

const enc = encodeURIComponent;

/** What a step whose record is still loading is called, so the trail keeps its
 * shape (and its links) while the name arrives. */
export const LOADING_LABEL = "…";

function named(name: string | null | undefined): string {
  return name && name.trim() ? name : LOADING_LABEL;
}

// ---- Data -----------------------------------------------------------------

export function tablesCrumbs(t: Translate): Crumb[] {
  return [{ label: t("Tables"), href: "#/tables" }];
}

/** Down to one table's own page. `label` is the table's display label, when it
 * has one. */
export function tableCrumbs(t: Translate, table: string, label?: string | null): Crumb[] {
  return [
    ...tablesCrumbs(t),
    { label: label?.trim() ? label : table, href: `#/tables/${enc(table)}` },
  ];
}

/** Down to the changeset browser, narrowed to one table when `table` is given. */
export function changesetsCrumbs(t: Translate, table?: string | null): Crumb[] {
  return [
    ...(table ? tableCrumbs(t, table) : tablesCrumbs(t)),
    {
      label: t("Changesets"),
      href: table ? `#/changesets?table=${enc(table)}` : "#/changesets",
    },
  ];
}

export function fileStoresCrumbs(t: Translate): Crumb[] {
  return [{ label: t("File stores"), href: "#/file-stores" }];
}

export function streamsCrumbs(t: Translate): Crumb[] {
  return [{ label: t("Streams"), href: "#/streams" }];
}

/** Down to one stream's settings. */
export function streamCrumbs(t: Translate, streamId: string, name: string | null): Crumb[] {
  return [...streamsCrumbs(t), { label: named(name), href: `#/streams/${enc(streamId)}/edit` }];
}

// ---- Automation -----------------------------------------------------------

export function triggersCrumbs(t: Translate): Crumb[] {
  return [{ label: t("Triggers"), href: "#/triggers" }];
}

/** Down to one trigger's settings. */
export function triggerCrumbs(t: Translate, triggerId: string, name: string | null): Crumb[] {
  return [...triggersCrumbs(t), { label: named(name), href: `#/triggers/${enc(triggerId)}/edit` }];
}

/** Down to a workflow trigger's run list. */
export function workflowRunsCrumbs(
  t: Translate,
  triggerId: string,
  name: string | null,
): Crumb[] {
  return [
    ...triggerCrumbs(t, triggerId, name),
    { label: t("Runs"), href: `#/triggers/${enc(triggerId)}/runs` },
  ];
}

export function agentsCrumbs(t: Translate): Crumb[] {
  return [{ label: t("Agents"), href: "#/agents" }];
}

/** Down to one agent: its chat is the agent's first screen. */
export function agentCrumbs(t: Translate, agent: string): Crumb[] {
  return [...agentsCrumbs(t), { label: agent, href: `#/agents/${enc(agent)}/chat` }];
}

/** Down to one agent's run list. */
export function agentRunsCrumbs(t: Translate, agent: string): Crumb[] {
  return [...agentCrumbs(t, agent), { label: t("Runs"), href: `#/agents/${enc(agent)}/runs` }];
}

export function llmProvidersCrumbs(t: Translate): Crumb[] {
  return [...agentsCrumbs(t), { label: t("LLM providers"), href: "#/llm-providers" }];
}

// ---- Administration -------------------------------------------------------

export function usersCrumbs(t: Translate): Crumb[] {
  return [{ label: t("Users"), href: "#/users" }];
}

export function settingsCrumbs(t: Translate): Crumb[] {
  return [{ label: t("Settings"), href: "#/settings" }];
}

/** Down to one tab of Settings. `label` is the tab's name as its strip shows
 * it. */
export function settingsTabCrumbs(t: Translate, tab: string, label: string): Crumb[] {
  return [...settingsCrumbs(t), { label, href: `#/settings/${enc(tab)}` }];
}

// ---- Applications ---------------------------------------------------------

export function applicationsCrumbs(t: Translate): Crumb[] {
  return [{ label: t("Applications"), href: "#/applications" }];
}

/** Down to one application. Its settings are its own screen — the one every
 * kind of application has. */
export function applicationCrumbs(t: Translate, appId: string, name: string | null): Crumb[] {
  return [
    ...applicationsCrumbs(t),
    { label: named(name), href: `#/applications/${enc(appId)}/edit` },
  ];
}

/** Down to an application's views or pages list. */
export function applicationListCrumbs(
  t: Translate,
  appId: string,
  name: string | null,
  list: "views" | "pages",
): Crumb[] {
  return [
    ...applicationCrumbs(t, appId, name),
    {
      label: list === "views" ? t("Views") : t("Pages"),
      href: `#/applications/${enc(appId)}/${list}`,
    },
  ];
}

/** Down to a checklist list: an application's, or the Data Layer's, which is a
 * tab of Settings (TODO-checklists C1.16). */
export function checklistsCrumbs(
  t: Translate,
  appId: string | null,
  appName: string | null,
): Crumb[] {
  if (!appId) return settingsTabCrumbs(t, "checklists", t("Checklists"));
  return [
    ...applicationCrumbs(t, appId, appName),
    { label: t("Checklists"), href: `#/applications/${enc(appId)}/checklists` },
  ];
}

/** Down to one checklist's own screen — what a run's trail ends in. */
export function checklistCrumbs(
  t: Translate,
  appId: string | null,
  appName: string | null,
  checklistId: string,
  checklistName: string | null,
): Crumb[] {
  const base = appId ? `#/applications/${enc(appId)}/checklists` : "#/checklists";
  return [
    ...checklistsCrumbs(t, appId, appName),
    { label: named(checklistName), href: `${base}/${enc(checklistId)}` },
  ];
}

/** The name of application `appId`, for a screen that has nothing else to load
 * it for: `null` while it loads (and for no application, or one that is gone),
 * which the trail shows as {@link LOADING_LABEL}. */
export function useApplicationName(appId: string | null): string | null {
  const [name, setName] = useState<string | null>(null);
  useEffect(() => {
    setName(null);
    if (!appId) return;
    let live = true;
    api
      .listApplications()
      .then((apps) => {
        if (live) setName(apps.find((a) => a.id === appId)?.name ?? null);
      })
      .catch(() => {
        // The trail keeps its placeholder; the screen still works.
      });
    return () => {
      live = false;
    };
  }, [appId]);
  return name;
}
