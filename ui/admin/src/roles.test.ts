/**
 * The role helpers (TODO F1.6): "Public" is role 100 named, never a second
 * stored fact.
 */

import { describe, expect, it } from "vitest";

import { PUBLIC_ROLE, isPublic, roleLabel, roleOptions } from "./roles";

describe("public endpoints", () => {
  it("is role 100 and nothing else", () => {
    expect(PUBLIC_ROLE).toBe(100);
    expect(isPublic(100)).toBe(true);
    for (const role of [1, 40, 80, 99, null, undefined]) expect(isPublic(role)).toBe(false);
  });
});

describe("role labels", () => {
  it("names a listed role and numbers one that is not", () => {
    const roles = [{ role: 100, name: "public" }] as unknown as Parameters<typeof roleLabel>[1];
    expect(roleLabel(100, roles)).toBe("public (100)");
    expect(roleLabel(7, roles)).toBe("Role 7");
    expect(roleOptions(7, roles).map((r) => r.role)).toEqual([7, 100]);
  });
});
