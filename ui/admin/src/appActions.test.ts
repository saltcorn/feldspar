/**
 * Build and client-update state shared by the applications list and the sidebar.
 */

import { describe, expect, it } from "vitest";

import {
  INITIAL_APP_ACTIONS,
  buildFinished,
  buildStarted,
  buildStatus,
  jobResult,
  jobUnfinished,
  targetBuilding,
  targetFinished,
  targetStarted,
  targetNotReadyNotice,
  updateFinished,
  updateStarted,
} from "./appActions";

const app = { id: "a1", name: "Todo" };

describe("application builds", () => {
  it("is not built until a build succeeds", () => {
    let state = INITIAL_APP_ACTIONS;
    expect(buildStatus(state, "a1")).toBe("unbuilt");
    state = buildStarted(state, "a1");
    expect(buildStatus(state, "a1")).toBe("building");
    state = buildFinished(state, app, { ok: true, log: "  done\n" });
    expect(buildStatus(state, "a1")).toBe("built");
    expect(state.outcome).toEqual({ ok: true, title: "Build succeeded — Todo", text: "done" });
  });

  it("clears the previous news when a new build starts, and reports a failure", () => {
    let state = buildFinished(INITIAL_APP_ACTIONS, app, { ok: true, log: "" });
    expect(state.outcome?.text).toBe("Build succeeded.");
    state = buildStarted(state, "a1");
    expect(state.outcome).toBeNull();
    state = buildFinished(state, app, { ok: false, error: "tsc: 2 errors" });
    expect(buildStatus(state, "a1")).toBe("failed");
    expect(state.outcome).toEqual({ ok: false, title: "Build failed — Todo", text: "tsc: 2 errors" });
  });

  it("turns the news red when a checklist run after the build did not pass", () => {
    const passed = { status: "passed" };
    let state = buildFinished(INITIAL_APP_ACTIONS, app, { ok: true, log: "done", checks: [passed] });
    expect(state.outcome?.ok).toBe(true);
    state = buildFinished(state, app, {
      ok: true,
      log: "done",
      checks: [passed, { status: "failed" }],
    });
    expect(buildStatus(state, "a1")).toBe("built");
    expect(state.outcome).toEqual({
      ok: false,
      title: "Build succeeded — Todo, but 1 of 2 checklists did not pass",
      text: "done",
    });
  });

  it("names a deep clean as one, while ending in the same build state", () => {
    let state = buildStarted(INITIAL_APP_ACTIONS, "a1");
    state = buildFinished(state, app, { ok: true, log: "Deleted node_modules; reinstalling." }, "Deep clean");
    expect(buildStatus(state, "a1")).toBe("built");
    expect(state.outcome?.title).toBe("Deep clean succeeded — Todo");
    state = buildFinished(state, app, { ok: false, error: "npm ERR! 404" }, "Deep clean");
    expect(buildStatus(state, "a1")).toBe("failed");
    expect(state.outcome?.title).toBe("Deep clean failed — Todo");
  });
});

describe("client updates", () => {
  it("does not touch the build status, and says whether it scaffolded", () => {
    let state = buildFinished(INITIAL_APP_ACTIONS, app, { ok: true, log: "ok" });
    state = updateStarted(state, "a1");
    expect(state.updating.a1).toBe(true);
    state = updateFinished(state, app, { ok: true, scaffolded: true, log: "wrote 3 files" });
    expect(state.updating.a1).toBeUndefined();
    expect(buildStatus(state, "a1")).toBe("built");
    expect(state.outcome?.title).toBe("Project scaffolded — Todo");
    state = updateFinished(updateStarted(state, "a1"), app, { ok: true, scaffolded: false, log: "" });
    expect(state.outcome?.title).toBe("Generated code updated — Todo");
  });
});

describe("target builds", () => {
  const android = { name: "android", label: "Android APK" };

  it("tracks a target apart from the web build, and says where the file is", () => {
    let state = targetStarted(INITIAL_APP_ACTIONS, "a1", "android");
    expect(targetBuilding(state, "a1", "android")).toBe(true);
    // Building an APK changes nothing the application serves.
    expect(buildStatus(state, "a1")).toBe("unbuilt");
    state = targetFinished(state, app, android, {
      ok: true,
      store: "rn",
      artifact: "android/app/build/outputs/apk/release/app-release.apk",
      size: 5 * 1024 * 1024,
      logPath: "android/build-logs/android-20260927-101500.log",
      log: "BUILD SUCCESSFUL\n",
    });
    expect(targetBuilding(state, "a1", "android")).toBe(false);
    expect(state.outcome?.ok).toBe(true);
    expect(state.outcome?.title).toBe("Android APK built — Todo");
    expect(state.outcome?.text).toContain("app-release.apk (5.0 MB)");
    expect(state.outcome?.text).toContain('file store "rn"');
    expect(state.outcome?.text).toContain("BUILD SUCCESSFUL");
    expect(state.outcome?.text).toContain("android/build-logs/android-20260927-101500.log");
  });

  it("reports a failed target build with the tools' own message", () => {
    let state = targetStarted(INITIAL_APP_ACTIONS, "a1", "android");
    state = targetFinished(state, app, android, { ok: false, error: "SDK location not found" });
    expect(targetBuilding(state, "a1", "android")).toBe(false);
    expect(state.outcome).toEqual({
      ok: false,
      title: "Android APK build failed — Todo",
      text: "SDK location not found",
    });
  });
});

describe("a polled target job", () => {
  const job = {
    target: "android",
    label: "Android APK",
    store: "rn",
    log_path: "build-logs/android-1.log",
    started_at: "2026-09-27T10:15:00Z",
  };

  it("reads a finished job as the file and its log", () => {
    expect(
      jobResult({
        ...job,
        status: "succeeded",
        artifact: "android/app-release.apk",
        size: 7,
        log: "BUILD SUCCESSFUL",
      }),
    ).toEqual({
      ok: true,
      store: "rn",
      artifact: "android/app-release.apk",
      size: 7,
      logPath: "build-logs/android-1.log",
      log: "BUILD SUCCESSFUL",
    });
  });

  it("reads a failed job as its error, and a success with no file as a failure", () => {
    expect(jobResult({ ...job, status: "failed", error: "SDK location not found" })).toEqual({
      ok: false,
      error: "SDK location not found",
    });
    expect(jobResult({ ...job, status: "succeeded" }).ok).toBe(false);
  });

  it("is followed while it waits for a build host, and says which host built it", () => {
    expect(jobUnfinished({ status: "queued" })).toBe(true);
    expect(jobUnfinished({ status: "running" })).toBe(true);
    expect(jobUnfinished({ status: "succeeded" })).toBe(false);
    const result = jobResult({
      ...job,
      status: "succeeded",
      artifact: "ios-output/app.ipa",
      size: 7,
      build_host: "Mac mini",
    });
    const state = targetFinished(INITIAL_APP_ACTIONS, app, { name: "ios", label: "iOS app" }, result);
    expect(state.outcome?.text).toMatch(/^Built on the build host "Mac mini"\. ios-output\/app\.ipa/);
  });
});

describe("a target this server cannot build yet", () => {
  it("lists everything missing, one line each", () => {
    expect(
      targetNotReadyNotice(app, {
        label: "Android APK",
        readiness: {
          missing: [
            "`ANDROID_HOME` is not set. Set it under Settings → Modules.",
            "`JAVA_HOME` is not set.",
          ],
        },
      }),
    ).toEqual({
      ok: false,
      title: "Android APK cannot be built yet — Todo",
      text: "• `ANDROID_HOME` is not set. Set it under Settings → Modules.\n• `JAVA_HOME` is not set.",
    });
  });
});
