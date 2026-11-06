// What the Home screen shows, decided without a DOM (`screens/Home.tsx` draws
// it, `home.test.ts` pins it).
//
// Home answers three questions an admin arriving in the admin UI has, in this
// order: *is anything broken*, *what is here*, and — on an installation that
// is still nearly empty — *what do I do next*. Each answer is built from the
// lists the other screens already fetch, so Home needs no endpoint of its own;
// a list that could not be fetched is `null` and its part of the answer is left
// out rather than reported as "nothing wrong".

import { checklistRunPath } from "./checklists";
import type {
  GetSystemInfoResponse,
  ListBackupSchedulesResponse,
  ListChecklistsResponse,
  ListLlmProvidersResponse,
  ListTriggersResponse,
} from "./client";
import type { Entity, EntityKind } from "./entities";
import { format, type Translator } from "./i18n";
import { usage } from "./systemInfo";

type T = Translator["t"];

/** Disk use, in percent, from which Home warns. */
export const DISK_WARN_PERCENT = 90;

/** One thing that needs the admin: what, why, and where it is fixed. */
export type AttentionItem = {
  key: string;
  tone: "red" | "yellow";
  title: string;
  detail?: string;
  href: string;
};

/** The lists the attention panel reads; `null` is one that did not load. */
export type AttentionInputs = {
  triggers: ListTriggersResponse | null;
  schedules: ListBackupSchedulesResponse | null;
  checklists: ListChecklistsResponse | null;
  system: GetSystemInfoResponse | null;
};

/** Whether a scheduled backup's latest attempt failed: it has an error and no
 * success since that attempt. */
export function backupFailing(schedule: ListBackupSchedulesResponse[number]): boolean {
  if (!schedule.last_error) return false;
  if (!schedule.last_success_at) return true;
  if (!schedule.last_attempt_at) return false;
  return Date.parse(schedule.last_success_at) < Date.parse(schedule.last_attempt_at);
}

/** Everything that needs fixing, the broken (red) before the worrying (yellow). */
export function attention(inputs: AttentionInputs, t: T = format): AttentionItem[] {
  const items: AttentionItem[] = [];
  for (const trigger of inputs.triggers ?? []) {
    if (!trigger.error || !trigger.enabled) continue;
    items.push({
      key: `trigger:${trigger.id}`,
      tone: "red",
      title: t("Trigger {name} cannot run", { name: trigger.name }),
      detail: trigger.error,
      href: `#/triggers/${encodeURIComponent(trigger.id)}/edit`,
    });
  }
  for (const schedule of inputs.schedules ?? []) {
    if (!backupFailing(schedule)) continue;
    items.push({
      key: `backup:${schedule.id}`,
      tone: "red",
      title: t("Scheduled backup to {location} failed", { location: schedule.location }),
      detail: schedule.last_error ?? undefined,
      href: "#/settings/backup",
    });
  }
  for (const checklist of inputs.checklists ?? []) {
    const run = checklist.last_run;
    if (!checklist.enabled || !run || (run.status !== "failed" && run.status !== "error")) {
      continue;
    }
    items.push({
      key: `checklist:${checklist.id}`,
      tone: "red",
      title: t("Checklist {name} failed its last run", { name: checklist.name }),
      href: `#${checklistRunPath(run.id, checklist.application)}`,
    });
  }
  const disk = inputs.system?.disk;
  const used = disk ? usage(disk.total_bytes, disk.available_bytes) : null;
  if (disk && used && used.percent >= DISK_WARN_PERCENT) {
    items.push({
      key: "disk",
      tone: "yellow",
      title: t("The disk is {percent}% full", { percent: used.percent }),
      detail: disk.path,
      href: "#/settings/system",
    });
  }
  return items.sort((a, b) => (a.tone === b.tone ? 0 : a.tone === "red" ? -1 : 1));
}

/** One of the checks `attention` runs, named by what it looks at. */
export type AttentionCheck = "triggers" | "backups" | "checklists" | "disk";

/** The checks that could not run because their list did not load (`null`).
 * Without this, a list that failed would read as "nothing wrong" with it: an
 * empty attention panel is only good news when every check ran. */
export function uncheckedChecks(inputs: AttentionInputs): AttentionCheck[] {
  const out: AttentionCheck[] = [];
  if (inputs.triggers === null) out.push("triggers");
  if (inputs.schedules === null) out.push("backups");
  if (inputs.checklists === null) out.push("checklists");
  if (inputs.system === null) out.push("disk");
  return out;
}

/** The sentence saying which checks did not run, in the reader's language;
 * `null` when every check ran. */
export function uncheckedMessage(checks: AttentionCheck[], t: T = format): string | null {
  if (checks.length === 0) return null;
  const names: Record<AttentionCheck, string> = {
    triggers: t("triggers"),
    backups: t("scheduled backups"),
    checklists: t("checklists"),
    disk: t("disk space"),
  };
  return t("Could not check {what}: the server did not answer. Reload to try again.", {
    what: checks.map((c) => names[c]).join(", "),
  });
}

/** How many there are of each kind of entity. A table the installation made
 * for itself (`users`) is not counted: it is there before anything is built. */
export function entityCounts(entities: Entity[]): Record<EntityKind, number> {
  const counts: Record<EntityKind, number> = {
    table: 0,
    file_store: 0,
    trigger: 0,
    stream: 0,
    agent: 0,
    application: 0,
  };
  for (const entity of entities) {
    if (entity.kind === "table" && entity.id === "users") continue;
    if (entity.kind in counts) counts[entity.kind as EntityKind] += 1;
  }
  return counts;
}

/** One step of getting started: what to do, why, where, and whether it is
 * already done. */
export type StartStep = {
  key: string;
  title: string;
  why: string;
  href: string;
  done: boolean;
};

/** The first things an installation needs. Home shows them until every one is
 * done, ticking off those that are, so the list is also a record of progress
 * rather than a list of everything that could be done. `null` when a list
 * they are judged from did not load: a step cannot be called undone on no
 * evidence. */
export function gettingStarted(
  inputs: {
    entities: Entity[] | null;
    providers: ListLlmProvidersResponse | null;
    schedules: ListBackupSchedulesResponse | null;
  },
  t: T = format,
): StartStep[] | null {
  if (!inputs.entities || !inputs.providers || !inputs.schedules) return null;
  const counts = entityCounts(inputs.entities);
  return [
    {
      key: "table",
      title: t("Create a table"),
      why: t("Tables hold your data; everything else reads or writes them."),
      href: "#/tables",
      done: counts.table > 0,
    },
    {
      key: "application",
      title: t("Create an application"),
      why: t("An application is what your users see, on its own address."),
      href: "#/applications/new",
      done: counts.application > 0,
    },
    {
      key: "llm",
      title: t("Connect an LLM provider"),
      why: t("Agents, the admin copilot and translations need one."),
      href: "#/llm-providers/new",
      done: inputs.providers.length > 0,
    },
    {
      key: "backup",
      title: t("Schedule a backup"),
      why: t("A backup taken on a schedule is one you do not have to remember."),
      href: "#/settings/backup",
      done: inputs.schedules.length > 0,
    },
  ];
}

/** Whether the getting-started card is still worth its space. */
export function stillStarting(steps: StartStep[] | null): steps is StartStep[] {
  return steps !== null && steps.some((step) => !step.done);
}
