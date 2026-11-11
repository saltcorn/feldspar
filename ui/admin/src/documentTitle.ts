// What the browser tab says: the screen the admin is on, then the product.
//
// A tab that says "Saltcorn" on every screen is a row of identical tabs, and a
// history menu of identical entries. Each screen already states its title once,
// in its `PageHeader`, so that is where the tab's text comes from (see
// `layout.tsx`); this is the pure half, kept apart so it can be tested without
// a DOM.

/** The product name, alone in the tab when there is no screen title yet. */
export const PRODUCT_TITLE = "Saltcorn";

/** `"books · Saltcorn"` for a screen titled "books"; just the product when the
 * title is empty (a screen still loading its record).
 *
 * The title is read from rendered text, so it is normalised: runs of
 * whitespace (the space between a title and a badge, a line break in JSX)
 * become one space, and the ends are trimmed. */
export function documentTitle(screen: string | null | undefined): string {
  const text = (screen ?? "").replace(/\s+/g, " ").trim();
  return text ? `${text} · ${PRODUCT_TITLE}` : PRODUCT_TITLE;
}
