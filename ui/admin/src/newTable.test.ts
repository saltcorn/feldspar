/**
 * The "New table" dialog's rules: when Create may be pressed, and what a chosen
 * file suggests the table be called.
 *
 * Both are worth pinning because both are silent when wrong. A dialog that lets
 * Create through with no file chosen sends `{name, csv: ""}` and the admin gets
 * a server error for a mistake the form could have caught; and a suggested name
 * that is not a SQL identifier gets refused by the server *after* the file has
 * been read and uploaded, which reads as "CSV import is broken" rather than as
 * "rename the file".
 */

import { describe, expect, it } from "vitest";

import {
  EMPTY_NEW_TABLE_FORM,
  PRIMARY_DATABASE,
  creatableDatabases,
  databaseLabel,
  importedMessage,
  newTableError,
  providerKey,
  providerLabel,
  splitProviderKey,
  tableNameFromFile,
  toBase64,
  type NewTableForm,
} from "./newTable";

/** A form with only what a test states set. */
function form(over: Partial<NewTableForm>): NewTableForm {
  return { ...EMPTY_NEW_TABLE_FORM, ...over };
}

/** A stand-in for a chosen file; only its presence matters here. */
const chosen = { name: "invoice.csv" } as File;

describe("newTableError", () => {
  it("wants a name for either kind of table", () => {
    expect(newTableError(form({}))).toMatch(/name/);
    expect(newTableError(form({ name: "   " }))).toMatch(/name/);
    expect(newTableError(form({ name: "invoice" }))).toBe(null);
  });

  it("wants a file only when the table comes from one", () => {
    expect(newTableError(form({ name: "invoice", source: "csv" }))).toMatch(/CSV/);
    expect(newTableError(form({ name: "invoice", source: "csv", file: chosen }))).toBe(null);
    // A blank table with a file left over from a switched-away choice is fine:
    // the file is simply not sent.
    expect(newTableError(form({ name: "invoice", source: "blank", file: chosen }))).toBe(null);
  });
});

describe("tableNameFromFile", () => {
  it("makes an identifier out of the file's base name", () => {
    expect(tableNameFromFile("invoice.csv")).toBe("invoice");
    expect(tableNameFromFile("Invoice List.csv")).toBe("invoice_list");
    expect(tableNameFromFile("invoice-list.CSV")).toBe("invoice_list");
    expect(tableNameFromFile("invoice_list.csv")).toBe("invoice_list");
    // Punctuation a SQL identifier cannot carry is dropped, not kept.
    expect(tableNameFromFile("2024 sales (final).csv")).toBe("_2024_sales_final");
    // A file with no extension is all base name.
    expect(tableNameFromFile("invoices")).toBe("invoices");
    // Nothing usable in it: the box stays empty and the admin types a name.
    expect(tableNameFromFile("!!.csv")).toBe("");
  });
});

describe("importedMessage", () => {
  it("counts in the singular when there is one row", () => {
    expect(importedMessage("invoice", 1)).toBe("1 row imported into invoice.");
    expect(importedMessage("invoice", 12)).toBe("12 rows imported into invoice.");
  });
});

describe("which database a new table goes in", () => {
  it("defaults to Saltcorn's own, so an installation with no connections is unchanged", () => {
    expect(EMPTY_NEW_TABLE_FORM.database).toBe(PRIMARY_DATABASE);
    expect(creatableDatabases([])).toEqual([PRIMARY_DATABASE]);
  });

  it("offers every connected connection beside it", () => {
    expect(
      creatableDatabases([
        { name: "reporting", connected: true },
        { name: "warehouse", connected: true },
      ]),
    ).toEqual([PRIMARY_DATABASE, "reporting", "warehouse"]);
  });

  it("leaves out a connection that is not connected", () => {
    // On the Connections screen a broken connection must be shown, because
    // editing it is the repair. Here it would be a choice that can only fail —
    // there is no driver to send the CREATE TABLE to.
    expect(
      creatableDatabases([
        { name: "reporting", connected: false },
        { name: "warehouse", connected: true },
      ]),
    ).toEqual([PRIMARY_DATABASE, "warehouse"]);
  });

  it("names the primary in words and a connection by its own name", () => {
    expect(databaseLabel(PRIMARY_DATABASE)).toMatch(/Saltcorn/);
    expect(databaseLabel("reporting")).toBe("reporting");
  });

  it("will not submit with no database chosen", () => {
    expect(newTableError(form({ name: "invoice", database: "" }))).toMatch(/database/);
  });
});

describe("the metadata-table choice", () => {
  it("asks only which metadata table — not a name, and not a database", () => {
    expect(newTableError(form({ source: "metadata" }))).toMatch(/metadata table/);
    expect(
      newTableError(form({ source: "metadata", metadataTable: "_fd_triggers", database: "" })),
    ).toBe(null);
  });

  it("starts with nothing chosen", () => {
    expect(EMPTY_NEW_TABLE_FORM.metadataTable).toBe("");
  });
});

describe("the table-provider choice", () => {
  it("will not create a provided table until a provider is chosen", () => {
    expect(newTableError(form({ name: "headlines", source: "provider" }))).toMatch(/provider/);
    expect(
      newTableError(
        form({
          name: "headlines",
          source: "provider",
          provider: providerKey("@saltcorn/rss", "RSS feed"),
        }),
      ),
    ).toBe(null);
  });

  it("does not ask a provided table which database it is in, because it is in none", () => {
    // A blank `database` blocks a database table and must not block this one:
    // the rows come from a module, and there is nothing to create anywhere.
    expect(
      newTableError(
        form({
          name: "headlines",
          source: "provider",
          provider: providerKey("@saltcorn/rss", "RSS feed"),
          database: "",
        }),
      ),
    ).toBe(null);
    expect(newTableError(form({ name: "invoice", database: "" }))).toMatch(/database/);
  });

  it("round-trips the (module, provider) pair the select holds as one value", () => {
    const key = providerKey("@saltcorn/rss", "RSS feed");
    expect(splitProviderKey(key)).toEqual({
      module: "@saltcorn/rss",
      provider: "RSS feed",
    });
    // Nothing chosen is not half a pair.
    expect(splitProviderKey("")).toBe(null);
    // A provider name containing the kind of punctuation a plugin author uses
    // survives, which is why the separator is a NUL and not a slash or a colon.
    const odd = providerKey("@acme/db", "Table: remote/primary");
    expect(splitProviderKey(odd)?.provider).toBe("Table: remote/primary");
  });

  it("names the module beside the provider, because two modules may both supply a `Table`", () => {
    expect(providerLabel("@saltcorn/rss", "RSS feed")).toBe("RSS feed (@saltcorn/rss)");
  });
});

describe("a table from a map file", () => {
  it("wants the file before Create may be pressed", () => {
    expect(newTableError(form({ name: "parks", source: "geo" }))).toMatch(/GeoJSON/);
    expect(newTableError(form({ name: "parks", source: "geo", file: chosen }))).toBe(null);
    expect(tableNameFromFile("London Parks.geojson")).toBe("london_parks");
  });

  it("sends the bytes as base64, however many there are", () => {
    expect(toBase64(new Uint8Array([0x50, 0x4b, 0x03, 0x04]))).toBe("UEsDBA==");
    // More than one slice of `String.fromCharCode` arguments.
    const big = new Uint8Array(100_000).map((_, i) => i % 251);
    const back = Uint8Array.from(atob(toBase64(big)), (c) => c.charCodeAt(0));
    expect(back).toEqual(big);
  });
});
