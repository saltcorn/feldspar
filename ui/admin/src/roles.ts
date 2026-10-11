// Roles as the admin picks them.
//
// A role is a row in `_fd_roles` (design §7.1, §9) carrying a number on the
// fixed 1–100 scale, a name, and role-specific settings; `users.role` is a
// foreign key onto it. `listRoles` reports what exists, so this module is the
// client half: load the list once per screen, and render a role number as the
// name an admin gave it.

import { useEffect, useState } from "react";

import { api } from "./api";
import type { ListRolesResponse } from "./client";

/** The role choices, or `null` while they are still loading. */
export type Roles = ListRolesResponse | null;

/** Load the role choices once. Failure yields an empty list, not a crash. */
export function useRoles(): Roles {
  const [roles, setRoles] = useState<Roles>(null);
  useEffect(() => {
    void (async () => {
      try {
        setRoles(await api.listRoles());
      } catch {
        setRoles([]);
      }
    })();
  }, []);
  return roles;
}

/**
 * A role rendered for a human: its name and number, or just its number when no
 * such role exists — which is what a database predating the foreign key can
 * still contain, and is worth showing as-is rather than hiding.
 */
export function roleLabel(role: number, roles: Roles): string {
  const named = roles?.find((r) => r.role === role);
  return named ? `${named.name} (${role})` : `Role ${role}`;
}

/**
 * The choices to offer for a role select, with `current` included even when the
 * server did not list it. A `null` current is a field with no role set, which
 * adds nothing: what the blank stands for is the caller's to name.
 *
 * A table configured for a role that has since been deleted must still show that
 * role as its current value — a select that silently snapped to the nearest
 * listed one would change who can reach the data the next time the form was
 * saved, and would do it without saying so.
 */
export function roleOptions(
  current: number | null,
  roles: Roles,
): { role: number; name: string }[] {
  const listed = (roles ?? []).map((r) => ({ role: r.role, name: r.name }));
  const options =
    current == null || listed.some((r) => r.role === current)
      ? listed
      : [...listed, { role: current, name: `Role ${current}` }];
  return options.sort((a, b) => a.role - b.role);
}

/** The role number anyone has, signed in or not (`sc_auth::ROLE_PUBLIC`). */
export const PUBLIC_ROLE = 100;

/** Whether a role floor lets anyone in — what the admin UI calls **Public**
 * (TODO F1.6). Role 100 stays the one stored fact; this only names it. */
export function isPublic(role: number | null | undefined): boolean {
  return role === PUBLIC_ROLE;
}
