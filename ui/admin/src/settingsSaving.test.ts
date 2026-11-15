/**
 * How the Settings screen saves: as each setting is edited, except the ones the
 * server holds for their card's Apply button.
 *
 * What is pinned here is what an admin would notice going wrong: a half-typed
 * value sent on a keystroke, an Apply that sends another card's edits, a card
 * whose Apply lights up with nothing to apply, or a save's answer wiping out
 * what was typed while it was in flight.
 */

import { describe, expect, it } from "vitest";

import { applyButtonLook } from "./ApplyButton";
import { SECRET_SENTINEL, initialValues, readConfig } from "./settings";
import {
  afterSave,
  appliedFields,
  autosavedFields,
  mixesModes,
  saveMode,
  unsavedNames,
  type SettingSpec,
} from "./settingsSaving";
import { settingsPayload } from "./screens/Settings";

function field(name: string, extra: Partial<SettingSpec> = {}): SettingSpec {
  return {
    name,
    label: name,
    type: "text",
    required: false,
    default: null,
    options: [],
    multiline: false,
    help: "",
    applied: false,
    ...extra,
  };
}

const sslMode = field("ssl_mode", { options: ["off", "custom"], default: "off", applied: true });
const certificate = field("ssl_certificate", { multiline: true, applied: true });
const privateKey = field("ssl_private_key", { multiline: true, secret: true, applied: true });
const adminSubdomain = field("admin_subdomain", { applied: true });
const logSql = field("log_sql", { type: "bool", default: false });
const verbosity = field("log_verbosity", { options: ["warning", "info"], default: "warning" });
const claudePath = field("claude_code_path");
const sessionHours = field("session_ttl_hours", { type: "int", default: 24 });

describe("when a setting saves", () => {
  it("saves a switch or a choice as it changes", () => {
    expect(saveMode(logSql)).toBe("change");
    expect(saveMode(verbosity)).toBe("change");
  });

  it("saves typing when the admin is done with the box, not on a keystroke", () => {
    expect(saveMode(claudePath)).toBe("commit");
    expect(saveMode(sessionHours)).toBe("commit");
  });

  it("holds a setting the server declares applied for its card's Apply, whatever its control", () => {
    expect(saveMode(sslMode)).toBe("apply");
    expect(saveMode(privateKey)).toBe("apply");
    expect(saveMode(adminSubdomain)).toBe("apply");
  });

  it("splits a spec into the settings that autosave and the ones Apply saves", () => {
    const spec = [adminSubdomain, logSql, verbosity, claudePath];
    expect(autosavedFields(spec).map((f) => f.name)).toEqual([
      "log_sql",
      "log_verbosity",
      "claude_code_path",
    ]);
    expect(appliedFields(spec).map((f) => f.name)).toEqual(["admin_subdomain"]);
  });

  it("says under a held setting that it waits for Apply only in a card that mixes the two", () => {
    expect(mixesModes([adminSubdomain, logSql])).toBe(true);
    expect(mixesModes([sslMode, certificate, privateKey])).toBe(false);
    expect(mixesModes([logSql, verbosity])).toBe(false);
  });
});

describe("what is unsaved", () => {
  const spec = [sslMode, certificate, privateKey];
  const stored = readConfig({ ssl_private_key: SECRET_SENTINEL });

  it("is nothing for a card as it was loaded, defaults and the secret's sentinel included", () => {
    expect(unsavedNames(spec, stored, initialValues(spec, stored))).toEqual([]);
  });

  it("is each box the admin changed", () => {
    const values = { ...initialValues(spec, stored), ssl_mode: "custom", ssl_certificate: "PEM" };
    expect(unsavedNames(spec, stored, values)).toEqual(["ssl_mode", "ssl_certificate"]);
  });

  it("includes a secret only once it has been typed over", () => {
    const values = { ...initialValues(spec, stored), ssl_private_key: "-----BEGIN" };
    expect(unsavedNames(spec, stored, values)).toEqual(["ssl_private_key"]);
  });

  it("is nothing again for a box changed back to what is stored", () => {
    const values = { ...initialValues(spec, stored), ssl_mode: "custom" };
    expect(unsavedNames(spec, stored, { ...values, ssl_mode: "off" })).toEqual([]);
  });
});

describe("an Apply", () => {
  it("sends only its card's changed settings, so another card's edits are not applied with it", () => {
    const held = [sslMode, certificate, privateKey];
    const stored = readConfig({ ssl_private_key: SECRET_SENTINEL });
    const values = {
      ...initialValues([...held, logSql], stored),
      ssl_mode: "custom",
      ssl_certificate: "CERT",
      log_sql: "true",
    };
    const names = unsavedNames(held, stored, values);
    const payload = settingsPayload(
      held.filter((f) => names.includes(f.name)),
      values,
    );
    // The key is not in it: untouched, the server keeps what it has.
    expect(payload).toEqual({ ssl_mode: "custom", ssl_certificate: "CERT" });
  });
});

describe("a save's answer", () => {
  it("shows each saved box as the server stored it", () => {
    const sent = { admin_subdomain: " Admin ", log_sql: "true" };
    const fresh = { admin_subdomain: "admin", log_sql: "true" };
    expect(afterSave(["admin_subdomain"], sent, { ...sent }, fresh)).toEqual({
      admin_subdomain: "admin",
      log_sql: "true",
    });
  });

  it("keeps what the admin typed while the save was in flight", () => {
    const sent = { claude_code_path: "/usr/bin/claude" };
    const current = { claude_code_path: "/usr/bin/claude-next" };
    expect(afterSave(["claude_code_path"], sent, current, { claude_code_path: "/usr/bin/claude" }))
      .toEqual(current);
  });

  it("leaves boxes the save was not about alone", () => {
    const current = { ssl_certificate: "half a PEM", log_sql: "true" };
    const fresh = { ssl_certificate: "", log_sql: "true" };
    expect(afterSave(["log_sql"], current, current, fresh).ssl_certificate).toBe("half a PEM");
  });
});

describe("the Apply button", () => {
  it("is disabled and outlined with nothing to apply", () => {
    expect(applyButtonLook(false, false)).toEqual({ variant: "outline-secondary", disabled: true });
  });

  it("turns primary and enabled the moment the card holds a change", () => {
    expect(applyButtonLook(true, false)).toEqual({ variant: "primary", disabled: false });
  });

  it("stays primary but waits while a save is in flight", () => {
    expect(applyButtonLook(true, true)).toEqual({ variant: "primary", disabled: true });
  });
});
