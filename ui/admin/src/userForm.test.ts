/**
 * The users screen's decisions, tested where they are: in `userForm.ts`.
 *
 * Four claims:
 *
 *   - the form is generated from the users table, so a column an admin added is
 *     a field and a column the system owns is not;
 *   - a new user defaults to the role nearest 80 — never Public, which is the
 *     role of somebody who has not signed in at all;
 *   - editing carries the user's values and never a password, and saving one
 *     sends a blank password, which is what "leave it alone" is on the wire;
 *   - the credentials handed over say where to sign in, not just what with.
 */

import { describe, expect, it } from "vitest";

import {
  adminUserFields,
  credentialsText,
  defaultUserRole,
  editUserForm,
  newUserForm,
  SYSTEM_USER_COLUMNS,
  userBody,
} from "./userForm";
import type { ListFieldsResponse, ListUsersResponse } from "./client";

const field = (name: string, extra: Partial<ListFieldsResponse[number]> = {}) =>
  ({
    name,
    label: "",
    description: "",
    sql_type: "text",
    type: "String",
    nullable: true,
    required: false,
    unique: false,
    primary_key: false,
    kind: { type: "plain" },
    attributes: {},
    ...extra,
  }) as ListFieldsResponse[number];

/** The users table as it is after an admin adds two columns to it. */
const fields: ListFieldsResponse = [
  field("id", { primary_key: true, sql_type: "uuid" }),
  field("role", { sql_type: "int" }),
  field("email"),
  field("password_hash"),
  field("disabled", { sql_type: "bool" }),
  field("language"),
  field("email_verified", { sql_type: "bool" }),
  field("attributes", { sql_type: "jsonb" }),
  field("nickname"),
  field("full_name", { kind: { type: "calc", expression: "nickname" } }),
];

const user: ListUsersResponse[number] = {
  id: "0f7d1f8e-1111-4222-8333-444455556666",
  email: "sam@example.com",
  role: 40,
  disabled: false,
  language: null,
  check_user: false,
  extra: { nickname: "Sam" },
};

describe("adminUserFields", () => {
  it("is the admin's columns and nothing the system owns", () => {
    expect(adminUserFields(fields).map((f) => f.name)).toEqual(["nickname"]);
  });

  // `language` is the system's (§16.1): it has a select of its own, so it must
  // not also appear as a generic text box.
  it("does not offer the language column as a text box", () => {
    expect(SYSTEM_USER_COLUMNS).toContain("language");
    expect(adminUserFields(fields).map((f) => f.name)).not.toContain("language");
  });

  // The server refuses either in `extra`, so offering them made every edit fail.
  it("does not offer email_verified or attributes as fields", () => {
    const names = adminUserFields(fields).map((f) => f.name);
    expect(names).not.toContain("email_verified");
    expect(names).not.toContain("attributes");
  });

  it("leaves out calculated fields, which have no column to write", () => {
    expect(adminUserFields(fields).map((f) => f.name)).not.toContain("full_name");
  });

  it("is empty, not a crash, before the fields have loaded", () => {
    expect(adminUserFields(null)).toEqual([]);
  });
});

describe("defaultUserRole", () => {
  const role = (n: number, name: string) => ({
    role: n,
    name,
    description: "",
    builtin: false,
  });

  it("is the role closest to 80", () => {
    expect(
      defaultUserRole([role(1, "Admin"), role(40, "Staff"), role(85, "Member"), role(100, "Public")]),
    ).toBe(85);
  });

  it("breaks a tie towards the lower number, so a tie never lands on Public", () => {
    // 70 and 90 are both ten away.
    expect(defaultUserRole([role(1, "Admin"), role(70, "Staff"), role(90, "Member")])).toBe(70);
    // And this is the tie that matters: 60 and 100 are both twenty away, and
    // the whole point of the default is not to be 100.
    expect(defaultUserRole([role(1, "Admin"), role(60, "Staff"), role(100, "Public")])).toBe(60);
  });

  it("falls back to Public only when Public is genuinely the nearest role", () => {
    // With just the two built-ins there is nothing else to pick: 100 is twenty
    // away and 1 is seventy-nine.
    expect(defaultUserRole([role(1, "Admin"), role(100, "Public")])).toBe(100);
    // But as soon as an ordinary role exists, that is the default — the point of
    // the whole rule, since a new account must not be a stranger by default.
    expect(defaultUserRole([role(1, "Admin"), role(75, "Staff"), role(100, "Public")])).toBe(75);
  });

  it("holds its default while the roles are still loading", () => {
    expect(defaultUserRole(null)).toBe(80);
  });
});

describe("the form", () => {
  it("starts a new user blank, on the default role", () => {
    const form = newUserForm([
      { role: 1, name: "Admin", description: "", builtin: true },
      { role: 75, name: "Member", description: "", builtin: false },
    ]);
    expect(form).toEqual({ email: "", password: "", role: 75, language: "", extra: {} });
  });

  it("carries an existing user's values, and never a password", () => {
    expect(editUserForm(user, fields)).toEqual({
      email: "sam@example.com",
      password: "",
      role: 40,
      language: "",
      extra: { nickname: "Sam" },
    });
  });

  it("sends a blank password, which is what leaves the stored one alone", () => {
    const body = userBody(editUserForm(user, fields));
    expect(body.password).toBe("");
    expect(body.email).toBe("sam@example.com");
    expect(body.role).toBe(40);
    expect(body.extra).toEqual({ nickname: "Sam" });
  });

  it("trims the email and turns obvious scalars into JSON", () => {
    const body = userBody({
      email: "  new@example.com ",
      password: "chosen",
      role: 40,
      language: "",
      extra: { nickname: "", age: "37", member: "true" },
    });
    expect(body.email).toBe("new@example.com");
    // An empty box is null, not the empty string: a field nobody filled in has
    // no value rather than a blank one.
    expect(body.extra).toEqual({ nickname: null, age: 37, member: true });
  });

  // §16.1: the language box has three states and the body has to distinguish
  // two of them, because the server reads an absent `language` as "leave it
  // alone" and an explicit null as "back to the site default".
  it("sends null for the site default and the tag otherwise", () => {
    const base = { email: "a@example.com", password: "", role: 40, extra: {} };
    expect(userBody({ ...base, language: "" }).language).toBeNull();
    expect(userBody({ ...base, language: "   " }).language).toBeNull();
    expect(userBody({ ...base, language: " pt-BR " }).language).toBe("pt-BR");
  });

  it("carries a stored language onto the form", () => {
    const french = { ...user, language: "fr" };
    expect(editUserForm(french, fields).language).toBe("fr");
  });
});

describe("credentialsText", () => {
  it("says where to sign in, not only what with", () => {
    const text = credentialsText({
      email: "sam@example.com",
      password: "abc123",
      url: "https://example.com",
    });
    expect(text).toContain("https://example.com");
    expect(text).toContain("sam@example.com");
    expect(text).toContain("abc123");
    // Three lines, so it pastes into a message as one readable block.
    expect(text.split("\n")).toHaveLength(3);
  });
});
