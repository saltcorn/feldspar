// The Source control screen is a tabbed card: the tabs are in the card's
// header (Tabler's card-header-tabs) and the open tab fills the card below
// them, not a free-standing nav above cards of its own.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import type { GetSyncStatusResponse } from "../client";
import {
  ChangedCount,
  rememberLinked,
  SourceControlPage,
  sourceControlPageKey,
  sourceControlTabs,
} from "./SourceControlPage";
import { LinkTab, StatusTab } from "./SyncPanels";

describe("SourceControlPage", () => {
  it("puts the tabs in the header of the card that holds the open tab", () => {
    rememberLinked(false);
    const html = renderToStaticMarkup(<SourceControlPage initialTab="zip" />);
    expect(html).toMatch(
      /<div data-testid="sync-card" class="card"><div class="card-header"><div class="[^"]*card-header-tabs[^"]*nav-tabs/,
    );
    const card = html.slice(html.indexOf('data-testid="sync-card"'));
    expect(card.indexOf('data-testid="sync-tab-zip"')).toBeLessThan(card.indexOf('data-testid="sync-zip"'));
    // The panel is the card's body, not a second card inside it.
    expect(html).toMatch(/<div data-testid="sync-zip"><div class="card-body">/);
  });

  it("lays Registry, the first tab when unlinked, out as two panes of the card, not cards of its own", () => {
    rememberLinked(false);
    const html = renderToStaticMarkup(<SourceControlPage />);
    const card = html.slice(html.indexOf('data-testid="sync-card"'));
    expect(card).toMatch(/<div class="g-0 row"><div class="registry-tree-pane col-lg-4">/);
    expect(card).toContain('data-testid="registry-tree"');
    expect(card).toContain("Pick an entry to see and edit its JSON.");
    // The tabbed card is the only card on the page.
    expect(html.match(/class="card"/g)).toHaveLength(1);
  });
});

describe("the Source control tabs", () => {
  it("are none until it is known whether the installation is linked, so none appears in front later", () => {
    expect(sourceControlTabs(null)).toEqual([]);
    expect(sourceControlTabs(false)).toEqual(["registry", "zip", "link"]);
    expect(sourceControlTabs(true)).toEqual(["status", "history", "registry", "zip", "link"]);
  });

  it("show Status, blank, while a linked installation's status is loading", () => {
    rememberLinked(true);
    const html = renderToStaticMarkup(<SourceControlPage />);
    expect(html).toMatch(/data-testid="sync-tab-status"[^>]*class="nav-link active"/);
    expect(html).toContain('data-testid="sync-tab-history"');
    expect(html).toMatch(/<div class="card-body" data-testid="sync-status-loading"><\/div>/);
    // The count is there already, a grey 0, so the tabs do not move when it loads.
    expect(html).toMatch(
      /data-testid="sync-tab-status"[^>]*>Status<span class="badge bg-secondary-lt ms-2 sync-count-badge">0<\/span>/,
    );
  });

  it("show History and Status on a tab change, as the remounted page remembers the link", () => {
    rememberLinked(true);
    const html = renderToStaticMarkup(<SourceControlPage initialTab="zip" />);
    const tabs = [...html.matchAll(/data-testid="sync-tab-(\w+)"/g)].map((m) => m[1]);
    expect(tabs).toEqual(["status", "history", "registry", "zip", "link"]);
  });
});

describe("sourceControlPageKey", () => {
  it("is the same for every tab, so a tab change keeps the page and what it loaded", () => {
    const key = sourceControlPageKey("/source-control");
    expect(sourceControlPageKey("/source-control?tab=status")).toBe(key);
    expect(sourceControlPageKey("/source-control?tab=zip")).toBe(key);
  });

  it("differs for a file to open, which starts a new page", () => {
    expect(sourceControlPageKey("/source-control?path=tables%2Fbooks.json&merge=1&tab=registry")).toBe(
      "/source-control?path=tables%2Fbooks.json&merge=1",
    );
    expect(sourceControlPageKey("/source-control?path=a.json")).not.toBe(
      sourceControlPageKey("/source-control?path=b.json"),
    );
  });
});

describe("ChangedCount", () => {
  it("is a grey 0 when nothing differs, blue with the count otherwise, at one fixed width", () => {
    expect(renderToStaticMarkup(<ChangedCount count={0} />)).toBe(
      '<span class="badge bg-secondary-lt ms-2 sync-count-badge">0</span>',
    );
    expect(renderToStaticMarkup(<ChangedCount count={12} />)).toBe(
      '<span class="badge bg-blue-lt ms-2 sync-count-badge">12</span>',
    );
  });
});

describe("StatusTab", () => {
  const status: GetSyncStatusResponse = {
    linked: true,
    link: {
      url: "git@example.com:acme/structure.git",
      branch: "main",
      credential: { kind: "deploy_key" },
      policy: { destructive: "confirm", full_backup: "optional" },
    },
    tip: "0123456789abcdef",
    base_commit: "0123456789abcdef",
    entries: [
      {
        key: "table:books",
        kind: "table",
        name: "books",
        class: "local_change",
        path: "tables/books.json",
        in_registry: true,
      },
    ],
    deploy_blockers: [],
    warnings: [],
  };

  it("lays the branch bar, the groups of changes and the commit form out in the card, not cards of their own", () => {
    const html = renderToStaticMarkup(
      <div className="card">
        <StatusTab status={status} onReload={async () => {}} onOpenRegistry={() => {}} />
      </div>,
    );
    expect(html).toMatch(
      /<div class="d-flex flex-wrap gap-3 align-items-center card-body" data-testid="sync-branch-bar">/,
    );
    expect(html).toMatch(
      /<div class="border-top" data-testid="sync-group-[a-z_]+"><div class="[^"]*bg-surface-tertiary/,
    );
    expect(html).toMatch(/<div class="card-footer">[^]*data-testid="sync-commit"/);
    // The wrapper above stands in for the tabbed card: Status adds none of its own.
    expect(html.match(/class="card"/g)).toHaveLength(1);
  });
});

describe("LinkTab", () => {
  it("has no policy options: data-losing changes are ticked one by one, and a full backup is offered on the review", () => {
    const html = renderToStaticMarkup(<LinkTab onLinked={async () => {}} />);
    expect(html).toContain('id="sync-url"');
    expect(html).not.toContain("sync-destructive");
    expect(html).not.toContain("sync-full-backup");
    expect(html).not.toContain("Changes that lose data");
    expect(html).not.toContain("Full backup before each deploy");
  });
});
