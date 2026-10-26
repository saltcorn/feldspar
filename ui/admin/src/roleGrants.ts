// Role grants on the users screen (TODO-payments P3.2).
//
// A user's role is the base role on their account *and* the grants they hold
// for a while: a subscription's, a trial's, one an admin gave by hand. The one
// they are served at — the effective role — is the most privileged of those.
// This module is the screen's pure half: what state a grant is in, how its
// source reads, and the body the "Grant a role" form sends.

import type { GrantUserRoleRequest, ListUserRoleGrantsResponse } from "./client";
import { format, type Translator } from "./i18n";

/** One grant, as `listUserRoleGrants` answers it. */
export type RoleGrantRow = ListUserRoleGrantsResponse["grants"][number];

/** Where a grant stands: counting now, still to start, run out, or revoked. */
export type GrantState = "in_force" | "pending" | "ended" | "revoked";

export function grantState(grant: RoleGrantRow, now: Date = new Date()): GrantState {
  if (grant.revoked_at) return "revoked";
  if (new Date(grant.starts_at) > now) return "pending";
  if (grant.expires_at && new Date(grant.expires_at) <= now) return "ended";
  return "in_force";
}

/** The badge words and tone for a state. */
export function grantStateBadge(
  state: GrantState,
  t: Translator["t"] = format,
): { label: string; tone: "green" | "blue" | "secondary" | "red" } {
  switch (state) {
    case "in_force":
      return { label: t("In force"), tone: "green" };
    case "pending":
      return { label: t("Not started"), tone: "blue" };
    case "ended":
      return { label: t("Ended"), tone: "secondary" };
    case "revoked":
      return { label: t("Revoked"), tone: "red" };
  }
}

/**
 * Who made a grant, for a human. An admin's own grants carry a random reference
 * that means nothing to read, so it is left out; a module's or a trigger's
 * reference (a subscription, `trial:…`) is what tells two grants apart.
 */
export function grantSourceLabel(grant: RoleGrantRow): string {
  if (grant.source === "admin") return "By an admin";
  if (grant.source === "grant_role") return `Trigger: ${grant.source_ref}`;
  return `${grant.source}: ${grant.source_ref}`;
}

/** The "Grant a role" form. Days blank means until revoked. */
export type GrantForm = { role: number | null; days: string; note: string };

export function newGrantForm(): GrantForm {
  return { role: null, days: "30", note: "" };
}

/**
 * The request body, or a sentence saying what is wrong with the form. The
 * server checks the same things; checking here keeps the dialog open with the
 * reason beside the box rather than after a round trip.
 */
export function grantBody(form: GrantForm): GrantUserRoleRequest | string {
  if (form.role == null || form.role < 1 || form.role >= 100) {
    return "Choose a role to grant.";
  }
  const days = form.days.trim();
  let expires_in_days: number | null = null;
  if (days !== "") {
    const n = Number(days);
    if (!Number.isInteger(n) || n < 1) {
      return "Days must be a whole number of at least 1, or blank for until revoked.";
    }
    expires_in_days = n;
  }
  const note = form.note.trim();
  return { role: form.role, expires_in_days, note: note === "" ? null : note };
}

/** Whether the effective role differs from the base, and so is worth saying. */
export function servedAbove(grants: ListUserRoleGrantsResponse): boolean {
  return grants.effective_role !== grants.base_role;
}
