// A client created with `offline` keeps a copy of the application's synced
// tables on the device (design §14.11). Reads and writes of those tables use the
// copy, so they work without a connection; writes also go into an outbox, which
// is uploaded when the device can reach the server, and the server's changes
// are pulled down.
//
// The copy is two layers: the rows as the server last sent them, and the outbox
// replayed over them. What a page reads is the second. A pull only ever touches
// the first, so it never overwrites a change that is not uploaded yet; and a
// change the server refuses simply leaves the outbox, and with it the page.

import {
  compareValues,
  filterOperand,
  liveRowsPlan,
  sortLiveRows,
} from "./helper";
import {
  SyncHttpError,
  type OfflineOptions,
  type OfflineSync,
  type RefusedChange,
  type SyncChange,
  type SyncStatus,
  type SyncTableSpec,
  type SyncTransport,
} from "./offline-types";
import type { SyncStorage } from "./offline-backends";

type SyncRow = Record<string, unknown>;

type PullAnswer = {
  watermark: string;
  tables: Array<{
    table: string;
    reset: boolean;
    rows: SyncRow[];
    removed: string[];
  }>;
};

type PushAnswer = {
  results: Array<{ mutation: number; status: string; error?: string }>;
};

/**
 * The options of a `list()` call (`select`, `filter`, `order`, `limit`,
 * `offset`), as the offline copy reads them when it answers the call itself
 * instead of asking the server.
 */
type LocalQuery = {
  select?: string;
  order?: string;
  limit?: number;
  offset?: number;
  filter?: Record<string, string | undefined>;
};

/**
 * The parts of a generated table object (e.g. `api.notes`) the offline copy
 * still calls: `list` and `create` to ask the server when the copy cannot
 * answer, and `changes` to hear that the server has something new. All
 * optional, since not every table exposes all of them.
 */
type OnlineTable = {
  list?(query?: LocalQuery): Promise<unknown[]>;
  create?(row: SyncRow): Promise<unknown>;
  changes?: {
    subscribe(
      query: object,
      handlers: { element(): void; resync?(): void }
    ): { close(): void };
  };
};

const UPLOAD_BATCH = 100;

/** Storage keys: `m/` settings, `b/<table>/<key>` the server's rows, `o/` the outbox, `r/` refusals. */
const outboxKey = (mutation: number): string =>
  `o/${String(mutation).padStart(12, "0")}`;
const refusedKey = (mutation: number): string =>
  `r/${String(mutation).padStart(12, "0")}`;
const serverRowKey = (table: string, key: string): string =>
  `b/${table}/${key}`;

/** A key as the change-log writes it: text as itself, a number as its digits. */
function keyText(value: unknown): string {
  return typeof value === "string" ? value : JSON.stringify(value);
}

/** A version 4 UUID, where `crypto.randomUUID` is missing too (Hermes). */
function newUuid(): string {
  const c = (
    globalThis as {
      crypto?: Partial<Pick<Crypto, "randomUUID" | "getRandomValues">>;
    }
  ).crypto;
  if (c?.randomUUID) return c.randomUUID();
  const bytes = new Uint8Array(16);
  if (c?.getRandomValues) c.getRandomValues(bytes);
  else for (let i = 0; i < 16; i++) bytes[i] = Math.floor(Math.random() * 256);
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x40;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join(
    ""
  );
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(
    12,
    16
  )}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** A row after `change`: an insert's row, an update merged in, a delete's nothing. */
function applyChange(
  row: SyncRow | undefined,
  change: SyncChange
): SyncRow | undefined {
  switch (change.op) {
    case "insert":
      return { ...change.row };
    case "update":
      return row ? { ...row, ...change.row } : undefined;
    case "delete":
      return undefined;
  }
}

/** A fetch that never reached the server: no connection, not an answer. */
function unreachable(error: unknown): boolean {
  return !(error instanceof SyncHttpError);
}

/**
 * The offline copy of an application's synced tables (`api.sync`).
 *
 * It keeps three things:
 * - `serverRows`: the rows as the server last sent them. Only a pull or an
 *   upload's answer changes them.
 * - `outbox`: the local creates, updates and deletes not uploaded yet, oldest
 *   first. Every write lands here first and is saved before it returns.
 * - `visibleRows`: `serverRows` with the outbox applied on top. This is what
 *   `list()` and `get()` return, so local changes show at once and a pull
 *   never overwrites them.
 */
export class OfflineCopy implements OfflineSync {
  // --- setup, given to the constructor ---------------------------------------
  // Resolves once the copy is loaded from storage; every read and write waits for it.
  readonly ready: Promise<void>;
  // Where the copy is saved on the device (IndexedDB, SQLite or memory).
  private readonly storage: SyncStorage;
  // Sends `register`, `push` and `pull` to the server.
  private readonly transport: SyncTransport;
  // Per synced table: its key column, key kind, column kinds and references.
  private readonly specs: Record<string, SyncTableSpec>;
  // The options the client was created with (`storage`, `auto`, `interval`, `deviceName`).
  private readonly options: OfflineOptions;

  // --- the copy --------------------------------------------------------------
  // Rows as the server last sent them: table → key → row. Changed only by pull and upload answers.
  private readonly serverRows = new Map<string, Map<string, SyncRow>>();
  // Rows the app sees: `serverRows` plus the outbox applied on top. Read by `list()` and `get()`.
  private readonly visibleRows = new Map<string, Map<string, SyncRow>>();
  // Local changes not uploaded yet, oldest first.
  private outbox: SyncChange[] = [];
  // Changes the server refused, with its reason, until `clearRefused()`.
  private refusals: RefusedChange[] = [];

  // --- sync position, saved on the device ------------------------------------
  // This device's id from `register`; `undefined` until the first sync.
  private client: string | undefined;
  // Where the last pull ended; the next pull asks for changes after it.
  private watermark: string | null = null;
  // Tables already copied to the device; the next pull only sends their changes.
  private held = new Set<string>();
  // Number for the next outbox change (its `mutation`).
  private next = 1;

  // --- background sync -------------------------------------------------------
  // The last status (`online`, `syncing`, error); `status()` adds `pending`.
  private current: SyncStatus = { online: false, syncing: false, pending: 0 };
  // The sync in progress, so `now()` never runs two at once.
  private running: Promise<void> | undefined;
  // Set when `now()` is called during a sync: run another one after it.
  private again = false;
  // The pending 300 ms timer of `soon()`.
  private soonTimer: ReturnType<typeof setTimeout> | undefined;
  // Cleanups for the background sync (timer, `online` listener, live streams), run by `stop()`.
  private readonly stoppers: Array<() => void> = [];
  // Set by `stop()`: no more background syncs.
  private stopped = false;

  // --- listeners -------------------------------------------------------------
  // Listeners of `onChange`: told which tables' rows changed.
  private readonly changeListeners = new Set<(tables: string[]) => void>();
  // Listeners of `onStatus`: told every status change.
  private readonly statusListeners = new Set<(status: SyncStatus) => void>();

  /**
   * Load the copy from storage (`ready`), and start syncing in the background
   * unless `auto` is off.
   */
  constructor(
    options: OfflineOptions,
    transport: SyncTransport,
    specs: Record<string, SyncTableSpec>
  ) {
    this.options = options;
    this.storage = options.storage;
    this.transport = transport;
    this.specs = specs;
    this.ready = this.load();
    if (options.auto ?? true) this.start();
  }

  // --- the public face -------------------------------------------------------

  /**
   * Sync now: upload the outbox, then pull. If a sync is already running, it
   * returns that one and runs another shortly after.
   */
  now(): Promise<void> {
    if (this.running) {
      this.again = true;
      return this.running;
    }
    this.running = this.ready
      .then(() => this.cycle())
      .finally(() => {
        this.running = undefined;
        if (this.again && !this.stopped) {
          this.again = false;
          this.soon();
        }
      });
    return this.running;
  }

  /** Online or not, syncing or not, how many changes wait, and the last error. */
  status(): SyncStatus {
    return { ...this.current, pending: this.outbox.length };
  }

  /** The changes the server refused, oldest first, with its reason for each. */
  refused(): RefusedChange[] {
    return [...this.refusals];
  }

  /** Forget the refused changes, e.g. once the user has seen them. */
  async clearRefused(): Promise<void> {
    const gone = this.refusals.map((r): [string, null] => [
      refusedKey(r.mutation),
      null,
    ]);
    this.refusals = [];
    await this.storage.write(gone);
  }

  /**
   * Call `listener` with the tables whose rows changed on the device. Returns the
   * unsubscribe.
   */
  onChange(listener: (tables: string[]) => void): () => void {
    this.changeListeners.add(listener);
    return () => this.changeListeners.delete(listener);
  }

  /** Call `listener` whenever `status()` changes. Returns the unsubscribe. */
  onStatus(listener: (status: SyncStatus) => void): () => void {
    this.statusListeners.add(listener);
    return () => this.statusListeners.delete(listener);
  }

  /** Stop the background sync: timers, the `online` listener and live streams. */
  stop(): void {
    this.stopped = true;
    if (this.soonTimer !== undefined) clearTimeout(this.soonTimer);
    for (const stop of this.stoppers.splice(0)) stop();
  }

  /**
   * Delete the whole copy, outbox included, from memory and storage (e.g. on
   * sign-out).
   */
  async clear(): Promise<void> {
    await this.ready;
    const stored = await this.storage.entries("");
    await this.storage.write(
      stored.map(([key]): [string, null] => [key, null])
    );
    const tables = [...this.visibleRows.keys()];
    this.serverRows.clear();
    this.visibleRows.clear();
    this.outbox = [];
    this.refusals = [];
    this.client = undefined;
    this.watermark = null;
    this.held = new Set();
    this.next = 1;
    this.emit(tables);
    this.setStatus({});
  }

  /**
   * `online`, the generated table object, with its reads and writes moved onto
   * the copy. Everything else (file columns, `changes`) stays as it is.
   */
  table<T extends object>(name: string, online: T): T {
    const spec = this.specs[name];
    if (!spec) return online;
    const source = online as OnlineTable;
    const store = this;
    const local = {
      ...online,
      async list(query?: LocalQuery): Promise<unknown[]> {
        return store.list(name, spec, source, query ?? {});
      },
      async get(id: unknown, query?: { select?: string }): Promise<unknown> {
        const rows = await store.list(name, spec, source, {
          select: query?.select,
          filter: { [spec.key]: `eq.${keyText(id)}` },
          limit: 1,
        });
        return rows[0];
      },
      async create(row: SyncRow): Promise<SyncRow> {
        return store.create(name, spec, source, row);
      },
      async update(id: unknown, row: SyncRow): Promise<SyncRow> {
        return store.update(name, spec, id, row);
      },
      async delete(id: unknown): Promise<{ deleted: true }> {
        await store.remove(name, id);
        return { deleted: true };
      },
    };
    // A live stream of this table's changes is a hint that a pull has something.
    if ((this.options.auto ?? true) && source.changes) {
      const changes = source.changes;
      const subscription = changes.subscribe(
        {},
        {
          element: () => this.soon(),
          resync: () => this.soon(),
        }
      );
      this.stoppers.push(() => subscription.close());
    }
    return local as unknown as T;
  }

  // --- reading the copy ------------------------------------------------------

  /**
   * Answer a `list()` from the copy: filter, order, page and shape the rows. A
   * table not copied yet is synced first; if it still is not here, the server
   * answers.
   */
  private async list(
    name: string,
    spec: SyncTableSpec,
    online: OnlineTable,
    query: LocalQuery
  ): Promise<unknown[]> {
    await this.ready;
    // A `select` the server would refuse is refused here too, before anything is read.
    const select =
      query.select === undefined ? undefined : parseSelect(query.select);
    if (select) checkSelect(select, name, spec, this.specs);
    const embeds = select ? embeddedTables(select, spec, this.specs) : [];
    if (![name, ...embeds].every((t) => this.held.has(t))) {
      // Not copied yet: the first sync brings it, and until it has, the server answers.
      await this.now().catch(() => undefined);
    }
    if (![name, ...embeds].every((t) => this.held.has(t))) {
      if (!online.list)
        throw new Error(`\`${name}\` is not on this device yet`);
      return online.list(query);
    }
    let rows = [...(this.visibleRows.get(name)?.values() ?? [])];
    for (const [column, text] of Object.entries(query.filter ?? {})) {
      if (text === undefined) continue;
      if (!(column in spec.columns))
        throw new Error(`\`${name}\` has no field \`${column}\` to filter on`);
      const filter = parseFilter(column, text);
      rows = rows.filter((row) =>
        matches(row[column], filter, spec.columns[column] ?? "text")
      );
    }
    if (query.order) {
      for (const part of query.order.split(",")) {
        const column = part.split(".")[0].trim();
        if (!(column in spec.columns))
          throw new Error(`\`${name}\` has no field \`${column}\` to order by`);
      }
      // Ordered the way Postgres orders: nulls last ascending, first descending.
      rows = sortLiveRows(
        rows,
        liveRowsPlan(spec.key, { orderBy: query.order })
      );
    }
    const offset = query.offset ?? 0;
    rows = rows.slice(
      offset,
      query.limit === undefined ? undefined : offset + query.limit
    );
    return rows.map((row) =>
      select ? this.applySelect(row, select, spec) : { ...row }
    );
  }

  /**
   * Shape one row the way `select` asks, like the server does:
   * `"id, name:title, owner(name)"` → `{ id, name: <title>, owner: { name } }`.
   * An embed like `owner(name)` reads the referenced row from the copy (`null` if missing).
   */
  private applySelect(
    row: SyncRow,
    items: SelectItem[],
    spec: SyncTableSpec
  ): SyncRow {
    const out: SyncRow = {};
    for (const item of items) {
      const key = item.alias ?? item.name;
      if (!item.children) {
        out[key] = row[item.name];
        continue;
      }
      const into = spec.rels[item.name];
      const intoSpec = into === undefined ? undefined : this.specs[into];
      const value = row[item.name];
      const found =
        into === undefined || value === null || value === undefined
          ? undefined
          : this.visibleRows.get(into)?.get(keyText(value));
      out[key] =
        found && intoSpec
          ? this.applySelect(found, item.children, intoSpec)
          : null;
    }
    return out;
  }

  // --- writing the copy ------------------------------------------------------

  /**
   * Add a row on the device and queue it for upload. A UUID key is made here;
   * an integer key comes from the server, so that create goes online (offline it
   * fails with a clear message).
   */
  private async create(
    name: string,
    spec: SyncTableSpec,
    online: OnlineTable,
    given: SyncRow
  ): Promise<SyncRow> {
    await this.ready;
    let id = given[spec.key];
    if (id === undefined || id === null) {
      if (spec.keyKind !== "uuid") {
        // The server gives this key out, so the row is made there.
        if (!online.create)
          throw new Error(`\`${name}\` cannot be created from this client`);
        let made: SyncRow;
        try {
          made = (await online.create(given)) as SyncRow;
        } catch (e) {
          // `fetch` rejects with a `TypeError` when the server cannot be reached.
          if (!(e instanceof TypeError)) throw e;
          throw new Error(
            `Adding to "${name}" while offline is not supported yet (it is planned). Please try again when you are back online.`
          );
        }
        await this.adopt(name, spec, made);
        return { ...made };
      }
      id = newUuid();
    }
    const key = keyText(id);
    if (this.visibleRows.get(name)?.has(key)) {
      throw new Error(
        `\`${name}\` already has a row with ${spec.key} = ${key}`
      );
    }
    const row: SyncRow = {};
    for (const column of Object.keys(spec.columns)) row[column] = null;
    Object.assign(row, given, { [spec.key]: id });
    await this.record({ mutation: 0, table: name, op: "insert", key, row });
    return { ...row };
  }

  /** Change a row on the device and queue the change. The key cannot change. */
  private async update(
    name: string,
    spec: SyncTableSpec,
    id: unknown,
    changes: SyncRow
  ): Promise<SyncRow> {
    await this.ready;
    const key = keyText(id);
    const current = this.visibleRows.get(name)?.get(key);
    if (!current)
      throw new Error(`\`${name}\` has no row with ${spec.key} = ${key}`);
    if (spec.key in changes && keyText(changes[spec.key]) !== key) {
      throw new Error(`a row's ${spec.key} cannot be changed`);
    }
    await this.record({
      mutation: 0,
      table: name,
      op: "update",
      key,
      row: { ...changes },
    });
    return { ...current, ...changes };
  }

  /** Delete a row on the device and queue the delete. */
  private async remove(name: string, id: unknown): Promise<void> {
    await this.ready;
    const key = keyText(id);
    if (!this.visibleRows.get(name)?.has(key))
      throw new Error(`\`${name}\` has no row with key ${key}`);
    await this.record({ mutation: 0, table: name, op: "delete", key });
  }

  /**
   * Put a local change in the outbox (numbered and stored), show it in `visibleRows`,
   * tell listeners, and sync shortly.
   */
  private async record(change: SyncChange): Promise<void> {
    const numbered = { ...change, mutation: this.next };
    await this.storage.write([
      [outboxKey(numbered.mutation), JSON.stringify(numbered)],
      ["m/next", String(numbered.mutation + 1)],
    ]);
    this.next = numbered.mutation + 1;
    this.outbox.push(numbered);
    this.refresh(change.table, [change.key]);
    this.emit([change.table]);
    this.setStatus({});
    if ((this.options.auto ?? true) && !this.stopped) this.soon();
  }

  /**
   * Keep a row the server just created (integer-key create) in `serverRows`, until the
   * next pull confirms it.
   */
  private async adopt(
    name: string,
    spec: SyncTableSpec,
    row: SyncRow
  ): Promise<void> {
    if (!this.held.has(name)) return;
    const key = keyText(row[spec.key]);
    await this.storage.write([[serverRowKey(name, key), JSON.stringify(row)]]);
    this.serverRowsOf(name).set(key, row);
    this.refresh(name, [key]);
    this.emit([name]);
  }

  // --- syncing ---------------------------------------------------------------

  /**
   * Sync in the background: once loaded, every `interval` (30 s by default) and
   * when the device comes back online.
   */
  private start(): void {
    const tick = () => void this.now().catch(() => undefined);
    void this.ready.then(tick);
    const timer = setInterval(tick, this.options.interval ?? 30_000);
    this.stoppers.push(() => clearInterval(timer));
    const target = globalThis as {
      addEventListener?: (type: string, fn: () => void) => void;
      removeEventListener?: (type: string, fn: () => void) => void;
    };
    if (target.addEventListener) {
      target.addEventListener("online", tick);
      this.stoppers.push(() => target.removeEventListener?.("online", tick));
    }
  }

  /**
   * Sync 300 ms from now. Calls in that window share one sync, so writes made
   * together go up together.
   */
  private soon(): void {
    if (this.stopped) return;
    if (this.soonTimer !== undefined) clearTimeout(this.soonTimer);
    this.soonTimer = setTimeout(() => {
      this.soonTimer = undefined;
      void this.now().catch(() => undefined);
    }, 300);
  }

  /**
   * One sync with status updates. If the server no longer knows this device, it
   * starts over as a new device once (the outbox is kept).
   */
  private async cycle(): Promise<void> {
    this.setStatus({ syncing: true });
    try {
      try {
        await this.exchange();
      } catch (e) {
        // The device is not known (any more), or not this user's: start over as a
        // new one. The outbox stays: the server decides what this user may do.
        if (!(e instanceof SyncHttpError && e.status === 404)) throw e;
        await this.forgetDevice();
        await this.exchange();
      }
      this.setStatus({
        syncing: false,
        online: true,
        lastSync: new Date().toISOString(),
        error: undefined,
      });
    } catch (e) {
      this.setStatus({
        syncing: false,
        online: !unreachable(e) && this.current.online,
        error: e instanceof Error ? e.message : String(e),
      });
      throw e;
    }
  }

  /**
   * Register the device when needed, upload the whole outbox in batches, then
   * pull.
   */
  private async exchange(): Promise<void> {
    if (this.client === undefined) {
      const answer = (await this.transport.post("register", {
        name: this.options.deviceName ?? null,
      })) as {
        client_id: string;
      };
      await this.storage.write([["m/client", answer.client_id]]);
      this.client = answer.client_id;
    }
    while (this.outbox.length)
      await this.upload(this.outbox.slice(0, UPLOAD_BATCH));
    await this.pull();
  }

  /**
   * Drop the device id and the copy of the server's rows. The outbox stays, so no
   * local change is lost.
   */
  private async forgetDevice(): Promise<void> {
    const stored = await this.storage.entries("b/");
    await this.storage.write([
      ...stored.map(([key]): [string, null] => [key, null]),
      ["m/client", null],
      ["m/watermark", null],
      ["m/held", null],
    ]);
    const tables = [...this.serverRows.keys()];
    this.client = undefined;
    this.watermark = null;
    this.held = new Set();
    this.serverRows.clear();
    for (const table of tables) this.rebuild(table);
    this.emit(tables);
  }

  /**
   * Send a batch of outbox changes. Applied ones move into `serverRows`, refused ones
   * into `refused()`. Changes the server did not answer stay for the next sync.
   */
  private async upload(batch: SyncChange[]): Promise<void> {
    const answer = (await this.transport.post("push", {
      client_id: this.client,
      changes: batch.map(({ mutation, table, op, key, row }) => ({
        mutation,
        table,
        op,
        key,
        row,
      })),
    })) as PushAnswer;
    const results = new Map(answer.results.map((r) => [r.mutation, r]));
    const writes: Array<[string, string | null]> = [];
    const done = new Set<number>();
    const touched = new Map<string, Set<string>>();
    for (const change of batch) {
      const result = results.get(change.mutation);
      if (!result) break;
      done.add(change.mutation);
      writes.push([outboxKey(change.mutation), null]);
      if (result.status === "refused") {
        const refusal: RefusedChange = {
          ...change,
          error: result.error ?? "refused",
        };
        this.refusals.push(refusal);
        writes.push([refusedKey(change.mutation), JSON.stringify(refusal)]);
      } else if (this.held.has(change.table)) {
        // Handled by the server: its row now, until the next pull says exactly.
        const row = applyChange(
          this.serverRows.get(change.table)?.get(change.key),
          change
        );
        if (row) this.serverRowsOf(change.table).set(change.key, row);
        else this.serverRows.get(change.table)?.delete(change.key);
        writes.push([
          serverRowKey(change.table, change.key),
          row ? JSON.stringify(row) : null,
        ]);
      }
      const keys = touched.get(change.table) ?? new Set<string>();
      keys.add(change.key);
      touched.set(change.table, keys);
    }
    if (!done.size) throw new Error("sync/push answered none of the changes");
    await this.storage.write(writes);
    this.outbox = this.outbox.filter((c) => !done.has(c.mutation));
    for (const [table, keys] of touched) this.refresh(table, [...keys]);
    this.emit([...touched.keys()]);
    this.setStatus({});
  }

  /**
   * Fetch what changed on the server since the last pull and write it into
   * `serverRows`, then rebuild `visibleRows`, so local changes not uploaded yet
   * stay on top.
   */
  private async pull(): Promise<void> {
    const answer = (await this.transport.post("pull", {
      client_id: this.client,
      watermark: this.watermark,
      tables: [...this.held],
    })) as PullAnswer;
    const writes: Array<[string, string | null]> = [];
    const held = new Set<string>();
    const changed: string[] = [];
    for (const entry of answer.tables) {
      const spec = this.specs[entry.table];
      // A table this client was generated before: nothing here could show it.
      if (!spec) continue;
      held.add(entry.table);
      const rows = this.serverRowsOf(entry.table);
      if (entry.reset) {
        for (const key of rows.keys())
          writes.push([serverRowKey(entry.table, key), null]);
        rows.clear();
      }
      for (const key of entry.removed) {
        rows.delete(key);
        writes.push([serverRowKey(entry.table, key), null]);
      }
      for (const row of entry.rows) {
        const key = keyText(row[spec.key]);
        rows.set(key, row);
        writes.push([serverRowKey(entry.table, key), JSON.stringify(row)]);
      }
      if (entry.reset || entry.rows.length || entry.removed.length)
        changed.push(entry.table);
    }
    // A table the server no longer sends is not this device's to keep.
    for (const table of this.held) {
      if (held.has(table)) continue;
      for (const key of this.serverRows.get(table)?.keys() ?? [])
        writes.push([serverRowKey(table, key), null]);
      this.serverRows.delete(table);
      changed.push(table);
    }
    writes.push(
      ["m/watermark", answer.watermark],
      ["m/held", JSON.stringify([...held])]
    );
    await this.storage.write(writes);
    this.watermark = answer.watermark;
    this.held = held;
    for (const table of changed) this.rebuild(table);
    if (changed.length) this.emit(changed);
  }

  // --- keeping `visibleRows` up to date --------------------------------------
  //
  // `visibleRows` = `serverRows` + the outbox applied on top. The methods below
  // load `serverRows` and the outbox from storage and recompute `visibleRows`
  // whenever either of them changes.

  /**
   * Read the device id, watermark, copy, outbox and refusals back from storage,
   * then build `visibleRows`.
   */
  private async load(): Promise<void> {
    for (const [key, value] of await this.storage.entries("")) {
      if (key === "m/client") this.client = value;
      else if (key === "m/watermark") this.watermark = value;
      else if (key === "m/held")
        this.held = new Set(JSON.parse(value) as string[]);
      else if (key === "m/next") this.next = Number(value);
      else if (key.startsWith("o/"))
        this.outbox.push(JSON.parse(value) as SyncChange);
      else if (key.startsWith("r/"))
        this.refusals.push(JSON.parse(value) as RefusedChange);
      else if (key.startsWith("b/")) {
        const slash = key.indexOf("/", 2);
        this.serverRowsOf(key.slice(2, slash)).set(
          key.slice(slash + 1),
          JSON.parse(value) as SyncRow
        );
      }
    }
    this.outbox.sort((a, b) => a.mutation - b.mutation);
    const tables = new Set([
      ...this.serverRows.keys(),
      ...this.outbox.map((c) => c.table),
    ]);
    for (const table of tables) this.rebuild(table);
    this.setStatus({});
  }

  /** The server rows of `table`, created empty on first use. */
  private serverRowsOf(table: string): Map<string, SyncRow> {
    let rows = this.serverRows.get(table);
    if (!rows) {
      rows = new Map();
      this.serverRows.set(table, rows);
    }
    return rows;
  }

  /**
   * Recompute the visible row for each of `keys` in `table`: start from the
   * server's row (if any), apply the outbox changes for that key in order, and
   * store the result in `visibleRows`, or remove it there when the outcome is
   * "deleted" / "never existed".
   */
  private refresh(table: string, keys: string[]): void {
    let visible = this.visibleRows.get(table);
    if (!visible) {
      visible = new Map();
      this.visibleRows.set(table, visible);
    }
    for (const key of keys) {
      let row = this.serverRows.get(table)?.get(key);
      for (const change of this.outbox) {
        if (change.table === table && change.key === key)
          row = applyChange(row, change);
      }
      if (row) visible.set(key, row);
      else visible.delete(key);
    }
  }

  /** Recompute all visible rows of `table` (see `refresh`). */
  private rebuild(table: string): void {
    this.visibleRows.set(table, new Map());
    const keys = new Set(this.serverRows.get(table)?.keys() ?? []);
    for (const change of this.outbox)
      if (change.table === table) keys.add(change.key);
    this.refresh(table, [...keys]);
  }

  /** Tell the `onChange` listeners which tables changed. */
  private emit(tables: string[]): void {
    if (!tables.length) return;
    for (const listener of this.changeListeners) listener(tables);
  }

  /** Update the status and tell the `onStatus` listeners. */
  private setStatus(change: Partial<SyncStatus>): void {
    this.current = { ...this.current, ...change };
    const status = this.status();
    for (const listener of this.statusListeners) listener(status);
  }
}

// --- the read vocabulary, applied to the copy ---------------------------------

/** One item of a `select`: `name`, `alias:name`, or `name(items…)` for an embed. */
type SelectItem = { alias?: string; name: string; children?: SelectItem[] };

/** How deep embeds may nest, as on the server. */
const MAX_EMBED_DEPTH = 8;

/**
 * Parse a `select` exactly like the server does (`rest/query.rs`): items
 * separated by `,`, each `name`, `alias:name` or `name(items…)`. What the
 * server refuses (`...`, `!inner`, empty names, unclosed brackets, too deep)
 * throws here with the same reason.
 */
function parseSelect(src: string): SelectItem[] {
  const cursor: Cursor = { src, at: 0 };
  const items = parseItems(cursor, 0);
  if (cursor.at < src.length)
    throw new Error(
      `\`select\`: unexpected \`${src[cursor.at]}\` at position ${cursor.at}`
    );
  return items;
}

/** A position in the `select` string. */
type Cursor = { src: string; at: number };

/** `item ("," item)*`. */
function parseItems(cursor: Cursor, depth: number): SelectItem[] {
  const items = [parseItem(cursor, depth)];
  while (cursor.src[cursor.at] === ",") {
    cursor.at++;
    items.push(parseItem(cursor, depth));
  }
  return items;
}

/** `(alias ":")? name ("(" items ")")?`. */
function parseItem(cursor: Cursor, depth: number): SelectItem {
  const first = parseName(cursor);
  const item: SelectItem = { name: first };
  if (cursor.src[cursor.at] === ":") {
    cursor.at++;
    item.alias = first;
    item.name = parseName(cursor);
  }
  if (cursor.src[cursor.at] === "(") {
    if (depth + 1 > MAX_EMBED_DEPTH)
      throw new Error(
        `\`select\`: embeds nest more than ${MAX_EMBED_DEPTH} levels deep`
      );
    cursor.at++;
    // `name()` is an embed of nothing, so `checkSelect` can say what is wrong.
    item.children =
      cursor.src[cursor.at] === ")" ? [] : parseItems(cursor, depth + 1);
    if (cursor.src[cursor.at] !== ")")
      throw new Error(`\`select\`: \`${item.name}(\` is never closed`);
    cursor.at++;
  }
  return item;
}

/** A bare name, up to the next `,`, `(`, `)` or `:`; refuses `...`, `!` and empty names. */
function parseName(cursor: Cursor): string {
  const start = cursor.at;
  while (
    cursor.at < cursor.src.length &&
    !",():".includes(cursor.src[cursor.at])
  )
    cursor.at++;
  const name = cursor.src.slice(start, cursor.at).trim();
  if (name.startsWith("..."))
    throw new Error(
      "`select`: the `...` spread operator is not taken by this API"
    );
  const bang = name.indexOf("!");
  if (bang >= 0)
    throw new Error(
      `\`select\`: \`!${name.slice(bang + 1)}\` on \`${name.slice(
        0,
        bang
      )}\` is not taken by this API`
    );
  if (!name)
    throw new Error(
      "`select`: a column name is missing (an item is `column`, `alias:column` or `relation(columns…)`)"
    );
  return name;
}

/**
 * Check a parsed `select` against the tables, like the server does: every
 * column exists, and every embed follows a reference to another table. Inside
 * an embed of a table that is not synced, the server decides.
 */
function checkSelect(
  items: SelectItem[],
  table: string,
  spec: SyncTableSpec,
  specs: Record<string, SyncTableSpec>
): void {
  for (const item of items) {
    if (item.name.includes("::"))
      throw new Error(
        `\`select\`: the \`::\` cast in \`${item.name}\` is not taken by this API`
      );
    if (!(item.name in spec.columns)) {
      if (item.children && item.name in specs)
        throw new Error(
          `\`select\`: \`${item.name}(…)\` is a one-to-many embed, which this API does not take yet`
        );
      throw new Error(`\`${table}\` has no field \`${item.name}\` to select`);
    }
    if (!item.children) continue;
    const into = spec.rels[item.name];
    if (into === undefined)
      throw new Error(
        `\`select\`: \`${table}\`.\`${item.name}\` is not a key to another table, so \`${item.name}(…)\` embeds nothing`
      );
    const intoSpec = specs[into];
    if (intoSpec) checkSelect(item.children, into, intoSpec, specs);
  }
}

/** The tables a `select` embeds, at any depth: they have to be on the device too. */
function embeddedTables(
  items: SelectItem[],
  spec: SyncTableSpec,
  specs: Record<string, SyncTableSpec>
): string[] {
  const out: string[] = [];
  for (const item of items) {
    if (!item.children) continue;
    const into = spec.rels[item.name];
    const intoSpec = into === undefined ? undefined : specs[into];
    // Not a reference, or to a table that is not synced: the server decides.
    if (into === undefined || !intoSpec) {
      out.push(`${item.name} (not synced)`);
      continue;
    }
    out.push(into, ...embeddedTables(item.children, intoSpec, specs));
  }
  return out;
}

type ColumnKind = SyncTableSpec["columns"][string];

/** A filter's text as a value of the column. */
function operand(text: string, kind: ColumnKind): unknown {
  if (kind === "number") return Number(text);
  if (kind === "boolean") return text === "true";
  return text;
}

/** The comparisons a filter may use, as on the server. */
const FILTER_OPS = [
  "eq",
  "ne",
  "gt",
  "gte",
  "lt",
  "lte",
  "in",
  "nin",
  "like",
  "ilike",
  "is_null",
];

/** A filter split into its comparison and its operand text. */
type Filter = { op: string; text: string };

/**
 * A filter (`op.operand`) split and checked, refusing what the server refuses.
 *
 *   `?rank=gte.3`           → { op: "gte", text: "3" }
 *   `?title=like.%milk%`    → { op: "like", text: "%milk%" }
 *   `?id=in.(1,2,3)`        → { op: "in", text: "(1,2,3)" }
 *   `?project=is_null.true` → { op: "is_null", text: "true" }
 */
function parseFilter(column: string, filter: string): Filter {
  const dot = filter.indexOf(".");
  if (dot < 0)
    throw new Error(
      `\`${column}=${filter}\` is not a filter: a filter is \`column=op.value\` — the comparisons are ${FILTER_OPS.join(
        ", "
      )}`
    );
  const op = filter.slice(0, dot);
  const text = filter.slice(dot + 1);
  if (op === "not")
    throw new Error(
      `\`${column}=not.…\` is not taken: negate with \`ne\`, \`nin\`, or \`is_null.false\``
    );
  if (!FILTER_OPS.includes(op)) throw new Error(`\`${op}\` is not a filter`);
  if (
    (op === "in" || op === "nin") &&
    !(text.startsWith("(") && text.endsWith(")"))
  )
    throw new Error(
      `\`${op}\` takes a parenthesised list, e.g. \`${op}.(1,2,3)\``
    );
  if (op === "is_null" && text !== "true" && text !== "false")
    throw new Error(`\`is_null\` takes \`true\` or \`false\`, not \`${text}\``);
  return { op, text };
}

/** Whether `value` passes `filter`, the way the server's SQL decides it. */
function matches(
  value: unknown,
  { op, text }: Filter,
  kind: ColumnKind
): boolean {
  const missing = value === null || value === undefined;
  if (op === "is_null") return missing === (text === "true");
  // SQL: a comparison with null is never true.
  if (missing) return false;
  switch (op) {
    case "eq":
      return compareValues(value, operand(text, kind)) === 0;
    case "ne":
      return compareValues(value, operand(text, kind)) !== 0;
    case "gt":
      return compareValues(value, operand(text, kind)) > 0;
    case "gte":
      return compareValues(value, operand(text, kind)) >= 0;
    case "lt":
      return compareValues(value, operand(text, kind)) < 0;
    case "lte":
      return compareValues(value, operand(text, kind)) <= 0;
    case "in":
    case "nin": {
      const list = filterOperand(op, text);
      const items = Array.isArray(list) ? list : [String(list)];
      const member = items.some(
        (t) => compareValues(value, operand(String(t), kind)) === 0
      );
      return op === "in" ? member : !member;
    }
    case "like":
    case "ilike": {
      const pattern = text
        .replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
        .replace(/%/g, ".*")
        .replace(/_/g, ".");
      return new RegExp(`^${pattern}$`, op === "ilike" ? "is" : "s").test(
        String(value)
      );
    }
    default:
      throw new Error(`\`${op}\` is not a filter`);
  }
}
