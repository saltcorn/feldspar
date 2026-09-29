/**
 * A file store as a filesystem, in the store's own vocabulary.
 *
 * This is the half of the IDE's filesystem that has nothing to do with VS Code:
 * store-relative paths, bytes, and the four outcomes a file operation can have.
 * The VS Code adapter is `fileSystemProvider.ts`, which is a translation layer
 * over this and nothing more.
 *
 * Splitting it this way is what makes the interesting part testable — path
 * mapping, base64, and which HTTP status means what — without booting a
 * workbench to do it.
 */

import { errorMessage, errorStatus } from "./api";
import type { ApiClient } from "./client";

/** What a store entry is. The store knows only these two. */
export type EntryKind = "file" | "directory";

/** One entry in a directory listing. */
export interface StoreEntry {
  readonly name: string;
  readonly kind: EntryKind;
  /** Bytes, for a file; `0` for a directory, which the store does not size. */
  readonly size: number;
}

/**
 * Why a file operation failed, in the terms a filesystem has.
 *
 * `notFound` and `exists` are ordinary control flow for an editor — asking
 * whether a path is there is how it decides to create or to open — so they are
 * *values*, not surprises. `noPermission` is the access rule of §9 refusing.
 * `failed` is everything else, and carries the server's own message.
 */
export type StoreFileErrorKind =
  | "notFound"
  | "notADirectory"
  | "exists"
  | "noPermission"
  | "failed";

/** An error from a store operation, tagged with which of the four it is. */
export class StoreFileError extends Error {
  constructor(
    readonly kind: StoreFileErrorKind,
    message: string,
  ) {
    super(message);
    this.name = "StoreFileError";
  }
}

/**
 * The store-relative path a workspace URI's path names.
 *
 * The workspace folder is `/<store>` (see `workspace.ts`), so `/my-app/src/x.ts`
 * in the store `my-app` is `src/x.ts`, and the folder itself is `""` — which is
 * what the API means by the store root. A path outside the folder is a
 * programming error, not a user error: the provider is only ever asked about
 * URIs inside the workspace it registered.
 */
export function toStorePath(store: string, uriPath: string): string {
  const root = `/${store}`;
  if (uriPath === root || uriPath === `${root}/`) return "";
  if (!uriPath.startsWith(`${root}/`)) {
    throw new StoreFileError("failed", `${uriPath} is not in the file store ${store}`);
  }
  return uriPath.slice(root.length + 1).replace(/\/+$/, "");
}

/**
 * The same, for a URI that **might not** be in the store: the path, or `null`.
 *
 * [`toStorePath`] throws because its callers are the filesystem provider, which
 * is only ever asked about URIs inside the workspace it registered. A menu
 * command is not in that position — what it receives is whatever the workbench
 * put in the argument list — so for that caller "not one of ours" is an answer
 * rather than a bug.
 */
export function toStorePathOrNull(store: string, uriPath: string): string | null {
  const prefix = `/${store}/`;
  if (!uriPath.startsWith(prefix)) return null;
  const path = uriPath.slice(prefix.length).replace(/\/+$/, "");
  return path === "" ? null : path;
}

/** The workspace URI path a store-relative path has. The inverse of the above. */
export function toUriPath(store: string, storePath: string): string {
  return storePath === "" ? `/${store}` : `/${store}/${storePath}`;
}

/**
 * The store path a page URL asks to have opened (`/ide/?store=s&path=a/b.stan`),
 * or `null` when it names none or names one that is not inside a store — an
 * absolute path or a `..` is dropped here rather than handed to the workbench.
 */
export function requestedPath(href: string): string | null {
  const raw = new URL(href).searchParams.get("path")?.trim().replace(/^\/+/, "");
  if (raw == null || raw === "") return null;
  if (raw.split("/").some((part) => part === ".." || part === "")) return null;
  return raw;
}

/** Decode base64 the API returned into the bytes it stands for. */
export function decodeBase64(base64: string): Uint8Array {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/**
 * Encode bytes as the base64 the API takes.
 *
 * In chunks because `String.fromCharCode(...bytes)` is a spread whose argument
 * count is the file's length: it throws on anything of size, which a source tree
 * contains.
 */
export function encodeBase64(bytes: Uint8Array): string {
  const chunk = 0x8000;
  let binary = "";
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

/** Everything above the last `/`, or `""` for a path at the store root. */
export function parentPath(storePath: string): string {
  const cut = storePath.lastIndexOf("/");
  return cut === -1 ? "" : storePath.slice(0, cut);
}

/** The last segment of a path. */
export function baseName(storePath: string): string {
  const cut = storePath.lastIndexOf("/");
  return cut === -1 ? storePath : storePath.slice(cut + 1);
}

/**
 * How long a directory listing is believed, in milliseconds.
 *
 * Long enough to collapse the burst of questions a workbench asks when it opens a
 * store, which lands within a few hundred milliseconds; short enough that a file
 * created outside the IDE is not invisible for longer than it takes to notice.
 * Note what the expiry costs when it fires: another *listing*, which succeeds —
 * never a request for a path that is not there.
 */
export const LISTING_TTL_MS = 5_000;

/** A directory's entries, and the moment they were asked for. */
interface CachedListing {
  readonly fetchedAt: number;
  readonly entries: Promise<Map<string, StoreEntry>>;
}

/**
 * The file operations of one store, on top of the generated client.
 *
 * **Existence questions are answered here, not asked of the server.** An editor
 * asks a great many of them — opening a store makes VS Code look for
 * `.vscode/settings.json`, `tasks.json`, `launch.json`, `mcp.json` and the
 * `.vscode` directory itself, none of which normally exist — and a store's API has
 * no way to answer "no" except by failing the request. Asking anyway would make
 * every one of those a 404 in the operator's log, and a 404 *should* be logged: it
 * means something asked for a thing that is not there, which is worth seeing when
 * it is not routine.
 *
 * So it is not routine here. A directory listing says what is in a directory, and
 * therefore also what is *not*: once the root's listing is known, "is there a
 * `.vscode/settings.json`?" is answerable without a request, because there is no
 * `.vscode`. Listings are kept as they are fetched, and a path is resolved by
 * walking them from the root — so the only requests that reach the server are for
 * paths that exist, plus the listings themselves.
 *
 * **What a remembered listing can get wrong**, since there is no watcher to tell
 * us otherwise, is a file created *outside* the IDE — a git pull, the file
 * manager, a build writing into the source tree. A deletion cannot mislead: the
 * listing still names the file, so reading it asks the server and gets the 404 it
 * deserves. A creation can, so three things bound it. Anything that goes through
 * [`list`](StoreFiles::list) — the explorer, Refresh, find-in-files, Go to File —
 * refetches and sees it. [`forgetEverything`](StoreFiles::forgetEverything) drops
 * the lot, which is what the page does when it regains focus, the moment an admin
 * is most likely to have just done something elsewhere. And a listing is only
 * believed for [`LISTING_TTL_MS`], so nothing is wrong for longer than that
 * anyway.
 */
export class StoreFiles {
  /**
   * Directory → its entries by name, and when they were fetched.
   *
   * Promises rather than values, so concurrent lookups under one directory (which
   * is what a workbench does on startup) share a single request.
   */
  private readonly listings = new Map<string, CachedListing>();

  constructor(
    readonly store: string,
    private readonly client: ApiClient,
    /** How long a listing is believed; see [`LISTING_TTL_MS`]. */
    private readonly ttlMs: number = LISTING_TTL_MS,
  ) {}

  /**
   * The entries of a directory, **freshly fetched**.
   *
   * This is what the explorer's Refresh runs, and refresh has to mean refresh: it
   * is the one way to see a change made outside the IDE (there is no watcher), so
   * it replaces what is remembered rather than reading it.
   */
  async list(dir: string): Promise<StoreEntry[]> {
    await this.requireDirectory(dir);
    return [...(await this.fetchListing(dir)).values()];
  }

  /**
   * What a path is, or `null` when nothing is there.
   *
   * There is no `stat` endpoint, and it turns out none is needed: a path exists
   * exactly when every segment of it appears in its parent's listing, which is
   * what this walks. A path under a directory that does not exist is `null`
   * without any request being made about it.
   */
  async stat(storePath: string): Promise<StoreEntry | null> {
    if (storePath === "") {
      return { name: this.store, kind: "directory", size: 0 };
    }
    const parent = parentPath(storePath);
    const parentEntry = await this.stat(parent);
    // Nothing can live under a path that is absent, or under a file.
    if (parentEntry === null || parentEntry.kind !== "directory") return null;
    const listing = await this.knownListing(parent);
    return listing.get(baseName(storePath)) ?? null;
  }

  /** A file's bytes. */
  async read(storePath: string): Promise<Uint8Array> {
    // Established first, so a file that is not there is answered rather than
    // requested: `readFile` on a missing path is a 404 the server is right to log.
    const entry = await this.stat(storePath);
    if (entry === null) {
      throw new StoreFileError(
        "notFound",
        `${storePath} does not exist in file store ${this.store}`,
      );
    }
    const content = await this.call("reading", () =>
      this.client.readFile(this.store, { path: storePath }),
    );
    return decodeBase64(content.base64);
  }

  /** Write a file, creating it and any missing parent directories. */
  async write(storePath: string, bytes: Uint8Array): Promise<void> {
    await this.call("writing", () =>
      this.client.writeFile(this.store, {
        path: storePath,
        base64: encodeBase64(bytes),
      }),
    );
    // The store creates missing parents, so a write can add directories anywhere
    // along the path — every listing from the root down may now be wrong.
    this.forget(storePath);
  }

  /** Create a directory. Succeeds if it is already there. */
  async makeDirectory(storePath: string): Promise<void> {
    await this.call("creating", () =>
      this.client.makeDirectory(this.store, { path: storePath }),
    );
    this.forget(storePath);
  }

  /** Delete a file, or a directory and everything in it. */
  async remove(storePath: string): Promise<void> {
    await this.call("deleting", () =>
      this.client.deleteFile(this.store, { path: storePath }),
    );
    this.forget(storePath);
  }

  /**
   * Move a path. The store never overwrites, so an occupied destination is
   * cleared first when the caller asked for that and refused otherwise.
   */
  async move(from: string, to: string, overwrite: boolean): Promise<void> {
    const destination = await this.stat(to);
    if (destination !== null) {
      if (!overwrite) {
        throw new StoreFileError("exists", `${to} already exists in ${this.store}`);
      }
      await this.remove(to);
    }
    await this.call("moving", () => this.client.renameFile(this.store, { from, to }));
    this.forget(from);
    this.forget(to);
  }

  /** Fail unless `dir` is a directory that exists, without asking about it. */
  private async requireDirectory(dir: string): Promise<void> {
    const entry = await this.stat(dir);
    if (entry === null) {
      throw new StoreFileError(
        "notFound",
        `${dir} does not exist in file store ${this.store}`,
      );
    }
    if (entry.kind !== "directory") {
      throw new StoreFileError("notADirectory", `${dir} is a file, not a directory`);
    }
  }

  /**
   * Forget every listing, so the next question is answered from the store.
   *
   * Called when the page regains focus: with no watcher, that is the cheapest
   * honest signal that an admin may have just done something elsewhere — pulled in
   * the git store's tab, edited in the file manager, run a build.
   */
  forgetEverything(): void {
    this.listings.clear();
  }

  /**
   * A directory's listing, fetching it unless one is remembered and still young
   * enough to believe.
   */
  private knownListing(dir: string): Promise<Map<string, StoreEntry>> {
    const cached = this.listings.get(dir);
    if (cached != null && Date.now() - cached.fetchedAt < this.ttlMs) {
      return cached.entries;
    }
    return this.fetchListing(dir);
  }

  /** Fetch a directory's listing and remember it. */
  private fetchListing(dir: string): Promise<Map<string, StoreEntry>> {
    const entries = this.call("listing", () => this.client.browseFiles(this.store, { dir }))
      .then(
        (listing) =>
          new Map(
            listing.map((entry) => [
              entry.name,
              {
                name: entry.name,
                kind: entry.is_dir ? ("directory" as const) : ("file" as const),
                size: entry.size ?? 0,
              },
            ]),
          ),
      )
      .catch((err: unknown) => {
        // A failed fetch must not be remembered as the truth about a directory.
        if (this.listings.get(dir)?.entries === entries) this.listings.delete(dir);
        throw err;
      });
    this.listings.set(dir, { fetchedAt: Date.now(), entries });
    return entries;
  }

  /**
   * Drop what is remembered about a path: every directory along it (an operation
   * may have created or removed any of them) and everything beneath it.
   */
  private forget(storePath: string): void {
    for (const dir of [...this.listings.keys()]) {
      const inside = dir === storePath || dir.startsWith(`${storePath}/`);
      const above = storePath === dir || storePath.startsWith(dir === "" ? "" : `${dir}/`);
      if (inside || above) this.listings.delete(dir);
    }
  }

  /** Run one client call, turning its failure into a [`StoreFileError`]. */
  private async call<T>(doing: string, run: () => Promise<T>): Promise<T> {
    try {
      return await run();
    } catch (err) {
      if (err instanceof StoreFileError) throw err;
      throw new StoreFileError(
        kindOfStatus(errorStatus(err)),
        errorMessage(err, `${doing} failed in file store ${this.store}`),
      );
    }
  }
}

/**
 * Which failure an HTTP status is (§16: the error kinds, as the API reports
 * them). A 400 is `exists` because the one 400 a filesystem call provokes is the
 * store refusing to clobber a destination — every other bad request here would be
 * a bug in this client rather than something the admin did.
 */
export function kindOfStatus(status: number | null): StoreFileErrorKind {
  switch (status) {
    case 404:
      return "notFound";
    case 400:
      return "exists";
    case 401:
    case 403:
      return "noPermission";
    default:
      return "failed";
  }
}
