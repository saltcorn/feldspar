// The admin SPA's API access, built on the generated typed client (`client.ts`).
//
// The generated client now carries the CSRF double-submit header itself, so a
// client of the server's endpoint set can call that server unaided — which is
// what every consumer of it needs, this SPA and a scaffolded app alike.
//
// What remains here are the requests whose *bytes* cannot be described by a
// `TypeSchema`, and which are therefore served by routes outside the typed
// endpoint set: `uploadFile`, and the two halves of backup and restore
// (`createBackup`, whose response is a zip, and `uploadBackup`, whose request is
// one). Each has to satisfy the CSRF contract by hand; `browserFetch` is what does
// that, and is also what makes the session cookie's journey explicit rather than
// relying on `fetch`'s default.

import { createClient, type ApiClient, type GetBackupOptionsResponse } from "./client";

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

/** The shared, browser-ready API client for the whole admin SPA. */
export const api: ApiClient = createClient({ fetch: browserFetch });

/**
 * Upload a file's bytes to a store.
 *
 * **Not in the generated client**, and deliberately so. The endpoint model is
 * JSON-only — a `TypeSchema` has no bytes shape — so a raw binary body cannot be
 * described by it, and the server serves this from a route outside the typed
 * `EndpointSet` (`POST /upload/{store}/{*path}`). Everything else in this SPA goes
 * through the generated client except the backup calls below, which are outside it
 * for the same reason; the exceptions live here beside the other hand-written
 * browser concerns rather than being scattered into a screen.
 *
 * `writeFile` remains the typed path for small text files (the editor uses it);
 * this is for arbitrary bytes at arbitrary size.
 */
export async function uploadFile(
  store: string,
  path: string,
  file: File,
): Promise<void> {
  // Each segment is encoded separately: the path is a `{*path}` capture, so its
  // slashes are structural and must survive, while any other special character
  // in a filename must not.
  const encodedPath = path
    .split("/")
    .map(encodeURIComponent)
    .join("/");
  const res = await browserFetch(
    `/upload/${encodeURIComponent(store)}/${encodedPath}`,
    { method: "POST", body: file },
  );
  if (!res.ok) throw await rawError("uploadFile", res);
}

/** The error a route outside the typed endpoint set failed with, in the same shape
 * the generated client throws — `<name> failed: <status>[: <server message>]` — so
 * `errorStatus` and `errorMessage` below read it the same way. */
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
 * Ask the server to build a backup, and hand the browser the file.
 *
 * The second call not in the generated client, for the mirror-image reason
 * `uploadFile` is not: the response *is* a zip, and the endpoint model has no
 * bytes shape to describe it with. The selection still travels as JSON, so this is
 * an ordinary POST whose response happens to be a file.
 *
 * The download is done by clicking a link at an object URL rather than by
 * navigating: a navigation cannot carry the CSRF header, and the archive is
 * already in memory by the time the response resolves. The name comes from the
 * server's `Content-Disposition` when it sent one, since the server is what knows
 * the date the backup was taken.
 */
export async function createBackup(include: unknown): Promise<void> {
  const res = await browserFetch("/backup/create", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ include }),
  });
  if (!res.ok) throw await rawError("createBackup", res);
  const blob = await res.blob();
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filenameFrom(res.headers.get("content-disposition")) ?? "feldspar-backup.zip";
  document.body.appendChild(link);
  link.click();
  link.remove();
  // Freed on the next tick rather than immediately: revoking the URL before the
  // click has been dispatched cancels the download in some browsers.
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
}

/**
 * Download a posterior fit's run as a zip (`downloadModelRun`, Stan TODO §16).
 *
 * The endpoint is in the typed set, but its answer is bytes, which the
 * generated client would try to read as JSON — so the request is made here, and
 * handed to the browser the way a backup is.
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
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
}

/** The `filename="…"` of a `Content-Disposition` header, if it has one. */
function filenameFrom(header: string | null): string | null {
  const match = header?.match(/filename="([^"]+)"/);
  return match ? match[1] : null;
}

/** What an uploaded backup turned out to hold, and the token that names it while
 * the admin decides what to take from it. */
export type UploadedBackup = {
  id: string;
  created_at: string | null;
  /** What wrote the file — "Feldspar 0.1.0", or "Saltcorn 1.7.0, imported" for a
   * Saltcorn 1 backup the server translated on the way in. */
  source: string | null;
  available: GetBackupOptionsResponse["available"];
  include: GetBackupOptionsResponse["include"];
};

/**
 * Hand a backup file to the server and get back what is in it.
 *
 * Nothing is restored by this: the file waits on the server while the admin
 * unticks what they do not want, and `restoreBackup` (which *is* in the generated
 * client) names it by the `id` this returns. One upload rather than two — the
 * alternative is sending a large archive again with the choice attached.
 */
export async function uploadBackup(file: File): Promise<UploadedBackup> {
  const res = await browserFetch("/backup/upload", { method: "POST", body: file });
  if (!res.ok) throw await rawError("uploadBackup", res);
  return (await res.json()) as UploadedBackup;
}

/**
 * The HTTP status embedded in an error thrown by the generated client, if any.
 * The client throws `Error("<name> failed: <status>[: <server message>]")`, so
 * we recover the code to distinguish, e.g., a 401 (bad credentials) from a real
 * network failure. The status is matched wherever it sits, since a server
 * message may follow it.
 */
export function errorStatus(err: unknown): number | null {
  if (err instanceof Error) {
    const match = err.message.match(/failed: (\d+)/);
    if (match) return Number(match[1]);
  }
  return null;
}

/**
 * The server's own message from an error the generated client threw, if it
 * carried one (the `: <server message>` the client appends after the status) —
 * otherwise the raw error message. This is what surfaces, e.g., a failed build's
 * bundler diagnostics to the admin.
 */
export function errorMessage(err: unknown, fallback: string): string {
  if (err instanceof Error) {
    const match = err.message.match(/failed: \d+: ([\s\S]+)$/);
    if (match) return match[1];
  }
  return fallback;
}
