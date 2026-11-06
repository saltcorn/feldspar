/**
 * What Settings → Development says about Claude Code: how the executable was
 * found, the badge, and when the panel reads the status again.
 */

import { describe, expect, it } from "vitest";

import { sourceLabel, statusBadge, statusKey, terminalTicked } from "./claudeStatus";

describe("the Claude Code status", () => {
  it("names how each executable was found", () => {
    expect(sourceLabel("option")).toBe("named by the server's --claude-cli option");
    expect(sourceLabel("setting")).toBe("named by the setting above");
    expect(sourceLabel("path")).toBe("found on the server's PATH");
    expect(sourceLabel("usual")).toBe("found in a usual install location");
    expect(sourceLabel(null)).toBeNull();
  });

  it("is ready only when the executable answered with a version", () => {
    expect(statusBadge({ path: "/x/claude", version: "2.1.0 (Claude Code)" }).label).toBe("Ready");
    expect(statusBadge({ path: "/x/claude", version: null }).label).toBe(
      "Found, but it did not answer",
    );
    expect(statusBadge({ path: null, version: null }).label).toBe("Not found");
  });

  it("is shown while the terminal switch is ticked", () => {
    expect(terminalTicked({ ide_terminal_enabled: "true" })).toBe(true);
    expect(terminalTicked({ ide_terminal_enabled: "false" })).toBe(false);
    expect(terminalTicked({})).toBe(false);
  });

  /** A save that changes the path or the switch reads the status again. */
  it("is read again when what it depends on is saved", () => {
    const before = statusKey({ ide_terminal_enabled: "true", claude_code_path: "" });
    expect(statusKey({ ide_terminal_enabled: "true", claude_code_path: "/opt/claude" })).not.toBe(
      before,
    );
    expect(statusKey({ ide_terminal_enabled: "true", claude_code_path: "", log_sql: "true" })).toBe(
      before,
    );
  });
});
