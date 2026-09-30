// The Analytics UI's API access: the generated typed client (`client.ts`, the
// admin API's, regenerated with it), over a `fetch` carrying the admin UI's
// session cookie and CSRF header — the two bundles share one origin, so a
// signed-in admin is signed in here.

import { createClient, type ApiClient } from "./client";

/** Header the server expects the CSRF cookie echoed in (matches `CSRF_HEADER`). */
const CSRF_HEADER = "x-csrf-token";
/** Non-`HttpOnly` cookie the server hands the SPA (matches `CSRF_COOKIE`). */
const CSRF_COOKIE = "sc_csrf";

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
  const csrf = readCookie(CSRF_COOKIE);
  if (csrf) headers.set(CSRF_HEADER, csrf);
  return fetch(input, { ...init, headers, credentials: "same-origin" });
};

/** The shared, browser-ready API client. */
export const api: ApiClient = createClient({ fetch: browserFetch });

/** The HTTP status an error the generated client threw carries, if any. */
export function errorStatus(err: unknown): number | null {
  if (err instanceof Error) {
    const match = err.message.match(/failed: (\d+)/);
    if (match) return Number(match[1]);
  }
  return null;
}

/**
 * The server's own sentence from an error the generated client threw — the
 * `: <server message>` it appends after the status — otherwise `fallback`.
 */
export function errorMessage(err: unknown, fallback: string): string {
  if (err instanceof Error) {
    const match = err.message.match(/failed: \d+: ([\s\S]+)$/);
    if (match) return match[1];
  }
  return fallback;
}
