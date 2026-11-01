/**
 * Roles are chosen from a list, never typed as a number.
 *
 * Two claims:
 *
 *   - `roleOptions` offers what the server lists, keeps a current role the
 *     server did *not* list, and adds nothing at all for a field with no role
 *     set — the blank's meaning differs per screen, so it is the caller's;
 *   - no screen renders a minimum role as a number box. That is checked against
 *     the sources rather than a rendered form because there is no DOM here, and
 *     the regression it guards against is exactly a hand-written
 *     `<Form.Control type="number">` creeping back into a new screen.
 */

import { describe, expect, it } from "vitest";

import { roleOptions } from "./roles";

const roles = [
  { role: 100, name: "Public", description: "", builtin: true, require_second_factor: false },
  { role: 1, name: "Admin", description: "", builtin: true, require_second_factor: false },
  { role: 40, name: "Staff", description: "", builtin: false, require_second_factor: false },
];

describe("roleOptions", () => {
  it("offers every listed role, lowest number first", () => {
    expect(roleOptions(1, roles)).toEqual([
      { role: 1, name: "Admin" },
      { role: 40, name: "Staff" },
      { role: 100, name: "Public" },
    ]);
  });

  it("keeps a current role the server no longer lists", () => {
    // Snapping to a listed role would hand access to a wider audience on the
    // next save, without saying so.
    expect(roleOptions(60, roles)).toContainEqual({ role: 60, name: "Role 60" });
    expect(roleOptions(60, roles).map((r) => r.role)).toEqual([1, 40, 60, 100]);
  });

  it("adds nothing for a field with no role set", () => {
    expect(roleOptions(null, roles).map((r) => r.role)).toEqual([1, 40, 100]);
  });

  it("survives roles that have not loaded yet", () => {
    expect(roleOptions(40, null)).toEqual([{ role: 40, name: "Role 40" }]);
    expect(roleOptions(null, null)).toEqual([]);
  });
});

describe("the screens", () => {
  const sources = Object.entries(
    import.meta.glob("./screens/*.tsx", {
      query: "?raw",
      import: "default",
      eager: true,
    }) as Record<string, string>,
  );

  it("never render a minimum role as a number box", () => {
    expect(sources.length).toBeGreaterThan(5);
    const offenders = sources.filter(([, text]) =>
      [...text.matchAll(/[Mm]inimum role/g)].some((m) =>
        // A number input belonging to this label would be inside the same
        // group; anything further off is some other field entirely.
        /type="number"/.test(text.slice(m.index, m.index + 400)),
      ),
    );
    expect(offenders.map(([name]) => name)).toEqual([]);
  });

  it("render every minimum role through a select", () => {
    const screens = sources.filter(([, text]) => /[Mm]inimum role/.test(text));
    expect(screens.length).toBeGreaterThan(0);
    for (const [name, text] of screens) {
      // Either the shared component, or — where the label is laid out by hand,
      // as in the custom-query rows — a plain select over `roleOptions`.
      const select = /RoleSelect/.test(text) || /roleOptions\(/.test(text);
      // A screen that only *reports* a role, rather than editing one, has no
      // input to check.
      const edits = /value=\{[^}]*[Mm]in[Rr]ole/.test(text) || /RoleSelect/.test(text);
      expect(select || !edits, `${name} edits a minimum role without a select`).toBe(true);
    }
  });
});
