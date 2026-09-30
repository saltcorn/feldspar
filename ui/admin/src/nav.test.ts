/**
 * The sidebar's shape.
 *
 * Two sections own a screen that is not one of their own entries — Agents owns
 * the LLM providers list, Users owns the roles list — and each of those screens
 * is reached from a button in its owner's page header. The claim worth pinning
 * down is that such a screen has no sidebar entry of its own *and* still lights
 * up its owner: without the second half, opening it would leave the sidebar
 * pointing nowhere and an admin with no idea where they are.
 */

import { describe, expect, it } from "vitest";

import { NAV } from "./App";

/** Which entry, if any, the sidebar marks as current for a route. */
function activeLabels(route: string): string[] {
  return NAV.filter((item) => item.matches.some((prefix) => route.startsWith(prefix))).map(
    (item) => item.label,
  );
}

describe("the admin sidebar", () => {
  it("has no entry for roles", () => {
    expect(NAV.map((item) => item.label)).not.toContain("Roles");
    expect(NAV.map((item) => item.href)).not.toContain("#/roles");
  });

  it("marks Users as the section the roles screen belongs to", () => {
    expect(activeLabels("/roles")).toEqual(["Users"]);
    expect(activeLabels("/users")).toEqual(["Users"]);
  });

  it("keeps the same arrangement for agents and their providers", () => {
    expect(NAV.map((item) => item.label)).not.toContain("LLM providers");
    expect(activeLabels("/llm-providers")).toEqual(["Agents"]);
  });

  it("keeps the same arrangement for tables and their database connections", () => {
    // A connection exists to put tables in the tables list, so the list of them
    // is reached from a button on that screen and lights that section up.
    expect(NAV.map((item) => item.label)).not.toContain("Database connections");
    expect(activeLabels("/db-connections")).toEqual(["Tables"]);
    expect(activeLabels("/tables")).toEqual(["Tables"]);
  });

  /** Settings is the installation's own section, so it *does* have an entry —
   * and one entry however many sections of settings the server declares. */
  it("has a single entry for settings", () => {
    expect(NAV.filter((item) => item.label === "Settings")).toHaveLength(1);
    expect(activeLabels("/settings")).toEqual(["Settings"]);
  });

  /** Applications are the sidebar's other section, about one application at a
   * time (`appNav.ts`), so the Data Layer list has no entry for them and no
   * application route lights any of its entries up. */
  it("keeps applications out of the Data Layer section", () => {
    expect(NAV.map((item) => item.label)).not.toContain("Applications");
    expect(activeLabels("/applications")).toEqual([]);
    expect(activeLabels("/applications/a1/views")).toEqual([]);
  });

  it("gives every entry a route that lights it up", () => {
    // Every entry that is a screen of this SPA; Analytics is a page of its own.
    for (const item of NAV.filter((entry) => entry.href.startsWith("#"))) {
      expect(activeLabels(item.href.replace(/^#/, ""))).toContain(item.label);
    }
  });

  /** The Analytics UI is a bundle of its own, so its entry is a page and not
   * a hash route; it sits beside *Predictive models*, which A3 retires. */
  it("links to the Analytics UI beside Predictive models", () => {
    const labels = NAV.map((item) => item.label);
    expect(labels.indexOf("Analytics")).toBe(labels.indexOf("Predictive models") - 1);
    expect(NAV.find((item) => item.label === "Analytics")?.href).toBe("/analytics/");
    expect(activeLabels("/models")).toEqual(["Predictive models"]);
  });
});
