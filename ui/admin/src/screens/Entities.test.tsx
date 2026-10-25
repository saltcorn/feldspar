// The entities list's shape: the table scrolls inside the card, and the
// "Create …" buttons sit outside that scroll, at the card's foot, so they stay
// in view however long the list is.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { Entities } from "./Entities";

describe("Entities", () => {
  const html = renderToStaticMarkup(<Entities />);

  it("claims the viewport, with the table in its own scroll", () => {
    expect(html).toMatch(/^<div class="entities-page">/);
    expect(html).toContain('<div class="entities-scroll"><table');
  });

  it("keeps the create buttons outside the scrolling area", () => {
    const scrollEnd = html.indexOf("</table></div>");
    const footer = html.indexOf('<div class="card-footer">');
    expect(scrollEnd).toBeGreaterThan(0);
    expect(footer).toBeGreaterThan(scrollEnd);
    expect(html.slice(footer)).toContain("Create table");
  });
});
