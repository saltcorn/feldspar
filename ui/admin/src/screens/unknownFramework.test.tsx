// The red box on an application's settings when the server does not have its
// framework (the Analytics UI, switched off for the first release, or an
// uninstalled module's): the application is kept, but not served or built.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { UnknownFrameworkAlert } from "./ApplicationForm";

describe("the unknown framework alert", () => {
  it("says the framework is unknown, and names it, in a red box", () => {
    const html = renderToStaticMarkup(
      <UnknownFrameworkAlert
        app={{
          framework: { name: "analytics", config: {} },
          framework_known: false,
        }}
      />,
    );
    expect(html).toContain("alert-danger");
    expect(html).toContain("Unknown framework");
    expect(html).toContain("analytics");
  });

  it("shows nothing for an application whose framework the server has", () => {
    const html = renderToStaticMarkup(
      <UnknownFrameworkAlert
        app={{
          framework: { name: "react", config: {} },
          framework_known: true,
        }}
      />,
    );
    expect(html).toBe("");
  });
});
