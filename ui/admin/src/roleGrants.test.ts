/**
 * Role grants on the users screen (TODO-payments P3.2): a grant's state, its
 * source, and the body the "Grant a role" form sends.
 */

import { describe, expect, it } from "vitest";

import {
  grantBody,
  grantSourceLabel,
  grantState,
  newGrantForm,
  servedAbove,
  type RoleGrantRow,
} from "./roleGrants";

const now = new Date("2026-10-22T12:00:00Z");

function grant(over: Partial<RoleGrantRow> = {}): RoleGrantRow {
  return {
    id: "g1",
    role: 60,
    source: "admin",
    source_ref: "8d6c…",
    starts_at: "2026-10-01T00:00:00Z",
    expires_at: "2026-11-01T00:00:00Z",
    revoked_at: null,
    note: null,
    in_force: true,
    ...over,
  };
}

describe("grant state", () => {
  it("reads in force, not started, ended and revoked off the dates", () => {
    expect(grantState(grant(), now)).toBe("in_force");
    expect(grantState(grant({ expires_at: null }), now)).toBe("in_force");
    expect(grantState(grant({ starts_at: "2026-10-23T00:00:00Z" }), now)).toBe("pending");
    expect(grantState(grant({ expires_at: "2026-10-22T11:59:59Z" }), now)).toBe("ended");
    // Revoked wins over everything, including a period still running.
    expect(grantState(grant({ revoked_at: "2026-10-20T00:00:00Z" }), now)).toBe("revoked");
  });
});

describe("grant source", () => {
  it("hides an admin grant's random reference and shows everyone else's", () => {
    expect(grantSourceLabel(grant())).toBe("By an admin");
    expect(grantSourceLabel(grant({ source: "grant_role", source_ref: "trial:u1" }))).toBe(
      "Trigger: trial:u1",
    );
    expect(grantSourceLabel(grant({ source: "payments", source_ref: "stripe:sub_1" }))).toBe(
      "payments: stripe:sub_1",
    );
  });
});

describe("the grant form", () => {
  it("needs a role, takes whole days or blank, and trims the note", () => {
    expect(grantBody(newGrantForm())).toBe("Choose a role to grant.");
    expect(grantBody({ role: 60, days: "30", note: " comp " })).toEqual({
      role: 60,
      expires_in_days: 30,
      note: "comp",
    });
    expect(grantBody({ role: 60, days: "", note: "" })).toEqual({
      role: 60,
      expires_in_days: null,
      note: null,
    });
    for (const days of ["0", "-3", "1.5", "soon"]) {
      expect(typeof grantBody({ role: 60, days, note: "" })).toBe("string");
    }
    expect(typeof grantBody({ role: 100, days: "", note: "" })).toBe("string");
  });

  it("says when the effective role is above the base", () => {
    expect(servedAbove({ base_role: 80, effective_role: 60, grants: [] })).toBe(true);
    expect(servedAbove({ base_role: 80, effective_role: 80, grants: [] })).toBe(false);
  });
});
