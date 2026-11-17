/**
 * A settings form must not be filled in by the browser's saved sign-in.
 *
 * The Email card has a "Username" box above a "Password" box, which a browser
 * takes for a login form: it filled in the admin's own email and password,
 * the form counted that as an edit, and the card's Apply button lit up with
 * nothing changed — and would have saved the admin's password as the SMTP one.
 */

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { SettingField, type FieldSpec } from "./settings";

function field(extra: Partial<FieldSpec>): FieldSpec {
  return {
    name: "smtp_password",
    label: "Password",
    type: "text",
    required: false,
    default: null,
    options: [],
    multiline: false,
    ...extra,
  };
}

const render = (spec: FieldSpec) =>
  renderToStaticMarkup(<SettingField field={spec} value="" onChange={() => {}} />);

describe("a setting's box and the browser's saved sign-in", () => {
  it("asks for a new password on a secret, which browsers do not fill", () => {
    const html = render(field({ secret: true }));
    expect(html).toContain('type="password"');
    expect(html).toContain('autoComplete="new-password"');
  });

  it("turns autofill off on a plain box, such as the username above it", () => {
    expect(render(field({ name: "smtp_username", label: "Username" }))).toContain(
      'autoComplete="off"',
    );
  });
});
