import { describe, expect, it } from "vitest";

import {
  formatBytes,
  formatUptime,
  restarted,
  shortCommit,
  usage,
  versionLine,
} from "./systemInfo";

describe("formatBytes", () => {
  it("uses binary units with one decimal", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KiB");
    expect(formatBytes(3 * 1024 ** 3)).toBe("3.0 GiB");
  });

  it("shows a dash for a reading this platform could not take", () => {
    expect(formatBytes(null)).toBe("—");
    expect(formatBytes(undefined)).toBe("—");
  });
});

describe("formatUptime", () => {
  it("shows the two largest units", () => {
    expect(formatUptime(42)).toBe("42s");
    expect(formatUptime(125)).toBe("2m 5s");
    expect(formatUptime(2 * 3600 + 5 * 60 + 9)).toBe("2h 5m");
    expect(formatUptime(3 * 86400 + 4 * 3600 + 59)).toBe("3d 4h");
  });
});

describe("usage", () => {
  it("is the total less what is available", () => {
    expect(usage(1000, 250)).toEqual({ used: 750, total: 1000, percent: 75 });
  });

  it("is nothing when a number is missing", () => {
    expect(usage(null, 10)).toBeNull();
    expect(usage(1000, undefined)).toBeNull();
    expect(usage(0, 0)).toBeNull();
  });
});

describe("shortCommit", () => {
  it("keeps the dirty marker", () => {
    const hash = "6f573d09a1b2c3d4e5f60718293a4b5c6d7e8f90";
    expect(shortCommit(hash)).toBe("6f573d09a1b2");
    expect(shortCommit(`${hash}-dirty`)).toBe("6f573d09a1b2-dirty");
  });
});

describe("versionLine", () => {
  it("names the build a release build stamped", () => {
    expect(
      versionLine({ version: "0.1.0", commit: "abc123", build_time: "2026-10-30T12:00:00Z" }),
    ).toBe("feldspar 0.1.0 · abc123 · built 2026-10-30T12:00:00Z");
  });

  it("says so for a binary built any other way", () => {
    expect(versionLine({ version: "0.1.0", commit: null, build_time: null })).toBe(
      "feldspar 0.1.0 · not a release build",
    );
  });
});

describe("restarted", () => {
  const before = { pid: 100, started_at: "2026-10-30T10:00:00Z" };

  it("is false while the same process answers", () => {
    expect(restarted(before, { ...before })).toBe(false);
  });

  it("is true for a new process, even one that got the same pid", () => {
    // In a container the server is often pid 1 every time.
    expect(restarted(before, { pid: 100, started_at: "2026-10-30T10:05:00Z" })).toBe(true);
    expect(restarted(before, { pid: 101, started_at: before.started_at })).toBe(true);
  });
});
