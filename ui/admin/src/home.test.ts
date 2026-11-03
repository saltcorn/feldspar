// Home's three answers (`home.ts`): what is broken, how much is here, and
// what to do next. The claims worth pinning are the ones that would quietly
// mislead: a list that did not load must not read as "all is well", and the
// installation's own `users` table must not count as the admin's first table.

import { describe, expect, it } from "vitest";

import type { ListBackupSchedulesResponse, ListTriggersResponse } from "./client";
import type { Entity } from "./entities";
import {
  attention,
  backupFailing,
  entityCounts,
  gettingStarted,
  stillStarting,
  uncheckedChecks,
  uncheckedMessage,
} from "./home";

type Schedule = ListBackupSchedulesResponse[number];
type Trigger = ListTriggersResponse[number];

const schedule = (over: Partial<Schedule>): Schedule =>
  ({ id: "s1", location: "/backups", ...over }) as Schedule;
const trigger = (over: Partial<Trigger>): Trigger =>
  ({ id: "t1", name: "nightly", enabled: true, when: "daily", ...over }) as Trigger;
const entity = (kind: string, id: string): Entity =>
  ({ kind, id, name: id, description: "", details: [], access: "Admin", runnable: true }) as Entity;

describe("backupFailing", () => {
  it("is the latest attempt failing, not any error ever", () => {
    expect(backupFailing(schedule({ last_error: "disk full" }))).toBe(true);
    expect(
      backupFailing(
        schedule({
          last_error: "disk full",
          last_attempt_at: "2026-11-02T00:00:00Z",
          last_success_at: "2026-11-01T00:00:00Z",
        }),
      ),
    ).toBe(true);
    // An error left over from an attempt before the last success.
    expect(
      backupFailing(
        schedule({
          last_error: "disk full",
          last_attempt_at: "2026-11-02T00:00:00Z",
          last_success_at: "2026-11-02T00:00:00Z",
        }),
      ),
    ).toBe(false);
    expect(backupFailing(schedule({}))).toBe(false);
  });
});

describe("attention", () => {
  const none = { triggers: [], schedules: [], checklists: [], system: null };

  it("lists an enabled trigger with a problem, linking to its form", () => {
    const items = attention({
      ...none,
      triggers: [trigger({ error: "unknown table" }), trigger({ id: "t2", error: "x", enabled: false })],
    });
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ tone: "red", detail: "unknown table", href: "#/triggers/t1/edit" });
  });

  it("links a failed checklist run to the run, under its application when it has one", () => {
    const items = attention({
      ...none,
      checklists: [
        { id: "c1", name: "smoke", enabled: true, application: "a1", last_run: { id: "r1", status: "failed", finished_at: "" } },
        { id: "c2", name: "ok", enabled: true, application: null, last_run: { id: "r2", status: "passed", finished_at: "" } },
      ] as never,
    });
    expect(items.map((i) => i.href)).toEqual(["#/applications/a1/checklists/runs/r1"]);
  });

  it("warns about a nearly full disk after the failures", () => {
    const items = attention({
      ...none,
      schedules: [schedule({ last_error: "refused" })],
      system: { disk: { path: "/", total_bytes: 100, available_bytes: 5 } } as never,
    });
    expect(items.map((i) => i.tone)).toEqual(["red", "yellow"]);
    expect(items[1].href).toBe("#/settings/system");
  });

  it("reports nothing for lists that did not load", () => {
    expect(attention({ triggers: null, schedules: null, checklists: null, system: null })).toEqual([]);
  });
});

describe("entityCounts", () => {
  it("counts by kind, leaving out the installation's own users table", () => {
    const counts = entityCounts([entity("table", "users"), entity("table", "books"), entity("trigger", "t")]);
    expect(counts.table).toBe(1);
    expect(counts.trigger).toBe(1);
    expect(counts.application).toBe(0);
  });
});

describe("gettingStarted", () => {
  it("ticks off what is done and stays until everything is", () => {
    const steps = gettingStarted({
      entities: [entity("table", "users"), entity("table", "books")],
      providers: [],
      schedules: [],
    });
    expect(steps?.map((s) => [s.key, s.done])).toEqual([
      ["table", true],
      ["application", false],
      ["llm", false],
      ["backup", false],
    ]);
    expect(stillStarting(steps)).toBe(true);
  });

  it("goes once every step is done", () => {
    const steps = gettingStarted({
      entities: [entity("table", "books"), entity("application", "a1")],
      providers: [{ id: "p" }] as never,
      schedules: [schedule({})],
    });
    expect(stillStarting(steps)).toBe(false);
  });

  it("is not shown on no evidence", () => {
    expect(gettingStarted({ entities: null, providers: [], schedules: [] })).toBeNull();
    expect(stillStarting(null)).toBe(false);
  });
});

describe("uncheckedChecks", () => {
  it("names each check whose list did not load, so silence is not read as all clear", () => {
    expect(uncheckedChecks({ triggers: null, schedules: [], checklists: [], system: null })).toEqual([
      "triggers",
      "disk",
    ]);
    expect(uncheckedChecks({ triggers: [], schedules: [], checklists: [], system: {} as never })).toEqual(
      [],
    );
  });

  it("says which checks did not run, and nothing when all ran", () => {
    expect(uncheckedMessage(["triggers", "backups"])).toBe(
      "Could not check triggers, scheduled backups: the server did not answer. Reload to try again.",
    );
    expect(uncheckedMessage([])).toBeNull();
  });
});
