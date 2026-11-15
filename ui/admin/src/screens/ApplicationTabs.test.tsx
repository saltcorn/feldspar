// An application's settings tabs are an underlined nav (Tabler's
// `.nav-underline`), the same strip the global Settings screen uses, rather
// than pills, which barely stand out from the page under Tabler.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { ApplicationTabs } from "./ApplicationViews";

describe("ApplicationTabs", () => {
  it("renders as an underlined nav with the current tab active", () => {
    const html = renderToStaticMarkup(
      <ApplicationTabs
        app={{
          id: "a1",
          settings_tabs: [
            { id: "site", label: "Site", groups: ["identity"] },
            { id: "views", label: "Views", groups: ["views"] },
          ],
        }}
        active="views"
      />,
    );
    expect(html).toMatch(/^<ul class="nav nav-underline mb-3">/);
    expect(html).not.toContain("nav-pills");
    expect(html).toMatch(/class="nav-link active" aria-current="page" href="#\/applications\/a1\/views"/);
  });
});
