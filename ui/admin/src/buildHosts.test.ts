/**
 * Settings → Build hosts: what the tab tells an admin about a host, and the
 * command it hands them for the build host.
 */

import { describe, expect, it } from "vitest";

import {
  type BuildHost,
  TOKEN_FILE,
  agentCommand,
  agentSummary,
  hostBadge,
  hostProblem,
  hostReadiness,
  refusal,
  targetChoices,
} from "./buildHosts";
import type { ListFrameworksResponse } from "./client";

function host(over: Partial<BuildHost> = {}): BuildHost {
  return {
    id: "h1",
    name: "Mac mini",
    targets: ["ios", "ios_simulator"],
    created_at: "2026-10-22T10:00:00Z",
    last_seen_at: null,
    online: false,
    report: null,
    ...over,
  };
}

/** The translator as the source locale has it. */
const same = (text: string) => text;

describe("a build host's state", () => {
  it("says whether its agent was ever heard from, is now, or was turned away", () => {
    expect(hostBadge(same, host()).label).toBe("Never connected");
    expect(hostBadge(same, host({ last_seen_at: "2026-10-22T10:00:00Z" })).label).toBe("Offline");
    expect(hostBadge(same, host({ last_seen_at: "x", online: true })).label).toBe("Online");
    const old = host({ last_seen_at: "x", report: { refused: "install feldspar 0.2.0" } });
    expect(hostBadge(same, old).label).toBe("Needs update");
    expect(refusal(old)).toBe("install feldspar 0.2.0");
  });

  it("reads the agent's report: who it is and what each target lacks", () => {
    const reported = host({
      report: {
        protocol: 1,
        version: "0.1.0",
        os: "macos",
        arch: "aarch64",
        readiness: { ios: { missing: ["`pod` is not on the PATH."] } },
      },
    });
    expect(agentSummary(reported)).toBe("feldspar 0.1.0 on macos (aarch64)");
    expect(hostReadiness(reported)).toEqual([
      { target: "ios", missing: ["`pod` is not on the PATH."] },
      { target: "ios_simulator", missing: null },
    ]);
    expect(agentSummary(host())).toBeNull();
  });
});

describe("adding a build host", () => {
  it("offers each declared target once", () => {
    const target = (name: string, label: string) => ({ name, label, options: [], operations: [] });
    const frameworks = [
      { targets: [target("android", "Android app"), target("ios", "iOS app")] },
      { targets: [target("ios", "iOS app (other)")] },
    ] as unknown as ListFrameworksResponse;
    expect(targetChoices(frameworks)).toEqual([
      { name: "android", label: "Android app" },
      { name: "ios", label: "iOS app" },
    ]);
  });

  it("needs a name and a target", () => {
    expect(hostProblem(" ", ["ios"])).not.toBeNull();
    expect(hostProblem("Mac mini", [])).not.toBeNull();
    expect(hostProblem("Mac mini", ["ios"])).toBeNull();
  });

  it("starts the agent with the token in a file, never on its command line", () => {
    const command = agentCommand("https://apps.example.com");
    expect(command).toBe(
      `feldspar build-agent --server https://apps.example.com --token-file ${TOKEN_FILE}`,
    );
    expect(command).not.toContain("fsbh_");
  });
});
