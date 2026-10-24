// The IDE's API access, built on the generated typed client (`client.ts`).
//
// The same arrangement `ui/admin` uses, and for the same reason: the client is
// generated from the server's endpoint set (§13.1), so the IDE cannot drift from
// the file API it drives. What it does not do for us is the browser's part of the
// session — the cookie and the CSRF double-submit header — which is what
// `browserFetch` adds.

import { createClient, type ApiClient } from "./client";

/** Header the server expects the CSRF cookie echoed in (matches `CSRF_HEADER`). */
const CSRF_HEADER = "x-csrf-token";
/** Non-`HttpOnly` cookie the server hands the browser (matches `CSRF_COOKIE`). */
const CSRF_COOKIE = "sc_csrf";
/** Its name behind TLS (matches `SECURE_CSRF_COOKIE`): `__Host-`, so no sibling
 * subdomain can plant it. */
const SECURE_CSRF_COOKIE = "__Host-sc_csrf";

/** Read a cookie value by name from `document.cookie`, or `null` if absent. */
function readCookie(name: string): string | null {
  const prefix = `${name}=`;
  for (const part of document.cookie.split("; ")) {
    if (part.startsWith(prefix)) return part.slice(prefix.length);
  }
  return null;
}

/** A `fetch` that carries the session cookie and the CSRF header. */
const browserFetch: typeof fetch = (input, init = {}) => {
  const headers = new Headers(init.headers);
  const csrf = readCookie(SECURE_CSRF_COOKIE) ?? readCookie(CSRF_COOKIE);
  if (csrf) headers.set(CSRF_HEADER, csrf);
  return fetch(input, { ...init, headers, credentials: "same-origin" });
};

/** The shared API client for the whole IDE. */
export const api: ApiClient = createClient({ fetch: browserFetch });

/**
 * The HTTP status embedded in an error the generated client threw, if any.
 *
 * The client throws `Error("<name> failed: <status>[: <server message>]")`. The
 * status is what tells a missing file (404) from a forbidden one (403) from a
 * server that broke (500), which is the whole of the filesystem provider's error
 * mapping (§16's error kinds, seen from the client side).
 */
export function errorStatus(err: unknown): number | null {
  if (err instanceof Error) {
    const match = err.message.match(/failed: (\d+)/);
    if (match) return Number(match[1]);
  }
  return null;
}

/** The server's own message from an error the client threw, else `fallback`. */
export function errorMessage(err: unknown, fallback: string): string {
  if (err instanceof Error) {
    const match = err.message.match(/failed: \d+: ([\s\S]+)$/);
    if (match) return match[1];
  }
  return err instanceof Error ? err.message : fallback;
}
