// Offline sync (design §14.11): the options a client is created with, what
// `api.sync` offers, and how the device's copy reaches the server.

import { requestHeaders } from "./helper";
import type { SyncStorage } from "./offline-backends";

/** How a client keeps its offline copy (`createClient({ offline })`). */
export interface OfflineOptions {
  storage: SyncStorage;
  /**
   * Sync by itself: at start, shortly after a local write, every `interval`
   * milliseconds, when the browser says it is online again, and when a live
   * stream of a synced table's changes reports one. On by default; off, nothing
   * syncs until `api.sync.now()`.
   */
  auto?: boolean;
  /** Milliseconds between syncs when `auto` is on. 30 seconds by default. */
  interval?: number;
  /** A name an admin can tell this device by ("Ann's phone"). */
  deviceName?: string;
}

/** What `client.ts` says about one synced table. */
export interface SyncTableSpec {
  /** The primary key's column. */
  key: string;
  /** `uuid` when the device may make keys itself. */
  keyKind: "uuid" | "number" | "text";
  /** The server issues the key (an integer with a default): a row made offline gets a temporary one. */
  serverKey?: boolean;
  /** Each column's kind, for comparing a filter's text with its values. */
  columns: Record<string, "number" | "boolean" | "text" | "json">;
  /** Columns that are keys into another synced table, for a `select` that embeds. */
  rels: Record<string, string>;
}

/** How sync is going. */
export interface SyncStatus {
  /** The last sync reached the server. */
  online: boolean;
  /** A sync is running now. */
  syncing: boolean;
  /** Local changes not uploaded yet. */
  pending: number;
  /** When the last sync finished, as an ISO time. */
  lastSync?: string;
  /** Why the last sync failed, until one succeeds. */
  error?: string;
}

/** One local change, as the outbox keeps it and the upload sends it. */
export interface SyncChange {
  mutation: number;
  table: string;
  op: "insert" | "update" | "delete";
  key: string;
  row?: Record<string, unknown>;
  /** An insert under a temporary key (`-mutation`); the upload's answer has the real one. */
  temp?: boolean;
}

/** A local change the server refused, and why. Its row is back to the server's. */
export interface RefusedChange extends SyncChange {
  error: string;
}

/** `api.sync`: the offline copy, and control over syncing it. */
export interface OfflineSync {
  /** Resolves once the copy is loaded from the device's storage. */
  readonly ready: Promise<void>;
  /** Upload the local changes and download the server's, now. */
  now(): Promise<void>;
  status(): SyncStatus;
  /** Changes the server refused, oldest first, until cleared. */
  refused(): RefusedChange[];
  clearRefused(): Promise<void>;
  /** `listener` hears which tables' rows changed on this device. Returns the unsubscribe. */
  onChange(listener: (tables: string[]) => void): () => void;
  /** `listener` hears every change of `status()`. Returns the unsubscribe. */
  onStatus(listener: (status: SyncStatus) => void): () => void;
  /** Stop syncing by itself: timers, listeners, live subscriptions. */
  stop(): void;
  /** Forget the copy and the outbox, e.g. when the user signs out. */
  clear(): Promise<void>;
}

/** The three sync endpoints, under `base` (`…/sync`). */
export function syncTransport(
  doFetch: typeof fetch,
  base: string
): SyncTransport {
  return {
    async post(endpoint, body) {
      const res = await doFetch(`${base}/${endpoint}`, {
        method: "POST",
        headers: requestHeaders("POST", true),
        body: JSON.stringify(body),
      });
      if (!res.ok) {
        let message = `${res.status}`;
        try {
          const answer: unknown = await res.json();
          if (
            answer &&
            typeof answer === "object" &&
            typeof (answer as { error?: unknown }).error === "string"
          ) {
            message = `${res.status}: ${(answer as { error: string }).error}`;
          }
        } catch {
          // Not JSON: the status says it.
        }
        throw new SyncHttpError(
          res.status,
          `sync/${endpoint} failed: ${message}`
        );
      }
      return res.json();
    },
  };
}

/** How the offline copy reaches the server. */
export interface SyncTransport {
  post(endpoint: "register" | "pull" | "push", body: unknown): Promise<unknown>;
}

/** A sync request the server answered with an error status. */
export class SyncHttpError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}
