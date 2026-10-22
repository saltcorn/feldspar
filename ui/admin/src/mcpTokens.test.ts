/**
 * The MCP token panel's model: what an admin mints, and what they are told.
 *
 * Three things here can go wrong quietly, and each has a test:
 *
 * - **The grant checkboxes are the copilot's**, not a second set with the same
 *   names. If this panel ever grew its own labels, an admin would be reading one
 *   description of `allow_drop` on the agent screen and another here — and the
 *   flag they decide about is the same flag.
 * - **The `claude mcp add` line is the setup step**, and it is the one that gets
 *   typed wrong. It has to carry this server's own URL and the token that is
 *   still on screen, because the alternative is an admin transcribing both from
 *   two places.
 * - **A token minted against a disabled server does not work.** The switch is
 *   directly above the panel, so the panel has to read the *stored* value —
 *   which is what `mcpEnabled` is for, and what the round trip below pins down.
 */

import { describe, expect, it } from "vitest";

import {
  FALLBACK_GRANT_FIELDS,
  GRANT_KEYS,
  TOKEN_PLACEHOLDER,
  claudeMcpAddLine,
  defaultGrants,
  emptyMint,
  grantFields,
  grantedKeys,
  holdsAll,
  mcpEnabled,
  mcpUrl,
  mintProblem,
  mintRequest,
  shortGrantLabel,
  tokenBadge,
  when,
  type ApiToken,
} from "./mcpTokens";
import { readConfig } from "./settings";

/** A `listAgentTraits` response, trimmed to what this panel reads: the
 * copilot's six checkboxes, in the order the server declares them. */
const traits = [
  {
    name: "admin_copilot",
    description: "Build the application",
    config_spec: [
      {
        name: "allow_create",
        label: "May create tables, fields, triggers and custom SQL queries",
        type: "bool",
        required: false,
        default: true,
        options: [],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
      },
      {
        name: "allow_edit",
        label: "May change existing tables, fields, triggers and custom SQL queries",
        type: "bool",
        required: false,
        default: true,
        options: [],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
      },
      {
        name: "allow_drop",
        label: "May drop tables and fields, and delete triggers and custom SQL queries",
        type: "bool",
        required: false,
        default: false,
        options: [],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
      },
      {
        name: "allow_access_changes",
        label: "May change access rules (roles, ownership formula, row-level security)",
        type: "bool",
        required: false,
        default: false,
        options: [],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
      },
      {
        name: "allow_triggers",
        label: "May work on triggers",
        type: "bool",
        required: false,
        default: true,
        options: [],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
      },
      {
        name: "allow_applications",
        label: "May work on applications' custom SQL queries",
        type: "bool",
        required: false,
        default: true,
        options: [],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
      },
    ],
  },
  { name: "other_trait", description: "Not this one", config_spec: [] },
];

/** One row of `listApiTokens`, with everything a badge or a cell reads. */
function token(overrides: Partial<ApiToken> = {}): ApiToken {
  return {
    id: "11111111-1111-1111-1111-111111111111",
    user_id: "22222222-2222-2222-2222-222222222222",
    label: "claude-code on my laptop",
    grants: {
      allow_create: true,
      allow_edit: true,
      allow_drop: false,
      allow_access_changes: false,
      allow_triggers: true,
      allow_applications: true,
    },
    created_at: "2026-09-03T10:00:00Z",
    expires_at: null,
    last_used_at: null,
    revoked_at: null,
    live: true,
    ...overrides,
  };
}

describe("the grant checkboxes", () => {
  it("are the copilot's own labels, in the copilot's own order", () => {
    const fields = grantFields(traits);
    expect(fields.map((field) => field.name)).toEqual([...GRANT_KEYS]);
    expect(fields[2].label).toBe(
      "May drop tables and fields, and delete triggers and custom SQL queries",
    );
    // Verbatim: nothing here rewrites, shortens or re-punctuates what the
    // server declared. A key the copilot does not declare — `allow_modules`,
    // which only a token can use — keeps its own fallback label and default.
    for (const field of fields) {
      const declared = traits[0].config_spec.find((spec) => spec.name === field.name);
      const fallback = FALLBACK_GRANT_FIELDS.find((spec) => spec.name === field.name);
      expect(field.label).toBe(declared ? declared.label : fallback?.label);
    }
    const modules = fields.find((field) => field.name === "allow_modules");
    expect(modules?.default).toBe(false);
  });

  it("fall back to their own words when no copilot is registered", () => {
    const fields = grantFields([{ name: "other_trait", description: "", config_spec: [] }]);
    expect(fields).toEqual(FALLBACK_GRANT_FIELDS);
    expect(fields.map((field) => field.name)).toEqual([...GRANT_KEYS]);
  });

  it("start all ticked, whatever the copilot's own defaults are", () => {
    const grants = defaultGrants(grantFields(traits));
    expect(grants).toEqual({
      allow_create: true,
      allow_edit: true,
      allow_drop: true,
      allow_access_changes: true,
      allow_triggers: true,
      allow_applications: true,
      allow_modules: true,
      allow_history: true,
    });
  });
});

describe("minting", () => {
  const fields = grantFields(traits);

  it("refuses a token with no label, because the log line names it", () => {
    expect(mintProblem({ ...emptyMint(fields), label: "  " })).toMatch(/label/i);
    expect(mintProblem({ ...emptyMint(fields), label: "laptop" })).toBeNull();
  });

  it("refuses an expiry that is not a whole number of days", () => {
    const form = { ...emptyMint(fields), label: "laptop" };
    expect(mintProblem({ ...form, expiresInDays: "1.5" })).toMatch(/whole number/);
    expect(mintProblem({ ...form, expiresInDays: "0" })).toMatch(/whole number/);
    expect(mintProblem({ ...form, expiresInDays: "-3" })).toMatch(/whole number/);
    expect(mintProblem({ ...form, expiresInDays: "" })).toBeNull();
  });

  it("sends all eight flags explicitly, whichever are ticked", () => {
    const form = {
      label: "  claude-code on my laptop  ",
      grants: { allow_create: true, allow_triggers: true },
      all: false,
      expiresInDays: "90",
    };
    expect(mintRequest(form, fields)).toEqual({
      label: "claude-code on my laptop",
      grants: {
        allow_create: true,
        allow_edit: false,
        allow_drop: false,
        allow_access_changes: false,
        allow_triggers: true,
        allow_applications: false,
        allow_modules: false,
        allow_history: false,
        allow_all: false,
      },
      expires_in_days: 90,
    });
  });

  it("sends the all-grants toggle as allow_all, with the unticked boxes beside it", () => {
    const form = { ...emptyMint(fields), label: "agent", all: true };
    form.grants = { ...form.grants, allow_drop: false };
    const request = mintRequest(form, fields);
    expect(request.grants?.allow_all).toBe(true);
    expect(request.grants?.allow_drop).toBe(false);
    expect(request.grants?.allow_modules).toBe(true);
    expect(emptyMint(fields).all).toBe(false);
  });

  it("omits the expiry entirely for a token that never lapses", () => {
    const request = mintRequest({ ...emptyMint(fields), label: "forever" }, fields);
    expect(request.expires_in_days).toBe(90);
    const never = mintRequest(
      { ...emptyMint(fields), label: "forever", expiresInDays: " " },
      fields,
    );
    expect("expires_in_days" in never).toBe(false);
  });
});

describe("the claude mcp add line", () => {
  it("carries this server's own URL and the token while it is on screen", () => {
    const line = claudeMcpAddLine("https://feldspar.example.com", "fspk_abc123");
    expect(line).toContain("https://feldspar.example.com/mcp");
    expect(line).toContain('--header "Authorization: Bearer fspk_abc123"');
    expect(line).toContain("--transport http");
  });

  it("shows an obvious placeholder once the token is gone", () => {
    const line = claudeMcpAddLine("http://localhost:3000", null);
    expect(line).toContain(TOKEN_PLACEHOLDER);
    expect(line).toContain("http://localhost:3000/mcp");
  });

  it("does not double the slash on an origin that has one", () => {
    expect(mcpUrl("http://localhost:3000/")).toBe("http://localhost:3000/mcp");
  });
});

describe("the token list", () => {
  it("tells a revoked token from a lapsed one, and both from a live one", () => {
    expect(tokenBadge(token()).label).toBe("Active");
    expect(tokenBadge(token({ live: false })).label).toBe("Expired");
    // Revoked wins over lapsed: somebody took this credential back, which is a
    // different thing to have happened.
    expect(tokenBadge(token({ live: false, revoked_at: "2026-09-03T11:00:00Z" })).label).toBe(
      "Revoked",
    );
  });

  it("shows the flags that are on, under their own words", () => {
    expect(grantedKeys(token())).toEqual([
      "allow_create",
      "allow_edit",
      "allow_triggers",
      "allow_applications",
    ]);
    expect(grantedKeys(token({ grants: null }))).toEqual([]);
    // `allow_all` holds a flag the row does not mention, not one it denies.
    const all = token({ grants: { allow_all: true, allow_drop: false } });
    expect(holdsAll(all)).toBe(true);
    expect(grantedKeys(all)).not.toContain("allow_drop");
    expect(grantedKeys(all)).toContain("allow_modules");
    expect(holdsAll(token())).toBe(false);
    expect(shortGrantLabel("allow_access_changes")).toBe("access changes");
  });

  it("says nothing rather than something wrong about a token never used", () => {
    expect(when(null)).toBe("—");
    expect(when("not a date")).toBe("not a date");
    expect(when("2026-09-03T10:00:00Z")).not.toBe("—");
  });
});

describe("the disabled server", () => {
  it("is what the panel reads out of the stored settings, not out of the form", () => {
    // The round trip an admin actually makes: `getSettings` answers with every
    // declared key, `readConfig` turns it into the bag the screen holds, and the
    // panel reads the switch out of *that* — so a ticked-but-unsaved checkbox
    // does not offer to mint a token that will not work.
    const stored = readConfig({ log_sql: false, mcp_enabled: false, mcp_loopback_only: true });
    expect(mcpEnabled(stored)).toBe(false);
    expect(mcpEnabled(readConfig({ mcp_enabled: true }))).toBe(true);
    // A server too old to declare the key at all is a server with no MCP route.
    expect(mcpEnabled({})).toBe(false);
  });
});
