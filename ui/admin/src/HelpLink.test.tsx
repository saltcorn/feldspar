// The help question-mark: where a setting puts it (beside its sublabel, or its
// label when it has none), and the two pure rules the popup follows — which
// links are other topics, and what context the server is asked with.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { helpQuery, helpTopicOfLink } from "./HelpLink";
import { SettingField, type FieldSpec } from "./settings";

const field = (over: Partial<FieldSpec> = {}): FieldSpec => ({
  name: "formula",
  label: "Formula",
  type: "text",
  required: false,
  options: [],
  multiline: false,
  ...over,
});

const render = (spec: FieldSpec) =>
  renderToStaticMarkup(<SettingField field={spec} value="" onChange={() => {}} />);

describe("HelpLink", () => {
  it("is not shown for a setting with no help topic", () => {
    expect(render(field({ sublabel: "Some words." }))).not.toContain("help-link");
  });

  it("sits beside the sublabel when the setting has one", () => {
    const html = render(field({ help_topic: "ownership-formula", sublabel: "Some words." }));
    expect(html).toMatch(/Some words\. <button[^>]*help-link/);
    expect(html).not.toMatch(/<label[^>]*>[^]*help-link[^]*<\/label>/);
  });

  it("sits beside the label when there is no sublabel", () => {
    const html = render(field({ help_topic: "ownership-formula" }));
    expect(html).toMatch(/<label[^>]*>Formula <button[^>]*help-link[^]*<\/label>/);
  });

  it("follows help: links in place and leaves every other link alone", () => {
    expect(helpTopicOfLink("help:javascript-code")).toBe("javascript-code");
    expect(helpTopicOfLink("HELP: ownership-formula")).toBe("ownership-formula");
    expect(helpTopicOfLink("help:")).toBeNull();
    expect(helpTopicOfLink("https://example.com")).toBeNull();
    expect(helpTopicOfLink(null)).toBeNull();
  });

  it("asks the server only with the context the screen has", () => {
    expect(helpQuery({ table: "books", event: "insert" })).toEqual({
      table: "books",
      event: "insert",
    });
    expect(helpQuery({ table: "", event: null })).toEqual({});
  });
});
