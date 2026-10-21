/**
 * The external identity providers' model (Settings → Authentication): what a
 * save sends, the redirect URL an admin registers, a preset filling in the
 * name, and the sign-in page's buttons and returned error.
 */

import { describe, expect, it } from "vitest";

import {
  callbackUrl,
  oauthErrorFrom,
  providerPayload,
  readSignInProviders,
  startUrl,
  withPreset,
} from "./oauthProviders";
import type { FieldSpec } from "./settings";

const field = (name: string, type: string, extra: Partial<FieldSpec> = {}): FieldSpec =>
  ({
    name,
    label: name,
    type,
    required: false,
    default: null,
    options: [],
    multiline: false,
    secret: false,
    create_only: false,
    show_if: [],
    ...extra,
  }) as FieldSpec;

describe("identity providers", () => {
  it("sends the settings typed by their declaration, under config", () => {
    const spec = [
      field("name", "text"),
      field("enabled", "bool", { default: true }),
      field("new_user_role", "int"),
      field("client_secret", "text", { secret: true }),
    ];
    expect(
      providerPayload(spec, {
        name: "google",
        enabled: "false",
        new_user_role: "80",
        client_secret: "",
      }),
    ).toEqual({ config: { name: "google", enabled: false, new_user_role: 80 } });
  });

  it("builds the redirect URL from this host's origin", () => {
    expect(callbackUrl("https://admin.example.com/", "/_feldspar/oauth/callback")).toBe(
      "https://admin.example.com/_feldspar/oauth/callback",
    );
  });

  it("fills a blank name and label from a preset, and leaves typed ones", () => {
    expect(withPreset({ name: "", label: "" }, "github")).toMatchObject({
      preset: "github",
      name: "github",
      label: "GitHub",
    });
    expect(withPreset({ name: "corp", label: "Corp" }, "google")).toMatchObject({
      name: "corp",
      label: "Corp",
    });
    expect(withPreset({ name: "" }, "custom").name).toBe("");
  });

  it("reads a returned error and starts a sign-in that comes back", () => {
    expect(oauthErrorFrom("?oauth_error=there%20is%20no%20account")).toBe(
      "there is no account",
    );
    expect(oauthErrorFrom("?x=1")).toBeNull();
    const [google] = readSignInProviders([
      { name: "google", label: "Google", url: "/_feldspar/oauth/google/start" },
      { name: 3 },
    ]);
    expect(startUrl(google, "/#/tables")).toBe(
      "/_feldspar/oauth/google/start?return_to=%2F%23%2Ftables",
    );
    expect(readSignInProviders(null)).toEqual([]);
  });
});
