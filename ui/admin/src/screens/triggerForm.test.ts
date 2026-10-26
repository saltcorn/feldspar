/**
 * Which table the trigger form reads an action's *declaration* for.
 *
 * The screen knows nothing about any particular setting — that is the whole
 * point of `config_spec` — but it does have to ask the right question, because
 * an action may declare different settings for different tables (`send_email`
 * offers one attachment checkbox per File field of the table the trigger fires
 * on). This is that question, and it is a function so it can be pinned without
 * rendering the form.
 */

import { describe, expect, it } from "vitest";

import { actionSpecTable, isStreamEvent } from "./TriggerForm";

describe("the table an action's settings are read for", () => {
  it("is the chosen table on a row event", () => {
    expect(actionSpecTable("insert", "orders")).toBe("orders");
    // Fires on either write, and is a table event like each of them.
    expect(actionSpecTable("insert_or_update", "places")).toBe("places");
    expect(actionSpecTable("update", "orders")).toBe("orders");
    expect(actionSpecTable("delete", "orders")).toBe("orders");
  });

  it("is nothing on a stream event, which has a channel but no table", () => {
    // A stream event's channel names a *stream* (§8), so asking for the
    // actions declared "for the table `boiler`" would be asking about a table
    // that does not exist.
    expect(actionSpecTable("stream", "boiler")).toBeUndefined();
  });

  it("is nothing on a kind that has no row", () => {
    // A login or a nightly job has no row, so it has no file to attach and no
    // table-dependent setting of any kind. Asking anyway would offer a control
    // that could not mean anything.
    for (const kind of ["login", "none", "daily", "error", "startup"]) {
      expect(actionSpecTable(kind, "orders")).toBeUndefined();
    }
  });

  it("is nothing until a table has been chosen", () => {
    expect(actionSpecTable("insert", "")).toBeUndefined();
    expect(actionSpecTable("insert", "   ")).toBeUndefined();
  });
});

describe("which kinds name a stream", () => {
  it("is the stream event and nothing else", () => {
    // What this decides is the channel box: a table picker for a table event,
    // the stream picker for this one, and no box at all for the rest.
    expect(isStreamEvent("stream")).toBe(true);
    for (const kind of ["insert", "update", "insert_or_update", "delete", "login", "none", "daily", "error", "startup", "often", "hourly", "weekly"]) {
      expect(isStreamEvent(kind)).toBe(false);
    }
    // A kind this bundle has never heard of names no stream, rather than
    // throwing on a server that added one.
    expect(isStreamEvent("something-newer")).toBe(false);
  });
});
