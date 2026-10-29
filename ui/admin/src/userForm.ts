// The users screen's decisions, separated from its rendering.
//
// A user is not a fixed form: the users table is the one table an admin is
// invited to add columns to (design §7.1), so the form is generated from what
// `listFields("users")` reports, minus the columns the system owns. That split —
// which columns are the admin's and which are the system's — is the whole of
// what this module knows, plus the two small judgements the screen makes around
// it: which role a new user gets by default, and what a generated password looks
// like when it is handed over.

import type { ListFieldsResponse, ListUsersResponse } from "./client";
import type { Roles } from "./roles";

/** One field of the users table, as `listFields` reports it. */
export type UserField = ListFieldsResponse[number];

/** One user, as `listUsers` reports it. */
export type UserRow = ListUsersResponse[number];

/**
 * The columns the system owns (matching `sc_auth::SYSTEM_USER_COLUMNS`).
 *
 * None of them is a text box on the user form: `id` is generated, `role` has its
 * own select, `email` its own input, `password_hash` is never shown at all, and
 * `disabled` is an action in the row menu rather than a field — disabling
 * somebody is a thing you do, not a checkbox you forget to tick — and `language`
 * is a select over the enabled locales rather than a text box somebody types
 * `french` into. `email_verified` and `attributes` are kept by the system
 * itself (sign-up, emailed links, the checklist user) and are not the admin's
 * to type into either.
 */
export const SYSTEM_USER_COLUMNS = [
  "id",
  "role",
  "email",
  "password_hash",
  "disabled",
  "language",
  "email_verified",
  "attributes",
];

/** The role a new user gets when the admin does not choose: see [defaultUserRole]. */
export const DEFAULT_NEW_USER_ROLE = 80;

/**
 * The admin-added fields of the users table: everything the system does not own,
 * and nothing calculated (a calc field has no column to write).
 */
export function adminUserFields(fields: ListFieldsResponse | null): UserField[] {
  return (fields ?? []).filter(
    (f) =>
      !SYSTEM_USER_COLUMNS.includes(f.name) &&
      (f.kind as { type?: string } | null)?.type !== "calc",
  );
}

/**
 * The role a new user starts on: the one closest to 80.
 *
 * Not the most privileged and — the point — not the least. Public (100) is the
 * role of somebody who has not logged in, so defaulting a *new account* to it
 * creates a user who is indistinguishable from a stranger, which is never what
 * the admin filling in the form meant. 80 is "an ordinary signed-in user" on the
 * 1–100 scale, and since an installation names its own roles, the nearest one to
 * that is the closest thing to that intent that exists here.
 *
 * Ties go to the **lower** number. An installation with roles at 60 and 100 has
 * a tie, and breaking it towards the higher one would land right back on Public
 * — which is the one answer this default exists to avoid. The cost of the other
 * direction is a default one notch more privileged than it might have been, on a
 * form where the role is a visible select the admin can change.
 */
export function defaultUserRole(roles: Roles): number {
  const listed = roles ?? [];
  if (listed.length === 0) return DEFAULT_NEW_USER_ROLE;
  return listed.reduce((best, candidate) => {
    const near = Math.abs(candidate.role - DEFAULT_NEW_USER_ROLE);
    const bestNear = Math.abs(best - DEFAULT_NEW_USER_ROLE);
    if (near < bestNear) return candidate.role;
    if (near === bestNear) return Math.min(best, candidate.role);
    return best;
  }, listed[0].role);
}

/** The form's state: the four system inputs plus one string per admin field. */
export type UserForm = {
  email: string;
  /** Blank means "generate one" when creating and "leave it alone" when editing. */
  password: string;
  role: number;
  /**
   * The BCP-47 tag this account reads the product in; **blank is "site
   * default"**, which is a real choice rather than an unfilled box — it is what
   * clears a language somebody set earlier.
   */
  language: string;
  extra: Record<string, string>;
};

/** A blank form for a new user, on the default role and no stated language. */
export function newUserForm(roles: Roles): UserForm {
  return { email: "", password: "", role: defaultUserRole(roles), language: "", extra: {} };
}

/** The form for editing an existing user — never carrying a password. */
export function editUserForm(user: UserRow, fields: ListFieldsResponse | null): UserForm {
  const extra: Record<string, string> = {};
  const bag = (user.extra ?? {}) as Record<string, unknown>;
  for (const field of adminUserFields(fields)) {
    extra[field.name] = displayValue(bag[field.name]);
  }
  return {
    email: user.email,
    password: "",
    role: user.role,
    language: user.language ?? "",
    extra,
  };
}

/** A stored value as the text box shows it: JSON for anything that is not a scalar. */
export function displayValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

/**
 * A typed form value on its way back to the server. The server coerces to the
 * column's real type, so this only has to turn the obvious scalars — `5`,
 * `true`, `null` — into their JSON form and leave text as text.
 */
export function parseValue(raw: string): unknown {
  const trimmed = raw.trim();
  if (trimmed === "") return null;
  try {
    return JSON.parse(trimmed) as unknown;
  } catch {
    return raw;
  }
}

/**
 * The create/update body for a form. Both endpoints take the same shape.
 *
 * `language` is sent as `null` when the select is on "Site default", never
 * omitted: an absent `language` tells the server to leave the stored one alone,
 * and this form always knows what it means.
 */
export function userBody(form: UserForm): {
  email: string;
  password: string;
  role: number;
  language: string | null;
  extra: Record<string, unknown>;
} {
  const extra: Record<string, unknown> = {};
  for (const [name, raw] of Object.entries(form.extra)) {
    extra[name] = parseValue(raw);
  }
  return {
    email: form.email.trim(),
    password: form.password,
    role: form.role,
    language: form.language.trim() === "" ? null : form.language.trim(),
    extra,
  };
}

/** Credentials to hand over out of band, and where they are used. */
export type Credentials = { email: string; password: string; url: string };

/**
 * The block of text the credentials dialog shows and its copy button copies.
 *
 * One block rather than three fields with three copy buttons: what the admin is
 * about to do is paste this into a message to one person, and a password without
 * the address it belongs to — or without saying where to sign in — arrives as a
 * puzzle.
 */
export function credentialsText({ email, password, url }: Credentials): string {
  return `Site:     ${url}\nEmail:    ${email}\nPassword: ${password}`;
}

/** The URL to tell a new user to sign in at: this admin's own origin. */
export function loginUrl(): string {
  return window.location.origin;
}
