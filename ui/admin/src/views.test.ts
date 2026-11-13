/**
 * The Views and Pages tabs' model (TODO "Saltcorn UI" 9.4).
 *
 * What can go wrong quietly here, each with a test:
 *
 * - **The tabs are the server's call.** An application that is not views and
 *   pages has only Settings; the screen never names a framework.
 * - **A v1 name has spaces in it.** The link to the view on its subdomain and
 *   the path parameter the API is called with both have to encode it, or
 *   `List Books` is a 404 that looks like a missing view.
 * - **A view whose pattern is not here is flagged**, but not while the pattern
 *   list is still loading — a flash of every view marked broken is a lie.
 */

import { describe, expect, it } from "vitest";

import type { Roles } from "./roles";
import {
  NO_VIEWS,
  appTabs,
  deleteConfirmation,
  deleteReferenceLines,
  nameParam,
  newPageHref,
  pagePropertiesHref,
  pageReferenceLines,
  pageReferencesReport,
  viewEditorHref,
  viewReferenceLines,
  pageRows,
  viewRows,
  viewUrl,
  applyStep,
  createViewBody,
  layoutJson,
  newViewError,
  referencesReport,
  saveViewBody,
  stepFormValues,
  stepTitle,
  type PageItem,
  type PatternItem,
  type StepItem,
  type ViewItem,
} from "./views";

const here = { protocol: "https:", host: "example.com:3032" };

const roles: Roles = [
  { role: 1, name: "admin" },
  { role: 100, name: "public" },
] as Roles;

function view(
  name: string,
  viewpattern: string,
  extra: Partial<ViewItem> = {},
): ViewItem {
  return {
    id: `id-${name}`,
    name,
    description: "",
    viewpattern,
    table_name: "books",
    configuration: {},
    min_role: 1,
    slug: null,
    attributes: {},
    ...extra,
  };
}

function pattern(name: string, label = name): PatternItem {
  return {
    name,
    label,
    description: "",
    table_required: true,
    view_quantity: null,
    routes: [],
    steps: [],
    module: null,
  };
}

describe("appTabs", () => {
  it("are the framework's settings tabs, linked to their screens", () => {
    const tabs = appTabs({
      id: "a1",
      settings_tabs: [
        { id: "site", label: "Site", groups: ["identity", "framework", "settings"] },
        { id: "views", label: "Views", groups: ["views"] },
        { id: "advanced", label: "Advanced", groups: ["apis", "translations"] },
      ],
    });
    expect(tabs.map((t) => t.id)).toEqual(["site", "views", "advanced"]);
    expect(tabs[0].href).toBe("#/applications/a1/edit");
    expect(tabs[1].href).toBe("#/applications/a1/views");
    expect(tabs[2].href).toBe("#/applications/a1/edit/advanced");
  });
});

describe("names with spaces", () => {
  it("open the view on the application's subdomain", () => {
    expect(viewUrl("booksdb", "List Books", here)).toBe(
      "https://booksdb.example.com:3032/view/List%20Books",
    );
  });

  it("are encoded as a path parameter", () => {
    expect(nameParam("List Books")).toBe("List%20Books");
    expect(nameParam("a/b")).toBe("a%2Fb");
  });
});

describe("viewRows", () => {
  const views = [
    view("List Books", "List"),
    view("Chat", "Room", { min_role: 100, table_name: null }),
  ];

  it("shows the pattern's label, the table, the role and the link", () => {
    const [row] = viewRows(
      views,
      [pattern("List", "List view")],
      roles,
      "booksdb",
      here,
    );
    expect(row).toEqual({
      name: "List Books",
      description: "",
      pattern: "List view",
      patternMissing: false,
      table: "books",
      role: "admin (1)",
      url: "https://booksdb.example.com:3032/view/List%20Books",
    });
  });

  it("flags a view whose pattern this server does not have", () => {
    const rows = viewRows(views, [pattern("List")], roles, "booksdb", here);
    expect(rows[1].patternMissing).toBe(true);
    expect(rows[1].pattern).toBe("Room");
    expect(rows[1].table).toBe("—");
    expect(rows[1].role).toBe("public (100)");
  });

  it("flags nothing while the pattern list is not there", () => {
    const rows = viewRows(views, null, roles, "booksdb", here);
    expect(rows.map((r) => r.patternMissing)).toEqual([false, false]);
  });

  it("is empty for an application with no views, which the tab explains", () => {
    expect(viewRows([], null, roles, "booksdb", here)).toEqual([]);
    expect(NO_VIEWS).toMatch(/serves nothing/);
  });
});

describe("pageRows", () => {
  const page: PageItem = {
    id: "p1",
    name: "BooksOverview",
    title: "Books",
    description: "",
    layout: {},
    min_role: 1,
    attributes: { root_page_for_roles: [1, "100"] },
  };

  it("names the roles the page is the home page of", () => {
    const [row] = pageRows([page], roles, "booksdb", here);
    expect(row.homeFor).toEqual(["admin (1)", "public (100)"]);
    expect(row.url).toBe("https://booksdb.example.com:3032/page/BooksOverview");
    expect(row.role).toBe("admin (1)");
  });

  it("is nobody's home page without the attribute", () => {
    const [row] = pageRows(
      [{ ...page, attributes: {} }],
      roles,
      "booksdb",
      here,
    );
    expect(row.homeFor).toEqual([]);
  });
});

describe("deleteConfirmation", () => {
  it("names the view, the application and what breaks", () => {
    const text = deleteConfirmation("view", "List Books", "BooksDB");
    expect(text).toContain('"List Books"');
    expect(text).toContain("BooksDB");
    expect(text).toMatch(/link to it/);
  });

  it("names what refers to it and the library items its layout places (9.5)", () => {
    const view = {
      embedded_in: [],
      linked_from: ["Show Books"],
      pages: [],
      library: ["Book header"],
      places: ["Book card", "Footer"],
    };
    const text = deleteConfirmation(
      "view",
      "List Books",
      "BooksDB",
      deleteReferenceLines(viewReferenceLines(view), view.places),
    );
    expect(text).toContain('Linked to from the view "Show Books".');
    expect(text).toContain('Shown or linked to by the library item "Book header".');
    expect(text).toContain(
      'Its layout places the library items "Book card", "Footer"; they stay in the library.',
    );
    expect(text.endsWith("This cannot be undone.")).toBe(true);

    const page = {
      menu: ["Home"],
      home_page_for: ["public"],
      views: [],
      pages: ["Landing"],
      library: [],
      places: ["Book header"],
    };
    const lines = deleteReferenceLines(pageReferenceLines(page), page.places);
    expect(lines).toEqual([
      'Opened by the menu entry "Home".',
      'The home page of the role "public".',
      'Shown or linked to by the page "Landing".',
      'Its layout places the library item "Book header"; it stays in the library.',
    ]);
  });
});

describe("links to the editors", () => {
  it("open the wizard at a step, and a page's properties", () => {
    expect(viewEditorHref("a1", "Show Books")).toBe("#/applications/a1/views/Show%20Books");
    expect(viewEditorHref("a1", "Show Books", 2)).toBe(
      "#/applications/a1/views/Show%20Books?step=2",
    );
    // The builder's **Page properties** link (`sc-server/src/builder.rs`).
    expect(pagePropertiesHref("a1", "Books Overview")).toBe(
      "#/applications/a1/pages/Books%20Overview/properties",
    );
    expect(newPageHref("a1")).toBe("#/applications/a1/pages/new");
  });
});

// --- Configuring a view (TODO "Saltcorn UI" Phase 10) ---------------------

type StepField = StepItem["fields"][number];

const field = (
  name: string,
  type: string,
  extra: Partial<StepField> = {},
): StepField => ({
  name,
  label: name,
  type,
  required: false,
  default: null,
  options: [],
  multiline: false,
  secret: false,
  create_only: false,
  show_if: [],
  code_language: null,
  ...extra,
});

const stepOf = (over: Partial<StepItem>): StepItem => ({
  index: 0,
  name: "Views",
  count: 2,
  builder: false,
  skip: false,
  context_field: null,
  blurb: null,
  fields: [],
  values: {},
  issues: [],
  ...over,
});

describe("a configuration step", () => {
  it("puts its answers at the top level, typed, and removes a setting that was emptied", () => {
    const step = stepOf({
      fields: [
        field("list_view", "text"),
        field("list_width", "int"),
        field("in_card", "bool"),
      ],
    });
    const before = {
      list_view: "List Books",
      list_width: 6,
      subtables: { a: true },
    };
    const after = applyStep(before, step, {
      list_view: "",
      list_width: "4",
      in_card: "true",
    });
    // `subtables` is another step's, and untouched.
    expect(after).toEqual({
      list_width: 4,
      in_card: true,
      subtables: { a: true },
    });
    expect(before.list_view).toBe("List Books");
  });

  it("puts a step with a context field's answers under that key, beside what is there", () => {
    const step = stepOf({
      name: "Default state",
      context_field: "default_state",
      fields: [field("author", "int")],
    });
    const after = applyStep(
      { columns: [1], default_state: { _descending: true } },
      step,
      { author: "2" },
    );
    expect(after).toEqual({
      columns: [1],
      default_state: { _descending: true, author: 2 },
    });
  });

  it("leaves the configuration alone on a layout step and on a skipped one", () => {
    const configuration = { layout: { above: [] } };
    expect(applyStep(configuration, stepOf({ builder: true }), {})).toBe(
      configuration,
    );
    expect(applyStep(configuration, stepOf({ skip: true }), {})).toBe(
      configuration,
    );
  });

  it("opens with what the configuration holds, else each field's default", () => {
    const step = stepOf({
      fields: [
        field("list_width", "int", { default: 6 }),
        field("in_card", "bool"),
      ],
      values: { in_card: true },
    });
    expect(stepFormValues(step)).toEqual({ list_width: "6", in_card: "true" });
  });

  it("shows a layout step's layout and columns, and nothing else", () => {
    const shown = JSON.parse(
      layoutJson({ layout: { type: "blank" }, columns: [], list_view: "x" }),
    );
    expect(shown).toEqual({ layout: { type: "blank" }, columns: [] });
    expect(stepTitle({ index: 1, count: 5, name: "Default state" })).toBe(
      "Step 2 of 5: Default state",
    );
  });
});

describe("creating and renaming a view", () => {
  const patterns = [
    {
      name: "List",
      label: "List",
      description: "",
      table_required: true,
      view_quantity: "Many",
      routes: [],
      steps: ["Columns"],
      module: null,
    },
  ] as PatternItem[];
  const empty = {
    name: "",
    description: "",
    viewpattern: "",
    table_name: "",
    min_role: 100,
  };

  it("asks for a name, a pattern and, for a pattern over a table, a table", () => {
    expect(newViewError(empty, patterns)).toMatch(/name/);
    expect(newViewError({ ...empty, name: "Books" }, patterns)).toMatch(
      /pattern/,
    );
    expect(
      newViewError({ ...empty, name: "Books", viewpattern: "List" }, patterns),
    ).toMatch(/table/);
    const complete = {
      ...empty,
      name: " Books ",
      viewpattern: "List",
      table_name: "books",
    };
    expect(newViewError(complete, patterns)).toBeNull();
    expect(createViewBody(complete)).toEqual({
      name: "Books",
      description: null,
      viewpattern: "List",
      table_name: "books",
      min_role: 100,
    });
  });

  it("says before a rename what will keep the old name, and that nothing is rewritten", () => {
    const lines = referencesReport("List Books", {
      embedded_in: ["Filter books"],
      linked_from: [],
      pages: ["BooksOverview"],
      library: [],
      places: [],
    });
    expect(lines[0]).toContain('the view "Filter books"');
    expect(lines[1]).toContain('the page "BooksOverview"');
    expect(lines[lines.length - 1]).toContain("will not be updated");
    const none = referencesReport("List Books", {
      embedded_in: [],
      linked_from: [],
      pages: [],
      library: [],
      places: [],
    });
    expect(none).toHaveLength(1);
    expect(none[0]).toContain("Nothing");
  });

  it("says before a rename which library items show the view, and which it places", () => {
    const lines = referencesReport("List Books", {
      embedded_in: [],
      linked_from: [],
      pages: [],
      library: ["Book header"],
      places: ["Footer"],
    });
    expect(lines).toEqual([
      'Shown or linked to by the library item "Book header".',
      expect.stringContaining('refer to the view as "List Books"'),
      'Its layout places the library item "Footer"; it stays in the library.',
    ]);
  });

  it("says before a page rename what names the page", () => {
    const lines = pageReferencesReport("Home", {
      menu: ["Home", "Start"],
      home_page_for: ["public", "user"],
      views: ["Show Books"],
      pages: [],
      library: [],
      places: [],
    });
    expect(lines).toEqual([
      'Opened by the menu entries "Home", "Start".',
      'The home page of the roles "public", "user".',
      'Shown or linked to by the view "Show Books".',
      expect.stringContaining('refer to the page as "Home"'),
    ]);
    const none = pageReferencesReport("Home", {
      menu: [],
      home_page_for: [],
      views: [],
      pages: [],
      library: [],
      places: [],
    });
    expect(none).toEqual([expect.stringContaining("Nothing")]);
  });

  it("renames by saving the same view under another name", () => {
    const view = {
      id: "v1",
      name: "List Books",
      description: "",
      viewpattern: "List",
      table_name: "books",
      configuration: { columns: [] },
      min_role: 100,
      slug: null,
      attributes: {},
    } as ViewItem;
    expect(saveViewBody(view, { columns: [] }, "Books")).toEqual({
      name: "Books",
      description: "",
      viewpattern: "List",
      table_name: "books",
      configuration: { columns: [] },
      min_role: 100,
      slug: null,
      attributes: {},
    });
  });
});
