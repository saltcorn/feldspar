// The front page: Workspaces, then Datasets, then Models. Each section's
// heading row carries its secondary buttons and then, last, its "+ New …"
// button, as the admin UI's page headers do; no section has an inline card
// for making one — the datasets and workspaces ask for theirs in a dialog.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { Home } from "./Home";

describe("the front page", () => {
  const html = renderToStaticMarkup(<Home />);

  it("lists the workspaces, then the datasets, then the models", () => {
    const at = (section: string) => html.indexOf(`>${section}</h2>`);
    expect(at("Workspaces")).toBeGreaterThan(-1);
    expect(at("Workspaces")).toBeLessThan(at("Datasets"));
    expect(at("Datasets")).toBeLessThan(at("Models"));
  });

  it("ends each heading row with its plus button, after the secondary ones", () => {
    for (const [section, secondary, label] of [
      ["Workspaces", [], "New workspace"],
      ["Datasets", ["Get datasets"], "New dataset"],
      ["Models", ["Compare"], "New model"],
    ] as const) {
      const before = secondary.map((text) => `<button[^>]*>${text}</button>`).join("");
      const row = new RegExp(
        `<h2[^>]*>${section}</h2>${before}<button[^>]*><svg[^>]*class="icon icon-2"[^>]*>.*?</svg>${label}</button></div>`,
      );
      expect(html).toMatch(row);
    }
  });

  it("has no inline creation form", () => {
    expect(html).not.toContain('id="dataset-name"');
    expect(html).not.toContain('id="workspace-name"');
  });
});
