import { describe, expect, it } from "vitest";
import {
  blankClientForm,
  clientFormOf,
  clientName,
  clientRequest,
  parseClaims,
  providerAddresses,
  type OAuthClientItem,
} from "./identityProvider";

const grafana: OAuthClientItem = {
  client_id: "fdc_1",
  name: "Grafana",
  confidential: true,
  redirect_uris: ["https://grafana.example.net/login/generic_oauth"],
  scopes: ["openid", "email", "profile", "offline_access"],
  skip_consent: false,
  claim_fields: [{ claim: "groups", field: "role" }],
  post_logout_redirect_uris: [],
  created_at: "2026-11-25T00:00:00Z",
};

describe("identity provider", () => {
  it("names the issuer and the discovery document at the application's address", () => {
    expect(providerAddresses("https://staff.example.com/")).toEqual({
      issuer: "https://staff.example.com",
      discovery: "https://staff.example.com/.well-known/openid-configuration",
    });
  });

  it("reads claim lines and says which line is not one", () => {
    expect(parseClaims("groups = role\n# a comment\n\ndept=department")).toEqual({
      ok: true,
      claims: [
        { claim: "groups", field: "role" },
        { claim: "dept", field: "department" },
      ],
    });
    expect(parseClaims("groups = role\ndepartment")).toEqual({ ok: false, line: 2 });
    expect(parseClaims("= role")).toEqual({ ok: false, line: 1 });
  });

  it("sends the form as a request, one URI per line", () => {
    const form = {
      ...blankClientForm(),
      name: " Wiki ",
      redirectUris: "https://wiki.example.net/cb\n\n https://wiki.example.net/cb2 ",
      claims: "dept = department",
    };
    expect(clientRequest(form)).toEqual({
      ok: true,
      body: {
        name: "Wiki",
        confidential: true,
        redirect_uris: ["https://wiki.example.net/cb", "https://wiki.example.net/cb2"],
        scopes: [],
        skip_consent: false,
        claim_fields: [{ claim: "dept", field: "department" }],
        post_logout_redirect_uris: [],
      },
    });
    expect(clientRequest({ ...form, claims: "nope" })).toEqual({ ok: false, claimLine: 1 });
  });

  it("edits a client as it is, with every scope shown as none ticked", () => {
    const form = clientFormOf(grafana);
    expect(form.scopes).toEqual([]);
    expect(form.claims).toBe("groups = role");
    expect(form.redirectUris).toBe("https://grafana.example.net/login/generic_oauth");
    expect(clientFormOf({ ...grafana, scopes: ["openid"] }).scopes).toEqual(["openid"]);
  });

  it("names a grant's client, or shows its id once it is gone", () => {
    expect(clientName([grafana], "fdc_1")).toBe("Grafana");
    expect(clientName([grafana], "fdc_gone")).toBe("fdc_gone");
  });
});
