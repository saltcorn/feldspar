// The generated client's live rows (TODO.md "Live updates" L3.10–L3.11): the
// filter a change subscription is sent with, the reducer that applies one
// change to a read's rows, and `LiveRows`, which keeps a read and a
// subscription together. `helper.ts` is the generated helper every client
// ships with, so this is the code an application runs — tested without a
// server, against a fake table.

import { describe, expect, it } from "vitest";

import {
  LiveRows,
  applyLiveChange,
  liveRowsPlan,
  mergeLiveRows,
  whereObject,
  type LiveChange,
  type LiveHandlers,
  type LiveTable,
} from "./helper";

type Card = { id: number; board: number; column: string; position: number | null; title: string };

const card = (id: number, position: number | null, extra: Partial<Card> = {}): Card => ({
  id,
  board: 7,
  column: "todo",
  position,
  title: `card ${id}`,
  ...extra,
});

const ids = (rows: readonly { id: number }[]) => rows.map((row) => row.id);

describe("whereObject", () => {
  it("lowers a read's filters to the socket's filter object", () => {
    expect(
      whereObject({
        board: "eq.7",
        position: "gte.1.5",
        column: "in.(todo,done)",
        title: 'nin.("a,b",c)',
        due: "is_null.true",
        done: "is_null.false",
        name: "ilike.%urgent%",
        skipped: undefined,
      }),
    ).toEqual({
      board: { eq: "7" },
      // Only the first dot separates the comparison from its operand.
      position: { gte: "1.5" },
      column: { in: ["todo", "done"] },
      title: { nin: ["a,b", "c"] },
      due: { is_null: true },
      done: { is_null: false },
      name: { ilike: "%urgent%" },
    });
  });

  it("leaves what it cannot read for the server to refuse, and refuses a value with no comparison", () => {
    expect(whereObject({ column: "in.todo", due: "is_null.maybe" })).toEqual({
      column: { in: "todo" },
      due: { is_null: "maybe" },
    });
    expect(whereObject({ column: "in.()" })).toEqual({ column: { in: [] } });
    expect(() => whereObject({ board: "7" })).toThrow(/eq\.7/);
  });
});

describe("liveRowsPlan", () => {
  it("parses the order and keeps whole rows when nothing is selected", () => {
    expect(liveRowsPlan("id", { orderBy: "column,position.desc" })).toEqual({
      key: "id",
      order: [
        { column: "column", desc: false },
        { column: "position", desc: true },
      ],
      columns: undefined,
      refetch: false,
      select: undefined,
    });
  });

  it("adds the key and the order columns a select leaves out", () => {
    const plan = liveRowsPlan("id", { orderBy: "position", select: "title" });
    expect(plan.select).toBe("title,id,position");
    expect(plan.columns).toEqual(["title", "id", "position"]);
    expect(plan.refetch).toBe(false);
  });

  it("reads changed rows again for a select with an embed or an alias", () => {
    expect(liveRowsPlan("id", { select: "title,board(name)" })).toMatchObject({
      refetch: true,
      columns: undefined,
      select: "title,board(name),id",
    });
    expect(liveRowsPlan("id", { select: "id,heading:title" }).refetch).toBe(true);
  });
});

describe("applyLiveChange", () => {
  const plan = liveRowsPlan("id", { orderBy: "position" });
  const rows = [card(1, 1), card(2, 2), card(3, 3)];

  it("inserts in order", () => {
    const step = applyLiveChange(rows, { op: "insert", key: 4, row: card(4, 1.5) }, plan);
    expect(ids(step.rows)).toEqual([1, 4, 2, 3]);
    expect(step.refetch).toBeUndefined();
  });

  it("updates in place and moves the row when its order column changes", () => {
    const renamed = applyLiveChange(rows, { op: "update", key: 2, row: card(2, 2, { title: "renamed" }) }, plan);
    expect(ids(renamed.rows)).toEqual([1, 2, 3]);
    expect(renamed.rows[1].title).toBe("renamed");
    const moved = applyLiveChange(rows, { op: "update", key: 1, row: card(1, 9) }, plan);
    expect(ids(moved.rows)).toEqual([2, 3, 1]);
  });

  it("puts an entering row in, and takes a leaving or deleted one out by key alone", () => {
    expect(ids(applyLiveChange(rows, { op: "enter", key: 5, row: card(5, 0) }, plan).rows)).toEqual([5, 1, 2, 3]);
    expect(ids(applyLiveChange(rows, { op: "leave", key: 2 }, plan).rows)).toEqual([1, 3]);
    expect(ids(applyLiveChange(rows, { op: "delete", key: "3" }, plan).rows)).toEqual([1, 2]);
    // A leave for a row this page never had changes nothing.
    expect(ids(applyLiveChange(rows, { op: "leave", key: 99 }, plan).rows)).toEqual([1, 2, 3]);
  });

  it("treats an insert of a row it already has as the row it now is", () => {
    const step = applyLiveChange(rows, { op: "insert", key: 2, row: card(2, 2, { title: "again" }) }, plan);
    expect(ids(step.rows)).toEqual([1, 2, 3]);
    expect(step.rows[1].title).toBe("again");
  });

  it("does not change the rows it was given", () => {
    const before = JSON.stringify(rows);
    applyLiveChange(rows, { op: "update", key: 1, row: card(1, 9) }, plan);
    applyLiveChange(rows, { op: "delete", key: 1 }, plan);
    expect(JSON.stringify(rows)).toBe(before);
  });

  it("orders nulls as Postgres does, and descending orders reverse them", () => {
    const withNull = applyLiveChange(rows, { op: "insert", key: 4, row: card(4, null) }, plan);
    expect(ids(withNull.rows)).toEqual([1, 2, 3, 4]);
    const desc = liveRowsPlan("id", { orderBy: "position.desc" });
    const reversed = applyLiveChange([card(3, 3), card(1, 1)], { op: "insert", key: 4, row: card(4, null) }, desc);
    expect(ids(reversed.rows)).toEqual([4, 3, 1]);
  });

  it("appends a new row when no order is asked for, and keeps ties in the order they had", () => {
    const unordered = liveRowsPlan("id");
    const asRead = [card(3, 1), card(1, 1)];
    expect(ids(applyLiveChange(asRead, { op: "insert", key: 2, row: card(2, 0) }, unordered).rows)).toEqual([3, 1, 2]);
    expect(ids(applyLiveChange(asRead, { op: "update", key: 1, row: card(1, 1) }, plan).rows)).toEqual([3, 1]);
  });

  it("cuts a change's row to the selected columns", () => {
    const narrow = liveRowsPlan("id", { orderBy: "position", select: "title" });
    const step = applyLiveChange<object>([], { op: "insert", key: 1, row: card(1, 1) }, narrow);
    expect(step.rows).toEqual([{ title: "card 1", id: 1, position: 1 }]);
  });

  it("names the key to read again when the change cannot stand in for a read", () => {
    // A stream whose payload is the key alone.
    const keyOnly = applyLiveChange(rows, { op: "update", key: 2 }, plan);
    expect(keyOnly).toEqual({ rows, refetch: 2 });
    // A select that embeds.
    const embeds = liveRowsPlan("id", { select: "title,board(name)" });
    expect(applyLiveChange(rows, { op: "insert", key: 4, row: card(4, 4) }, embeds).refetch).toBe(4);
    // Taking a row out never needs a read.
    expect(applyLiveChange(rows, { op: "delete", key: 2 }, embeds).refetch).toBeUndefined();
  });
});

describe("mergeLiveRows", () => {
  it("puts back the rows a read answered and takes out the keys it did not", () => {
    const plan = liveRowsPlan("id", { orderBy: "position" });
    const rows = [card(1, 1), card(2, 2), card(3, 3)];
    const merged = mergeLiveRows(rows, [2, "3", 4], [card(2, 5), card(4, 0)], plan);
    expect(ids(merged)).toEqual([4, 1, 2]);
  });
});

// --- LiveRows, against a fake table ------------------------------------------

type Read = {
  query: { select?: string; filter?: Record<string, string | undefined>; order?: string } | undefined;
  resolve(rows: unknown[]): void;
  reject(error: unknown): void;
};

function fakeTable() {
  const reads: Read[] = [];
  const subscriptions: Array<{
    query: unknown;
    handlers: LiveHandlers<{ value: LiveChange<unknown, unknown> }>;
    closed: boolean;
  }> = [];
  const table = {
    list(query?: Read["query"]) {
      return new Promise<unknown[]>((resolve, reject) => reads.push({ query, resolve, reject }));
    },
    changes: {
      name: "cards_live",
      table: "cards",
      key: "id",
      connection: undefined as never,
      subscribe(query: unknown, handlers: LiveHandlers<{ value: LiveChange<unknown, unknown> }>) {
        const subscription = { query, handlers, closed: false };
        subscriptions.push(subscription);
        return {
          close() {
            subscription.closed = true;
          },
        };
      },
    },
  };
  const send = (value: LiveChange<unknown, unknown>, at = subscriptions.length - 1) =>
    subscriptions[at].handlers.element({ value });
  return { table: table as unknown as LiveTable, reads, subscriptions, send };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("LiveRows", () => {
  it("subscribes with the read's filter and reads once the subscription is ready", async () => {
    const { table, reads, subscriptions } = fakeTable();
    const live = new LiveRows<Card>(table, { where: { board: "eq.7" }, orderBy: "position" } as never);
    live.start();
    expect(subscriptions.map((s) => s.query)).toEqual([{ where: { board: "eq.7" } }]);
    expect(reads).toHaveLength(0);
    subscriptions[0].handlers.ready?.({} as never);
    expect(reads.map((r) => r.query)).toEqual([{ select: undefined, filter: { board: "eq.7" }, order: "position" }]);
    expect(live.current.loading).toBe(true);
    reads[0].resolve([card(1, 1), card(2, 2)]);
    await settle();
    expect(live.current).toEqual({ rows: [card(1, 1), card(2, 2)], loading: false, error: undefined });
  });

  it("applies changes that arrive while the read is in flight on top of it, in order", async () => {
    const { table, reads, subscriptions, send } = fakeTable();
    const live = new LiveRows<Card>(table, { orderBy: "position" } as never);
    live.start();
    subscriptions[0].handlers.ready?.({} as never);
    send({ op: "update", key: 1, row: card(1, 5) });
    send({ op: "insert", key: 3, row: card(3, 0) });
    send({ op: "delete", key: 2 });
    // The read was taken before some of those changes and after others: it
    // ends as the last change left it either way.
    reads[0].resolve([card(1, 1), card(2, 2)]);
    await settle();
    expect(ids(live.current.rows)).toEqual([3, 1]);
    expect(live.current.rows[1].position).toBe(5);
  });

  it("tells its listeners after every change, with a new state object", async () => {
    const { table, reads, subscriptions, send } = fakeTable();
    const live = new LiveRows<Card>(table);
    let calls = 0;
    const stop = live.onChange(() => (calls += 1));
    live.start();
    subscriptions[0].handlers.ready?.({} as never);
    reads[0].resolve([card(1, 1)]);
    await settle();
    const before = live.current;
    send({ op: "leave", key: 1 });
    expect(live.current).not.toBe(before);
    expect(live.current.rows).toEqual([]);
    stop();
    send({ op: "insert", key: 2, row: card(2, 2) });
    expect(calls).toBe(2);
  });

  it("reads again after a lag and after a reconnect, and drops a read that was overtaken", async () => {
    const { table, reads, subscriptions } = fakeTable();
    const live = new LiveRows<Card>(table);
    live.start();
    const handlers = subscriptions[0].handlers;
    handlers.ready?.({} as never);
    handlers.lagged?.(3);
    expect(reads).toHaveLength(2);
    // The first read lands late: the second is the one that counts.
    reads[1].resolve([card(2, 2)]);
    reads[0].resolve([card(1, 1)]);
    await settle();
    expect(ids(live.current.rows)).toEqual([2]);
    // A reconnect: `resync`, then `ready` again once resubscribed.
    handlers.resync?.();
    handlers.ready?.({} as never);
    expect(reads).toHaveLength(3);
    reads[2].resolve([card(2, 2), card(3, 3)]);
    await settle();
    expect(ids(live.current.rows)).toEqual([2, 3]);
  });

  it("reads changed keys again, in one read, when the stream sends keys only", async () => {
    const { table, reads, subscriptions, send } = fakeTable();
    const live = new LiveRows<Card>(table, { orderBy: "position" } as never);
    live.start();
    subscriptions[0].handlers.ready?.({} as never);
    reads[0].resolve([card(1, 1), card(2, 2)]);
    await settle();
    send({ op: "update", key: 1 });
    send({ op: "insert", key: 3 });
    send({ op: "enter", key: 4 });
    await settle();
    expect(reads.map((r) => r.query)).toContainEqual({ select: undefined, filter: { id: "in.(1,3,4)" } });
    // 4 changed again before the read answered: the newer change settles it.
    send({ op: "leave", key: 4 });
    reads[1].resolve([card(1, 9), card(3, 0), card(4, 4)]);
    await settle();
    expect(ids(live.current.rows)).toEqual([3, 2, 1]);
  });

  it("takes out a re-read key the read no longer answers", async () => {
    const { table, reads, subscriptions, send } = fakeTable();
    const live = new LiveRows<Card>(table, { select: "title,board(name)" } as never);
    live.start();
    subscriptions[0].handlers.ready?.({} as never);
    expect(reads[0].query?.select).toBe("title,board(name),id");
    reads[0].resolve([{ id: 1, title: "a", board: { name: "B" } }]);
    await settle();
    send({ op: "update", key: 1, row: card(1, 1) });
    await settle();
    reads[1].resolve([]);
    await settle();
    expect(live.current.rows).toEqual([]);
  });

  it("reports a refusal and a failed read, and stops when closed", async () => {
    const { table, reads, subscriptions, send } = fakeTable();
    const live = new LiveRows<Card>(table);
    live.start();
    subscriptions[0].handlers.error?.({ code: "unavailable", message: "" });
    expect(live.current).toMatchObject({ loading: false, error: { code: "unavailable" } });

    const again = fakeTable();
    const other = new LiveRows<Card>(again.table);
    other.start();
    again.subscriptions[0].handlers.ready?.({} as never);
    again.reads[0].reject(new Error("list failed: 500"));
    await settle();
    expect(other.current.error).toEqual(new Error("list failed: 500"));

    live.close();
    expect(subscriptions[0].closed).toBe(true);
    send({ op: "insert", key: 1, row: card(1, 1) }, 0);
    expect(reads).toHaveLength(0);
    // Started again (as React's strict mode does), it subscribes afresh.
    live.start();
    expect(subscriptions).toHaveLength(2);
  });
});
