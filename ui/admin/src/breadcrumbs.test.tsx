// The trail over a screen's title: that each screen's trail is the way down to
// it from its sidebar section, that every step links to the screen it names,
// and that the header draws the trail in place of the pre-title.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import {
  LOADING_LABEL,
  agentRunsCrumbs,
  applicationListCrumbs,
  changesetsCrumbs,
  checklistCrumbs,
  checklistsCrumbs,
  tableCrumbs,
  workflowRunsCrumbs,
  type Crumb,
} from "./breadcrumbs";
import { PageHeader } from "./layout";

const t = (text: string) => text;

/** A trail as `label → href` lines, which is what a reader of a failure wants. */
const shape = (crumbs: Crumb[]) => crumbs.map((c) => `${c.label} → ${c.href ?? "(text)"}`);

describe("breadcrumb trails", () => {
  it("takes an application's checklist run back up to the application", () => {
    expect(shape(checklistCrumbs(t, "app 1", "Shop", "cl/9", "Smoke test"))).toEqual([
      "Applications → #/applications",
      "Shop → #/applications/app%201/edit",
      "Checklists → #/applications/app%201/checklists",
      "Smoke test → #/applications/app%201/checklists/cl%2F9",
    ]);
  });

  it("takes a Data Layer checklist back up through its Settings tab", () => {
    expect(shape(checklistsCrumbs(t, null, null))).toEqual([
      "Settings → #/settings",
      "Checklists → #/settings/checklists",
    ]);
    const run = shape(checklistCrumbs(t, null, null, "c1", "Nightly"));
    expect(run[run.length - 1]).toBe("Nightly → #/checklists/c1");
  });

  it("names a table by its label but links it by its name", () => {
    expect(shape(tableCrumbs(t, "book_loans", "Book loans"))).toEqual([
      "Tables → #/tables",
      "Book loans → #/tables/book_loans",
    ]);
    // No label (or a blank one) is the name.
    expect(tableCrumbs(t, "books", " ")[1].label).toBe("books");
  });

  it("keeps a narrowed changeset list under its table", () => {
    expect(shape(changesetsCrumbs(t, "books"))).toEqual([
      "Tables → #/tables",
      "books → #/tables/books",
      "Changesets → #/changesets?table=books",
    ]);
    expect(shape(changesetsCrumbs(t))).toEqual([
      "Tables → #/tables",
      "Changesets → #/changesets",
    ]);
  });

  it("goes through the trigger or the agent a run belongs to", () => {
    expect(shape(workflowRunsCrumbs(t, "tr1", "On signup"))).toEqual([
      "Triggers → #/triggers",
      "On signup → #/triggers/tr1/edit",
      "Runs → #/triggers/tr1/runs",
    ]);
    expect(shape(agentRunsCrumbs(t, "help desk"))).toEqual([
      "Agents → #/agents",
      "help desk → #/agents/help%20desk/chat",
      "Runs → #/agents/help%20desk/runs",
    ]);
  });

  it("keeps its shape and links while a name is still loading", () => {
    const crumbs = applicationListCrumbs(t, "a1", null, "pages");
    expect(shape(crumbs)).toEqual([
      "Applications → #/applications",
      `${LOADING_LABEL} → #/applications/a1/edit`,
      "Pages → #/applications/a1/pages",
    ]);
  });
});

describe("PageHeader with a trail", () => {
  it("draws every step as a link, in place of the pre-title", () => {
    const html = renderToStaticMarkup(
      <PageHeader
        pretitle="Data"
        crumbs={tableCrumbs(t, "books")}
        title="Data"
        docTitle="books data"
      />,
    );
    expect(html).toContain('aria-label="Breadcrumb"');
    expect(html).toMatch(
      /<li class="breadcrumb-item"><a href="#\/tables">Tables<\/a><\/li><li class="breadcrumb-item"><a href="#\/tables\/books">books<\/a><\/li>/,
    );
    expect(html).not.toContain("page-pretitle");
  });

  it("keeps the pre-title on a root screen, which has no trail", () => {
    const html = renderToStaticMarkup(<PageHeader pretitle="Data" title="Tables" />);
    expect(html).toContain('<div class="page-pretitle">Data</div>');
    expect(html).not.toContain("breadcrumb");
  });
});
