import { describe, expect, it } from "vitest";

import { THEME_KEY, pickTheme } from "./theme";

describe("the colour scheme", () => {
  it("follows the admin UI's stored choice, and the OS without one", () => {
    // One key for both bundles: the toggle in either is one setting.
    expect(THEME_KEY).toBe("saltcorn-admin-theme");
    expect(pickTheme("dark", false)).toBe("dark");
    expect(pickTheme("light", true)).toBe("light");
    expect(pickTheme(null, true)).toBe("dark");
    expect(pickTheme("purple", false)).toBe("light");
  });
});
