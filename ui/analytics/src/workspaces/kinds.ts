// The workspace kinds, as the create dialog offers them (analytics TODO A1.15):
// all eight, the ones not here yet disabled and marked "not yet available" —
// so the list shows where the Analytics UI is going, and nothing in it
// opens onto nothing.

import type { ListWorkspaceKindsResponse } from "../client";
import { workspaceKindName } from "../labels";

/** One kind, from `listWorkspaceKinds`. */
export type KindItem = ListWorkspaceKindsResponse[number];

/** One option of the kind picker. */
export type KindOption = { value: string; label: string; disabled: boolean };

/** The kind picker's options, in the server's order. */
export function kindOptions(
  kinds: KindItem[],
  t: (text: string, args?: Record<string, string | number>) => string,
): KindOption[] {
  return kinds.map((k) => ({
    value: k.kind,
    label: k.available
      ? workspaceKindName(k.kind, t)
      : t("{kind} (not yet available)", { kind: workspaceKindName(k.kind, t) }),
    disabled: !k.available,
  }));
}

/** The first kind that can be created — what the picker starts on. */
export function firstAvailable(kinds: KindItem[]): string {
  return kinds.find((k) => k.available)?.kind ?? "";
}

/** A kind's label, or its name when the server did not list it. */
export function kindLabel(kinds: KindItem[], kind: string): string {
  return kinds.find((k) => k.kind === kind)?.label ?? kind;
}
