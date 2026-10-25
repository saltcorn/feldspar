// The application picker's markup: a dropdown toggle with a caption over the
// current name, and a menu that ticks the current application and keeps "All
// applications" and "New application" at the foot.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { AppPicker, type PickerApp } from "./AppPicker";

const apps: PickerApp[] = [
  { id: "a", name: "Bookshop" },
  { id: "b", name: "Helpdesk" },
];

function render(props: Partial<Parameters<typeof AppPicker>[0]> = {}) {
  return renderToStaticMarkup(
    <AppPicker
      apps={apps}
      current={apps[1]}
      open={false}
      path="/entities"
      listActive={false}
      folded={false}
      onToggle={() => {}}
      onChoose={() => {}}
      onClose={() => {}}
      {...props}
    />,
  );
}

describe("AppPicker", () => {
  it("shows an Application caption over the current name, with a caret", () => {
    const html = render();
    expect(html).toMatch(
      /app-picker-caption">Application<\/span><span class="app-picker-name">Helpdesk</,
    );
    expect(html).toContain("app-picker-caret");
    expect(html).toContain('aria-expanded="false"');
    // A dropdown toggle, not one of Tabler's inline submenu toggles.
    expect(html).not.toContain("dropdown-toggle");
    expect(html).not.toContain("<select");
  });

  it("is closed until opened, and then shows the menu", () => {
    expect(render()).not.toMatch(/app-picker-menu show/);
    const html = render({ open: true });
    expect(html).toMatch(/app-picker-menu show/);
    expect(html).toContain('aria-expanded="true"');
  });

  it("ticks only the current application", () => {
    const html = render({ open: true });
    const items = html.match(/<button[^>]*role="menuitemradio"[\s\S]*?<\/button>/g) ?? [];
    expect(items).toHaveLength(2);
    expect(items[0]).toContain('aria-checked="false"');
    expect(items[0]).not.toContain("<svg");
    expect(items[1]).toContain('aria-checked="true"');
    expect(items[1]).toContain("<svg");
  });

  it("puts All applications and New application after the names", () => {
    const html = render({ open: true });
    const last = html.lastIndexOf("Helpdesk");
    expect(html.indexOf("All applications")).toBeGreaterThan(last);
    expect(html.indexOf("New application")).toBeGreaterThan(html.indexOf("All applications"));
  });

  it("says so when there is no current application", () => {
    expect(render({ current: null })).toContain("Choose application");
    expect(render({ apps: [], current: null })).toContain("No applications");
  });
});
