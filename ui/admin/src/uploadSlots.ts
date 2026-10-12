// The application form's **upload slots** (TODO F2, §2.3): named places a
// client uploads a large file to, resumably, at `{api mount}/_upload/{name}`.
//
// The form edits every setting as text, in one repeating row per slot, so this
// module is the translation between that and the record — and, like
// `staticDirs.ts`, a decision testable without a browser
// (`uploadSlots.test.ts`): a size typed as `1G`, a type list typed with commas,
// and a blank hook are what an admin writes, and what the server stores is
// bytes, a list and `null`.

import type { CreateApplicationRequest } from "./client";

/** One upload slot, as the application record carries it. */
export type UploadSlotItem = CreateApplicationRequest["uploads"][number];

/** One row of the upload slots list, every cell as the form edits it. */
export type UploadRow = {
  name: string;
  store: string;
  folder: string;
  min_role: string;
  max_bytes: string;
  mime_allow: string;
  hook: string;
};

/** An empty row — what "Add" produces. Admin-only until the admin says
 * otherwise, as a custom query is. */
export function blankUploadRow(): UploadRow {
  return {
    name: "",
    store: "",
    folder: "",
    min_role: "1",
    max_bytes: "",
    mime_allow: "",
    hook: "",
  };
}

const UNITS: Record<string, number> = {
  "": 1,
  K: 1024,
  M: 1024 ** 2,
  G: 1024 ** 3,
};

/** A size as the form shows it: whole binary units where they fit, empty for
 * "the server's limit" (0). */
export function formatBytes(bytes: number): string {
  if (!bytes) return "";
  for (const unit of ["G", "M", "K"]) {
    const scale = UNITS[unit];
    if (bytes % scale === 0) return `${bytes / scale}${unit}`;
  }
  return String(bytes);
}

/** A size the admin typed: bytes, or a number with `K`, `M` or `G`. Empty is
 * 0, the server's own limit. `null` when it is not a size. */
export function parseBytes(text: string): number | null {
  const match = /^\s*(\d+(?:\.\d+)?)\s*([kmg]?)b?\s*$/i.exec(text);
  if (!text.trim()) return 0;
  if (!match) return null;
  return Math.round(Number(match[1]) * UNITS[match[2].toUpperCase()]);
}

/** The stored slots as rows. */
export function uploadRowsFromApp(slots: UploadSlotItem[]): UploadRow[] {
  return slots.map((s) => ({
    name: s.name,
    store: s.store,
    folder: s.folder,
    min_role: String(s.min_role),
    max_bytes: formatBytes(s.max_bytes),
    mime_allow: s.mime_allow.join(", "),
    hook: s.hook ?? "",
  }));
}

/** The rows as the application record stores them. A row with neither a name
 * nor a store is one added and never filled in, and is dropped. A size or role
 * that does not parse is thrown with the slot's name, so the form says which
 * cell to fix; everything else is the server's to check. */
export function uploadRowsToRequest(rows: UploadRow[]): UploadSlotItem[] {
  return rows
    .filter((r) => r.name.trim() || r.store.trim())
    .map((r) => {
      const name = r.name.trim();
      const maxBytes = parseBytes(r.max_bytes);
      if (maxBytes === null) {
        throw new Error(
          `Upload slot “${name}”: “${r.max_bytes}” is not a size (e.g. 500M or 1G).`,
        );
      }
      const minRole = Number(r.min_role.trim() || "1");
      if (!Number.isInteger(minRole)) {
        throw new Error(
          `Upload slot “${name}”: the minimum role should be a role number.`,
        );
      }
      return {
        name,
        store: r.store.trim(),
        folder: r.folder.trim().replace(/^\/+|\/+$/g, ""),
        min_role: minRole,
        max_bytes: maxBytes,
        mime_allow: r.mime_allow
          .split(",")
          .map((m) => m.trim())
          .filter(Boolean),
        hook: r.hook.trim() || null,
      };
    });
}
