// Settings in groups under headings (v1's `section_header`).
//
// A setting may open a group: its `section` is the group's heading, and the
// settings after it belong to that group until the next one opens. A heading
// is drawn above the first *shown* setting of its group, so a group whose
// opening setting is hidden by its `show_if` keeps its heading, and a group
// with nothing shown has none.
//
// A module rather than inline in `SettingsFields` so the grouping is testable
// without a browser (`settingSections.test.ts`).

import type { FieldSpec } from "./settings";

/** One thing to draw: a group's heading, or a setting. */
export type SectionItem =
  | { kind: "heading"; key: string; label: string }
  | { kind: "field"; field: FieldSpec };

/** `spec` as headings and the settings `shown` keeps, in order. */
export function withSections(
  spec: readonly FieldSpec[],
  shown: (field: FieldSpec) => boolean,
): SectionItem[] {
  const out: SectionItem[] = [];
  let group: { start: string; label: string } | null = null;
  let drawn: string | null = null;
  for (const field of spec) {
    const label = field.section?.trim();
    if (label) group = { start: field.name, label };
    if (!shown(field)) continue;
    if (group && drawn !== group.start) {
      out.push({ kind: "heading", key: `section-${group.start}`, label: group.label });
      drawn = group.start;
    }
    out.push({ kind: "field", field });
  }
  return out;
}
