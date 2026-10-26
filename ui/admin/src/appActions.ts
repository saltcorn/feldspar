// What an application is doing right now: building, having its generated code
// rewritten, and the news the last of those left behind.
//
// Both are started from two places — a row on the applications list and the
// current application's links in the sidebar — and an admin who presses Build in
// the sidebar and then opens the list should see the same build in progress
// there, not a row claiming the app was never built. So the state lives here,
// outside any screen, the way popped-out chats do (`chatWindows.ts`), and a build
// outlives the screen it was started from: it finishes, and says so, wherever the
// admin has got to by then.
//
// The server keeps no persisted build status (design §13.2), so this is the only
// record of one: a freshly loaded app is **not built yet** rather than pretending
// a save deployed anything. As in `chatWindows.ts`, the transitions are pure and
// the store is a thin shell around them, so the interesting part is testable
// without a browser.

import { useSyncExternalStore } from "react";

import { api, errorMessage } from "./api";
import type { GetApplicationTargetBuildResponse, ListApplicationsResponse } from "./client";
import { formatSize } from "./fileSelection";
import type { Notice } from "./notice";

type AppItem = ListApplicationsResponse[number];
/** One build of a target, as the server reports it while it runs and after. */
type TargetJob = GetApplicationTargetBuildResponse;

/** Per-app build state. */
export type BuildStatus = "unbuilt" | "building" | "built" | "failed";

export type AppActionsState = {
  /** Build state by application id; an app with no entry is `unbuilt`. */
  build: Record<string, BuildStatus>;
  /** Applications whose generated code is being rewritten. Separate from the
   * build status: regenerating does not build, so it must not claim an app is
   * built — nor forget that it was. */
  updating: Record<string, true>;
  /** Target builds in progress, keyed by {@link targetKey}. Separate from the
   * build status: an APK is a file to take away, and building one changes
   * nothing the application serves. */
  targets: Record<string, true>;
  /** The most recent outcome, shown until dismissed. */
  outcome: Notice | null;
  /** Bumped whenever the set of applications (or their agents) changes without
   * a route change, so the sidebar knows to fetch them again. */
  version: number;
};

export const INITIAL_APP_ACTIONS: AppActionsState = {
  build: {},
  updating: {},
  targets: {},
  outcome: null,
  version: 0,
};

/** The build state of one application. */
export function buildStatus(state: AppActionsState, appId: string): BuildStatus {
  return state.build[appId] ?? "unbuilt";
}

/** A build has started: the app is building, and the previous news is stale. */
export function buildStarted(state: AppActionsState, appId: string): AppActionsState {
  return { ...state, build: { ...state.build, [appId]: "building" }, outcome: null };
}

/** A build has finished, one way or the other. `action` names what the admin
 * pressed — a Deep clean is a build too, and ends in the same state, but the
 * news should say which one it was. */
export function buildFinished(
  state: AppActionsState,
  app: Pick<AppItem, "id" | "name">,
  result: { ok: true; log: string } | { ok: false; error: string },
  action = "Build",
): AppActionsState {
  return {
    ...state,
    build: { ...state.build, [app.id]: result.ok ? "built" : "failed" },
    outcome: result.ok
      ? {
          ok: true,
          title: `${action} succeeded — ${app.name}`,
          text: result.log.trim() || `${action} succeeded.`,
        }
      : { ok: false, title: `${action} failed — ${app.name}`, text: result.error },
  };
}

/** A rewrite of the generated code has started. */
export function updateStarted(state: AppActionsState, appId: string): AppActionsState {
  return { ...state, updating: { ...state.updating, [appId]: true }, outcome: null };
}

/** A rewrite of the generated code has finished. It says whether the server
 * rewrote the generated files or rescaffolded an emptied project, because they
 * are not the same news. */
export function updateFinished(
  state: AppActionsState,
  app: Pick<AppItem, "id" | "name">,
  result: { ok: true; scaffolded: boolean; log: string } | { ok: false; error: string },
): AppActionsState {
  const updating = { ...state.updating };
  delete updating[app.id];
  return {
    ...state,
    updating,
    outcome: result.ok
      ? {
          ok: true,
          title: result.scaffolded
            ? `Project scaffolded — ${app.name}`
            : `Generated code updated — ${app.name}`,
          text: result.log,
        }
      : {
          ok: false,
          title: `Could not update the generated code — ${app.name}`,
          text: result.error,
        },
  };
}

/** The key one application's one target is tracked under. */
export function targetKey(appId: string, target: string): string {
  return `${appId}/${target}`;
}

/** Whether a target of an application is being built. */
export function targetBuilding(state: AppActionsState, appId: string, target: string): boolean {
  return Boolean(state.targets[targetKey(appId, target)]);
}

/** A target build has started. */
export function targetStarted(
  state: AppActionsState,
  appId: string,
  target: string,
): AppActionsState {
  return {
    ...state,
    targets: { ...state.targets, [targetKey(appId, target)]: true },
    outcome: null,
  };
}

/** Why a target is not ready to build here, as the news: everything missing,
 * one line each, so the admin fixes it all before pressing the button again. The
 * server refuses the same build with the same reasons; this says so without
 * asking. */
export function targetNotReadyNotice(
  app: Pick<AppItem, "name">,
  target: { label: string; readiness: { missing: string[] } },
): Notice {
  return {
    ok: false,
    title: `${target.label} cannot be built yet — ${app.name}`,
    text: target.readiness.missing.map((line) => `• ${line}`).join("\n"),
  };
}

/** What a finished job means for the news: the file, or why there is none. */
export type TargetResult =
  | {
      ok: true;
      store: string;
      artifact: string;
      size: number;
      logPath: string;
      log: string;
      buildHost?: string;
    }
  | { ok: false; error: string };

/** Whether a job is still to finish: building, or waiting for a build host to
 * take it. */
export function jobUnfinished(job: Pick<TargetJob, "status">): boolean {
  return job.status === "running" || job.status === "queued";
}

/** A finished job as a {@link TargetResult}. A job that says it succeeded but
 * names no artifact is reported as the failure it is, not as a file of size 0. */
export function jobResult(job: TargetJob): TargetResult {
  if (job.status === "succeeded" && job.artifact) {
    return {
      ok: true,
      store: job.store,
      artifact: job.artifact,
      size: job.size ?? 0,
      logPath: job.log_path,
      log: job.log ?? "",
      buildHost: job.build_host ?? undefined,
    };
  }
  return { ok: false, error: job.error || `The build ended as "${job.status}".` };
}

/** A target build has finished. The news leads with where the file is, because
 * that is what the admin wants next; then where the whole log is, and its end. */
export function targetFinished(
  state: AppActionsState,
  app: Pick<AppItem, "id" | "name">,
  target: { name: string; label: string },
  result: TargetResult,
): AppActionsState {
  const targets = { ...state.targets };
  delete targets[targetKey(app.id, target.name)];
  return {
    ...state,
    targets,
    outcome: result.ok
      ? {
          ok: true,
          title: `${target.label} built — ${app.name}`,
          text:
            (result.buildHost ? `Built on the build host "${result.buildHost}". ` : "") +
            `${result.artifact} (${formatSize(result.size)}) in file store "${result.store}". ` +
            `Download it from Files.\nThe whole log: ${result.logPath}\n\n${result.log.trim()}`,
        }
      : { ok: false, title: `${target.label} build failed — ${app.name}`, text: result.error },
  };
}

/* ------------------------------------------------------------------ *
 * The store.
 * ------------------------------------------------------------------ */

let state: AppActionsState = INITIAL_APP_ACTIONS;
const listeners = new Set<() => void>();

function publish(next: AppActionsState): void {
  if (next === state) return;
  state = next;
  for (const listener of listeners) listener();
}

function current(): AppActionsState {
  return state;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Subscribe a component to builds, updates and their outcome. */
export function useAppActions(): AppActionsState {
  return useSyncExternalStore(subscribe, current, current);
}

/** Build (and mount) an application. */
export async function buildApplication(app: AppItem): Promise<void> {
  publish(buildStarted(state, app.id));
  try {
    const report = await api.buildApplication(app.id);
    publish(buildFinished(state, app, { ok: true, log: report.log }));
  } catch (err) {
    publish(buildFinished(state, app, { ok: false, error: errorMessage(err, "The build failed.") }));
  }
}

/** How often a running target build is asked how it is going. */
const POLL_MS = 3000;

/** Target builds this page is already following, so a build is polled once
 * however many buttons and screens ask about it. */
const following = new Set<string>();

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Poll a running job until it is done, then publish the news. */
async function follow(
  app: AppItem,
  target: { name: string; label: string },
  job: TargetJob,
): Promise<void> {
  const key = targetKey(app.id, target.name);
  if (following.has(key)) return;
  following.add(key);
  try {
    let current = job;
    while (jobUnfinished(current)) {
      await sleep(POLL_MS);
      current = await api.getApplicationTargetBuild(app.id, target.name);
    }
    publish(targetFinished(state, app, target, jobResult(current)));
  } catch (err) {
    publish(
      targetFinished(state, app, target, {
        ok: false,
        error: errorMessage(err, `Lost track of the ${target.label} build.`),
      }),
    );
  } finally {
    following.delete(key);
  }
}

/** Build one of the targets an application's framework offers — an Android APK.
 * The server starts it and answers at once; this then polls until it is done, so
 * a build of a quarter of an hour is not one request any proxy may cut off. */
export async function buildApplicationTarget(
  app: AppItem,
  target: { name: string; label: string },
): Promise<void> {
  publish(targetStarted(state, app.id, target.name));
  try {
    const job = await api.buildApplicationTarget(app.id, target.name);
    await follow(app, target, job);
  } catch (err) {
    publish(
      targetFinished(state, app, target, {
        ok: false,
        error: errorMessage(err, `The ${target.label} build could not be started.`),
      }),
    );
  }
}

/** Pick up the builds of `app`'s targets that are still running — after a
 * reload, or in a second tab — so their buttons say so and their news arrives.
 * A target that was never built, or whose build has finished, needs nothing. */
export async function resumeTargetBuilds(app: AppItem): Promise<void> {
  for (const target of app.targets) {
    if (targetBuilding(state, app.id, target.name)) continue;
    let job: TargetJob;
    try {
      job = await api.getApplicationTargetBuild(app.id, target.name);
    } catch {
      continue;
    }
    if (!jobUnfinished(job)) continue;
    publish(targetStarted(state, app.id, target.name));
    void follow(app, target, job);
  }
}

/** Deep clean an application: the server deletes its installed dependencies
 * (`node_modules`) and builds it, which installs them from scratch. Shown as a
 * build in progress, because that is what most of it is. */
export async function deepCleanApplication(app: AppItem): Promise<void> {
  publish(buildStarted(state, app.id));
  try {
    const report = await api.deepCleanApplication(app.id);
    publish(buildFinished(state, app, { ok: true, log: report.log }, "Deep clean"));
  } catch (err) {
    publish(
      buildFinished(
        state,
        app,
        { ok: false, error: errorMessage(err, "The deep clean failed.") },
        "Deep clean",
      ),
    );
  }
}

/** Rewrite an application's generated code (`src/feldspar/**`) without building
 * it. The server does this by itself whenever the API definition changes, so this
 * is the "now, please" case: a store that was unreachable when a table changed, or
 * a project directory that was emptied — which the server rescaffolds. */
export async function updateApplicationClient(app: AppItem): Promise<void> {
  publish(updateStarted(state, app.id));
  try {
    const report = await api.updateApplicationClient(app.id);
    publish(updateFinished(state, app, { ok: true, scaffolded: report.scaffolded, log: report.log }));
  } catch (err) {
    publish(
      updateFinished(state, app, {
        ok: false,
        error: errorMessage(err, "The generated code could not be rewritten."),
      }),
    );
  }
}

/** Show a message about an application (a scaffold, a deletion). */
export function showAppOutcome(outcome: Notice | null): void {
  publish({ ...state, outcome });
}

/** Say that applications or agents were added, removed or renamed. */
export function noteApplicationsChanged(): void {
  publish({ ...state, version: state.version + 1 });
}
