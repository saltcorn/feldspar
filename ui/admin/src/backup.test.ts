/**
 * The Backup tab's model: what the dialog does to a selection.
 *
 * The screen is buttons and a modal, which a browser tests better than a suite
 * does. What can go wrong *silently* is the arithmetic underneath, and two rules
 * in it matter enough to pin down:
 *
 * - rows cannot be included without their table's metadata, and unticking a table
 *   has to take its rows with it — otherwise the dialog shows a selection the
 *   server will narrow behind the admin's back;
 * - the same shape describes what this server has and what an uploaded file has,
 *   so "everything" and the summary line have to work on both without knowing
 *   which they were handed.
 */

import { describe, expect, it } from "vitest";

import {
  choices,
  dataChoices,
  everything,
  isEmpty,
  itemDescription,
  summarise,
  withAnalytics,
  withTableData,
  withTables,
  type BackupContents,
} from "./backup";

/** What the server offers — this installation's, or a backup file's. */
const contents: BackupContents = {
  tables: [
    { name: "books", label: "Books", count: 3 },
    { name: "authors", label: "authors", count: 1 },
  ],
  applications: [{ name: "blog", label: "The blog", count: null }],
  file_stores: [{ name: "assets", label: "Uploaded files", count: null }],
  users: 2,
  modules: 1,
  db_connections: 1,
  streams: 2,
  datasets: 2,
  models: 1,
  workspaces: 1,
  fits: 3,
  llm_providers: 1,
  agents: 1,
  code_libraries: 2,
  triggers: 4,
  views: 7,
  pages: 1,
  ssl: true,
  settings: true,
};

describe("what a backup includes", () => {
  it("starts with everything on offer", () => {
    const selection = everything(contents);
    expect(selection.tables).toEqual(["books", "authors"]);
    expect(selection.table_data).toEqual(["books", "authors"]);
    expect(selection.applications).toEqual(["blog"]);
    expect(selection.file_stores).toEqual(["assets"]);
    expect(selection.users && selection.agents && selection.triggers && selection.ssl).toBe(
      true,
    );
    expect(selection.views && selection.pages && selection.llm_providers).toBe(true);
    expect(
      selection.modules &&
        selection.db_connections &&
        selection.streams &&
        selection.analytics &&
        selection.fits &&
        selection.settings,
    ).toBe(true);
    expect(isEmpty(selection)).toBe(false);
  });

  /** A file that holds nothing offers nothing to tick — which is what keeps the
   * restore dialog from showing rows for things the backup does not carry. */
  it("selects nothing when nothing is on offer", () => {
    const selection = everything({
      tables: [],
      applications: [],
      file_stores: [],
      users: 0,
      modules: 0,
      db_connections: 0,
      streams: 0,
      datasets: 0,
      models: 0,
      workspaces: 0,
      fits: 0,
      llm_providers: 0,
      agents: 0,
      code_libraries: 0,
      triggers: 0,
      views: 0,
      pages: 0,
      ssl: false,
      settings: false,
    });
    expect(isEmpty(selection)).toBe(true);
  });

  /** A backup can hold a table's definition and not its rows: the restore dialog
   * must not offer to put back rows that are not in the file. */
  it("offers no rows for a table whose data is not there", () => {
    const selection = everything({
      ...contents,
      tables: [
        { name: "books", label: "Books", count: 3 },
        { name: "authors", label: "authors", count: null },
      ],
    });
    expect(selection.tables).toEqual(["books", "authors"]);
    expect(selection.table_data).toEqual(["books"]);
  });

  it("takes a table's rows out with the table", () => {
    const selection = withTables(everything(contents), ["books"]);
    expect(selection.tables).toEqual(["books"]);
    expect(selection.table_data).toEqual(["books"]);
    // …and the rows picker now offers only what is left.
    expect(dataChoices(contents, selection).map((c) => c.value)).toEqual(["books"]);
  });

  it("refuses rows for a table whose definition is not included", () => {
    const selection = withTableData(withTables(everything(contents), ["books"]), [
      "books",
      "authors",
    ]);
    expect(selection.table_data).toEqual(["books"]);
  });

  it("can include definitions with no rows at all", () => {
    const selection = withTableData(everything(contents), []);
    expect(selection.tables).toEqual(["books", "authors"]);
    expect(selection.table_data).toEqual([]);
    expect(isEmpty(selection)).toBe(false);
  });

  it("describes how much of each thing there is", () => {
    expect(itemDescription(contents.tables[0], "row")).toBe("3 rows");
    // Singular, because "1 rows" is the tell of a count that was not thought
    // about.
    expect(itemDescription(contents.tables[1], "row")).toBe("1 row");
    // Not counted: the item's own label stands in, and a label that is only the
    // name says nothing worth showing twice.
    expect(itemDescription(contents.applications[0], "")).toBe("The blog");
    expect(itemDescription({ name: "assets", label: "assets", count: null }, "file")).toBe("");
    expect(choices(contents.tables, "row")[0]).toEqual({
      value: "books",
      label: "books",
      description: "3 rows",
    });
  });

  it("says what is ticked in a sentence", () => {
    expect(summarise(everything(contents), contents)).toBe(
      "all 2 tables, 1 application, views, pages, 1 file store, users, modules, database connections, streams, analytics with fits, LLM providers, agents, code libraries, triggers, SSL settings, other settings.",
    );

    const narrowed = withTableData(withTables(everything(contents), ["books"]), []);
    expect(summarise({ ...narrowed, agents: false, ssl: false, pages: false }, contents)).toBe(
      "1 table, no rows, 1 application, views, 1 file store, users, modules, database connections, streams, analytics with fits, LLM providers, code libraries, triggers, other settings.",
    );
    // With no application to carry them, views and pages are not worth a word.
    expect(summarise({ ...narrowed, applications: [], file_stores: [] }, contents)).toBe(
      "1 table, no rows, users, modules, database connections, streams, analytics with fits, LLM providers, agents, code libraries, triggers, SSL settings, other settings.",
    );

    expect(
      summarise(
        {
          tables: [],
          table_data: [],
          applications: [],
          file_stores: [],
          users: false,
          modules: false,
          db_connections: false,
          streams: false,
          analytics: false,
          fits: false,
          llm_providers: false,
          agents: false,
          code_libraries: false,
          triggers: false,
          views: false,
          pages: false,
          ssl: false,
          settings: false,
        },
        contents,
      ),
    ).toBe("Nothing selected.");
  });
});

describe("the fits", () => {
  /** Fits belong to models: unticking Analytics takes them with it, and
   * ticking it again does not bring them back unasked. */
  it("go when the Analytics choice goes", () => {
    const off = withAnalytics(everything(contents), false);
    expect(off.analytics).toBe(false);
    expect(off.fits).toBe(false);
    const on = withAnalytics(off, true);
    expect(on.analytics).toBe(true);
    expect(on.fits).toBe(false);
    expect(summarise({ ...on, fits: false }, contents)).toContain("analytics,");
  });
});
