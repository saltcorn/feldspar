// The browser tab's text: the screen's title, then the product.

import { renderToStaticMarkup } from "react-dom/server";
import { createElement } from "react";
import { describe, expect, it } from "vitest";

import { PRODUCT_TITLE, documentTitle } from "./documentTitle";
import { PageHeader } from "./layout";

describe("documentTitle", () => {
  it("puts the screen before the product", () => {
    expect(documentTitle("Tables")).toBe("Tables · Saltcorn");
    expect(documentTitle("books table")).toBe("books table · Saltcorn");
  });

  it("is the product alone while there is no screen title", () => {
    expect(documentTitle("")).toBe(PRODUCT_TITLE);
    expect(documentTitle("   ")).toBe(PRODUCT_TITLE);
    expect(documentTitle(null)).toBe(PRODUCT_TITLE);
    expect(documentTitle(undefined)).toBe(PRODUCT_TITLE);
  });

  it("normalises the whitespace of rendered text", () => {
    // A heading's textContent: a name, a line break from JSX, a badge.
    expect(documentTitle("\n  Run \n  succeeded ")).toBe("Run succeeded · Saltcorn");
  });
});

describe("PageHeader", () => {
  it("still renders its title as the page heading", () => {
    // `docTitle` names the tab only; the heading shows `title`.
    const html = renderToStaticMarkup(
      createElement(PageHeader, { title: "books", docTitle: "books table" }),
    );
    expect(html).toContain('<h2 class="page-title">books</h2>');
    expect(html).not.toContain("books table");
  });
});
