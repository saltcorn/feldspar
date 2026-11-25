// An application as an OpenID Connect identity provider (TODO IP1.8): the
// model behind the "Identity provider" tab. The switch is the application's
// `identity_provider` field, saved with the rest of its settings; the clients,
// their users' grants and the signing keys have endpoints of their own.
//
// What lives here is what the screen computes: the issuer and the addresses a
// relying party is configured with, and the client form's text boxes — one
// URI per line, one `claim = field` per line — turned into the request body
// and back.

import type { CreateOAuthClientRequest, GetIdentityProviderResponse } from "./client";

/** One registered client, as the screen lists it. */
export type OAuthClientItem = GetIdentityProviderResponse["clients"][number];

/** The scopes a client may be limited to. None ticked is all of them. */
export const OIDC_SCOPES = ["openid", "email", "profile", "offline_access"] as const;

/** What a relying party is told: the issuer, and the discovery document at
 * it. The issuer is the application's own address, as it is served. */
export function providerAddresses(appOrigin: string): {
  issuer: string;
  discovery: string;
} {
  const issuer = appOrigin.replace(/\/+$/, "");
  return { issuer, discovery: `${issuer}/.well-known/openid-configuration` };
}

/** The client form's state: text boxes and switches. */
export type ClientForm = {
  name: string;
  confidential: boolean;
  redirectUris: string;
  logoutUris: string;
  scopes: string[];
  skipConsent: boolean;
  claims: string;
};

/** A blank form for a new client: confidential, every scope. */
export function blankClientForm(): ClientForm {
  return {
    name: "",
    confidential: true,
    redirectUris: "",
    logoutUris: "",
    scopes: [],
    skipConsent: false,
    claims: "",
  };
}

/** The form for editing `client`. A client allowed every scope shows none
 * ticked, which is what "every scope" is sent as. */
export function clientFormOf(client: OAuthClientItem): ClientForm {
  const all = OIDC_SCOPES.every((s) => client.scopes.includes(s));
  return {
    name: client.name,
    confidential: client.confidential,
    redirectUris: client.redirect_uris.join("\n"),
    logoutUris: client.post_logout_redirect_uris.join("\n"),
    scopes: all ? [] : client.scopes,
    skipConsent: client.skip_consent,
    claims: client.claim_fields.map((c) => `${c.claim} = ${c.field}`).join("\n"),
  };
}

/** The non-blank lines of a text box, trimmed. */
export function lines(text: string): string[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l !== "");
}

/** `claim = field` lines as claim mappings. A line that is not one is
 * reported by its number, so the form can say which. */
export function parseClaims(
  text: string,
): { ok: true; claims: { claim: string; field: string }[] } | { ok: false; line: number } {
  const claims: { claim: string; field: string }[] = [];
  const all = text.split("\n");
  for (let i = 0; i < all.length; i++) {
    const line = all[i].trim();
    if (line === "" || line.startsWith("#")) continue;
    const at = line.indexOf("=");
    const claim = at < 0 ? "" : line.slice(0, at).trim();
    const field = at < 0 ? "" : line.slice(at + 1).trim();
    if (!claim || !field) return { ok: false, line: i + 1 };
    claims.push({ claim, field });
  }
  return { ok: true, claims };
}

/** The body a create (or, without `confidential`, an update) sends, or the
 * line of the claims box that is not a mapping. */
export function clientRequest(
  form: ClientForm,
): { ok: true; body: CreateOAuthClientRequest } | { ok: false; claimLine: number } {
  const claims = parseClaims(form.claims);
  if (!claims.ok) return { ok: false, claimLine: claims.line };
  return {
    ok: true,
    body: {
      name: form.name.trim(),
      confidential: form.confidential,
      redirect_uris: lines(form.redirectUris),
      scopes: form.scopes,
      skip_consent: form.skipConsent,
      claim_fields: claims.claims,
      post_logout_redirect_uris: lines(form.logoutUris),
    },
  };
}

/** A client's name by its id, for the grants list; the id when it is gone. */
export function clientName(clients: OAuthClientItem[], clientId: string): string {
  return clients.find((c) => c.client_id === clientId)?.name ?? clientId;
}
