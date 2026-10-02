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

/** The server's error for a request made outside the generated client, in the
 * generated client's shape (`<op> failed: <status>: <sentence>`), so
 * [`errorMessage`] reads it the same way. */
async function rawError(op: string, res: Response): Promise<Error> {
  let detail = "";
  try {
    const body: unknown = await res.json();
    if (body && typeof body === "object" && "error" in body) {
      const message = (body as { error: unknown }).error;
      if (typeof message === "string") detail = `: ${message}`;
    }
  } catch {
    // Non-JSON body: the status alone will have to describe the failure.
  }
  return new Error(`${op} failed: ${res.status}${detail}`);
}

/**
 * Download a posterior fit's run as a zip (`downloadModelRun`, Stan TODO §16).
 *
 * The endpoint is in the typed set, but its answer is bytes, which the
 * generated client would try to read as JSON — so the request is made here, and
 * handed to the browser as a file.
 */
export async function downloadModelRun(instance: string): Promise<void> {
  const res = await browserFetch(`/api/model-instances/${encodeURIComponent(instance)}/run`);
  if (!res.ok) throw await rawError("downloadModelRun", res);
  const blob = await res.blob();
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filenameFrom(res.headers.get("content-disposition")) ?? "run.zip";
  document.body.appendChild(link);
  link.click();
  link.remove();
  // Freed on the next tick: revoking before the click is dispatched cancels
  // the download in some browsers.
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
}

/** The `filename="…"` of a `Content-Disposition` header, if it has one. */
function filenameFrom(header: string | null): string | null {
  const match = header?.match(/filename="([^"]+)"/);
  return match ? match[1] : null;
}

/** The address of a fit's progress socket (`GET /api/model-instances/{id}/progress`,
 * analytics TODO A3.3): the page's own host, over `wss:` when the page is `https:`. */
export function progressSocketUrl(instance: string, location: Pick<Location, "protocol" | "host">): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}/api/model-instances/${encodeURIComponent(instance)}/progress`;
}
