// Where a device keeps its offline copy (design §14.11): one key/value
// interface, and a storage each for memory, IndexedDB and SQLite.

/**
 * Where a device saves its offline copy: a simple key/value store of text.
 * Rows, the outbox and sync state are all saved as text values under text
 * keys. Each `write` is saved completely or not at all, so a crash halfway
 * through never leaves the copy half-updated.
 */
export interface SyncStorage {
  /** Every stored entry whose key starts with `prefix`. */
  entries(prefix: string): Promise<Array<[string, string]>>;
  /** Store and delete together (`null` deletes): all of it, or none. */
  write(changes: Array<[string, string | null]>): Promise<void>;
}

/** A copy that lives as long as the page: for tests, and for a device that should not keep one. */
export function memoryStorage(): SyncStorage {
  const map = new Map<string, string>();
  return {
    async entries(prefix) {
      return [...map].filter(([key]) => key.startsWith(prefix));
    },
    async write(changes) {
      for (const [key, value] of changes) {
        if (value === null) map.delete(key);
        else map.set(key, value);
      }
    },
  };
}

/** The browser's IndexedDB, in a database called `name`. */
export function indexedDbStorage(name: string): SyncStorage {
  let opened: Promise<IDBDatabase> | undefined;
  const database = (): Promise<IDBDatabase> =>
    (opened ??= new Promise((resolve, reject) => {
      const request = indexedDB.open(name, 1);
      request.onupgradeneeded = () => request.result.createObjectStore("kv");
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    }));
  const result = <T>(request: IDBRequest<T>): Promise<T> =>
    new Promise((resolve, reject) => {
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
  return {
    async entries(prefix) {
      const store = (await database())
        .transaction("kv", "readonly")
        .objectStore("kv");
      const range = IDBKeyRange.bound(prefix, `${prefix}\uffff`);
      const [keys, values] = await Promise.all([
        result(store.getAllKeys(range)),
        result(store.getAll(range)),
      ]);
      return keys.map((key, i): [string, string] => [
        String(key),
        String(values[i]),
      ]);
    },
    async write(changes) {
      const tx = (await database()).transaction("kv", "readwrite");
      const store = tx.objectStore("kv");
      for (const [key, value] of changes) {
        if (value === null) store.delete(key);
        else store.put(value, key);
      }
      await new Promise<void>((resolve, reject) => {
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
        tx.onabort = () => reject(tx.error);
      });
    },
  };
}

/**
 * An `expo-sqlite` database, as `SQLite.openDatabaseSync("…")` returns it: the
 * few methods this storage calls, so the client does not import `expo-sqlite`.
 */
export interface SqliteDatabase {
  execAsync(sql: string): Promise<void>;
  runAsync(
    sql: string,
    ...params: Array<string | number | null>
  ): Promise<unknown>;
  getAllAsync<T>(
    sql: string,
    ...params: Array<string | number | null>
  ): Promise<T[]>;
  withTransactionAsync(task: () => Promise<void>): Promise<void>;
}

/** A SQLite database on the phone (`expo-sqlite`), in the table `feldspar_sync`. */
export function sqliteStorage(db: SqliteDatabase): SyncStorage {
  let created: Promise<void> | undefined;
  const table = (): Promise<void> =>
    (created ??= db.execAsync(
      "CREATE TABLE IF NOT EXISTS feldspar_sync (k TEXT PRIMARY KEY NOT NULL, v TEXT NOT NULL)"
    ));
  return {
    async entries(prefix) {
      await table();
      const rows = await db.getAllAsync<{ k: string; v: string }>(
        "SELECT k, v FROM feldspar_sync WHERE k >= ? AND k < ? ORDER BY k",
        prefix,
        `${prefix}\uffff`
      );
      return rows.map((row): [string, string] => [row.k, row.v]);
    },
    async write(changes) {
      await table();
      await db.withTransactionAsync(async () => {
        for (const [key, value] of changes) {
          if (value === null)
            await db.runAsync("DELETE FROM feldspar_sync WHERE k = ?", key);
          else
            await db.runAsync(
              "INSERT OR REPLACE INTO feldspar_sync (k, v) VALUES (?, ?)",
              key,
              value
            );
        }
      });
    },
  };
}
